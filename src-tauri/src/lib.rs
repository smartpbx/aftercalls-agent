mod config;
mod detector;
mod pipeline;
mod portal;
mod recorder;
mod summary;
mod transcription;
mod upload;
mod vault;

use detector::{Detector, UserDecision};
use recorder::Recorder;
use serde::Serialize;
use std::path::PathBuf;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

// Holds references to tray menu items we need to mutate (toggle label) so we
// can fetch them out of app state instead of hunting through the menu tree.
struct TrayItems {
    toggle: MenuItem<Wry>,
}

#[derive(Clone, Copy)]
enum TrayState {
    Idle,
    Recording,
    Processing,
}

pub(crate) fn tray_set_processing(app: &AppHandle) {
    apply_tray_state(app, TrayState::Processing);
}
pub(crate) fn tray_set_idle(app: &AppHandle) {
    apply_tray_state(app, TrayState::Idle);
}

fn apply_tray_state(app: &AppHandle, state: TrayState) {
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    // tauri::include_image! decodes the PNG at compile time into raw RGBA,
    // so this is a zero-IO, zero-decode swap at runtime.
    let (img, tip): (Image<'static>, &str) = match state {
        TrayState::Idle => (
            tauri::include_image!("icons/tray-idle.png"),
            "aftercalls — idle",
        ),
        TrayState::Recording => (
            tauri::include_image!("icons/tray-recording.png"),
            "aftercalls — recording",
        ),
        TrayState::Processing => (
            tauri::include_image!("icons/tray-processing.png"),
            "aftercalls — processing",
        ),
    };
    let _ = tray.set_icon(Some(img));
    let _ = tray.set_tooltip(Some(tip));

    // Flip the start/stop menu label to match.
    if let Some(items) = app.try_state::<TrayItems>() {
        let label = match state {
            TrayState::Recording => "Stop recording",
            _ => "Start recording",
        };
        let _ = items.toggle.set_text(label);
    }
}

#[derive(Serialize, Clone)]
struct RecordingStateEvent {
    recording: bool,
}

/// Writes a small metadata file into a newly-created session_dir capturing
/// how the recording started and what (if any) app triggered it. The
/// pipeline reads this at upload time so the backend knows whether a call
/// was auto-detected from Teams, manually started, or imported from a file.
pub(crate) fn write_session_source(
    session_dir: &std::path::Path,
    kind: &str,
    app: Option<&str>,
) {
    let payload = serde_json::json!({
        "kind": kind,
        "app": app,
    });
    let path = session_dir.join("source.json");
    if let Err(e) = std::fs::write(&path, payload.to_string()) {
        eprintln!(
            "aftercalls: failed to write source.json for {}: {e}",
            session_dir.display()
        );
    }
}

fn emit_state(app: &AppHandle, recording: bool) {
    let _ = app.emit("recording-state", RecordingStateEvent { recording });
    apply_tray_state(
        app,
        if recording {
            TrayState::Recording
        } else {
            // Pipeline handler will bump us to Processing immediately after stop;
            // this covers the "start failed" / "no recording" case.
            TrayState::Idle
        },
    );
}

pub(crate) fn do_start(state: &Recorder, app: &AppHandle) -> Result<String, String> {
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    let path = state.start(base)?;
    emit_state(app, true);
    Ok(path.to_string_lossy().into_owned())
}

pub(crate) fn do_stop(state: &Recorder, app: &AppHandle) -> Result<String, String> {
    let path: PathBuf = state.stop()?;
    emit_state(app, false);
    let session_dir = path.clone();
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        pipeline::run(session_dir, app_clone).await;
    });
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
fn start_recording(state: State<Recorder>, app: AppHandle) -> Result<String, String> {
    let path = do_start(&state, &app)?;
    write_session_source(std::path::Path::new(&path), "manual", None);
    Ok(path)
}

#[tauri::command]
fn stop_recording(state: State<Recorder>, app: AppHandle) -> Result<String, String> {
    do_stop(&state, &app)
}

#[derive(serde::Serialize)]
struct RecordingStatus {
    recording: bool,
    // Unix-ms timestamp of start; None when idle. Lets the UI rebuild
    // the running timer after a webview remount (tray hide+show,
    // route nav) rather than restarting from 00:00.
    started_at_ms: Option<i64>,
}

// Point-in-time query used by the Record page on mount. The
// "recording-state" event only fires on transitions, so a page that
// remounts mid-recording has no other way to learn the current state.
#[tauri::command]
fn is_recording(state: State<Recorder>) -> RecordingStatus {
    RecordingStatus {
        recording: state.is_active(),
        started_at_ms: state.started_at_ms(),
    }
}

#[tauri::command]
async fn process_imported_file(app: AppHandle, source_path: String) -> Result<String, String> {
    let src = std::path::PathBuf::from(&source_path);
    if !src.exists() {
        return Err(format!("file not found: {source_path}"));
    }
    // New session dir named after the import moment so it sorts alongside real
    // recordings. The "imp_" prefix is just for humans eyeballing the folder.
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings")
        .join(format!("imp_{stamp}"));
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;

    // Normalize whatever the user picked (mp3/m4a/mp4/etc.) into WAV so the
    // pipeline's AssemblyAI upload path (which re-encodes to Opus anyway) gets
    // a consistent input. Stored as system.wav so diarization kicks in — a
    // Zoom/Meet export usually has multiple voices mixed together.
    let dest = base.join("system.wav");
    let status = tokio::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&src)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg(&dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| format!("run ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg exited with {status} (unsupported format?)"));
    }

    let source_app = src.file_name().map(|s| s.to_string_lossy().into_owned());
    write_session_source(&base, "imported", source_app.as_deref());

    let session_dir = base.clone();
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        pipeline::run(session_dir, app_clone).await;
    });
    Ok(base.to_string_lossy().into_owned())
}

#[tauri::command]
fn confirm_auto_start(detector: State<Detector>) {
    detector.decide(UserDecision::ConfirmStart);
}

#[tauri::command]
fn dismiss_auto_start(detector: State<Detector>) {
    detector.decide(UserDecision::DismissStart);
}

#[tauri::command]
fn confirm_auto_end(detector: State<Detector>) {
    detector.decide(UserDecision::ConfirmEnd);
}

#[tauri::command]
fn keep_auto_recording(detector: State<Detector>) {
    detector.decide(UserDecision::KeepRecording);
}

#[tauri::command]
async fn list_calls() -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::list_calls(backend).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_call(id: String) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::get_call(backend, &id).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn get_session_audio_path(
    app: AppHandle,
    session_id: String,
    track: String,
) -> Result<String, String> {
    if !matches!(track.as_str(), "mic" | "system" | "mixed") {
        return Err("invalid track".into());
    }
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings")
        .join(&session_id)
        .join(format!("{track}.wav"));
    if !dir.exists() {
        return Err(format!("not found: {}", dir.display()));
    }
    Ok(dir.to_string_lossy().into_owned())
}

#[derive(Serialize)]
struct LoginResult {
    email: String,
    display_name: String,
    role: String,
    org_display_name: String,
}

#[tauri::command]
async fn login(email: String, password: String) -> Result<LoginResult, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    let auth = portal::login(backend, &email, &password)
        .await
        .map_err(|e| e.to_string())?;
    Ok(LoginResult {
        email: auth.email,
        display_name: auth.display_name,
        role: auth.role,
        org_display_name: auth.org_display_name,
    })
}

#[tauri::command]
async fn logout() -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::logout(backend).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn current_user() -> Result<Option<LoginResult>, String> {
    let auth = config::read_auth_file().map_err(|e| e.to_string())?;
    Ok(auth.map(|a| LoginResult {
        email: a.email,
        display_name: a.display_name,
        role: a.role,
        org_display_name: a.org_display_name,
    }))
}

#[tauri::command]
async fn get_org_vocab() -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::get_org_vocab(backend)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_org_vocab(
    custom_spelling: serde_json::Value,
    word_boost: Vec<String>,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::set_org_vocab(backend, &custom_spelling, &word_boost)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_highlights(call_id: String) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::list_highlights(backend, &call_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_highlight(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::create_highlight(backend, &call_id, &body)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_highlight(id: String, body: serde_json::Value) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::update_highlight(backend, &id, &body)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn auto_highlight(call_id: String) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::auto_highlight(backend, &call_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_highlight(id: String) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::delete_highlight(backend, &id)
        .await
        .map_err(|e| e.to_string())
}

// ── Vault (Obsidian) per-machine settings ────────────────────────────

#[derive(serde::Serialize)]
struct VaultSettings {
    enabled: bool,
    path: String,
    clients_subpath: String,
}

#[tauri::command]
fn get_vault_settings() -> Result<VaultSettings, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    Ok(match cfg.vault {
        Some(v) => VaultSettings {
            enabled: true,
            path: v.path,
            clients_subpath: v.clients_subpath,
        },
        None => VaultSettings {
            enabled: false,
            path: String::new(),
            clients_subpath: String::new(),
        },
    })
}

#[tauri::command]
fn set_vault_settings(
    enabled: bool,
    path: String,
    clients_subpath: String,
) -> Result<(), String> {
    let mut cfg = config::Config::load().map_err(|e| e.to_string())?;
    if enabled {
        let path = path.trim();
        if path.is_empty() {
            return Err("vault path is required when enabled".into());
        }
        cfg.vault = Some(config::Vault {
            path: path.to_string(),
            clients_subpath: clients_subpath.trim().to_string(),
        });
    } else {
        cfg.vault = None;
    }
    cfg.save().map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_audio_urls(id: String) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::get_audio_urls(backend, &id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_peaks(id: String) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::get_peaks(backend, &id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_call(id: String) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::delete_call(backend, &id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_utterance_speaker(
    id: String,
    idx: i32,
    speaker: String,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::update_utterance(backend, &id, idx, &speaker)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn rename_speaker(id: String, from: String, to: String) -> Result<u64, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::rename_speaker(backend, &id, &from, &to)
        .await
        .map_err(|e| e.to_string())
}

fn toggle_recording(app: &AppHandle) {
    let state = app.state::<Recorder>();
    if state.is_active() {
        if let Err(e) = do_stop(&state, app) {
            eprintln!("aftercalls: hotkey stop error: {e}");
        }
    } else {
        match do_start(&state, app) {
            Ok(path) => {
                // Hotkey + tray-menu toggles are manual starts from the user.
                write_session_source(std::path::Path::new(&path), "manual", None);
            }
            Err(e) => eprintln!("aftercalls: hotkey start error: {e}"),
        }
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        // Only call show() when the window is actually hidden. On some
        // wlroots-family compositors (Hyprland, sway) a redundant show()
        // on an already-visible window can materialize a duplicate surface
        // (see #15 + the recent "3 windows on join call" report).
        match win.is_visible() {
            Ok(true) => {}
            _ => {
                let _ = win.show();
            }
        }
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Start recording", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Open aftercalls", true, None::<&str>)?;
    let settings =
        MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&toggle, &sep1, &show, &settings, &sep2, &quit],
    )?;

    let idle_icon = tauri::include_image!("icons/tray-idle.png");

    app.manage(TrayItems {
        toggle: toggle.clone(),
    });

    TrayIconBuilder::with_id("main")
        .tooltip("aftercalls — idle")
        .icon(idle_icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => toggle_recording(app),
            "show" => show_main_window(app),
            "settings" => {
                show_main_window(app);
                // Frontend listens for this and routes to /settings.
                let _ = app.emit("tray-open", "settings");
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray: &tauri::tray::TrayIcon, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn setup_hotkey(app: &AppHandle) -> tauri::Result<()> {
    // Super+Shift+R: rare conflict vs. Ctrl+Shift+R (browser hard-reload).
    // On Wayland/Hyprland this still depends on xdg-desktop-portal-hyprland
    // implementing the GlobalShortcuts portal; falling back to a Hyprland
    // bind → CLI trigger is tracked as a follow-up.
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyR);
    if let Err(e) = app.global_shortcut().on_shortcut(shortcut, |app, _sc, event| {
        if event.state() == ShortcutState::Pressed {
            toggle_recording(app);
        }
    }) {
        eprintln!("aftercalls: global shortcut unavailable ({e}); use the UI or tray");
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Single-instance *must* be registered first (upstream guidance).
        // A second launch — tray double-click, CLI from a script, .desktop
        // autostart, Hyprland bind, etc. — would otherwise spawn its own
        // process + "main" window, producing the multi-window behavior the
        // user saw on Linux. When a second launch happens the callback
        // fires in the original process; we just re-show the existing
        // main window and let the second process exit.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(Recorder::new())
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            is_recording,
            process_imported_file,
            confirm_auto_start,
            dismiss_auto_start,
            confirm_auto_end,
            keep_auto_recording,
            login,
            logout,
            current_user,
            list_calls,
            get_call,
            get_session_audio_path,
            get_audio_urls,
            get_peaks,
            get_vault_settings,
            set_vault_settings,
            get_org_vocab,
            set_org_vocab,
            list_highlights,
            create_highlight,
            update_highlight,
            delete_highlight,
            auto_highlight,
            delete_call,
            update_utterance_speaker,
            rename_speaker,
        ])
        .setup(|app| {
            setup_tray(app.handle())?;
            setup_hotkey(app.handle())?;
            // Drop the native window chrome on Windows so the webview
            // can draw an integrated dark titlebar (see #25). Linux
            // users keep GTK decorations since they already theme
            // with the system.
            #[cfg(target_os = "windows")]
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.set_decorations(false);
            }
            let detector = Detector::spawn(app.handle().clone());
            app.manage(detector);
            Ok(())
        })
        .on_window_event(|win, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if win.label() == "main" {
                    api.prevent_close();
                    let _ = win.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
