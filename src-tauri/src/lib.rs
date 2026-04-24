mod config;
mod detector;
mod notes;
mod pipeline;
mod portal;
mod recorder;
mod recovery;
mod summary;
mod telemetry;
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
use tauri_plugin_autostart::ManagerExt;
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
    /// #142 · v0.4.5 — which mode the in-flight recording is in.
    /// `"call"` for a regular capture (full pipeline incl. system
    /// loopback), `"self_note"` for a mic-only dictation. Absent /
    /// null when `recording = false` OR when the frontend is behind
    /// a backend build that doesn't emit the field — pages default
    /// to "call" shape in that fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
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

fn emit_state(app: &AppHandle, recording: bool, mode: Option<&'static str>) {
    let _ = app.emit(
        "recording-state",
        RecordingStateEvent { recording, mode },
    );
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

/// Spawns the hard-ceiling watchdog for an in-flight recording. Used
/// by both the regular recorder path (`do_start`) and the note-to-
/// self path (`do_start_self_note`). Captures the current session_seq
/// so a manual stop-then-start can't let a stale watchdog nuke the
/// new session. `minutes` is the cap the caller picks (per-user
/// `max_recording_minutes` for regular calls, `max_self_note_minutes`
/// for self-notes).
fn spawn_max_length_watchdog(app: &AppHandle, captured_seq: i64, minutes: u32, label: &'static str) {
    let app_for_watchdog = app.clone();
    let minutes_owned = minutes;
    tauri::async_runtime::spawn(async move {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(minutes_owned as u64 * 60);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let rec = app_for_watchdog.state::<Recorder>();
            // Session changed out from under us — manual stop (and
            // possibly a new start). Stale watchdog; bail.
            if rec.session_seq() != captured_seq {
                return;
            }
            if !rec.is_active() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "aftercalls: {label} hit max={minutes_owned}m; auto-stopping"
                );
                telemetry::log(
                    "info",
                    "recorder::max_length_auto_stop",
                    format!("auto-stopped {label} after {minutes_owned} minutes"),
                    None,
                    None,
                );
                if let Err(e) = do_stop(&rec, &app_for_watchdog) {
                    eprintln!("aftercalls: watchdog auto-stop failed: {e}");
                }
                return;
            }
        }
    });
}

pub(crate) fn do_start(state: &Recorder, app: &AppHandle) -> Result<String, String> {
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    // Pull the saved input-device preference (#3). `None` means "use
    // system default" — both the fresh-install state and the explicit
    // reset state. Config read failures fall through to `None` so a
    // bad config can never block a recording.
    let saved_device = config::Config::load()
        .ok()
        .and_then(|c| c.input_device);
    let path = state.start(base, saved_device, false)?;
    emit_state(app, true, Some("call"));
    // If the saved-name preference didn't resolve, surface a one-time
    // toast on the Record page. Dedupe lives inside the Recorder
    // (HashSet<String>), so repeat Start/Stop in the same session with
    // the same stale name stays quiet.
    if let Some(fallback) = state.take_last_fallback() {
        let _ = app.emit("mic-fallback", &fallback);
    }
    // Hard ceiling watchdog (#75). Reads the per-user cap from config
    // once at spawn (runtime changes take effect on the next session).
    let max_minutes = config::Config::load()
        .map(|c| c.max_recording_minutes)
        .unwrap_or(120);
    let captured_seq = app.state::<Recorder>().session_seq();
    spawn_max_length_watchdog(app, captured_seq, max_minutes, "recording");
    Ok(path.to_string_lossy().into_owned())
}

/// #142 · v0.4.5 — Start a note-to-self (mic-only) recording. Same
/// recorder state machine as `do_start`, but passes `mic_only=true`
/// so the worker skips `start_system_loopback`, and spawns the
/// watchdog at the per-user `max_self_note_minutes` ceiling (default
/// 5m) instead of `max_recording_minutes`. Caller is responsible for
/// writing `source.json` with `kind = "self_note"` (see
/// `start_self_note`).
pub(crate) fn do_start_self_note(state: &Recorder, app: &AppHandle) -> Result<String, String> {
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    let saved_device = config::Config::load()
        .ok()
        .and_then(|c| c.input_device);
    let path = state.start(base, saved_device, true)?;
    emit_state(app, true, Some("self_note"));
    if let Some(fallback) = state.take_last_fallback() {
        let _ = app.emit("mic-fallback", &fallback);
    }
    let max_minutes = config::Config::load()
        .map(|c| c.max_self_note_minutes)
        .unwrap_or(5);
    let captured_seq = app.state::<Recorder>().session_seq();
    spawn_max_length_watchdog(app, captured_seq, max_minutes, "note-to-self");
    Ok(path.to_string_lossy().into_owned())
}

pub(crate) fn do_stop(state: &Recorder, app: &AppHandle) -> Result<String, String> {
    let path: PathBuf = state.stop()?;
    emit_state(app, false, None);
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

/// #142 · v0.4.5 — Start a note-to-self dictation. Mic-only capture;
/// writes `source.json.kind = "self_note"` so the pipeline + backend
/// list views can surface the distinct "Note to self" treatment on
/// the call row. Auto-stops at `config.max_self_note_minutes`
/// (default 5m). Rejects when a regular recording is already active
/// — the caller surfaces the inline notice.
#[tauri::command]
fn start_self_note(state: State<Recorder>, app: AppHandle) -> Result<String, String> {
    let path = do_start_self_note(&state, &app)?;
    write_session_source(std::path::Path::new(&path), "self_note", None);
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

// Used by the updater-prompt gate in +layout.svelte (#79). Returns
// true when the agent is either actively recording OR still running
// the post-recording pipeline (upload/transcribe/summarize/write-note).
// Mirrors the exact pair `quit_with_confirm` (#62) checks — keeping a
// single source of truth for "work in flight" so the two paths can
// never disagree.
#[tauri::command]
fn is_processing(state: State<Recorder>) -> bool {
    state.is_active() || pipeline::is_pipeline_active()
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
    let mut cmd = tokio::process::Command::new(crate::pipeline::ffmpeg_binary());
    cmd.arg("-y")
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
        .stderr(std::process::Stdio::null());
    crate::pipeline::no_console(&mut cmd);
    let status = cmd
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
async fn list_calls(
    scope: Option<String>,
    user: Option<String>,
    tags: Option<Vec<String>>,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    let tags = tags.unwrap_or_default();
    portal::list_calls(backend, scope.as_deref(), user.as_deref(), &tags)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn tag_suggestions(
    kind: Option<String>,
    q: Option<String>,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::tag_suggestions(backend, kind.as_deref(), q.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_trashed() -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::list_trashed(backend).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn restore_call(id: String) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::restore_call(backend, &id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn permadelete_call(
    app: AppHandle,
    id: String,
    session_id: Option<String>,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::permadelete_call(backend, &id)
        .await
        .map_err(|e| e.to_string())?;
    cleanup_local_session(&app, session_id.as_deref()).await;
    Ok(())
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
    // User id mirrored from auth.json so the frontend can gate
    // call-edit UI (tag edit, speaker rename) against ownership
    // without a roundtrip to /v1/auth/me.
    user_id: String,
    email: String,
    // Structured names (#96) ride alongside display_name. Serde-
    // defaulted to "" in AuthFile so old auth.json files stay
    // parseable; once the user re-logs in they're populated.
    first_name: String,
    last_name: String,
    display_name: String,
    role: String,
    org_display_name: String,
    // Surfaced to the layout + Record page so the PIPEDA ack modal
    // (#44) knows not to prompt a user who's already acknowledged.
    recording_acknowledged: bool,
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
        user_id: auth.user_id,
        email: auth.email,
        first_name: auth.first_name,
        last_name: auth.last_name,
        display_name: auth.display_name,
        role: auth.role,
        org_display_name: auth.org_display_name,
        recording_acknowledged: auth.recording_acknowledged,
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
        user_id: a.user_id,
        email: a.email,
        first_name: a.first_name,
        last_name: a.last_name,
        display_name: a.display_name,
        role: a.role,
        org_display_name: a.org_display_name,
        recording_acknowledged: a.recording_acknowledged,
    }))
}

/// #96: profile-edit Save handler. PATCHes /v1/auth/me with
/// structured first/last, merges the returned user into auth.json,
/// and returns the updated LoginResult so the UI re-renders the
/// user-menu and rail-foot without a page reload.
#[tauri::command]
async fn update_me(
    first_name: String,
    last_name: String,
) -> Result<LoginResult, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    let auth = portal::update_me(backend, &first_name, &last_name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(LoginResult {
        user_id: auth.user_id,
        email: auth.email,
        first_name: auth.first_name,
        last_name: auth.last_name,
        display_name: auth.display_name,
        role: auth.role,
        org_display_name: auth.org_display_name,
        recording_acknowledged: auth.recording_acknowledged,
    })
}

// ── PIPEDA recording-ack + org prefs (#44, #45, #48) ─────────────────

/// Check the backend for an existing recording-ack. Used as the
/// fallback when the cached `recording_acknowledged` flag says
/// false — we don't want to re-prompt users who acknowledged on
/// another device just because their local auth.json predates this
/// field. Returns true on 200, false on 404, error otherwise.
#[tauri::command]
async fn get_recording_ack() -> Result<bool, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    let resp = portal::get_recording_ack(backend)
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.is_some())
}

/// Post the user's ack to the backend and flip the cached flag on
/// auth.json so future Start Recording clicks don't re-prompt. The
/// frontend passes the running agent version + platform (it already
/// has cheaper access to both than Rust does on some paths).
#[tauri::command]
async fn post_recording_ack(
    agent_version: String,
    platform: String,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::post_recording_ack(backend, &agent_version, &platform)
        .await
        .map_err(|e| e.to_string())?;
    // Best-effort mirror: flip the cached flag in auth.json. If the
    // write fails (rare — bad perms), we've still succeeded on the
    // server side and the next `get_recording_ack` call will reflect
    // reality, so treat this as non-fatal.
    if let Ok(Some(mut a)) = config::read_auth_file() {
        if !a.recording_acknowledged {
            a.recording_acknowledged = true;
            let _ = config::write_auth_file(&a);
        }
    }
    Ok(())
}

#[tauri::command]
async fn get_recording_prefs() -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::get_recording_prefs(backend)
        .await
        .map_err(|e| e.to_string())
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

#[derive(serde::Serialize)]
struct AppPrefs {
    close_to_tray: bool,
    auto_detect: bool,
    telemetry_enabled: bool,
    sounds_enabled: bool,
    max_recording_minutes: u32,
    /// #142 · v0.4.5 — user-configurable note-to-self cap. 5 default,
    /// [1, 60] range. Reads fresh from config on each new self-note
    /// session, so a runtime edit takes effect on the next note.
    max_self_note_minutes: u32,
    manual_notes_enabled: bool,
    wayland_hotkey_notice_dismissed: bool,
    // Saved cpal name of the preferred input microphone (#3). None
    // means "use system default". Surfaced to the Settings dropdown
    // so a mounted form reflects what's actually on disk.
    input_device: Option<String>,
}

#[tauri::command]
fn get_app_prefs() -> Result<AppPrefs, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    Ok(AppPrefs {
        close_to_tray: cfg.close_to_tray,
        auto_detect: cfg.auto_detect,
        telemetry_enabled: cfg.telemetry_enabled,
        sounds_enabled: cfg.sounds_enabled,
        max_recording_minutes: cfg.max_recording_minutes,
        max_self_note_minutes: cfg.max_self_note_minutes,
        manual_notes_enabled: cfg.manual_notes_enabled,
        wayland_hotkey_notice_dismissed: cfg.wayland_hotkey_notice_dismissed,
        input_device: cfg.input_device,
    })
}

#[tauri::command]
fn set_app_prefs(
    close_to_tray: bool,
    auto_detect: bool,
    telemetry_enabled: bool,
    sounds_enabled: bool,
    max_recording_minutes: u32,
    max_self_note_minutes: u32,
    manual_notes_enabled: bool,
    wayland_hotkey_notice_dismissed: bool,
    input_device: Option<String>,
) -> Result<(), String> {
    // Clamp to the same [5, 1440] range the Settings UI enforces so a
    // hand-edited config.toml or a future caller can't pass an
    // absurd value (0 would auto-stop immediately; u32::MAX would
    // overflow the Duration arithmetic in the watchdog).
    let max_recording_minutes = max_recording_minutes.clamp(5, 1440);
    // #142 — clamp self-note cap to [1, 60]. Looser floor than the
    // regular cap because a 60-second dictation is a plausible
    // intentional length.
    let max_self_note_minutes = max_self_note_minutes.clamp(1, 60);
    // Treat an empty/whitespace-only string the same as None so the UI
    // can send a cleared select as either null or "" without a
    // separate "reset" command. `skip_serializing_if` on the config
    // field then keeps the TOML key absent entirely — matches "fresh
    // install" / "use system default".
    let input_device = input_device
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut cfg = config::Config::load().map_err(|e| e.to_string())?;
    cfg.close_to_tray = close_to_tray;
    cfg.auto_detect = auto_detect;
    cfg.telemetry_enabled = telemetry_enabled;
    cfg.sounds_enabled = sounds_enabled;
    cfg.max_recording_minutes = max_recording_minutes;
    cfg.max_self_note_minutes = max_self_note_minutes;
    cfg.manual_notes_enabled = manual_notes_enabled;
    cfg.wayland_hotkey_notice_dismissed = wayland_hotkey_notice_dismissed;
    cfg.input_device = input_device;
    cfg.save().map_err(|e| e.to_string())
}

/// Enumerate input microphones for the Settings dropdown (#3). Returns
/// a list of `{ name, is_default }` entries, with PipeWire monitor
/// sources and any device where `default_input_config()` fails
/// filtered out. Enumeration failure is surfaced as a command error so
/// the Settings UI can render the "Couldn't load devices" state with
/// a Try-again button.
#[tauri::command]
fn list_input_devices() -> Result<Vec<recorder::DeviceEntry>, String> {
    recorder::enumerate_input_devices()
}

/// Platform-detection helper for the Linux-only in-app hotkey note.
/// The Svelte layer uses this to decide whether to render the
/// "configure this in your desktop environment" copy. Returns the
/// same strings `std::env::consts::OS` produces ("linux", "windows",
/// "macos", etc.) — callers match on "linux" specifically.
#[tauri::command]
fn platform_os() -> &'static str {
    std::env::consts::OS
}

// ── Launch-at-sign-in (#4) ───────────────────────────────────────────
//
// Thin wrappers over tauri-plugin-autostart so the Svelte layer can stay
// on the same `invoke()` bus it already uses for every other pref. The
// OS is the source of truth (Linux: ~/.config/autostart/*.desktop,
// Windows: HKCU\...\Run) — we deliberately do NOT persist this in
// config.toml. Settings reads it fresh on every mount.

#[tauri::command]
fn get_autostart(app: AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())
    } else {
        mgr.disable().map_err(|e| e.to_string())
    }
}

/// Stream an audio URL to a user-chosen file path. Exists because
/// `fetch()` from the Tauri webview (`tauri://localhost`) to Spaces
/// is blocked by CORS — Spaces doesn't ack the origin. Native
/// `<audio>` playback works because media elements bypass CORS, but
/// the Download button in call-detail needs the bytes and can't get
/// them browser-side. Going through Rust's reqwest sidesteps the
/// whole origin check.
#[tauri::command]
async fn download_audio(url: String, dest: String) -> Result<(), String> {
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fetch failed: HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body: {e}"))?;
    std::fs::write(&dest, &bytes).map_err(|e| format!("write: {e}"))?;
    Ok(())
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
async fn delete_call(
    app: AppHandle,
    id: String,
    session_id: Option<String>,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::delete_call(backend, &id)
        .await
        .map_err(|e| e.to_string())?;
    // Soft-deleted calls are filtered out by the backend's
    // get_call_by_session (deleted_at IS NULL), so leaving the local
    // session_dir on disk makes scan_orphans flag it as an unfinished
    // call and the "N unfinished calls" chip reappears. Restore from
    // the recycle bin replays audio from cloud storage, so the local
    // dir isn't needed to recover.
    cleanup_local_session(&app, session_id.as_deref()).await;
    Ok(())
}

/// Best-effort removal of a session_dir after a call has been deleted
/// (soft or hard). Used by delete_call + permadelete_call. Silent on
/// failure: backend truth already reflects the deletion, a leftover
/// folder is strictly a cleanup concern.
async fn cleanup_local_session(app: &AppHandle, session_id: Option<&str>) {
    let Some(sid) = session_id else { return };
    let Some(dir) = recovery::resolve_session_dir(app, sid) else { return };
    if let Err(e) = recovery::discard(&dir).await {
        eprintln!("aftercalls: delete_call local cleanup failed for {sid}: {e}");
    }
}

#[tauri::command]
async fn update_utterance_speaker(
    id: String,
    idx: i32,
    speaker: String,
    // #82: optional FK forwarded to the backend. Tauri's JS -> Rust
    // bridge is snake-case by convention (camelCase from the JS side
    // maps to snake_case Rust args), so the UI invokes this with
    // `speakerUserId`.
    speaker_user_id: Option<String>,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::update_utterance(backend, &id, idx, &speaker, speaker_user_id.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn rename_speaker(
    id: String,
    from: String,
    to: String,
    to_user_id: Option<String>,
) -> Result<u64, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::rename_speaker(backend, &id, &from, &to, to_user_id.as_deref())
        .await
        .map_err(|e| e.to_string())
}

// Slim org roster for the speaker-rename autocomplete (#65). Any
// authed member can read; callers that aren't logged in surface the
// auth-header error which the UI already swallows.
#[tauri::command]
async fn org_members() -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::list_org_members(backend)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_call_tags(
    id: String,
    tags: serde_json::Value,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::update_call_tags(backend, &id, &tags)
        .await
        .map_err(|e| e.to_string())
}

// ── Phase 2 (#19): resummarize + edit-in-place ────────────────────

/// POST /v1/calls/{id}/resummarize. Returns the updated CallDetail
/// on success; on 429 cooldown the error string is shaped as
/// `cooldown:{N}` so the front-end can split it back into a
/// numeric retry_after_seconds + render a countdown.
#[tauri::command]
async fn resummarize_call(id: String) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::resummarize_call(backend, &id)
        .await
        .map_err(|e| e.to_string())
}

/// PATCH /v1/calls/{id}. Accepts tri-state fields verbatim from the
/// front-end JSON; forwarded to the backend without re-shaping so
/// the TS side is the authoritative definition of "absent vs null
/// vs value".
#[tauri::command]
async fn patch_call(
    id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::patch_call(backend, &id, &body)
        .await
        .map_err(|e| e.to_string())
}

/// PATCH /v1/calls/{id}/action-items/{item_id}. Returns the updated
/// row; cross-org assignee writes bubble up as 400 which the caller
/// renders as an inline picker error.
#[tauri::command]
async fn patch_action_item(
    call_id: String,
    item_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::patch_action_item(backend, &call_id, &item_id, &body)
        .await
        .map_err(|e| e.to_string())
}

/// POST /v1/calls/{id}/action-items/manual — Phase 3 (#104) manual
/// add. Body is forwarded verbatim; frontend pre-shapes
/// `{description, assignee_user_id?}`. Backend returns 201 with the
/// created row which the caller appends to local state.
#[tauri::command]
async fn add_action_item(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::add_action_item(backend, &call_id, &body)
        .await
        .map_err(|e| e.to_string())
}

/// GET /v1/me/action-items — Phase 4 (#105). Returns the caller's
/// own action items across every call in their org. Cursor-paginated;
/// the frontend passes `cursor=null` for the first page and feeds
/// `next_cursor` back on follow-up pages.
#[tauri::command]
async fn list_me_action_items(
    status: String,
    cursor: Option<String>,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    let resolved_limit = limit.unwrap_or(50);
    portal::list_me_action_items(backend, &status, cursor.as_deref(), resolved_limit)
        .await
        .map_err(|e| e.to_string())
}

/// DELETE /v1/calls/{id}/action-items/{item_id} — Phase 3 (#104).
/// 404 is converted to Ok(()) on the portal helper side so the TS
/// frontend's deleteActionItem matches the portal's "silent success
/// on already-gone" behaviour (ui-phase-3 §G).
#[tauri::command]
async fn delete_action_item(
    call_id: String,
    item_id: String,
) -> Result<(), String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| "no backend configured".to_string())?;
    portal::delete_action_item(backend, &call_id, &item_id)
        .await
        .map_err(|e| e.to_string())
}


// ── Orphan session recovery (#63) ────────────────────────────────────

#[tauri::command]
async fn list_orphan_sessions(app: AppHandle) -> Result<Vec<recovery::OrphanSession>, String> {
    Ok(recovery::scan_orphans(&app).await)
}

#[tauri::command]
async fn resume_orphan_session(app: AppHandle, session_id: String) -> Result<(), String> {
    let path = recovery::resolve_session_dir(&app, &session_id)
        .ok_or_else(|| format!("session not found: {session_id}"))?;
    // Spawn so the frontend's await resolves as soon as the pipeline
    // has been handed off, rather than blocking until the whole
    // transcribe+summarize run finishes (minutes).
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        recovery::resume(app_clone, path).await;
    });
    Ok(())
}

#[tauri::command]
async fn discard_orphan_session(app: AppHandle, session_id: String) -> Result<(), String> {
    let path = recovery::resolve_session_dir(&app, &session_id)
        .ok_or_else(|| format!("session not found: {session_id}"))?;
    recovery::discard(&path).await
}

/// Recognized CLI flags handed to a second aftercalls launch. Parsed
/// out of argv inside the `tauri-plugin-single-instance` callback so
/// the running process — not the new one — performs the action. This
/// is how Wayland users route a compositor-bound hotkey (e.g.
/// `bind = SUPER SHIFT, R, exec, aftercalls --toggle-recording`)
/// into the recorder when the agent window is unfocused: the second
/// launch's argv is delivered to the original process, the callback
/// dispatches, and the second process exits without ever creating a
/// window.
///
/// Unknown argv → `None`, which preserves the pre-existing
/// "double-launch raises the window" semantics for the naked
/// `aftercalls` invocation and any future flags we haven't taught
/// this parser yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CliAction {
    ToggleRecording,
    Start,
    Stop,
    // #142 · v0.4.5 — note-to-self flag. Starts a mic-only capture
    // with the self-note auto-cap. Guarded against "already
    // recording" — no-op in that case (matches `--start`'s shape).
    NoteToSelf,
}

fn parse_cli_action(argv: &[String]) -> Option<CliAction> {
    // Explicit allowlist: unknown flags fall through to `None`, which
    // is the "raise the window" branch. Argv[0] is the binary path
    // and must be skipped.
    for arg in argv.iter().skip(1) {
        match arg.as_str() {
            "--toggle-recording" => return Some(CliAction::ToggleRecording),
            "--start" => return Some(CliAction::Start),
            "--stop" => return Some(CliAction::Stop),
            "--note-to-self" => return Some(CliAction::NoteToSelf),
            _ => {}
        }
    }
    None
}

/// Dispatch a parsed `CliAction` against the running recorder.
/// Deliberately returns nothing — `--start` when already recording
/// and `--stop` when idle are no-ops by design (scripts may call
/// them preemptively to ensure a known state).
fn run_cli_action(app: &AppHandle, action: CliAction) {
    let state = app.state::<Recorder>();
    match action {
        CliAction::ToggleRecording => toggle_recording(app),
        CliAction::Start => {
            if state.is_active() {
                return;
            }
            match do_start(&state, app) {
                Ok(path) => {
                    write_session_source(std::path::Path::new(&path), "manual", None);
                }
                Err(e) => eprintln!("aftercalls: cli start error: {e}"),
            }
        }
        CliAction::Stop => {
            if !state.is_active() {
                return;
            }
            if let Err(e) = do_stop(&state, app) {
                eprintln!("aftercalls: cli stop error: {e}");
            }
        }
        CliAction::NoteToSelf => {
            start_note_to_self(app);
        }
    }
}

/// Shared entry point for the tray "Note to self" menu item, the
/// global Super+Shift+N hotkey, and the `--note-to-self` CLI flag.
/// No-ops (with a log) when a regular recording is active — a
/// self-note is additive, not interruptive. Writes
/// `source.json.kind = "self_note"` on success so the pipeline +
/// backend list view render the distinct title + glyph.
fn start_note_to_self(app: &AppHandle) {
    let state = app.state::<Recorder>();
    if state.is_active() {
        eprintln!("aftercalls: cannot start note — a recording is already in progress");
        return;
    }
    match do_start_self_note(&state, app) {
        Ok(path) => {
            write_session_source(std::path::Path::new(&path), "self_note", None);
        }
        Err(e) => eprintln!("aftercalls: start note-to-self error: {e}"),
    }
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

/// Tray Quit handler with a confirm dialog when work is in flight
/// (#62). Close-to-tray path (X button) is separate — that one
/// intentionally never quits. Only reached from the tray "Quit"
/// menu item and any future programmatic quit.
fn quit_with_confirm(app: AppHandle) {
    // Recorder active or pipeline in flight → ask first. Otherwise
    // exit immediately so casual quits aren't slowed down by a
    // popup on every session.
    let pipeline_busy = pipeline::is_pipeline_active();
    let recorder_busy = app
        .try_state::<recorder::Recorder>()
        .map(|r| r.is_active())
        .unwrap_or(false);
    if !pipeline_busy && !recorder_busy {
        app.exit(0);
        return;
    }
    let body = if recorder_busy && pipeline_busy {
        "aftercalls is recording and still processing a call. Quit anyway?"
    } else if recorder_busy {
        "aftercalls is recording right now. Quit anyway?"
    } else {
        "aftercalls is still processing a call in the background. Quit anyway?"
    };
    // tauri-plugin-dialog::ask pops a native OS dialog. Running it on
    // the async runtime so the tray menu callback returns promptly
    // rather than blocking the event loop.
    let app_for_dialog = app.clone();
    tauri::async_runtime::spawn(async move {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        // Show the window so the dialog has a visible parent on Linux
        // (GTK dialogs can land offscreen when the parent is hidden).
        show_main_window(&app_for_dialog);
        let (tx, rx) = tokio::sync::oneshot::channel();
        app_for_dialog
            .dialog()
            .message(body)
            .title("Quit aftercalls?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Quit anyway".into(),
                "Keep running".into(),
            ))
            .show(move |confirmed| {
                let _ = tx.send(confirmed);
            });
        if rx.await.unwrap_or(false) {
            app_for_dialog.exit(0);
        }
    });
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
    // #142 · v0.4.5 — "Note to self" sits above "Start recording" so
    // the two recording entry points cluster visually and the user
    // sees the lighter (self-note) option first. Tray label matches
    // the in-app button copy verbatim.
    let note_self =
        MenuItem::with_id(app, "note_to_self", "Note to self", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Start recording", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Open aftercalls", true, None::<&str>)?;
    let settings =
        MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&note_self, &toggle, &sep1, &show, &settings, &sep2, &quit],
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
            "note_to_self" => start_note_to_self(app),
            "show" => show_main_window(app),
            "settings" => {
                show_main_window(app);
                // Frontend listens for this and routes to /settings.
                let _ = app.emit("tray-open", "settings");
            }
            "quit" => quit_with_confirm(app.clone()),
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
    // #142 · v0.4.5 — Super+Shift+N (note). Doesn't clash with
    // Super+Shift+R (primary toggle) or Ctrl+Shift+N (browser
    // incognito window). Wayland users who can't grab a global
    // shortcut get the `--note-to-self` CLI flag via a compositor
    // keybind.
    let note_shortcut =
        Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyN);
    if let Err(e) = app.global_shortcut().on_shortcut(note_shortcut, |app, _sc, event| {
        if event.state() == ShortcutState::Pressed {
            start_note_to_self(app);
        }
    }) {
        eprintln!(
            "aftercalls: note-to-self shortcut unavailable ({e}); use the UI, tray, or --note-to-self CLI"
        );
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
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // A second launch that carries one of our recognized CLI
            // flags (e.g. `aftercalls --toggle-recording`, wired from
            // a compositor keybind) MUST NOT raise the main window —
            // that would defeat the "invisible global hotkey" UX on
            // Wayland (issue #6). Unknown argv falls through to the
            // pre-existing `show_main_window` behaviour, which is
            // what plain double-launches from a tray/.desktop click
            // expect. The parser is an explicit allowlist so future
            // flags we haven't taught it about don't silently swallow
            // the relaunch.
            if let Some(action) = parse_cli_action(&argv) {
                run_cli_action(app, action);
                return;
            }
            show_main_window(app);
        }))
        // Launch-at-sign-in (#4). MacosLauncher::LaunchAgent is inert on
        // Linux/Windows so it's safe to include now; when we ship macOS
        // it's the variant we want. Passing None for args: the plugin
        // defaults to the current exe with no extra flags, which lands
        // us straight in the tray via the existing single-instance +
        // close-to-tray behavior — no silent-start flag needed.
        .plugin(tauri_plugin_autostart::Builder::new()
            .build())
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
            get_autostart,
            set_autostart,
            start_recording,
            start_self_note,
            stop_recording,
            is_recording,
            is_processing,
            process_imported_file,
            confirm_auto_start,
            dismiss_auto_start,
            confirm_auto_end,
            keep_auto_recording,
            login,
            logout,
            current_user,
            update_me,
            list_calls,
            tag_suggestions,
            list_trashed,
            restore_call,
            permadelete_call,
            get_call,
            get_session_audio_path,
            get_audio_urls,
            download_audio,
            get_app_prefs,
            set_app_prefs,
            list_input_devices,
            platform_os,
            telemetry::log_event,
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
            org_members,
            update_call_tags,
            resummarize_call,
            patch_call,
            patch_action_item,
            add_action_item,
            delete_action_item,
            list_me_action_items,
            get_recording_ack,
            post_recording_ack,
            get_recording_prefs,
            list_orphan_sessions,
            resume_orphan_session,
            discard_orphan_session,
            notes::save_notes,
            notes::load_notes,
            notes::update_call_notes,
        ])
        .setup(|app| {
            // Telemetry must start FIRST so panics during subsequent
            // setup() calls still land in the buffer. The panic hook
            // runs async flush on its way out; if the process
            // terminates before flush completes, next launch's first
            // batch ships what the old process couldn't.
            telemetry::install_panic_hook();
            telemetry::start(app.handle().clone());
            telemetry::log(
                "info",
                "agent::startup",
                format!("agent {} starting on {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS),
                None,
                None,
            );

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
                    // Per-user preference: default true (hide to tray),
                    // false means the X button really quits. Fallback to
                    // hide-on-close if config can't be read so users
                    // can't accidentally lock themselves out of tray
                    // behavior via a bad config.
                    let close_to_tray = config::Config::load()
                        .map(|c| c.close_to_tray)
                        .unwrap_or(true);
                    if close_to_tray {
                        api.prevent_close();
                        let _ = win.hide();
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
