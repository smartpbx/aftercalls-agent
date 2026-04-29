mod app_observations;
mod audio_observer;
mod auto_recorder;
mod config;
mod detector;
mod error;
mod mic_consumers;
mod notes;
mod notify_actions;
mod pipeline;
mod portal;
mod recorder;
mod recovery;
mod summary;
mod support;
mod telemetry;
mod transcription;
mod upload;
mod vault;

use auto_recorder::AutoRecorder;
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
    SelfNote,
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
        TrayState::SelfNote => (
            tauri::include_image!("icons/tray-self-note.png"),
            "aftercalls — recording note",
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
            TrayState::SelfNote => "Stop note",
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
    /// #164 · v0.5.2 — session directory for the in-flight recording.
    /// Populated when `recording = true`. The manual `start_recording`
    /// IPC return-value already carries this for /record's notes
    /// panel, but the auto-detect `confirm_auto_start` path calls
    /// `do_start` internally and never surfaces the path. Propagating
    /// it here lets the frontend populate `sessionDir` regardless of
    /// entry-point, so the notes panel mounts for both modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_dir: Option<String>,
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

fn emit_state(
    app: &AppHandle,
    recording: bool,
    mode: Option<&'static str>,
    session_dir: Option<String>,
) {
    let _ = app.emit(
        "recording-state",
        RecordingStateEvent {
            recording,
            mode,
            session_dir,
        },
    );
    apply_tray_state(
        app,
        if recording {
            // #151: distinct tray icon during self-note recording
            // (amber-dot badge) so the user can see at-a-glance whether
            // the tray is showing a regular call vs a mic-only note.
            match mode {
                Some("self_note") => TrayState::SelfNote,
                _ => TrayState::Recording,
            }
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
    emit_state(
        app,
        true,
        Some("call"),
        Some(path.to_string_lossy().into_owned()),
    );
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
    emit_state(
        app,
        true,
        Some("self_note"),
        Some(path.to_string_lossy().into_owned()),
    );
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
    emit_state(app, false, None, None);
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
    // Active session directory path as a string; None when idle. Lets
    // the Record page rehydrate `sessionDir` (and the `currentSessionId`
    // derived from it) on a route-nav remount so the manual-notes
    // panel's render gate re-passes without waiting for a new
    // transition event (#185). String (not struct-wrapped PathBuf) so
    // the webview's existing string-split parse of the session id
    // continues to work unchanged.
    session_dir: Option<String>,
}

// Point-in-time query used by the Record page on mount. The
// "recording-state" event only fires on transitions, so a page that
// remounts mid-recording has no other way to learn the current state.
#[tauri::command]
fn is_recording(state: State<Recorder>) -> RecordingStatus {
    RecordingStatus {
        recording: state.is_active(),
        started_at_ms: state.started_at_ms(),
        session_dir: state
            .session_dir()
            .map(|p| p.to_string_lossy().into_owned()),
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

// ── #596 — auto-record IPC surface ─────────────────────────────────────
//
// Five commands + four events drive the per-app whitelist UX:
//   - `auto_record_settings_get` returns the bundle the Settings page
//     paints (master toggles + per-app rows + platform_supported).
//   - `auto_record_settings_set_master` flips the two booleans that
//     gate the auto-start / auto-stop paths.
//   - `auto_record_settings_toggle_app` updates one row's `enabled`
//     flag — the per-row checkbox in the apps list.
//   - `auto_record_settings_forget_app` removes a row; it'll reappear
//     the next time that app captures the mic, by design.
//   - `confirm_auto_record_cancel` clears the in-flight pending-start
//     when the user clicks Cancel on the 5s toast.
//
// Events emitted from `auto_recorder` and listened to by +layout.svelte:
//   `auto-record-pending`, `auto-record-fired`, `auto-record-cancelled`,
//   `observed-apps-updated`. See `auto_recorder::on_event`.

#[derive(serde::Serialize)]
struct AutoRecordAppRow {
    bundle_id: String,
    friendly_name: String,
    first_seen_at: chrono::DateTime<chrono::Utc>,
    last_seen_at: chrono::DateTime<chrono::Utc>,
    enabled: bool,
}

#[derive(serde::Serialize)]
struct AutoRecordSettings {
    start_enabled: bool,
    stop_enabled: bool,
    /// False on macOS (and any other future OS without an observer impl).
    /// The Settings UI uses this to show the "App detection isn't
    /// supported on this OS yet" banner instead of the empty list.
    platform_supported: bool,
    apps: Vec<AutoRecordAppRow>,
}

#[tauri::command]
fn auto_record_settings_get(
    auto: State<AutoRecorder>,
) -> Result<AutoRecordSettings, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    let rows = auto.store().list().map_err(|e| e.to_string())?;
    Ok(AutoRecordSettings {
        start_enabled: cfg.auto_record_start_enabled,
        stop_enabled: cfg.auto_record_stop_enabled,
        platform_supported: audio_observer::is_supported(),
        apps: rows
            .into_iter()
            .map(|r| AutoRecordAppRow {
                bundle_id: r.bundle_id,
                friendly_name: r.friendly_name,
                first_seen_at: r.first_seen_at,
                last_seen_at: r.last_seen_at,
                enabled: r.enabled,
            })
            .collect(),
    })
}

#[tauri::command]
fn auto_record_settings_set_master(
    start: bool,
    stop: bool,
) -> Result<(), String> {
    let mut cfg = config::Config::load().map_err(|e| e.to_string())?;
    cfg.auto_record_start_enabled = start;
    cfg.auto_record_stop_enabled = stop;
    cfg.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn auto_record_settings_toggle_app(
    auto: State<AutoRecorder>,
    bundle_id: String,
    enabled: bool,
) -> Result<(), String> {
    auto.store()
        .set_enabled(&bundle_id, enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn auto_record_settings_forget_app(
    auto: State<AutoRecorder>,
    bundle_id: String,
) -> Result<(), String> {
    auto.store().forget(&bundle_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn confirm_auto_record_cancel(
    auto: State<AutoRecorder>,
    app: AppHandle,
    pending_id: String,
) -> Result<(), String> {
    auto.cancel_pending(&app, &pending_id);
    Ok(())
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
    // #146 — optional date-range bounds. The Svelte layer passes
    // `YYYY-MM-DDTHH:MM:SSZ` strings (client-side expansion of the
    // `<input type="date">` values); we forward them straight to the
    // backend, which parses with `chrono::DateTime<Utc>`.
    from_date: Option<String>,
    to_date: Option<String>,
    // #386 — keyset pagination. `cursor` is the opaque RFC-3339 token
    // returned in the previous response's `next_cursor`; `limit`
    // defaults to the backend's 50 when None.
    cursor: Option<String>,
    limit: Option<i64>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let tags = tags.unwrap_or_default();
    portal::list_calls(
        backend,
        scope.as_deref(),
        user.as_deref(),
        &tags,
        from_date.as_deref(),
        to_date.as_deref(),
        cursor.as_deref(),
        limit,
    )
    .await
}

#[tauri::command]
async fn tag_suggestions(
    kind: Option<String>,
    q: Option<String>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::tag_suggestions(backend, kind.as_deref(), q.as_deref()).await
}

#[tauri::command]
async fn list_trashed(
    // #176 — Mine / All-team scope parity with the portal's trash
    // view. Defaults to "mine" so non-admins keep their existing
    // behavior; the UI only shows the All-team pill for admin/owner.
    scope: Option<String>,
    // #163 (v0.5.2) — optional date-range filter on the trash list.
    // Agent UI passes `YYYY-MM-DDTHH:MM:SSZ` strings already expanded
    // on the JS side; backend narrows by `recorded_at`.
    from_date: Option<String>,
    to_date: Option<String>,
    // #386 — keyset pagination. Trash cursors anchor on `deleted_at`
    // server-side; the wire shape is the same opaque RFC-3339 token.
    cursor: Option<String>,
    limit: Option<i64>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let scope = scope.as_deref().unwrap_or("mine");
    portal::list_trashed(
        backend,
        Some(scope),
        from_date.as_deref(),
        to_date.as_deref(),
        cursor.as_deref(),
        limit,
    )
    .await
}

#[tauri::command]
async fn restore_call(id: String) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::restore_call(backend, &id).await
}

#[tauri::command]
async fn hydrate_call(id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::hydrate_call(backend, &id).await
}

#[tauri::command]
async fn permadelete_call(
    app: AppHandle,
    id: String,
    session_id: Option<String>,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::permadelete_call(backend, &id).await?;
    cleanup_local_session(&app, session_id.as_deref()).await;
    Ok(())
}

#[tauri::command]
async fn get_call(id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::get_call(backend, &id).await
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
    /// #86 — aftercalls-staff capability, orthogonal to role. Surfaced
    /// so the agent sidebar can gate the STAFF section.
    is_platform_staff: bool,
    org_display_name: String,
    // Surfaced to the layout + Record page so the PIPEDA ack modal
    // (#44) knows not to prompt a user who's already acknowledged.
    recording_acknowledged: bool,
    /// #215 — per-org feature flags. Mirrors the backend's
    /// `FeaturesSnapshot` shape; the SvelteKit layout + call-detail
    /// page gate the Send-to-CRM affordances on `features.zoho`.
    features: config::FeatureFlags,
    /// #320 — outstanding ToS / privacy versions. The SvelteKit layout
    /// gates on `length > 0` and routes the user to `/accept-terms`
    /// before any recording surface is reachable. Mirror of the
    /// portal's `Me.pending_tos` shape.
    pending_tos: Vec<config::PendingTos>,
}

#[tauri::command]
async fn login(email: String, password: String) -> Result<LoginResult, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let auth = portal::login(backend, &email, &password).await?;
    Ok(LoginResult {
        user_id: auth.user_id,
        email: auth.email,
        first_name: auth.first_name,
        last_name: auth.last_name,
        display_name: auth.display_name,
        role: auth.role,
        is_platform_staff: auth.is_platform_staff,
        org_display_name: auth.org_display_name,
        recording_acknowledged: auth.recording_acknowledged,
        features: auth.features,
        pending_tos: auth.pending_tos,
    })
}

#[tauri::command]
async fn logout() -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::logout(backend).await
}

/// #107 — offline indicator probe. Returns true when the backend's
/// `/health` route answers 2xx within a short timeout. Used by the
/// agent's `onlineStatus` store to drive the topstrip OfflineBanner;
/// no auth required (matches the route's public stance). Returns
/// false on every error path — the JS layer only needs the verdict.
#[tauri::command]
async fn backend_health() -> bool {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let backend = match cfg.backend.as_ref() {
        Some(b) => b,
        None => return false,
    };
    portal::health_check(backend).await
}

#[tauri::command]
fn current_user() -> Result<Option<LoginResult>, error::PortalError> {
    let auth = config::read_auth_file().map_err(error::PortalError::from)?;
    Ok(auth.map(|a| LoginResult {
        user_id: a.user_id,
        email: a.email,
        first_name: a.first_name,
        last_name: a.last_name,
        display_name: a.display_name,
        role: a.role,
        is_platform_staff: a.is_platform_staff,
        org_display_name: a.org_display_name,
        recording_acknowledged: a.recording_acknowledged,
        features: a.features,
        pending_tos: a.pending_tos,
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
) -> Result<LoginResult, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let auth = portal::update_me(backend, &first_name, &last_name).await?;
    Ok(LoginResult {
        user_id: auth.user_id,
        email: auth.email,
        first_name: auth.first_name,
        last_name: auth.last_name,
        display_name: auth.display_name,
        role: auth.role,
        is_platform_staff: auth.is_platform_staff,
        org_display_name: auth.org_display_name,
        recording_acknowledged: auth.recording_acknowledged,
        features: auth.features,
        pending_tos: auth.pending_tos,
    })
}

// ── Session handoff (#34) ────────────────────────────────────────────

/// Mint a single-use, 60-second session-handoff token via the backend
/// and return a fully-qualified `<portal>/handoff?token=<t>` URL the
/// user-menu's "Open web app" handler can pass straight to `openUrl`.
///
/// Building the URL Rust-side (rather than just the token) keeps the
/// portal base URL — derived from the configured backend host with the
/// `api.` → `app.` swap that mirrors `portal/src/lib/api.ts` — out of
/// the SvelteKit bundle and centralised next to the existing backend
/// config. The browser opens, the page redeems, the user lands on
/// /calls without a second login.
///
/// Returns `Err` if the user isn't signed in (no auth.json), the
/// network is down, or the backend rejects the mint (e.g. an
/// impersonating session — `mint` rejects those outright per #181).
#[tauri::command]
async fn mint_handoff_url() -> Result<String, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let token = portal::mint_handoff_token(backend).await?;
    let portal_base = derive_portal_base(&backend.url);
    Ok(format!("{portal_base}/handoff?token={token}"))
}

/// Map an api-host backend URL to the matching portal-host URL.
/// Mirrors portal/src/lib/api.ts §resolveBase in reverse:
/// `api.aftercalls.io` ↔ `app.aftercalls.io`. Localhost dev splits the
/// two by port (`:3001` API → `:5173` portal); we hard-code the dev
/// pair because that's what `pnpm dev` ships and it matches the
/// existing legacy setup. Anything we don't recognise falls through
/// untouched — the worst case is the browser lands on the api host
/// and renders nothing, and the user can re-launch from a fresh build.
fn derive_portal_base(api_url: &str) -> String {
    let trimmed = api_url.trim_end_matches('/');
    // Production: api.<apex> → app.<apex>.
    if let Some(rest) = trimmed.strip_prefix("https://api.") {
        return format!("https://app.{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("http://api.") {
        return format!("http://app.{rest}");
    }
    // Local dev: 127.0.0.1:3001 → 127.0.0.1:5173 (matches pnpm dev).
    if trimmed.contains("127.0.0.1:3001") || trimmed.contains("localhost:3001") {
        return trimmed
            .replace("127.0.0.1:3001", "127.0.0.1:5173")
            .replace("localhost:3001", "localhost:5173");
    }
    trimmed.to_string()
}

#[cfg(test)]
mod handoff_url_tests {
    use super::derive_portal_base;

    #[test]
    fn derive_portal_base_prod_https() {
        assert_eq!(
            derive_portal_base("https://api.aftercalls.io"),
            "https://app.aftercalls.io",
        );
    }

    #[test]
    fn derive_portal_base_trims_trailing_slash() {
        assert_eq!(
            derive_portal_base("https://api.aftercalls.io/"),
            "https://app.aftercalls.io",
        );
    }

    #[test]
    fn derive_portal_base_dev_localhost() {
        assert_eq!(
            derive_portal_base("http://127.0.0.1:3001"),
            "http://127.0.0.1:5173",
        );
        assert_eq!(
            derive_portal_base("http://localhost:3001"),
            "http://localhost:5173",
        );
    }

    #[test]
    fn derive_portal_base_unknown_pattern_passes_through() {
        // Custom staging URL we don't know about — fall through; the
        // handoff path may end up wrong but we return SOMETHING usable
        // rather than failing the click.
        assert_eq!(
            derive_portal_base("https://staging.example/"),
            "https://staging.example",
        );
    }
}

// ── PIPEDA recording-ack + org prefs (#44, #45, #48) ─────────────────

/// Check the backend for an existing recording-ack. Used as the
/// fallback when the cached `recording_acknowledged` flag says
/// false — we don't want to re-prompt users who acknowledged on
/// another device just because their local auth.json predates this
/// field. Returns true on 200, false on 404, error otherwise.
#[tauri::command]
async fn get_recording_ack() -> Result<bool, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let resp = portal::get_recording_ack(backend).await?;
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
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::post_recording_ack(backend, &agent_version, &platform).await?;
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
async fn get_recording_prefs() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::get_recording_prefs(backend).await
}

// ── ToS / privacy gate (#320) ────────────────────────────────────────

/// Wrap `GET /v1/tos/current`. Public endpoint — the wrapper sends an
/// auth header when one is available but doesn't require it. The
/// SvelteKit `/accept-terms` page calls this on mount to fetch the
/// `body_md` for each pending kind.
#[tauri::command]
async fn tos_current() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::tos_current(backend).await
}

/// Wrap `POST /v1/tos/accept`. After a successful POST, refetches
/// `/v1/auth/me` and persists the resulting bundle into `auth.json` so
/// the next `current_user` read sees the cleared `pending_tos` —
/// matching the portal's "POST then re-fetch /me" flow without
/// requiring a second roundtrip from the SvelteKit layer.
#[tauri::command]
async fn tos_accept(ids: Vec<String>) -> Result<LoginResult, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::tos_accept(backend, ids).await?;
    // Best-effort: a refetch failure leaves the cached pending_tos in
    // place but the acceptance itself succeeded server-side. We surface
    // the refresh error so the front-end can ask the user to reload —
    // matching the portal's "Acceptance recorded but the server still
    // reports pending items" branch.
    let auth = portal::refresh_me(backend).await?;
    Ok(LoginResult {
        user_id: auth.user_id,
        email: auth.email,
        first_name: auth.first_name,
        last_name: auth.last_name,
        display_name: auth.display_name,
        role: auth.role,
        is_platform_staff: auth.is_platform_staff,
        org_display_name: auth.org_display_name,
        recording_acknowledged: auth.recording_acknowledged,
        features: auth.features,
        pending_tos: auth.pending_tos,
    })
}

/// #592 — agent surface parity with the portal's `/settings/privacy`.
/// Wraps `GET /v1/auth/me/privacy`; returns the bundle that backs the
/// page paint (joined_at, TOS acceptances, calls count, first page of
/// the access log).
#[tauri::command]
async fn me_privacy_bundle() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_privacy_bundle(backend).await
}

/// #592 — cursor-paginated access log feeding the privacy page's "Load
/// more" CTA. `cursor` is the previous page's `next_cursor` (RFC-3339);
/// `limit` defaults to 25 to match the portal's TS client.
#[tauri::command]
async fn me_privacy_access_log(
    cursor: Option<String>,
    limit: Option<i64>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let resolved_limit = limit.unwrap_or(25);
    portal::me_privacy_access_log(backend, cursor.as_deref(), resolved_limit).await
}

/// #592 — POST `/v1/auth/me/export`. Kicks the data-export worker;
/// 24h cooldown enforced server-side surfaces as a 400 with a
/// `retry_after_seconds=N` token in the body so the frontend can show
/// a precise hint.
#[tauri::command]
async fn data_export_request() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::data_exports_request(backend).await
}

/// #592 — GET `/v1/auth/me/exports`. Newest-first list of the caller's
/// exports. Shape: `{ exports: DataExportRow[] }`. No download URLs in
/// this payload — call `data_export_get_status` for those.
#[tauri::command]
async fn data_export_list() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::data_exports_list(backend).await
}

/// #592 — GET `/v1/auth/me/exports/{id}`. Single row plus a freshly-
/// presigned `download_url` when the row is `ready` and the archive
/// hasn't expired. The frontend uses this to refresh the URL on every
/// click rather than caching whatever the list endpoint last reported.
#[tauri::command]
async fn data_export_get_status(
    id: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::data_exports_get_status(backend, &id).await
}

/// #595 — GET `/v1/import-candidates`. Caller's own open candidates
/// (per-user, not org-wide). `source` narrows by `ingest_source` —
/// `smartpbx` or `zoho_meeting`; pass `None` for both. `include_dismissed`
/// flips the default open-only filter so dismissed rows surface too.
/// Mirrors the portal's `api.importCandidates.list()` wrapper.
#[tauri::command]
async fn import_candidates_list(
    source: Option<String>,
    include_dismissed: Option<bool>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::import_candidates_list(
        backend,
        source.as_deref(),
        include_dismissed.unwrap_or(false),
    )
    .await
}

/// #595 — POST `/v1/import-candidates/{id}/import`. Promote a candidate
/// to a real `calls` row; the backend kicks the deferred download +
/// pipeline and returns `{ candidate_id, call_id, was_new }`. The
/// agent's `/calls` page uses `call_id` for the optimistic row
/// replacement.
#[tauri::command]
async fn import_candidate_import(
    id: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::import_candidate_import(backend, &id).await
}

/// #595 — POST `/v1/import-candidates/{id}/dismiss`. Soft-delete the
/// candidate so it stops appearing in the user's `/calls` candidate
/// list. Idempotent server-side; cross-org / unknown ids surface as a
/// 404 → `PortalError::NotFound`.
#[tauri::command]
async fn import_candidate_dismiss(id: String) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::import_candidate_dismiss(backend, &id).await
}

#[tauri::command]
async fn get_org_vocab() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::get_org_vocab(backend).await
}

#[tauri::command]
async fn set_org_vocab(
    custom_spelling: serde_json::Value,
    word_boost: Vec<String>,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::set_org_vocab(backend, &custom_spelling, &word_boost).await
}

#[tauri::command]
async fn list_highlights(call_id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::list_highlights(backend, &call_id).await
}

#[tauri::command]
async fn create_highlight(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::create_highlight(backend, &call_id, &body).await
}

#[tauri::command]
async fn update_highlight(id: String, body: serde_json::Value) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::update_highlight(backend, &id, &body).await
}

#[tauri::command]
async fn auto_highlight(call_id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::auto_highlight(backend, &call_id).await
}

#[tauri::command]
async fn delete_highlight(id: String) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::delete_highlight(backend, &id).await
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
async fn get_audio_urls(id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::get_audio_urls(backend, &id).await
}

#[derive(serde::Serialize)]
struct AppPrefs {
    close_to_tray: bool,
    auto_detect: bool,
    /// #180 — sub-toggle of auto_detect. When auto_detect is on but
    /// this is off, detection still fires the in-app slide-out
    /// (event emit) without raising / focusing the window. Settings
    /// UI hides this row when auto_detect is off.
    auto_detect_popup: bool,
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
    /// #149 (v0.4.7) — user-configurable self-note hotkey. Shipped
    /// default "Super+Shift+N"; `None` disables the global hotkey
    /// entirely (tray + button + CLI keep working).
    self_note_shortcut: Option<String>,
    /// #161 (v0.5.2) — user-configurable record-toggle hotkey.
    /// Shipped default "Super+Shift+R"; `None` disables the global
    /// hotkey entirely (tray + UI Record button + CLI keep working).
    record_toggle_shortcut: Option<String>,
    /// #56 — per-machine toggle for the spoken recording-start
    /// announcement. Surfaced in Settings only when the org's
    /// `recording_notification_mode` is `user`; when the org is in
    /// `enforced` mode the announcement plays unconditionally and
    /// the toggle is hidden. Default false so existing installs
    /// don't suddenly start speaking.
    consent_announcement_enabled: bool,
    /// #596 — auto-record master switches. Default false; the
    /// Settings page renders both as switches in the new "Auto-record"
    /// section. Round-tripped through this AppPrefs blob alongside
    /// every other per-machine pref so a single save() call covers
    /// any combination of changes.
    auto_record_start_enabled: bool,
    auto_record_stop_enabled: bool,
}

#[tauri::command]
fn get_app_prefs() -> Result<AppPrefs, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    Ok(AppPrefs {
        close_to_tray: cfg.close_to_tray,
        auto_detect: cfg.auto_detect,
        auto_detect_popup: cfg.auto_detect_popup,
        telemetry_enabled: cfg.telemetry_enabled,
        sounds_enabled: cfg.sounds_enabled,
        max_recording_minutes: cfg.max_recording_minutes,
        max_self_note_minutes: cfg.max_self_note_minutes,
        manual_notes_enabled: cfg.manual_notes_enabled,
        wayland_hotkey_notice_dismissed: cfg.wayland_hotkey_notice_dismissed,
        input_device: cfg.input_device,
        self_note_shortcut: cfg.self_note_shortcut,
        record_toggle_shortcut: cfg.record_toggle_shortcut,
        consent_announcement_enabled: cfg.consent_announcement_enabled,
        auto_record_start_enabled: cfg.auto_record_start_enabled,
        auto_record_stop_enabled: cfg.auto_record_stop_enabled,
    })
}

#[tauri::command]
fn set_app_prefs(
    app: AppHandle,
    close_to_tray: bool,
    auto_detect: bool,
    auto_detect_popup: bool,
    telemetry_enabled: bool,
    sounds_enabled: bool,
    max_recording_minutes: u32,
    max_self_note_minutes: u32,
    manual_notes_enabled: bool,
    wayland_hotkey_notice_dismissed: bool,
    input_device: Option<String>,
    self_note_shortcut: Option<String>,
    record_toggle_shortcut: Option<String>,
    consent_announcement_enabled: bool,
    auto_record_start_enabled: bool,
    auto_record_stop_enabled: bool,
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
    // #149 — same empty-as-None treatment for the self-note shortcut.
    // An empty string clears the binding; a non-empty value has to
    // parse (`parse_shortcut_str`) or we reject the save rather than
    // persist a dead binding.
    let self_note_shortcut = self_note_shortcut
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(ref s) = self_note_shortcut {
        if parse_shortcut_str(s).is_none() {
            return Err(format!("unrecognized shortcut: {s}"));
        }
    }
    // #161 — same empty-as-None treatment for the record-toggle
    // shortcut. Mirrors the self-note path above.
    let record_toggle_shortcut = record_toggle_shortcut
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(ref s) = record_toggle_shortcut {
        if parse_shortcut_str(s).is_none() {
            return Err(format!("unrecognized shortcut: {s}"));
        }
    }
    let mut cfg = config::Config::load().map_err(|e| e.to_string())?;
    let prev_self_note_shortcut = cfg.self_note_shortcut.clone();
    let prev_record_toggle_shortcut = cfg.record_toggle_shortcut.clone();
    cfg.close_to_tray = close_to_tray;
    cfg.auto_detect = auto_detect;
    cfg.auto_detect_popup = auto_detect_popup;
    cfg.telemetry_enabled = telemetry_enabled;
    cfg.sounds_enabled = sounds_enabled;
    cfg.max_recording_minutes = max_recording_minutes;
    cfg.max_self_note_minutes = max_self_note_minutes;
    cfg.manual_notes_enabled = manual_notes_enabled;
    cfg.wayland_hotkey_notice_dismissed = wayland_hotkey_notice_dismissed;
    cfg.input_device = input_device;
    cfg.self_note_shortcut = self_note_shortcut.clone();
    cfg.record_toggle_shortcut = record_toggle_shortcut.clone();
    cfg.consent_announcement_enabled = consent_announcement_enabled;
    cfg.auto_record_start_enabled = auto_record_start_enabled;
    cfg.auto_record_stop_enabled = auto_record_stop_enabled;
    cfg.save().map_err(|e| e.to_string())?;
    // #149 — re-register the self-note hotkey in place if it changed.
    // Best-effort: a failure to bind (portal denied, combo in use)
    // is logged, not surfaced, because tray + button + CLI keep
    // triggering note-to-self regardless.
    if prev_self_note_shortcut != self_note_shortcut {
        reapply_self_note_hotkey(
            &app,
            prev_self_note_shortcut.as_deref(),
            self_note_shortcut.as_deref(),
        );
    }
    // #161 — same rebind-in-place for the record-toggle shortcut.
    if prev_record_toggle_shortcut != record_toggle_shortcut {
        reapply_record_toggle_hotkey(
            &app,
            prev_record_toggle_shortcut.as_deref(),
            record_toggle_shortcut.as_deref(),
        );
    }
    Ok(())
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
async fn get_peaks(id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::get_peaks(backend, &id).await
}

#[tauri::command]
async fn delete_call(
    app: AppHandle,
    id: String,
    session_id: Option<String>,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::delete_call(backend, &id).await?;
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
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::update_utterance(backend, &id, idx, &speaker, speaker_user_id.as_deref()).await
}

#[tauri::command]
async fn rename_speaker(
    id: String,
    from: String,
    to: String,
    to_user_id: Option<String>,
    // #188: optional subset of utterance idxs. `None` or empty vec →
    // existing global rename; non-empty → subset-only rewrite on
    // backend.
    utterance_ids: Option<Vec<i32>>,
) -> Result<u64, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::rename_speaker(
        backend,
        &id,
        &from,
        &to,
        to_user_id.as_deref(),
        utterance_ids.as_deref(),
    )
    .await
}

// Slim org roster for the speaker-rename autocomplete (#65). Any
// authed member can read; callers that aren't logged in surface the
// auth-header error which the UI already swallows.
#[tauri::command]
async fn org_members() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::list_org_members(backend).await
}

#[tauri::command]
async fn update_call_tags(
    id: String,
    tags: serde_json::Value,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::update_call_tags(backend, &id, &tags).await
}

// ── Phase 2 (#19): resummarize + edit-in-place ────────────────────

/// POST /v1/calls/{id}/resummarize. Returns the updated CallDetail
/// on success; failures bubble up as a structured `PortalError`
/// (#124) so the front-end can switch on `kind === "cooldown"` /
/// `"network"` / `"server"` instead of regex-sniffing a stringified
/// error message.
#[tauri::command]
async fn resummarize_call(id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::resummarize_call(backend, &id).await
}

/// PATCH /v1/calls/{id}. Accepts tri-state fields verbatim from the
/// front-end JSON; forwarded to the backend without re-shaping so
/// the TS side is the authoritative definition of "absent vs null
/// vs value". Errors arrive as `PortalError` (#124).
#[tauri::command]
async fn patch_call(
    id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::patch_call(backend, &id, &body).await
}

/// POST /v1/calls/{id}/text-replace — highlight-to-correct (#11).
/// Body shape mirrors `text_replace::TextReplaceBody` on the backend
/// (verbatim forwarded so the TS layer stays the source of truth).
/// Returns `{ replaced, regions }` on success.
#[tauri::command]
async fn text_replace(
    id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::text_replace(backend, &id, &body).await
}

/// PATCH /v1/calls/{id}/action-items/{item_id}. Returns the updated
/// row; cross-org assignee writes bubble up as `PortalError::BadRequest`
/// (#124) which the caller renders as an inline picker error.
#[tauri::command]
async fn patch_action_item(
    call_id: String,
    item_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::patch_action_item(backend, &call_id, &item_id, &body).await
}

/// POST /v1/org/client-allowlist — auto-populate from the chip-menu
/// "Leave as text" path (#195). Fire-and-forget: the frontend calls
/// this and ignores the result so a failure doesn't nag the user.
/// Duplicates / cap rejections are server-side no-ops from the
/// caller's perspective.
#[tauri::command]
async fn add_client_allowlist_entry(
    name: String,
    source: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::add_client_allowlist_entry(backend, &name, &source).await
}

/// POST /v1/calls/{id}/action-items/manual — Phase 3 (#104) manual
/// add. Body is forwarded verbatim; frontend pre-shapes
/// `{description, assignee_user_id?}`. Backend returns 201 with the
/// created row which the caller appends to local state. Errors
/// arrive as `PortalError` (#124).
#[tauri::command]
async fn add_action_item(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::add_action_item(backend, &call_id, &body).await
}

// ── Share call CRUD shims (#243) ────────────────────────────────────
//
// Three IPC commands wrapping the backend's owner-side share routes
// (#35). The frontend modal calls these via `invoke()` and gets back
// the same JSON the portal's fetch wrapper would surface. Errors
// flow as the structured `PortalError` shape (#124) so the agent
// UI doesn't regex-sniff stringified messages.

/// POST /v1/calls/{id}/shares — body shape:
///   {expires_in_days?: number | null, included_sections?: {...}}
/// Returns the create-time response (raw token + assembled URL +
/// echoed `included_sections`). Token is shown once; subsequent
/// list calls never recover it.
#[tauri::command]
async fn create_call_share(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::create_call_share(backend, &call_id, &body).await
}

/// GET /v1/calls/{id}/shares — list active + historical shares.
/// Returns an array of summaries (status + view count + per-link
/// toggle row). Never includes the raw token / URL.
#[tauri::command]
async fn list_call_shares(
    call_id: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::list_call_shares(backend, &call_id).await
}

/// DELETE /v1/calls/{id}/shares/{share_id} — flip `revoked_at`. The
/// public reader 401s any subsequent open. Idempotent on already-
/// revoked rows.
#[tauri::command]
async fn revoke_call_share(
    call_id: String,
    share_id: String,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::revoke_call_share(backend, &call_id, &share_id).await
}

/// GET /v1/me/action-items — Phase 4 (#105). Returns the caller's
/// own action items across every call in their org. Cursor-paginated;
/// the frontend passes `cursor=null` for the first page and feeds
/// `next_cursor` back on follow-up pages. Errors arrive as
/// `PortalError` (#124).
#[tauri::command]
async fn list_me_action_items(
    status: String,
    cursor: Option<String>,
    limit: Option<i64>,
    // #173 — Due filter forwarded straight to the backend's
    // `?due=...` param. Default `all` matches the backend default.
    due: Option<String>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let resolved_limit = limit.unwrap_or(50);
    let resolved_due = due.unwrap_or_else(|| "all".to_string());
    portal::list_me_action_items(
        backend,
        &status,
        cursor.as_deref(),
        resolved_limit,
        &resolved_due,
    )
    .await
}

/// DELETE /v1/calls/{id}/action-items/{item_id} — Phase 3 (#104).
/// 404 is converted to Ok(()) on the portal helper side so the TS
/// frontend's deleteActionItem matches the portal's "silent success
/// on already-gone" behaviour (ui-phase-3 §G). Errors arrive as
/// `PortalError` (#124).
#[tauri::command]
async fn delete_action_item(
    call_id: String,
    item_id: String,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::delete_action_item(backend, &call_id, &item_id).await
}

/// GET /v1/org/zoho/status — Zoho connection probe (#186). Used by
/// the call-detail page on mount to gate the "Send to CRM" button.
/// Failure (env-disabled, network down) is the caller's concern;
/// the frontend treats it as "not connected" and just hides the
/// button.
#[tauri::command]
async fn zoho_status() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_status(backend).await
}

/// GET /v1/zoho/record-types — record-type picker payload (#197).
/// Returns `{ standard, custom, custom_refreshed_at }` so the
/// SendToZohoModal Step-1 picker can render standards + customs with
/// a divider. Failure (network, 404 not connected) bubbles to the
/// modal's `recordTypesError` banner; the picker keeps working with
/// the v1 fallback.
#[tauri::command]
async fn zoho_record_types() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_record_types(backend).await
}

/// GET /v1/zoho/records?module=…&q=… — Zoho record search (#186).
/// Step 2 of SendToZohoModal. Returns up to 20 hits per
/// (module, query) tuple.
#[tauri::command]
async fn zoho_search_records(
    module: String,
    q: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_search_records(backend, &module, &q).await
}

/// POST /v1/calls/{id}/zoho/push — push a call to Zoho as a Call
/// activity linked to the picked record (#186). Body is forwarded
/// verbatim from the SendToZohoModal Step-3 review state.
#[tauri::command]
async fn zoho_push_call(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_push_call(backend, &call_id, &body).await
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

/// #313 — single source of truth for "is the agent doing work that
/// blocks a quit / close." Reused by `quit_with_confirm` (tray Quit)
/// and the X-button window-event handler so both paths funnel
/// through the same confirmation dialog rather than each rolling
/// their own busy-check.
fn is_busy(app: &AppHandle) -> bool {
    let pipeline_busy = pipeline::is_pipeline_active();
    let recorder_busy = app
        .try_state::<recorder::Recorder>()
        .map(|r| r.is_active())
        .unwrap_or(false);
    pipeline_busy || recorder_busy
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

/// #149 (v0.4.7) — parse the user-facing shortcut string written by
/// the Settings capture widget + persisted to config.toml.
///
/// Accepts `+`-separated segments in any order, case-insensitive.
/// Modifier synonyms: `Super|Meta|Cmd|Command|Win` → SUPER,
/// `Ctrl|Control` → CONTROL, `Alt|Option|Opt` → ALT, `Shift` → SHIFT.
/// Key token accepts either a single letter/digit (`N`, `1`) or the
/// raw `Code` variant name (`KeyN`, `Digit1`, `F7`). Returns `None`
/// when no non-modifier key is supplied or any segment is unknown —
/// the caller either logs + ignores (setup path) or rejects the save
/// (set_app_prefs).
fn parse_shortcut_str(s: &str) -> Option<Shortcut> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for raw in s.split('+') {
        let seg = raw.trim();
        if seg.is_empty() {
            return None;
        }
        let lower = seg.to_ascii_lowercase();
        match lower.as_str() {
            "super" | "meta" | "cmd" | "command" | "win" => {
                mods |= Modifiers::SUPER;
            }
            "ctrl" | "control" => {
                mods |= Modifiers::CONTROL;
            }
            "alt" | "option" | "opt" => {
                mods |= Modifiers::ALT;
            }
            "shift" => {
                mods |= Modifiers::SHIFT;
            }
            _ => {
                if code.is_some() {
                    // Two non-modifier tokens in the same string is
                    // malformed; reject rather than silently dropping.
                    return None;
                }
                code = parse_key_code(seg);
                code?;
            }
        }
    }
    let code = code?;
    let mods = if mods.is_empty() { None } else { Some(mods) };
    Some(Shortcut::new(mods, code))
}

/// Map a single capture-string segment onto a Tauri `Code`. Accepts
/// either the raw `Code` variant name (`KeyN`, `Digit1`, `F7`) or a
/// user-friendly short form (`N`, `1`). The short form keeps the
/// settings input readable; the long form matches what the Svelte
/// side will canonicalise.
fn parse_key_code(raw: &str) -> Option<Code> {
    let upper = raw.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return match c {
                'A' => Some(Code::KeyA), 'B' => Some(Code::KeyB),
                'C' => Some(Code::KeyC), 'D' => Some(Code::KeyD),
                'E' => Some(Code::KeyE), 'F' => Some(Code::KeyF),
                'G' => Some(Code::KeyG), 'H' => Some(Code::KeyH),
                'I' => Some(Code::KeyI), 'J' => Some(Code::KeyJ),
                'K' => Some(Code::KeyK), 'L' => Some(Code::KeyL),
                'M' => Some(Code::KeyM), 'N' => Some(Code::KeyN),
                'O' => Some(Code::KeyO), 'P' => Some(Code::KeyP),
                'Q' => Some(Code::KeyQ), 'R' => Some(Code::KeyR),
                'S' => Some(Code::KeyS), 'T' => Some(Code::KeyT),
                'U' => Some(Code::KeyU), 'V' => Some(Code::KeyV),
                'W' => Some(Code::KeyW), 'X' => Some(Code::KeyX),
                'Y' => Some(Code::KeyY), 'Z' => Some(Code::KeyZ),
                _ => None,
            };
        }
        if c.is_ascii_digit() {
            return match c {
                '0' => Some(Code::Digit0), '1' => Some(Code::Digit1),
                '2' => Some(Code::Digit2), '3' => Some(Code::Digit3),
                '4' => Some(Code::Digit4), '5' => Some(Code::Digit5),
                '6' => Some(Code::Digit6), '7' => Some(Code::Digit7),
                '8' => Some(Code::Digit8), '9' => Some(Code::Digit9),
                _ => None,
            };
        }
    }
    if let Some(letter) = upper.strip_prefix("KEY") {
        return parse_key_code(letter);
    }
    if let Some(digit) = upper.strip_prefix("DIGIT") {
        return parse_key_code(digit);
    }
    if let Some(rest) = upper.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u8>() {
            return match n {
                1 => Some(Code::F1), 2 => Some(Code::F2), 3 => Some(Code::F3),
                4 => Some(Code::F4), 5 => Some(Code::F5), 6 => Some(Code::F6),
                7 => Some(Code::F7), 8 => Some(Code::F8), 9 => Some(Code::F9),
                10 => Some(Code::F10), 11 => Some(Code::F11), 12 => Some(Code::F12),
                13 => Some(Code::F13), 14 => Some(Code::F14), 15 => Some(Code::F15),
                16 => Some(Code::F16), 17 => Some(Code::F17), 18 => Some(Code::F18),
                19 => Some(Code::F19), 20 => Some(Code::F20), 21 => Some(Code::F21),
                22 => Some(Code::F22), 23 => Some(Code::F23), 24 => Some(Code::F24),
                _ => None,
            };
        }
    }
    None
}

/// #149 (v0.4.7) — rebind the self-note hotkey at runtime. Called
/// from `set_app_prefs` when `self_note_shortcut` changes; unregisters
/// the previous combo (if any) and re-registers the new one. Pure
/// side-effect wrapper so the settings save stays transactional.
fn reapply_self_note_hotkey(
    app: &AppHandle,
    prev: Option<&str>,
    next: Option<&str>,
) {
    if let Some(prev_str) = prev {
        if let Some(prev_sc) = parse_shortcut_str(prev_str) {
            if let Err(e) = app.global_shortcut().unregister(prev_sc) {
                eprintln!(
                    "aftercalls: unregister prev self-note shortcut failed ({e})"
                );
            }
        }
    }
    if let Some(next_str) = next {
        let Some(next_sc) = parse_shortcut_str(next_str) else {
            eprintln!("aftercalls: parse self-note shortcut failed: {next_str}");
            return;
        };
        if let Err(e) = app
            .global_shortcut()
            .on_shortcut(next_sc, |app, _sc, event| {
                if event.state() == ShortcutState::Pressed {
                    start_note_to_self(app);
                }
            })
        {
            eprintln!(
                "aftercalls: note-to-self shortcut unavailable ({e}); use the UI, tray, or --note-to-self CLI"
            );
        }
    }
}

/// #161 (v0.5.2) — rebind the record-toggle hotkey at runtime. Mirror
/// of `reapply_self_note_hotkey` above, but the handler calls
/// `toggle_recording` instead of `start_note_to_self`. Called from
/// `set_app_prefs` when `record_toggle_shortcut` changes.
fn reapply_record_toggle_hotkey(
    app: &AppHandle,
    prev: Option<&str>,
    next: Option<&str>,
) {
    if let Some(prev_str) = prev {
        if let Some(prev_sc) = parse_shortcut_str(prev_str) {
            if let Err(e) = app.global_shortcut().unregister(prev_sc) {
                eprintln!(
                    "aftercalls: unregister prev record-toggle shortcut failed ({e})"
                );
            }
        }
    }
    if let Some(next_str) = next {
        let Some(next_sc) = parse_shortcut_str(next_str) else {
            eprintln!("aftercalls: parse record-toggle shortcut failed: {next_str}");
            return;
        };
        if let Err(e) = app
            .global_shortcut()
            .on_shortcut(next_sc, |app, _sc, event| {
                if event.state() == ShortcutState::Pressed {
                    toggle_recording(app);
                }
            })
        {
            eprintln!(
                "aftercalls: record-toggle shortcut unavailable ({e}); use the UI, tray, or --toggle-recording CLI"
            );
        }
    }
}

fn setup_hotkey(app: &AppHandle) -> tauri::Result<()> {
    // #161 (v0.5.2) — record-toggle hotkey, now user-configurable.
    // Fresh installs get "Super+Shift+R" via the serde default in
    // config.rs. Parse failures log + fall through to the tray + UI
    // Record button + --toggle-recording CLI. Super+Shift+R has a rare
    // conflict vs. Ctrl+Shift+R (browser hard-reload).
    // On Wayland/Hyprland this still depends on
    // xdg-desktop-portal-hyprland implementing the GlobalShortcuts
    // portal; falling back to a Hyprland bind → CLI trigger is tracked
    // as a follow-up.
    let record_toggle_configured = config::Config::load()
        .ok()
        .and_then(|c| c.record_toggle_shortcut);
    if let Some(ref raw) = record_toggle_configured {
        match parse_shortcut_str(raw) {
            Some(rec_shortcut) => {
                if let Err(e) = app
                    .global_shortcut()
                    .on_shortcut(rec_shortcut, |app, _sc, event| {
                        if event.state() == ShortcutState::Pressed {
                            toggle_recording(app);
                        }
                    })
                {
                    eprintln!(
                        "aftercalls: record-toggle shortcut unavailable ({e}); use the UI, tray, or --toggle-recording CLI"
                    );
                }
            }
            None => {
                eprintln!(
                    "aftercalls: could not parse record-toggle shortcut {raw:?}; falling back to tray + UI + CLI"
                );
            }
        }
    }
    // #142 · v0.4.5 + #149 · v0.4.7 — self-note hotkey, now user-
    // configurable. Fresh installs get "Super+Shift+N" via the serde
    // default in config.rs. Parse failures log + fall through to the
    // tray / button / --note-to-self CLI.
    let configured = config::Config::load()
        .ok()
        .and_then(|c| c.self_note_shortcut);
    if let Some(ref raw) = configured {
        match parse_shortcut_str(raw) {
            Some(note_shortcut) => {
                if let Err(e) = app
                    .global_shortcut()
                    .on_shortcut(note_shortcut, |app, _sc, event| {
                        if event.state() == ShortcutState::Pressed {
                            start_note_to_self(app);
                        }
                    })
                {
                    eprintln!(
                        "aftercalls: note-to-self shortcut unavailable ({e}); use the UI, tray, or --note-to-self CLI"
                    );
                }
            }
            None => {
                eprintln!(
                    "aftercalls: could not parse self-note shortcut {raw:?}; falling back to tray + CLI"
                );
            }
        }
    }
    Ok(())
}

/// #286 — Surface the OS-level "your call is ready" tray
/// notification. Frontend (agent call detail page) calls this when a
/// previously-delayed pipeline finishes so the user gets pinged even
/// if the agent window is buried. Mirrors the existing
/// `pipeline::notify_done` shape — title + short body, fire-and-forget,
/// Result<()> ignored on the JS side. The notification permission is
/// already granted via `capabilities/default.json` (`notification:default`,
/// shipped originally for the recorder pipeline).
#[tauri::command]
fn notify_call_ready(app: AppHandle, title: String, body: String) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    let display_title = if title.trim().is_empty() {
        "aftercalls: your call is ready".to_string()
    } else {
        format!("aftercalls: '{}' is ready", title.trim())
    };
    let display_body = if body.trim().is_empty() {
        "Your call is ready to view.".to_string()
    } else {
        body
    };
    app.notification()
        .builder()
        .title(display_title)
        .body(display_body)
        .show()
        .map_err(|e| e.to_string())
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
            // #596 — auto-record per-app whitelist surface. Five
            // commands + four events drive the Settings page + cancel
            // toast; see the AutoRecord module group above.
            auto_record_settings_get,
            auto_record_settings_set_master,
            auto_record_settings_toggle_app,
            auto_record_settings_forget_app,
            confirm_auto_record_cancel,
            login,
            logout,
            current_user,
            backend_health,
            update_me,
            // #34 — agent → portal session handoff. Frontend calls this
            // from the user-menu "Open web app" item; returns a fully-
            // qualified handoff URL the menu hands straight to
            // `openUrl`.
            mint_handoff_url,
            list_calls,
            tag_suggestions,
            list_trashed,
            restore_call,
            permadelete_call,
            hydrate_call,
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
            text_replace,
            patch_action_item,
            add_client_allowlist_entry,
            add_action_item,
            delete_action_item,
            list_me_action_items,
            // #243 — share-call CRUD shims (mirrors the portal's
            // `api.calls.{create,list,revoke}Share`). All three return
            // the structured PortalError shape from #124.
            create_call_share,
            list_call_shares,
            revoke_call_share,
            get_recording_ack,
            post_recording_ack,
            get_recording_prefs,
            // #320 — ToS / privacy gate parity with the portal. Layout
            // routes to /accept-terms when current_user surfaces a
            // non-empty pending_tos; the page hits these two commands
            // to render + accept.
            tos_current,
            tos_accept,
            // #592 — `/settings/privacy` parity with the portal. Bundle
            // + paginated access log back the read-only page paint;
            // the three data-export commands drive the "Export my
            // data" action card.
            me_privacy_bundle,
            me_privacy_access_log,
            data_export_request,
            data_export_list,
            data_export_get_status,
            // #595 — per-user import-candidate flow. Mirror of the
            // portal's `api.importCandidates.*` client; the agent's
            // `/calls` page renders these alongside real call rows
            // and uses the same Import / Dismiss button cluster.
            import_candidates_list,
            import_candidate_import,
            import_candidate_dismiss,
            list_orphan_sessions,
            resume_orphan_session,
            discard_orphan_session,
            // #186 Zoho CRM: status probe + search + push.
            // Connect/disconnect happens on the portal admin page; the
            // agent links out via openUrl, so no connect command here.
            zoho_status,
            zoho_record_types,
            zoho_search_records,
            zoho_push_call,
            notes::save_notes,
            notes::load_notes,
            notes::save_title,
            notes::update_call_notes,
            // #183 — in-agent support reports.
            // #203 — stage_support_video lands webview-produced webm
            // blobs on a temp path so the existing path-based submit
            // pipeline rides unchanged.
            support::inspect_support_attachment,
            support::stage_support_video,
            support::submit_support_report,
            // #286 — surface the OS tray notification when a
            // previously-delayed call finishes processing. Called
            // from the agent call-detail page.
            notify_call_ready,
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

            // #203: sweep any lingering support-report video stage
            // dirs older than 24 h. If a crash interrupted a submit
            // yesterday, those temp bytes get cleaned up here rather
            // than sitting forever. Non-blocking and best-effort.
            support::sweep_stage_dir();

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

            // #596 — auto-record observer + state machine. Open the
            // local sqlite store; if it fails (disk full, perm
            // denied), log + continue without auto-record so the rest
            // of the agent stays functional. The Settings UI will see
            // an empty observed_apps list and the master toggles flip
            // configs but never trigger a start.
            match AutoRecorder::open() {
                Ok(auto) => {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    audio_observer::spawn(tx);
                    auto.run(app.handle().clone(), rx);
                    app.manage(auto);
                }
                Err(e) => {
                    eprintln!("aftercalls: auto-record disabled — store open failed: {e}");
                    telemetry::log(
                        "warn",
                        "auto_record::disabled",
                        format!("auto_record store open failed: {e}"),
                        None,
                        None,
                    );
                }
            }
            Ok(())
        })
        .on_window_event(|win, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if win.label() == "main" {
                    let app = win.app_handle().clone();
                    // #313 — busy-state takes precedence over the
                    // close-to-tray preference. A user who clicks X
                    // mid-recording (or while the post-call pipeline
                    // is still running) hits the same confirmation
                    // dialog the tray Quit path uses, regardless of
                    // their close_to_tray pref. quit_with_confirm
                    // routes through the dialog and exits on confirm
                    // / no-ops on cancel; we just need to prevent the
                    // close from running synchronously.
                    if is_busy(&app) {
                        api.prevent_close();
                        quit_with_confirm(app);
                        return;
                    }
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
                    } else {
                        // #313 — close-to-tray = false AND not busy →
                        // honour the user's pref and let the close run,
                        // but emit a one-time advisory event so the
                        // SvelteKit layout can render a dismissible
                        // ".hotkey-note" on the next launch reading
                        // "X closes the window — aftercalls keeps
                        // running in the tray". The webview listener
                        // gates on a localStorage flag so the note
                        // only ever appears once per machine.
                        //
                        // Emitted before the close completes so the
                        // event queue lands the payload while the
                        // webview is still alive; the layout listener
                        // immediately writes its localStorage flag,
                        // and the next process startup picks the note
                        // up from there.
                        let _ = app.emit("close-advisory", ());
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
