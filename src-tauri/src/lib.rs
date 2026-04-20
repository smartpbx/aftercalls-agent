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
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

#[derive(Serialize, Clone)]
struct RecordingStateEvent {
    recording: bool,
}

fn emit_state(app: &AppHandle, recording: bool) {
    let _ = app.emit("recording-state", RecordingStateEvent { recording });
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
    do_start(&state, &app)
}

#[tauri::command]
fn stop_recording(state: State<Recorder>, app: AppHandle) -> Result<String, String> {
    do_stop(&state, &app)
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

fn toggle_recording(app: &AppHandle) {
    let state = app.state::<Recorder>();
    let result = if state.is_active() {
        do_stop(&state, app)
    } else {
        do_start(&state, app)
    };
    if let Err(e) = result {
        eprintln!("callscribe: hotkey toggle error: {e}");
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show callscribe", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &sep, &quit])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    TrayIconBuilder::with_id("main")
        .tooltip("callscribe")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
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
        eprintln!("callscribe: global shortcut unavailable ({e}); use the UI or tray");
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(Recorder::new())
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            confirm_auto_start,
            dismiss_auto_start,
            confirm_auto_end,
            keep_auto_recording,
            list_calls,
            get_call,
            get_session_audio_path,
            delete_call,
        ])
        .setup(|app| {
            setup_tray(app.handle())?;
            setup_hotkey(app.handle())?;
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
