mod app_observations;
mod audio_observer;
mod auto_recorder;
mod config;
mod detector;
mod error;
mod ipc_security;
mod live;
#[cfg(target_os = "macos")]
mod macos_loopback;
mod media_manifest;
mod media_process;
mod media_upload;
mod mic_consumers;
mod notes;
mod notify_actions;
mod permissions;
mod pipeline;
mod portal;
mod recorder;
mod recovery;
mod rolling_encode;
mod screen_recorder;
mod session_fs;
mod summary;
mod support;
mod telemetry;
mod transcription;
mod upload;
mod vault;

use auto_recorder::AutoRecorder;
use detector::{Detector, UserDecision};
use recorder::Recorder;
use rolling_encode::RollingEncoder;
use screen_recorder::ScreenRecorder;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent, Wry,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

// Holds references to tray menu items we need to mutate (toggle label) so we
// can fetch them out of app state instead of hunting through the menu tree.
struct TrayItems {
    toggle: MenuItem<Wry>,
}

/// Process-wide transaction boundary for recording lifecycle changes. Every
/// producer (audio, rolling Opus, screen video, live relay) and the public UI
/// state transition is started/stopped while this one mutex is held.
///
/// The generation token is shared with every producer. A caller captures the
/// token it intends to stop before waiting for the mutex; if another stop +
/// start wins the race, the stale token is rejected instead of tearing down
/// the newer recording.
struct RecordingLifecycle {
    inner: Mutex<LifecycleState>,
}

#[derive(Debug, Default)]
struct LifecycleState {
    next_generation: u64,
    active: Option<LifecycleToken>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LifecycleToken {
    generation: u64,
    session_dir: PathBuf,
}

struct StartTransition<'a> {
    guard: MutexGuard<'a, LifecycleState>,
    generation: u64,
}

struct StopTransition<'a> {
    guard: MutexGuard<'a, LifecycleState>,
    token: LifecycleToken,
}

impl RecordingLifecycle {
    fn new() -> Self {
        Self {
            inner: Mutex::new(LifecycleState::default()),
        }
    }

    fn begin_start(&self) -> Result<StartTransition<'_>, String> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(active) = &guard.active {
            return Err(format!(
                "recording already in progress ({})",
                active.session_dir.display()
            ));
        }
        guard.next_generation = guard
            .next_generation
            .checked_add(1)
            .ok_or_else(|| "recording lifecycle generation exhausted".to_string())?;
        let generation = guard.next_generation;
        Ok(StartTransition { guard, generation })
    }

    fn current_token(&self) -> Option<LifecycleToken> {
        self.inner.lock().unwrap().active.clone()
    }

    fn token_for_session(&self, session_dir: &std::path::Path) -> Option<LifecycleToken> {
        self.inner
            .lock()
            .unwrap()
            .active
            .as_ref()
            .filter(|active| active.session_dir == session_dir)
            .cloned()
    }

    fn begin_stop(&self, token: &LifecycleToken) -> Result<StopTransition<'_>, String> {
        let guard = self.inner.lock().unwrap();
        match guard.active.as_ref() {
            Some(active) if active == token => Ok(StopTransition {
                guard,
                token: token.clone(),
            }),
            Some(active) => Err(format!(
                "stale stop rejected: requested generation {} for {}, active generation is {} for {}",
                token.generation,
                token.session_dir.display(),
                active.generation,
                active.session_dir.display()
            )),
            None => Err("no active recording".into()),
        }
    }
}

impl StartTransition<'_> {
    fn generation(&self) -> u64 {
        self.generation
    }

    fn commit(mut self, session_dir: PathBuf) -> LifecycleToken {
        let token = LifecycleToken {
            generation: self.generation,
            session_dir,
        };
        self.guard.active = Some(token.clone());
        token
    }
}

impl StopTransition<'_> {
    fn token(&self) -> &LifecycleToken {
        &self.token
    }

    /// Clear the lifecycle identity only after the audio worker has
    /// authoritatively stopped. The guard remains held until the whole stop
    /// transaction (auxiliary producers + UI event + pipeline handoff) ends.
    fn mark_stopped(&mut self) {
        if self.guard.active.as_ref() == Some(&self.token) {
            self.guard.active = None;
        }
    }
}

#[derive(Clone, Copy)]
enum TrayState {
    Idle,
    Recording,
    SelfNote,
    Processing,
}

impl TrayState {
    fn to_u8(self) -> u8 {
        match self {
            TrayState::Idle => 0,
            TrayState::Recording => 1,
            TrayState::SelfNote => 2,
            TrayState::Processing => 3,
        }
    }

    fn from_u8(n: u8) -> Self {
        match n {
            1 => TrayState::Recording,
            2 => TrayState::SelfNote,
            3 => TrayState::Processing,
            _ => TrayState::Idle,
        }
    }
}

/// #634 — last-applied `TrayState`, mirrored into managed state on
/// every `apply_tray_state` call so the unread-badge IPC path can
/// repaint the tooltip + icon without collapsing SelfNote / Processing
/// down to the binary recording-or-not derivation. `AtomicU8` keeps
/// this lock-free; the discriminant set is fixed at four values.
struct CurrentTrayState(AtomicU8);
impl CurrentTrayState {
    fn new() -> Self {
        Self(AtomicU8::new(TrayState::Idle.to_u8()))
    }
    fn get(&self) -> TrayState {
        TrayState::from_u8(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, state: TrayState) {
        self.0.store(state.to_u8(), Ordering::Relaxed);
    }
}

/// #634 — process-wide unread-call counter. The webview's
/// `+layout.svelte` poll calls `set_unread_badge(count)` every 60s
/// (and on every `unread-count-changed` window event) so the tray
/// tooltip stays accurate without each tray-state transition having
/// to re-fetch from the backend. Wired into `apply_tray_state` so the
/// tooltip composition reads the latest count on every state flip.
///
/// `AtomicU32` keeps the surface lock-free; we don't care about
/// strict ordering between the JS poll and the recorder's state
/// flips — the worst case is a 60s-stale count for a single tooltip
/// repaint, fixed by the next poll tick.
struct TrayUnreadCount(AtomicU32);
impl TrayUnreadCount {
    fn new() -> Self {
        Self(AtomicU32::new(0))
    }
    fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
    fn set(&self, n: u32) {
        self.0.store(n, Ordering::Relaxed);
    }
}

pub(crate) fn tray_set_processing(app: &AppHandle) {
    apply_tray_state(app, TrayState::Processing);
}

pub(crate) fn tray_refresh_after_pipeline(app: &AppHandle, pipeline_still_active: bool) {
    if app
        .try_state::<recorder::Recorder>()
        .map(|r| r.is_active())
        .unwrap_or(false)
    {
        apply_tray_state(app, TrayState::Recording);
    } else if pipeline_still_active {
        apply_tray_state(app, TrayState::Processing);
    } else {
        apply_tray_state(app, TrayState::Idle);
    }
}

fn apply_tray_state(app: &AppHandle, state: TrayState) {
    // #634 — record the state we're about to paint so the unread-badge
    // IPC path can repaint without re-deriving from the recorder
    // (which collapses SelfNote → Recording and Processing → Idle).
    if let Some(current) = app.try_state::<CurrentTrayState>() {
        current.set(state);
    }
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    // tauri::include_image! decodes the PNG at compile time into raw RGBA,
    // so this is a zero-IO, zero-decode swap at runtime.
    let (img, base_tip): (Image<'static>, &str) = match state {
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
    let _ = tray.set_tooltip(Some(compose_tray_tooltip(app, state, base_tip).as_str()));

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

/// #634 — fold the live unread-call count into the tray tooltip per
/// the ui.md matrix. macOS NSStatusItem + Windows notify-area icons +
/// Linux libayatana-appindicator all expose the tooltip uniformly;
/// numeric badges on the icon itself are not first-class on any of
/// the three menu-bar surfaces (NSStatusItem has no badge API at all,
/// Windows tray overlay icons require a per-state image variant, and
/// Linux DE coverage is sparse). Tooltip is the lowest-common-
/// denominator that works everywhere.
///
/// Format follows the issue-634 ui.md spec:
///   Idle, no unread       → "aftercalls"
///   Idle, N unread        → "aftercalls — N unread"
///   Busy state, no unread → "<base_tip>"             (e.g. "aftercalls — recording")
///   Busy state, N unread  → "<base_tip> (N unread)"
///
/// Number is the raw integer (no "99+"); the tooltip has space and
/// the full count is more informative than a cap.
fn compose_tray_tooltip(app: &AppHandle, state: TrayState, base_tip: &str) -> String {
    let count = app
        .try_state::<TrayUnreadCount>()
        .map(|s| s.get())
        .unwrap_or(0);
    if count == 0 {
        // Idle's "no unread" tip per ui.md is the bare word
        // "aftercalls" without the " — idle" suffix; busy states keep
        // the existing suffix because the suffix carries useful info
        // (recording / processing / etc.).
        return match state {
            TrayState::Idle => "aftercalls".to_string(),
            _ => base_tip.to_string(),
        };
    }
    match state {
        TrayState::Idle => format!("aftercalls — {count} unread"),
        _ => format!("{base_tip} ({count} unread)"),
    }
}

/// #634 — re-paint the tray with the latest unread-count tooltip,
/// preserving whatever `TrayState` was last applied. Used by
/// `set_unread_badge` so a webview-side count change repaints the
/// tooltip without the layout having to know what recording state the
/// agent is in. Reads from the `CurrentTrayState` managed cell rather
/// than re-deriving from `Recorder::is_active()` — the recorder-only
/// derivation collapses SelfNote → Recording and Processing → Idle,
/// which produced a visible flicker on every poll tick when the agent
/// was self-note-recording or post-stop processing.
fn tray_refresh_with_current_state(app: &AppHandle) {
    let state = app
        .try_state::<CurrentTrayState>()
        .map(|s| s.get())
        .unwrap_or(TrayState::Idle);
    apply_tray_state(app, state);
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
    /// #660 co-pilot P1 — the live `session_uuid` minted at record-start
    /// for a Call capture with the `live_transcript` relay open. Surfaced
    /// to the webview here (it was previously only persisted to disk +
    /// used by the post-call reconcile) so the co-pilot ask-chip /
    /// highlight surfaces can address the live session. Present only on
    /// the start transition of a live Call; absent for self-notes, stops,
    /// and flag-off orgs. Additive + `skip_if_none`, so the recording-
    /// state shape stays backward-compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_uuid: Option<String>,
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
    if let Err(e) = session_fs::write_private_file(&path, payload.to_string().as_bytes()) {
        eprintln!(
            "aftercalls: failed to write source.json for {}: {e}",
            session_dir.display()
        );
    }
}

/// #live — persist the record-start `session_uuid` into the session_dir so
/// the post-call create_call flow can hand it to the backend, which
/// reconciles the disposable live session to the newly-created call row.
/// Written only for sessions that opened a live relay; absent otherwise.
pub(crate) fn write_live_session(session_dir: &std::path::Path, session_uuid: &str) {
    let payload = serde_json::json!({ "session_uuid": session_uuid });
    let path = session_dir.join("live_session.json");
    if let Err(e) = session_fs::write_private_file(&path, payload.to_string().as_bytes()) {
        eprintln!(
            "aftercalls: failed to write live_session.json for {}: {e}",
            session_dir.display()
        );
    }
}

fn emit_state(
    app: &AppHandle,
    recording: bool,
    mode: Option<&'static str>,
    session_dir: Option<String>,
    // #660 — the live session_uuid for a Call start (None otherwise).
    session_uuid: Option<String>,
) {
    // #659 P4 — keep the overlay's cold-start cache in step with the
    // recording lifecycle. A Call start resets the cached coaching snapshot +
    // stashes the session_uuid (so a freshly-opened overlay can address the
    // session); any stop marks not-recording (final snapshot stays glanceable
    // through the grace period). Self-notes never open the overlay, so their
    // starts don't touch the cache.
    if let Some(cache) = app.try_state::<LiveSnapshotCache>() {
        if recording {
            if mode == Some("call") {
                cache.begin_session(session_uuid.clone());
            }
        } else {
            cache.end_session();
        }
    }

    let _ = app.emit(
        "recording-state",
        RecordingStateEvent {
            recording,
            mode,
            session_dir,
            session_uuid,
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
/// self path (`do_start_self_note`). Captures the lifecycle generation
/// so a manual stop-then-start can't let a stale watchdog nuke the
/// new session. `minutes` is the cap the caller picks (per-user
/// `max_recording_minutes` for regular calls, `max_self_note_minutes`
/// for self-notes).
fn spawn_max_length_watchdog(
    app: &AppHandle,
    token: LifecycleToken,
    minutes: u32,
    label: &'static str,
) {
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
            if rec.active_generation() != Some(token.generation) {
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
                if let Err(e) = do_stop_token(&rec, &app_for_watchdog, token.clone()) {
                    eprintln!("aftercalls: watchdog auto-stop failed: {e}");
                }
                return;
            }
        }
    });
}

/// Payload for the `screen-source-request` event — the global
/// `ScreenSourceChooser` reads `sources` (the advertised kinds) to render
/// only the buttons this platform can drive, and echoes `session_dir` back
/// into `start_screen_source` so a stale request (call already stopped /
/// restarted) is rejected.
#[derive(serde::Serialize, Clone)]
struct ScreenSourceRequest {
    session_dir: String,
    sources: Vec<String>,
}

/// #302 follow-up — at Call-mode record-start, EITHER ask the user to pick a
/// screen / window / area for this call (ask-each-call, the default) OR
/// auto-start the remembered screen (opt-in). Gates ONLY: Call mode (this is
/// only ever called from `do_start`; self-notes take `do_start_self_note`
/// and NEVER capture the screen), the org `screen_capture` feature (cached in
/// auth.json), the per-user `screen_capture_enabled` opt-in, and a runtime-
/// available backend advertising at least one source kind. A `true`
/// `screen_capture_enabled` implies the user completed the consent ack; the
/// backend upload path is the authoritative consent backstop. This helper
/// NEVER blocks or fails the audio recording — it returns instantly (the
/// ask-each-call path just emits an event; the actual capture starts later
/// via `start_screen_source`).
fn maybe_request_screen_source(
    app: &AppHandle,
    session_dir: &std::path::Path,
    generation: u64,
) {
    // Org feature flag (cached in auth.json). Absent / off → no capture.
    let feature_on = config::read_auth_file()
        .ok()
        .flatten()
        .map(|a| a.features.screen_capture)
        .unwrap_or(false);
    if !feature_on {
        return;
    }
    // Per-user opt-in + the capture knobs.
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(_) => return,
    };
    if !cfg.screen_capture_enabled {
        return;
    }
    // Backend availability + advertised kinds. Absent backend / empty kinds
    // (e.g. macOS) → nothing to offer; skip silently.
    let sources = screen_recorder::supported_source_kinds();
    if sources.is_empty() || !app.state::<ScreenRecorder>().is_available() {
        return;
    }

    if cfg.screen_capture_ask_each_call {
        // Ask-each-call (default): hand the chooser the advertised kinds. The
        // user's pick invokes `start_screen_source`; Cancel = audio-only.
        let _ = app.emit(
            "screen-source-request",
            ScreenSourceRequest {
                session_dir: session_dir.to_string_lossy().into_owned(),
                sources: sources.iter().map(|s| s.to_string()).collect(),
            },
        );
        return;
    }

    // Remembered-screen (opt-in): auto-start the saved display (Screen only —
    // window/area are inherently per-call). Best-effort; the bool is advisory.
    let start_cfg = screen_recorder::StartConfig {
        fps: cfg.screen_capture_fps,
        resolution: cfg.screen_capture_resolution.clone(),
        bitrate_kbps: cfg.screen_capture_bitrate_kbps,
    };
    let source = screen_recorder::CaptureSource::Screen {
        monitor: cfg.screen_capture_display.clone(),
    };
    let audio_started_at_ms = app
        .state::<Recorder>()
        .started_at_ms()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let _ = app
        .state::<ScreenRecorder>()
        .start(
            session_dir,
            generation,
            source,
            &start_cfg,
            audio_started_at_ms,
        );
}

pub(crate) fn do_start(
    state: &Recorder,
    app: &AppHandle,
    // #653 — Zoho contact id the user pre-picked in the co-pilot panel,
    // forwarded onto the live-relay `start` frame so the backend resolves
    // the counterpart off the audio hot path. `None` for CLI/hotkey starts
    // and whenever copilot is off or no contact was chosen.
    contact_hint: Option<String>,
    source_kind: &'static str,
    source_app: Option<&str>,
) -> Result<String, String> {
    let lifecycle = app.state::<RecordingLifecycle>();
    let transition = lifecycle.begin_start()?;
    let generation = transition.generation();

    // A lifecycle identity should own every auxiliary producer. Refuse to
    // start over an orphaned producer rather than silently replacing it and
    // risking capture/upload attribution across sessions.
    if let Some(active) = app.state::<RollingEncoder>().active_generation() {
        return Err(format!(
            "rolling encoder is still active for generation {active}"
        ));
    }
    if let Some(active) = app.state::<ScreenRecorder>().active_generation() {
        return Err(format!(
            "screen recorder is still active for generation {active}"
        ));
    }
    if let Some(active) = app.state::<live::LiveRelay>().active_generation() {
        return Err(format!("live relay is still active for generation {active}"));
    }

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

    // #live — Live-transcript relay (Phase 1). Gated on the org
    // `live_transcript` feature flag (cached in auth.json). When on, mint a
    // session_uuid at record-start and open the relay: it streams a lossy
    // COPY of the mic/system audio to the backend and forwards draft
    // segments to the UI. The relay degrades silently on any failure and
    // never blocks or fails the recording — the batch pipeline stays the
    // source of truth. Flag-off orgs pay nothing here.
    let live_on = config::read_auth_file()
        .ok()
        .flatten()
        .map(|a| a.features.live_transcript)
        .unwrap_or(false);
    let mut live_tap = None;
    let mut session_uuid = None;
    if live_on {
        if let Some(backend) = config::Config::load().ok().and_then(|c| c.backend) {
            let uuid = uuid::Uuid::new_v4().to_string();
            live_tap = Some(app.state::<live::LiveRelay>().begin(
                app.clone(),
                backend.url,
                uuid.clone(),
                generation,
                contact_hint.clone(),
            ));
            session_uuid = Some(uuid);
        }
    }

    let path = match state.start(base, generation, saved_device, false, live_tap) {
        Ok(p) => p,
        Err(e) => {
            // Recording never started — tear down the relay we just opened.
            app.state::<live::LiveRelay>().end(generation);
            return Err(e);
        }
    };
    if let Err(e) = media_manifest::initialize(&path) {
        let cleanup = state.stop_generation(generation, &path).err();
        app.state::<live::LiveRelay>().end(generation);
        return Err(format!(
            "initialize durable media checkpoint: {e:#}{}",
            cleanup
                .map(|error| format!("; recorder cleanup also failed: {error}"))
                .unwrap_or_default()
        ));
    }

    // Persist the session_uuid so the post-call create_call flow can hand it
    // to the backend for live-session reconciliation.
    if let Some(uuid) = &session_uuid {
        write_live_session(&path, uuid);
    }
    // Persist source attribution before publishing the active lifecycle token.
    // A Stop caller cannot pass the lifecycle mutex until this write finishes,
    // so the pipeline can never race a just-returned Start and observe a
    // missing/default source descriptor.
    write_session_source(&path, source_kind, source_app);

    // chunked-upload — start the rolling per-channel Opus encode so the
    // mic + system `.opus` are ready at stop (encode off the end-of-call
    // path). Starting this producer is part of the lifecycle transaction: an
    // invariant failure aborts the session instead of silently starting only
    // a subset of its owned producers.
    if let Err(error) = app.state::<RollingEncoder>().start(&path, generation) {
        let cleanup = state.stop_generation(generation, &path).err();
        app.state::<live::LiveRelay>().end(generation);
        return Err(format!(
            "start rolling media encoder: {error}{}",
            cleanup
                .map(|error| format!("; recorder cleanup also failed: {error}"))
                .unwrap_or_default()
        ));
    }

    // Publish the recording/session identity BEFORE any dependent request.
    // Tauri preserves event order; the chooser can therefore correlate the
    // following screen-source request with the live store instead of seeing
    // an idle state and immediately dismissing itself.
    emit_state(
        app,
        true,
        Some("call"),
        Some(path.to_string_lossy().into_owned()),
        // #660 — surface the live session_uuid (Some only when the
        // relay opened) so the co-pilot ask/highlight surfaces can
        // address it. Same value already persisted to live_session.json.
        session_uuid.clone(),
    );

    // #302 follow-up — best-effort per-call screen-source request (Call mode
    // only). Emits the chooser event (ask-each-call) or auto-starts the
    // remembered screen. Returns instantly; NEVER blocks or fails the audio
    // recording.
    maybe_request_screen_source(app, &path, generation);
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
    let token = transition.commit(path.clone());
    spawn_max_length_watchdog(app, token, max_minutes, "recording");
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
    let lifecycle = app.state::<RecordingLifecycle>();
    let transition = lifecycle.begin_start()?;
    let generation = transition.generation();
    if let Some(active) = app.state::<RollingEncoder>().active_generation() {
        return Err(format!(
            "rolling encoder is still active for generation {active}"
        ));
    }
    if let Some(active) = app.state::<ScreenRecorder>().active_generation() {
        return Err(format!(
            "screen recorder is still active for generation {active}"
        ));
    }
    if let Some(active) = app.state::<live::LiveRelay>().active_generation() {
        return Err(format!("live relay is still active for generation {active}"));
    }
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    let saved_device = config::Config::load()
        .ok()
        .and_then(|c| c.input_device);
    // Note-to-self is mic-only private dictation — no live You/Them relay in
    // Phase 1 (no system channel, and the feature targets calls). Pass a
    // `None` tap so the recorder skips the live copy entirely.
    let path = state.start(base, generation, saved_device, true, None)?;
    if let Err(e) = media_manifest::initialize(&path) {
        let cleanup = state.stop_generation(generation, &path).err();
        return Err(format!(
            "initialize durable media checkpoint: {e:#}{}",
            cleanup
                .map(|error| format!("; recorder cleanup also failed: {error}"))
                .unwrap_or_default()
        ));
    }
    write_session_source(&path, "self_note", None);
    emit_state(
        app,
        true,
        Some("self_note"),
        Some(path.to_string_lossy().into_owned()),
        // Self-notes are mic-only with no live relay → no session_uuid.
        None,
    );
    if let Some(fallback) = state.take_last_fallback() {
        let _ = app.emit("mic-fallback", &fallback);
    }
    let max_minutes = config::Config::load()
        .map(|c| c.max_self_note_minutes)
        .unwrap_or(5);
    let token = transition.commit(path.clone());
    spawn_max_length_watchdog(app, token, max_minutes, "note-to-self");
    Ok(path.to_string_lossy().into_owned())
}

pub(crate) fn do_stop(state: &Recorder, app: &AppHandle) -> Result<String, String> {
    let token = app
        .state::<RecordingLifecycle>()
        .current_token()
        .ok_or_else(|| "no active recording".to_string())?;
    do_stop_token(state, app, token)
}

pub(crate) fn do_stop_session(
    state: &Recorder,
    app: &AppHandle,
    expected_session_dir: &std::path::Path,
) -> Result<String, String> {
    let token = app
        .state::<RecordingLifecycle>()
        .token_for_session(expected_session_dir)
        .ok_or_else(|| {
            format!(
                "stale stop rejected: {} is no longer the active recording",
                expected_session_dir.display()
            )
        })?;
    do_stop_token(state, app, token)
}

fn do_stop_token(
    state: &Recorder,
    app: &AppHandle,
    token: LifecycleToken,
) -> Result<String, String> {
    let lifecycle = app.state::<RecordingLifecycle>();
    let mut transition = lifecycle.begin_stop(&token)?;
    let token = transition.token().clone();

    // Every teardown is attempted even when another component reports an
    // error. The lifecycle mutex remains held until state emission and the
    // pipeline handoff, preventing an old stop from overwriting a new start.
    // Native safety backstop: close/invalidate the always-on-top area picker
    // before potentially slow media finalization. This does not depend on a
    // responsive main webview observing `recording-state`.
    close_region_select_window(app);
    let recorder_result = state.stop_generation(token.generation, &token.session_dir);
    let recorder_report = recorder_result.as_ref().ok();
    let expected_session = Some(token.session_dir.clone());

    let screen_report = app
        .state::<ScreenRecorder>()
        .stop_and_persist(Some(token.generation), expected_session.as_deref());
    app.state::<live::LiveRelay>().end(token.generation);

    let rolling_report = app
        .state::<RollingEncoder>()
        .stop_and_persist(token.generation, expected_session.as_deref());

    // Repair public/UI state after all synchronous producers have received
    // their stop signal. A resolved worker clears the authoritative active
    // flag even when individual finalizers failed; a transport/worker failure
    // remains visibly active and continues blocking Start until resolved.
    let recorder_still_active = state.is_active();
    if !recorder_still_active {
        transition.mark_stopped();
    }
    let active_session = if recorder_still_active {
        expected_session
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
    } else {
        None
    };
    emit_state(
        app,
        recorder_still_active,
        None,
        active_session,
        None,
    );

    let path = expected_session
        .or_else(|| rolling_report.as_ref().map(|report| report.session_dir.clone()))
        .or_else(|| {
            screen_report
                .path
                .as_ref()
                .and_then(|path| path.parent())
                .and_then(|screen_dir| screen_dir.parent())
                .map(PathBuf::from)
        });
    let mut issues = Vec::new();
    if let Err(error) = &recorder_result {
        issues.push(format!("audio recorder: {error}"));
    }
    if let Some(report) = recorder_report {
        issues.extend(
            report
                .issues
                .iter()
                .map(|issue| format!("{}: {}", issue.component, issue.error)),
        );
    }
    if let Some(error) = &screen_report.error {
        issues.push(format!("screen recorder: {error}"));
    }
    if let Some(report) = &rolling_report {
        for track in &report.tracks {
            if track.state == rolling_encode::TrackPublicationState::FallbackRequired {
                issues.push(format!(
                    "{} rolling encoder: {}",
                    track.track,
                    track.error.as_deref().unwrap_or("fallback encode required")
                ));
            }
        }
    }

    if let Some(session_dir) = &path {
        let mut checkpoint = |result: anyhow::Result<()>| {
            if let Err(error) = result {
                issues.push(format!("media checkpoint: {error:#}"));
            }
        };
        if let Some(report) = recorder_report {
            for track in &report.tracks {
                match track.state {
                    recorder::RawTrackState::Finalized => checkpoint(
                        media_manifest::mark_audio_raw(session_dir, track.track, true, None),
                    ),
                    recorder::RawTrackState::NotPresent => checkpoint(
                        media_manifest::mark_audio_not_present(session_dir, track.track),
                    ),
                    recorder::RawTrackState::Failed => checkpoint(
                        media_manifest::mark_audio_raw(
                            session_dir,
                            track.track,
                            false,
                            track.error.clone(),
                        ),
                    ),
                }
            }
        }
        if let Some(report) = &rolling_report {
            for track in &report.tracks {
                match track.state {
                    rolling_encode::TrackPublicationState::Published => {
                        if let Some(opus_path) = &track.opus_path {
                            checkpoint(media_manifest::mark_audio_published(
                                session_dir,
                                &track.track,
                                opus_path,
                            ));
                        }
                    }
                    rolling_encode::TrackPublicationState::FallbackRequired => {
                        checkpoint(media_manifest::mark_audio_fallback(
                            session_dir,
                            &track.track,
                            track
                                .error
                                .clone()
                                .unwrap_or_else(|| "rolling publication failed".into()),
                        ));
                    }
                    rolling_encode::TrackPublicationState::NotRecorded => {}
                }
            }
        }
        // The system loopback can open successfully and still capture nothing —
        // the wrong monitor source, or an app routing audio somewhere the
        // monitor cannot see. The recording then contains only the local
        // speaker, which reads as "it only recorded my audio" and has been
        // reported twice months apart, each time discovered long after the
        // call. Say so now, while the user is still in front of the app.
        let system_wav = session_dir.join("system.wav");
        if system_wav.exists() && wav_is_silent(&system_wav) {
            eprintln!("aftercalls: system audio track captured no sound this session");
            crate::telemetry::log(
                "warn",
                "recorder::system_audio_silent",
                "system loopback captured no audible sound for the whole call",
                None,
                session_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
            );
            crate::telemetry::emit_app_event("system-audio-silent", &serde_json::json!({}));
        }

        if let Some(error) = &screen_report.error {
            let retained_path = screen_report.path.clone().unwrap_or_else(|| {
                session_dir
                    .join(screen_recorder::SCREEN_SUBDIR)
                    .join(screen_recorder::RECORDING_FILENAME)
            });
            // Only checkpoint a retryable screen job when bytes actually
            // exist. The ordinary degrade cases — the user cancelling the
            // portal window picker, a 0-byte output, a gdigrab child whose
            // window closed mid-call — all report an error with no usable
            // file. Marking those `UploadPending` strands the manifest in a
            // state the uploader can neither complete nor skip, which fails
            // the whole call pipeline with no recovery path. Screen capture is
            // opt-in and best-effort: absent is a valid terminal state.
            let has_bytes = std::fs::metadata(&retained_path)
                .map(|m| m.is_file() && m.len() > 0)
                .unwrap_or(false);
            if has_bytes {
                checkpoint(media_manifest::mark_screen_upload_pending(
                    session_dir,
                    None,
                    &retained_path,
                    error.clone(),
                ));
            } else {
                eprintln!(
                    "aftercalls: screen capture produced no file ({error}); recording the call without it"
                );
                checkpoint(media_manifest::mark_screen_not_present(session_dir));
            }
        }

        // Only a resolved recorder worker proves the WAV writers are closed.
        // Transport/worker errors retain the source and block a premature
        // pipeline read.
        if recorder_report.is_some() {
            let rolling = rolling_report.clone().unwrap_or_else(|| {
                rolling_encode::RollingFinalizationReport::conservative(session_dir)
            });
            let session_dir = session_dir.clone();
            let app_clone = app.clone();
            tauri::async_runtime::spawn(async move {
                pipeline::run_after_stop(session_dir, app_clone, rolling).await;
            });
        }
    } else {
        issues.push("unable to resolve the stopped session directory".into());
    }

    let path_display = path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown session>".into());
    if issues.is_empty() {
        Ok(path_display)
    } else {
        Err(format!(
            "recording stopped at {path_display}, but teardown reported: {}",
            issues.join("; ")
        ))
    }
}

#[tauri::command]
fn start_recording(
    state: State<Recorder>,
    app: AppHandle,
    // #653 — optional Zoho contact id from the co-pilot "who are you
    // calling?" picker (invoked as `{ contactHint }` from the webview).
    // Threaded onto the live-relay start frame; `None` leaves today's
    // behavior unchanged.
    contact_hint: Option<String>,
) -> Result<String, String> {
    do_start(&state, &app, contact_hint, "manual", None)
}

/// #142 · v0.4.5 — Start a note-to-self dictation. Mic-only capture;
/// writes `source.json.kind = "self_note"` so the pipeline + backend
/// list views can surface the distinct "Note to self" treatment on
/// the call row. Auto-stops at `config.max_self_note_minutes`
/// (default 5m). Rejects when a regular recording is already active
/// — the caller surfaces the inline notice.
#[tauri::command]
fn start_self_note(state: State<Recorder>, app: AppHandle) -> Result<String, String> {
    do_start_self_note(&state, &app)
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

/// The same two signals `is_processing` collapses, reported separately.
///
/// The quit path can word its prompt precisely because it reads the pair
/// directly in Rust; anything in the webview only had the collapsed bool and
/// would have to say something vague. Sign-out needs to tell a user mid-call
/// apart from one whose call is still uploading, so it needs the pair too.
#[derive(serde::Serialize)]
struct BusyDetail {
    recording: bool,
    processing: bool,
}

#[tauri::command]
fn busy_detail(state: State<Recorder>) -> BusyDetail {
    BusyDetail {
        recording: state.is_active(),
        processing: pipeline::is_pipeline_active(),
    }
}

#[tauri::command]
async fn select_import_file(
    app: AppHandle,
    security: State<'_, ipc_security::IpcSecurity>,
) -> Result<Option<String>, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(
            "Audio",
            &["wav", "mp3", "m4a", "mp4", "ogg", "opus", "flac", "webm"],
        )
        .blocking_pick_file();
    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|e| format!("resolve selected file: {e}"))?;
    let canonical = ipc_security::canonical_existing_file(&path.to_string_lossy())?;
    security.approve_path(ipc_security::PathPurpose::ImportAudio, canonical.clone());
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

#[tauri::command]
async fn process_imported_file(
    app: AppHandle,
    security: State<'_, ipc_security::IpcSecurity>,
    source_path: String,
) -> Result<String, String> {
    let src = security.consume_approved_file(
        ipc_security::PathPurpose::ImportAudio,
        &source_path,
    )?;
    // Allocate, never reuse, a private session directory. The timestamp prefix
    // preserves sort order; the UUID suffix prevents same-second imports from
    // overwriting each other or racing two pipelines into one backend call.
    let recordings = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    let base = session_fs::allocate(&recordings, session_fs::SessionKind::Import)
        .map_err(|e| format!("allocate imported session: {e:#}"))?;
    media_manifest::initialize(&base)
        .map_err(|e| format!("initialize durable media checkpoint: {e:#}"))?;

    // Normalize whatever the user picked (mp3/m4a/mp4/etc.) into WAV so the
    // pipeline's AssemblyAI upload path (which re-encodes to Opus anyway) gets
    // a consistent input. Stored as system.wav so diarization kicks in — a
    // Zoom/Meet export usually has multiple voices mixed together.
    let dest = base.join("system.wav");
    let staged_dest = base.join(format!(
        "system.wav.part.{}",
        uuid::Uuid::new_v4()
    ));
    let _stage_guard = media_manifest::reserve_private_stage(&staged_dest)
        .map_err(|e| format!("reserve imported audio stage: {e:#}"))?;
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
        // The private stage suffix is intentionally not a WAV extension.
        .arg("-f")
        .arg("wav")
        .arg(&staged_dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::pipeline::no_console(&mut cmd);
    let status = match cmd.status().await {
        Ok(status) => status,
        Err(e) => {
            let message = format!("run import ffmpeg: {e}");
            let _ = media_manifest::mark_audio_fallback(&base, "system", message.clone());
            return Err(message);
        }
    };
    if !status.success() {
        let message = format!("ffmpeg exited with {status} (unsupported format?)");
        let _ = media_manifest::mark_audio_fallback(&base, "system", message.clone());
        return Err(message);
    }
    let reader = hound::WavReader::open(&staged_dest)
        .map_err(|e| format!("validate imported WAV: {e}"))?;
    if reader.spec().sample_rate == 0 || reader.duration() == 0 {
        return Err("import produced an empty WAV".to_string());
    }
    drop(reader);
    media_manifest::enforce_private_file(&staged_dest)
        .map_err(|e| format!("protect imported WAV: {e:#}"))?;
    media_manifest::sync_staged_file(&staged_dest)
        .map_err(|e| format!("sync imported WAV: {e:#}"))?;
    media_manifest::atomic_replace_file(&staged_dest, &dest)
        .map_err(|e| format!("publish imported WAV: {e:#}"))?;
    media_manifest::mark_audio_not_present(&base, "mic")
        .map_err(|e| format!("checkpoint imported mic state: {e:#}"))?;
    media_manifest::mark_audio_raw(&base, "system", true, None)
        .map_err(|e| format!("checkpoint imported audio: {e:#}"))?;

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
// Six commands + four events drive the per-app whitelist UX:
//   - `auto_record_settings_get` returns the bundle the Settings page
//     paints (master toggles + per-app rows + platform_supported).
//     Each row carries both `mode` (auto/ask/never, authoritative) and
//     `enabled` (legacy mirror, kept for one release for downgrade
//     safety — `mode == 'auto' ⇒ enabled == true`).
//   - `auto_record_settings_set_master` flips the two booleans that
//     gate the auto-start / auto-stop paths.
//   - `auto_record_settings_set_app_mode` updates one row's `mode` to
//     the three-state user choice ('auto' / 'ask' / 'never'). The
//     `never` mode silences the detector entirely for that bundle —
//     no toast, no in-app slide-out, no PIPEDA modal.
//   - `auto_record_settings_toggle_app` is the legacy boolean shim,
//     kept so a stale frontend after an in-place update doesn't 500.
//     Maps enabled=true → mode=auto, enabled=false → mode=ask. A
//     row currently set to `never` that the legacy shim flips to
//     enabled=false stays semantically equivalent (ask).
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
    /// Legacy mirror of `mode == Auto`. Kept on the wire for one
    /// release so a v0.17.x frontend reading a v2 backend still gets
    /// the right behaviour for ticked/unticked rows.
    enabled: bool,
    /// Authoritative per-row state. `auto` = auto-record on mic
    /// capture (master toggle permitting), `ask` = current prompt
    /// flow (default), `never` = silenced — detector filters this
    /// bundle out of `interesting_mic_consumers`.
    mode: String,
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
                mode: r.mode.as_str().to_string(),
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
    // Legacy shim — see the module-level comment block. enabled=true
    // maps to mode=Auto, enabled=false maps to mode=Ask. The new
    // mode-aware frontend calls `auto_record_settings_set_app_mode`
    // instead; this command stays in place for one release so a
    // stale frontend after an in-place update doesn't 500.
    auto.store()
        .set_enabled(&bundle_id, enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn auto_record_settings_set_app_mode(
    auto: State<AutoRecorder>,
    bundle_id: String,
    mode: String,
) -> Result<(), String> {
    let parsed = match mode.as_str() {
        "auto" => app_observations::AppMode::Auto,
        "ask" => app_observations::AppMode::Ask,
        "never" => app_observations::AppMode::Never,
        other => return Err(format!("unknown app mode {other:?}")),
    };
    auto.store()
        .set_mode(&bundle_id, parsed)
        .map_err(|e| e.to_string())?;
    // When the user silences an app, also clear any in-flight snooze
    // for that consumer so future un-silencing isn't stuck behind a
    // stale 5-minute snooze window. Snooze is keyed on the consumer
    // string — same key the detector uses — and clearing on Never
    // is a one-line nicety per the plan §Risks "Snooze table
    // interaction" note.
    if matches!(parsed, app_observations::AppMode::Never) {
        notify_actions::clear_snooze(&bundle_id);
    }
    Ok(())
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
    q: Option<String>,
    view: Option<String>,
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
        q.as_deref(),
        view.as_deref(),
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
    let recordings = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    let session_dir = session_fs::resolve_existing_dir(&recordings, &session_id)
        .ok_or_else(|| "recording session was not found".to_string())?;
    let audio_path = session_dir.join(format!("{track}.wav"));
    let metadata = audio_path
        .symlink_metadata()
        .map_err(|_| "recording audio was not found".to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("recording audio is not a regular local file".into());
    }
    let canonical = audio_path
        .canonicalize()
        .map_err(|e| format!("resolve recording audio: {e}"))?;
    if canonical.parent() != Some(session_dir.as_path()) {
        return Err("recording audio escaped its session directory".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
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
    /// #659 P5a — the org's default in-call co-pilot persona
    /// (`"sales"` / `"support"`). Surfaced so the Record page seeds the
    /// CoPilotPanel mode toggle from the org default at mount.
    copilot_default_mode: String,
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
        copilot_default_mode: auth.copilot_default_mode,
        recording_acknowledged: auth.recording_acknowledged,
        features: auth.features,
        pending_tos: auth.pending_tos,
    })
}

/// Peak amplitude floor below which a whole track counts as "captured nothing".
/// -50 dBFS. Room tone and dither sit under this; any real speech is far above.
const SILENT_TRACK_PEAK: i16 = 104;

/// Whether a 16-bit PCM WAV carries no audible content anywhere.
///
/// Returns `false` the instant a sample clears the floor, so a normal recording
/// costs a few kilobytes of read. Only a genuinely dead track is scanned end to
/// end, which is exactly the case worth being certain about. A file we cannot
/// read or parse returns `false` — never claim silence we did not observe.
fn wav_is_silent(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    // Walk the RIFF chunks to find `data`; the header is not always 44 bytes.
    let mut header = [0u8; 12];
    if file.read_exact(&mut header).is_err() || &header[0..4] != b"RIFF" {
        return false;
    }
    loop {
        let mut chunk = [0u8; 8];
        if file.read_exact(&mut chunk).is_err() {
            return false; // ran out before finding `data` — treat as unknown
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        if &chunk[0..4] == b"data" {
            break;
        }
        if std::io::Seek::seek(&mut file, std::io::SeekFrom::Current(i64::from(size))).is_err() {
            return false;
        }
    }
    let mut buffer = [0u8; 64 * 1024];
    let mut saw_any_sample = false;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return false,
        };
        for pair in buffer[..read - (read % 2)].chunks_exact(2) {
            saw_any_sample = true;
            let sample = i16::from_le_bytes([pair[0], pair[1]]);
            if sample.saturating_abs() >= SILENT_TRACK_PEAK {
                return false;
            }
        }
    }
    // An empty data chunk is "nothing recorded", handled elsewhere as absent.
    saw_any_sample
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
        copilot_default_mode: a.copilot_default_mode,
        recording_acknowledged: a.recording_acknowledged,
        features: a.features,
        pending_tos: a.pending_tos,
    }))
}

/// #659 — best-effort network refresh of the cached profile bundle.
/// `current_user` reads only `auth.json`, so a feature enabled for the
/// org server-side (e.g. `copilot` bought mid-session) doesn't surface
/// until a full re-login. This command re-fetches `/v1/auth/me` via the
/// refresh-aware `build_auth_header`, persists the fresh bundle (incl.
/// `features`) to `auth.json`, and returns the updated `LoginResult` so
/// a surface can re-gate its panels without a re-login. Callers invoke
/// it best-effort *after* the instant, offline-safe `current_user`
/// paint; on error (offline / expired refresh token) the cached values
/// stand — the panel is never blanked. Reuses `portal::refresh_me`;
/// no new portal code. Mirrors the `tos_accept` `AuthFile → LoginResult`
/// tail.
#[tauri::command]
async fn refresh_current_user() -> Result<LoginResult, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
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
        copilot_default_mode: auth.copilot_default_mode,
        recording_acknowledged: auth.recording_acknowledged,
        features: auth.features,
        pending_tos: auth.pending_tos,
    })
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
        copilot_default_mode: auth.copilot_default_mode,
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

#[cfg(test)]
mod recording_lifecycle_tests {
    use super::*;

    #[test]
    fn stale_stop_token_cannot_claim_a_new_generation() {
        let lifecycle = RecordingLifecycle::new();
        let first = lifecycle
            .begin_start()
            .unwrap()
            .commit(PathBuf::from("/recordings/first"));
        {
            let mut stop = lifecycle.begin_stop(&first).unwrap();
            stop.mark_stopped();
        }
        let second = lifecycle
            .begin_start()
            .unwrap()
            .commit(PathBuf::from("/recordings/second"));

        assert!(lifecycle.begin_stop(&first).is_err());
        assert_eq!(lifecycle.current_token(), Some(second));
    }

    #[test]
    fn concurrent_start_reservation_is_fail_closed() {
        let lifecycle = RecordingLifecycle::new();
        let first = lifecycle
            .begin_start()
            .unwrap()
            .commit(PathBuf::from("/recordings/first"));
        let error = lifecycle.begin_start().err().expect("second start must fail");
        assert!(error.contains("already in progress"));
        assert_eq!(lifecycle.current_token(), Some(first));
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
        copilot_default_mode: auth.copilot_default_mode,
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

/// #630 — GET `/v1/me/summary-style`. Returns the user's stored
/// override (`null` = inherit), the resolved effective style, and the
/// org default — feeding the Settings page's "Use team default (X)"
/// segment label without a second round-trip.
#[tauri::command]
async fn me_summary_style_get() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_summary_style_get(backend).await
}

/// #630 — PATCH `/v1/me/summary-style`. `style: None` reverts to
/// inherit; `Some("narrative"|"hybrid"|"bulleted")` sets the override.
/// Unknown values reject with a backend 400 surfaced as `PortalError`.
#[tauri::command]
async fn me_summary_style_patch(
    style: Option<String>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_summary_style_patch(backend, style.as_deref()).await
}

/// Phase 3 — GET `/v1/me/zoho-autopush`. Returns the caller's stored
/// call-end push mode (`"prompt"` | `"auto"`), feeding the "Push to CRM"
/// Settings card.
#[tauri::command]
async fn me_zoho_autopush_get() -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_zoho_autopush_get(backend).await
}

/// Phase 3 — PATCH `/v1/me/zoho-autopush`. `mode` is `"prompt"` |
/// `"auto"`; unknown values reject with a backend 400 surfaced as
/// `PortalError`.
#[tauri::command]
async fn me_zoho_autopush_patch(
    mode: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_zoho_autopush_patch(backend, &mode).await
}

// ── #634 — per-user unread call state ────────────────────────────────
//
// Three IPC shims for the webview's mark-as-read flow + a tiny tray-
// badge helper. Mirrors the portal's `api.calls.mark*` /
// `me.unread_calls` surface so the two `/calls` pages converge on
// identical optimistic-update behaviour. Tray badge is agent-only;
// see the `apply_tray_state` + `compose_tray_tooltip` doc-comments
// above for the per-OS strategy.

/// POST `/v1/calls/{id}/read` — idempotent mark-as-read for a single
/// call. Cross-org / unknown id → 404 (per learning #82); the
/// PortalError surfaces to the webview as `kind: "not_found"` so the
/// page can downgrade gracefully.
#[tauri::command]
async fn mark_call_read(id: String) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::mark_call_read(backend, &id).await
}

/// POST `/v1/calls/read-bulk` — mark-as-read for a set of calls or
/// every unread complete call in the caller's org. Exactly one of
/// `all` / `call_ids` is set per the discriminated body the backend
/// expects; the IPC layer enforces this by accepting the two args
/// separately and constructing the body. `all=true` AND a non-empty
/// `call_ids` is rejected client-side as a programmer error rather
/// than letting the backend 400 round-trip.
#[tauri::command]
async fn mark_calls_read_bulk(
    all: Option<bool>,
    call_ids: Option<Vec<String>>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let want_all = all.unwrap_or(false);
    let ids = call_ids.unwrap_or_default();
    let body = match (want_all, ids.is_empty()) {
        (true, true) => serde_json::json!({ "all": true }),
        (false, false) => serde_json::json!({ "call_ids": ids }),
        // Empty bulk ({all:false, call_ids:[]}) short-circuits to a
        // no-op so the page can call markBulkRead([]) without an
        // error path. Mirrors the portal client's defensive check.
        (false, true) => return Ok(serde_json::json!({ "marked": 0 })),
        (true, false) => {
            return Err(error::PortalError::Other {
                message: "mark_calls_read_bulk: pass `all` OR `call_ids`, not both".into(),
            });
        }
    };
    portal::mark_calls_read_bulk(backend, body).await
}

/// GET `/v1/auth/me` → `unread_calls`. Live count for the layout's
/// 60s poll; doesn't merge into `auth.json` (the cached profile
/// stays long-lived, matching the existing summary-style / privacy
/// pattern where webview-driven reads bypass the cache).
#[tauri::command]
async fn me_unread_count() -> Result<i64, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_unread_count(backend).await
}

/// #602/WS-G — GET `/v1/auth/me` → `subscription`. Fresh snapshot for
/// the Settings subscription card (plan/trial indicator + Manage-billing
/// link). Doesn't merge into `auth.json`; mirrors the `me_unread_count`
/// reach-around pattern for webview-driven reads that want live data.
#[tauri::command]
async fn me_subscription() -> Result<config::Subscription, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::me_subscription(backend).await
}

/// #634 — webview-driven tray-badge update. Pushes the latest count
/// into `TrayUnreadCount` and re-applies the tray state so the
/// tooltip reflects the new value immediately. Negative inputs are
/// not representable (u32). The webview calls this on every poll
/// tick AND on every `unread-count-changed` window event so the
/// tooltip stays in lockstep with the sidebar pill.
#[tauri::command]
fn set_unread_badge(app: AppHandle, count: u32) {
    if let Some(state) = app.try_state::<TrayUnreadCount>() {
        state.set(count);
    }
    tray_refresh_with_current_state(&app);
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
async fn select_vault_directory(
    app: AppHandle,
    security: State<'_, ipc_security::IpcSecurity>,
) -> Result<Option<String>, String> {
    let picked = app
        .dialog()
        .file()
        .set_title("Select your Obsidian vault folder")
        .blocking_pick_folder();
    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|e| format!("resolve selected folder: {e}"))?;
    let canonical = ipc_security::canonical_existing_dir(&path.to_string_lossy())?;
    security.approve_path(ipc_security::PathPurpose::VaultRoot, canonical.clone());
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

#[tauri::command]
fn set_vault_settings(
    security: State<'_, ipc_security::IpcSecurity>,
    enabled: bool,
    path: String,
    clients_subpath: String,
) -> Result<(), String> {
    let mut cfg = config::Config::load().map_err(|e| e.to_string())?;
    if enabled {
        let supplied = path.trim();
        if supplied.is_empty() {
            return Err("vault path is required when enabled".into());
        }
        let canonical = ipc_security::canonical_existing_dir(supplied)?;
        let already_configured = cfg
            .vault
            .as_ref()
            .and_then(|vault| ipc_security::canonical_existing_dir(vault.path.trim()).ok())
            .as_ref()
            == Some(&canonical);
        if !already_configured {
            security.require_approved_dir(ipc_security::PathPurpose::VaultRoot, supplied)?;
        }
        let clients_subpath = ipc_security::normalize_relative_subpath(&clients_subpath)?;
        cfg.vault = Some(config::Vault {
            path: canonical.to_string_lossy().into_owned(),
            clients_subpath,
        });
    } else {
        cfg.vault = None;
    }
    cfg.save().map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_audio_urls(
    security: State<'_, ipc_security::IpcSecurity>,
    id: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    let body = portal::get_audio_urls(backend, &id).await?;
    security.approve_audio_urls(&body);
    Ok(body)
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
    /// #659 P4 — per-machine opt-in for the floating always-on-top co-pilot
    /// overlay. Default false; the Settings → Co-pilot row renders it as a
    /// switch and it round-trips through this AppPrefs blob like every other
    /// per-machine pref.
    overlay_enabled: bool,
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
        overlay_enabled: cfg.overlay_enabled,
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
    overlay_enabled: bool,
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
    cfg.overlay_enabled = overlay_enabled;
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

// ── macOS capture-permission pre-flight (#623) ───────────────────────
//
// Thin IPC wrappers over `permissions.rs`. Callers read
// `check_capture_permissions` before `start_recording` so an
// already-*denied* mic permission can short-circuit with an actionable
// message instead of a raw `cpal` error string at the record
// aha-moment. Off macOS every command degrades to a no-op /
// `not_applicable` so the JS contract stays uniform across builds.
//
// The Start-path gating and the mic-only screen-note banner are
// separate work items on #623.
//
// Every command here is also listed under `main-commands` in
// `permissions/app.toml` — without that the webview cannot reach them.

/// Read the live grant state for mic + screen-recording capture.
/// Cheap status read (never prompts) so it's safe on the hot Start
/// path.
#[tauri::command]
fn check_capture_permissions() -> permissions::CapturePermissions {
    permissions::check_capture_permissions()
}

/// Fire the OS mic-permission prompt (macOS) and return the resulting
/// status. `not_applicable` off macOS.
#[tauri::command]
fn request_mic_permission() -> permissions::PermStatus {
    permissions::request_mic_permission()
}

/// Prompt for screen-recording access (macOS `CGRequestScreenCaptureAccess`)
/// and return whether it's now granted. Always `true` off macOS.
#[tauri::command]
fn request_screen_capture_access() -> bool {
    permissions::request_screen_capture_access()
}

/// Open the relevant macOS Privacy & Security pane so the user can flip
/// a denied grant. `pane` is "microphone" or "screen".
///
/// This is the only remedy for an already-*denied* grant: macOS will
/// not re-prompt once the user has said no, so `request_mic_permission`
/// / `request_screen_capture_access` return the same `denied` forever
/// and the user has to flip the switch in System Settings by hand.
///
/// The URL allowlist lives here (Rust) rather than widening the JS
/// opener capability scope: `app.opener().open_url(..)` called from a
/// command is not subject to the `opener:allow-open-url` scope in
/// `capabilities/default.json`, so the `x-apple.systempreferences:`
/// scheme does not need to be added there. Keeping the match in Rust
/// also means no caller-supplied text is ever interpolated into the
/// URL — an unrecognized `pane` is rejected, not passed through.
///
/// A failure to open is non-fatal: it surfaces as an `Err` the callers
/// swallow, never as a crash on the record path.
#[tauri::command]
fn open_privacy_settings(app: AppHandle, pane: String) -> Result<(), String> {
    // `pane` is named (not `_pane`) on purpose: Tauri derives the IPC
    // payload key from the parameter ident, so renaming it would break
    // the `{ pane }` call the TS mirror already sends. `app` is
    // injected by Tauri and is *not* part of that payload.
    let url = match pane.as_str() {
        "microphone" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        "screen" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        other => return Err(format!("unknown privacy pane: {other}")),
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

// ── #302 Slice B — screen capture: displays, prefs, consent ──────────
//
// The Settings UI (Slice C) drives these: `list_displays` populates the
// monitor picker, `get/set_screen_capture_prefs` round-trips the per-user
// knobs, `screen_capture_ack` posts the (distinct, heavier) consent before
// the toggle may be enabled, and `screen_capture_status` reports whether
// capture can actually run (backend binary present + a display). The org
// feature flag gate lives on `me.features.screen_capture` (read in the
// layout) and is the security boundary server-side.

/// Enumerate capturable monitors for the Settings picker. `name` is the
/// exact capture target; `width`/`height` feed the "3840×2160" hint;
/// `is_primary` marks the focused/primary output. Empty on a machine with
/// no capture backend → the UI shows the "capture unavailable" state.
#[tauri::command]
fn list_displays() -> Vec<screen_recorder::DisplayInfo> {
    screen_recorder::enumerate_displays()
}

#[derive(serde::Serialize)]
struct ScreenCapturePrefs {
    enabled: bool,
    /// Saved monitor name; None = "use primary". Only used when
    /// `ask_each_call == false` (remembered-screen capture).
    display: Option<String>,
    fps: u32,
    /// `"720p" | "1080p" | "native"`.
    resolution: Option<String>,
    bitrate_kbps: u32,
    /// #302 follow-up — ask for a screen/window/area each call (default) vs
    /// always auto-record the remembered `display`.
    ask_each_call: bool,
}

#[tauri::command]
fn get_screen_capture_prefs() -> Result<ScreenCapturePrefs, String> {
    let cfg = config::Config::load().map_err(|e| e.to_string())?;
    Ok(ScreenCapturePrefs {
        enabled: cfg.screen_capture_enabled,
        display: cfg.screen_capture_display,
        fps: cfg.screen_capture_fps,
        resolution: cfg.screen_capture_resolution,
        bitrate_kbps: cfg.screen_capture_bitrate_kbps,
        ask_each_call: cfg.screen_capture_ask_each_call,
    })
}

/// Persist the per-user screen-capture knobs. Enabling (`enabled = true`)
/// should only be reached after the consent ack in the Settings flow — the
/// backend upload path is the authoritative backstop regardless. fps is
/// clamped to [10, 30]; an empty display string is normalized to None
/// ("use primary"); an unrecognized resolution falls back to "1080p".
#[tauri::command]
fn set_screen_capture_prefs(
    app: AppHandle,
    enabled: bool,
    display: Option<String>,
    fps: u32,
    resolution: Option<String>,
    bitrate_kbps: u32,
    ask_each_call: bool,
) -> Result<(), String> {
    let display = display
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let fps = screen_recorder::clamp_fps(fps);
    let resolution = match resolution.as_deref().map(str::trim) {
        Some("720p") => Some("720p".to_string()),
        Some("native") => Some("native".to_string()),
        // Empty / unknown / "1080p" → the 1080p default cap.
        _ => Some("1080p".to_string()),
    };
    // Clamp the bitrate to a sane band so a hand-edited pref can't request
    // an absurd ceiling (0 would starve the encoder; the upper bound keeps
    // the storage cost bounded).
    let bitrate_kbps = bitrate_kbps.clamp(500, 20_000);
    let mut cfg = config::Config::load().map_err(|e| e.to_string())?;
    cfg.screen_capture_enabled = enabled;
    cfg.screen_capture_display = display;
    cfg.screen_capture_fps = fps;
    cfg.screen_capture_resolution = resolution;
    cfg.screen_capture_bitrate_kbps = bitrate_kbps;
    cfg.screen_capture_ask_each_call = ask_each_call;
    cfg.save().map_err(|e| e.to_string())?;

    // Disabling is a privacy action, not merely a preference for the next
    // call. Serialize against Start/Stop and finalize the current capture
    // immediately. With no lifecycle identity, stop any orphaned screen
    // producer fail-closed rather than letting it continue invisibly.
    if !enabled {
        close_region_select_window(&app);
        let lifecycle = app.state::<RecordingLifecycle>();
        let _lifecycle_guard = lifecycle.inner.lock().unwrap();
        // This is the one intentional generation-agnostic stop: disabling the
        // privacy preference must halt whichever screen producer exists, even
        // if an earlier invariant failure orphaned it from lifecycle state.
        // Holding the lifecycle mutex prevents a concurrent Start from being
        // mistaken for that orphan.
        let report = app
            .state::<ScreenRecorder>()
            .stop_and_persist(None, None);
        if let Some(error) = report.error {
            return Err(format!(
                "screen capture was disabled, but capture finalization reported: {error}"
            ));
        }
    }
    Ok(())
}

/// #302 follow-up — list visible top-level windows for the chooser's Window
/// sub-list. Real titles on Windows; empty on Linux (the compositor's native
/// picker owns window selection) + macOS (no capture surface).
#[tauri::command]
fn list_windows() -> Vec<screen_recorder::WindowInfo> {
    screen_recorder::enumerate_windows()
}

/// #302 follow-up — resolve a drag-selected area to a canonical `WxH+X+Y`
/// geometry. Linux drives the native region tool (`slurp`); `None` on
/// ESC/cancel. Windows returns `None` — that platform uses the in-app
/// `region-select` overlay window instead (see `ScreenSourceChooser`).
#[tauri::command]
fn pick_region() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        screen_recorder::resolve_region_via_slurp()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[derive(Clone)]
struct RegionSelectRequest {
    request_token: String,
    session_dir: String,
}

#[derive(Default)]
struct RegionSelectState(std::sync::Mutex<Option<RegionSelectRequest>>);

#[derive(Serialize, Clone)]
struct RegionPickedEvent {
    request_token: String,
    geometry: String,
}

#[derive(Serialize, Clone)]
struct RegionCancelledEvent {
    request_token: String,
}

/// #302 follow-up — the Windows `region-select` overlay window submits its
/// result here. Closes the overlay and re-emits the outcome to the main
/// window's chooser as `region-picked` (with the geometry) or
/// `region-cancelled`. The secondary window receives only this command through
/// the dedicated `region-select-commands` capability.
#[tauri::command]
fn submit_region_selection(
    app: AppHandle,
    state: State<RegionSelectState>,
    request_token: String,
    geometry: Option<String>,
) {
    // A replaced/late overlay must not close or answer the current selector.
    // Token + session ownership is recorded Rust-side when the window opens.
    let request = {
        let mut guard = state.0.lock().unwrap();
        match guard.as_ref() {
            Some(active) if active.request_token == request_token => guard.take().unwrap(),
            _ => return,
        }
    };
    if let Some(w) = app.get_webview_window("region-select") {
        let _ = w.close();
    }
    if !recording_session_matches(&app, &request.session_dir) {
        let _ = app.emit(
            "region-cancelled",
            RegionCancelledEvent { request_token },
        );
        return;
    }
    // Never forward arbitrary webview text as a capture argument. Parse and
    // rebuild the one accepted shape so dimensions/coordinates are integral,
    // non-zero, and free of trailing tokens.
    match geometry
        .as_deref()
        .and_then(screen_recorder::parse_region_geometry)
    {
        Some((width, height, x, y)) => {
            let _ = app.emit(
                "region-picked",
                RegionPickedEvent {
                    request_token,
                    geometry: format!("{width}x{height}+{x}+{y}"),
                },
            );
        }
        None => {
            let _ = app.emit(
                "region-cancelled",
                RegionCancelledEvent { request_token },
            );
        }
    }
}

/// #302 review — open the Windows drag-select overlay window FROM RUST
/// (mirrors `open_overlay`). Creating the window server-side with a hardcoded
/// `WebviewUrl::App` means the main window needs NO
/// `core:webview:allow-create-webview-window` grant — an unscoped capability
/// that let any script in `main` pop a chromeless window at an arbitrary URL.
/// The transparent, always-on-top, decorationless `region-select` window is
/// sized to the target monitor rect; `x`/`y` (the monitor origin) travel as
/// query params so the overlay page maps client coords → absolute screen
/// coords. Idempotent: refocus if already open. The overlay reports back
/// through `submit_region_selection` (which closes it); `close_region_select`
/// is the force-close counterpart.
#[tauri::command]
fn open_region_select(
    app: AppHandle,
    state: State<RegionSelectState>,
    session_dir: String,
    request_token: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    uuid::Uuid::parse_str(&request_token)
        .map_err(|_| "invalid area-selector request token".to_string())?;
    if !cfg!(windows) {
        return Err("area selector is only available on Windows".to_string());
    }
    let feature_on = config::read_auth_file()
        .ok()
        .flatten()
        .map(|auth| auth.features.screen_capture)
        .unwrap_or(false);
    let opted_in = config::Config::load()
        .map(|config| config.screen_capture_enabled)
        .unwrap_or(false);
    if !feature_on || !opted_in || !app.state::<ScreenRecorder>().is_available() {
        return Err("screen capture is not enabled or available".to_string());
    }
    open_region_select_window(
        &app,
        &state,
        &session_dir,
        &request_token,
        x,
        y,
        width,
        height,
    )
}

fn open_region_select_window(
    app: &AppHandle,
    state: &RegionSelectState,
    session_dir: &str,
    request_token: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    if !recording_session_matches(app, session_dir) {
        return Err("recording session is no longer active".to_string());
    }

    // The chooser obtains this rect from `list_displays`, but the command is
    // still an IPC trust boundary. Require an exact current physical monitor
    // and use its OS scale factor; arbitrary giant/topmost windows are denied.
    let monitor = app
        .available_monitors()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|monitor| {
            let position = monitor.position();
            let size = monitor.size();
            position.x == x
                && position.y == y
                && size.width == width
                && size.height == height
        })
        .ok_or_else(|| "requested area-selector monitor is unavailable".to_string())?;
    let scale = monitor.scale_factor();
    if !scale.is_finite() || scale <= 0.0 {
        return Err("requested monitor has an invalid scale factor".to_string());
    }

    // Idempotent only for the same request. A newer token replaces a late
    // overlay so its eventual submit cannot answer the new chooser.
    if let Some(w) = app.get_webview_window("region-select") {
        let same_request = state
            .0
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|active| {
                active.request_token == request_token && active.session_dir == session_dir
            });
        if same_request {
            let _ = w.set_focus();
            return Ok(());
        }
        let _ = w.close();
        *state.0.lock().unwrap() = None;
    }
    let url = format!(
        "/region-select?x={x}&y={y}&scale={scale:.6}&token={request_token}"
    );
    // Transparent is safe here: this window only ever opens on Windows (the
    // chooser gates the invoke on `platform === "windows"`), so the Linux
    // WEBKIT_DISABLE_COMPOSITING_MODE caveat that keeps the co-pilot overlay
    // opaque does not apply — the region page needs alpha to show the screen
    // through its dim.
    let builder = WebviewWindowBuilder::new(app, "region-select", WebviewUrl::App(url.into()))
        .title("Select area")
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        // Builder dimensions are logical pixels. Build hidden, then apply the
        // exact physical monitor rect before the overlay can paint.
        .focused(false)
        .visible(false)
        .inner_size(1.0, 1.0);
    // `.transparent()` only exists on macOS behind Tauri's `macos-private-api`
    // feature, which we deliberately do not enable — it bars Mac App Store
    // submission. Per the note above this window is Windows-only at runtime,
    // so gating the call is free rather than paying that cost for a window no
    // Mac ever opens. Without this the agent does not compile on macOS at all
    // (E0599), which went unnoticed until macOS joined CI.
    #[cfg(not(target_os = "macos"))]
    let builder = builder.transparent(true);
    let window = builder.build().map_err(|e| e.to_string())?;
    if let Err(error) = window.set_position(tauri::PhysicalPosition::new(x, y)) {
        let _ = window.close();
        return Err(error.to_string());
    }
    if let Err(error) = window.set_size(tauri::PhysicalSize::new(width, height)) {
        let _ = window.close();
        return Err(error.to_string());
    }
    // Stop/restart can race the native window construction. Re-check before
    // exposing an always-on-top overlay and close the hidden window if stale.
    if !recording_session_matches(app, session_dir) {
        let _ = window.close();
        return Err("recording session ended while opening selector".to_string());
    }
    *state.0.lock().unwrap() = Some(RegionSelectRequest {
        request_token: request_token.to_string(),
        session_dir: session_dir.to_string(),
    });
    if let Err(error) = window.show() {
        clear_region_select_request(state, request_token, session_dir);
        let _ = window.close();
        return Err(error.to_string());
    }
    if let Err(error) = window.set_focus() {
        clear_region_select_request(state, request_token, session_dir);
        let _ = window.close();
        return Err(error.to_string());
    }
    Ok(())
}

fn clear_region_select_request(
    state: &RegionSelectState,
    request_token: &str,
    session_dir: &str,
) {
    let mut guard = state.0.lock().unwrap();
    if guard.as_ref().is_some_and(|active| {
        active.request_token == request_token && active.session_dir == session_dir
    }) {
        *guard = None;
    }
}

/// #302 review — force-close the `region-select` overlay if present. No-op
/// when absent. Called by the chooser when the call leaves the recording
/// state mid-drag (mirrors `close_overlay`), so a stuck fullscreen
/// always-on-top window can't survive the call ending;
/// `submit_region_selection` already closes it on a normal pick / cancel.
#[tauri::command]
fn close_region_select(app: AppHandle) {
    close_region_select_window(&app);
}

fn close_region_select_window(app: &AppHandle) {
    if let Some(state) = app.try_state::<RegionSelectState>() {
        *state.0.lock().unwrap() = None;
    }
    if let Some(w) = app.get_webview_window("region-select") {
        let _ = w.close();
    }
}

fn recording_session_matches(app: &AppHandle, session_dir: &str) -> bool {
    let recorder = app.state::<Recorder>();
    recorder.is_active()
        && recorder
            .session_dir()
            .is_some_and(|active| active.to_string_lossy() == session_dir)
}

/// #302 follow-up — start the per-call screen capture on the chosen source.
/// Called by `ScreenSourceChooser` after the user picks (or the sub-list
/// resolves a monitor/window/area). Guards against a stale request: the
/// capture must still be the active recording AND the frontend's
/// `session_dir` must match the recorder's, else the call was stopped /
/// restarted and we return `cancelled` (no row). Best-effort — a spawn
/// failure returns `unavailable` and the call proceeds audio-only. Returns
/// one of `"started" | "cancelled" | "unavailable"`.
#[tauri::command]
fn start_screen_source(
    app: AppHandle,
    session_dir: String,
    kind: String,
    target: Option<String>,
) -> String {
    if !recording_session_matches(&app, &session_dir) {
        return "cancelled".to_string();
    }
    let recorder = app.state::<Recorder>();
    // Serialize the final privacy/config recheck and capture spawn with both
    // lifecycle Stop and preference disable. If disable wins this lock, the
    // fresh config read below sees false; if this start wins, disable waits and
    // then immediately stops the producer.
    let lifecycle = app.state::<RecordingLifecycle>();
    let lifecycle_guard = lifecycle.inner.lock().unwrap();

    // Defense-in-depth (#302 security review): re-assert the same gates
    // `maybe_request_screen_source` applied — the org `screen_capture`
    // feature (cached in auth.json) + the per-user opt-in — before starting
    // capture. A flag/opt-in flip (or a stale / forged request) between the
    // chooser event and the pick must not start capture. Silent audio-only
    // fallback ("cancelled"), never an error.
    let feature_on = config::read_auth_file()
        .ok()
        .flatten()
        .map(|a| a.features.screen_capture)
        .unwrap_or(false);
    if !feature_on {
        return "cancelled".to_string();
    }

    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(_) => return "unavailable".to_string(),
    };
    if !cfg.screen_capture_enabled {
        return "cancelled".to_string();
    }
    let target = target.filter(|s| !s.trim().is_empty());
    let source = match kind.as_str() {
        "screen" => screen_recorder::CaptureSource::Screen { monitor: target },
        "window" => screen_recorder::CaptureSource::Window { target },
        "region" => match target {
            Some(geo) => {
                #[cfg(windows)]
                if !screen_recorder::region_within_any_display(
                    &geo,
                    &screen_recorder::enumerate_displays(),
                ) {
                    return "unavailable".to_string();
                }
                screen_recorder::CaptureSource::Region { geometry: geo }
            }
            // Region with no geometry = cancelled drag-select → audio-only.
            None => return "cancelled".to_string(),
        },
        _ => return "unavailable".to_string(),
    };

    let start_cfg = screen_recorder::StartConfig {
        fps: cfg.screen_capture_fps,
        resolution: cfg.screen_capture_resolution.clone(),
        bitrate_kbps: cfg.screen_capture_bitrate_kbps,
    };
    let audio_started_at_ms = recorder
        .started_at_ms()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let session_path = std::path::PathBuf::from(&session_dir);
    let Some(token) = lifecycle_guard
        .active
        .as_ref()
        .filter(|active| active.session_dir == session_path)
    else {
        return "cancelled".to_string();
    };
    let started = app.state::<ScreenRecorder>().start(
        &session_path,
        token.generation,
        source,
        &start_cfg,
        audio_started_at_ms,
    );
    if started {
        "started".to_string()
    } else {
        "unavailable".to_string()
    }
}

/// POST the screen-capture consent ack. The Settings toggle calls this
/// before it enables capture (screen video is a distinct, heavier consent
/// than the audio recording). Frontend passes the running agent version +
/// platform (it has cheaper access to both than Rust on some paths).
#[tauri::command]
async fn screen_capture_ack(
    agent_version: String,
    platform: String,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::post_screen_capture_ack(backend, &agent_version, &platform).await
}

/// Fetch the call's screen-recording metadata (Slice C player). The agent
/// webview can't reach the backend directly (no token in the webview), so
/// this shim proxies `GET /v1/calls/{id}/screen`. `None` on 404 → the
/// player renders nothing. Playback credentials are minted separately.
#[tauri::command]
async fn get_screen_recording(
    id: String,
) -> Result<Option<serde_json::Value>, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::get_screen_recording(backend, &id).await
}

/// Lazily mint a short-lived playback credential for a ready screen
/// recording. The shared player correlates the returned generation id before
/// binding the URL, so a publication race cannot attach the wrong object.
#[tauri::command]
async fn create_screen_playback_url(
    id: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::create_screen_playback_url(backend, &id).await
}

#[derive(serde::Serialize)]
struct ScreenCaptureLocalStatus {
    available: bool,
    capturing: bool,
    sources: Vec<String>,
    source_kind: Option<String>,
}

#[derive(serde::Serialize)]
struct ScreenCaptureStatus {
    /// Whether a capture backend can run right now (binary + a display).
    available: bool,
    /// Whether a capture is in flight this instant.
    capturing: bool,
    /// Whether the user has recorded the screen-capture consent ack. Best-
    /// effort network read; None when it couldn't be determined (offline).
    consented: Option<bool>,
    /// ISO-8601 `accepted_at` from the consent ack, for the Settings stamp
    /// ("consent accepted on {date}"). None when there's no ack or it
    /// couldn't be determined.
    consented_at: Option<String>,
    /// #302 follow-up — the source kinds this platform advertises
    /// ("screen"/"window"/"region"). Empty hides the whole surface (macOS).
    sources: Vec<String>,
    /// #302 follow-up — the active capture's kind while it's running, else
    /// None. Drives the floater's kind-aware label and drops the cue when a
    /// capture dies mid-call (denied / closed / gdigrab exit).
    source_kind: Option<String>,
}

/// Cheap, local-only capture probe for the always-visible recording floater.
/// It intentionally performs no backend consent request, so the 2s privacy
/// indicator poll works offline and cannot create background API traffic.
#[tauri::command]
fn screen_capture_local_status(app: AppHandle) -> ScreenCaptureLocalStatus {
    let source_kind = app.state::<ScreenRecorder>().active_source_kind();
    ScreenCaptureLocalStatus {
        available: app.state::<ScreenRecorder>().is_available(),
        capturing: source_kind.is_some(),
        sources: screen_recorder::supported_source_kinds()
            .iter()
            .map(|source| source.to_string())
            .collect(),
        source_kind,
    }
}

/// Report screen-capture readiness for the Settings UI: backend
/// availability, whether a capture is live, and (best-effort) whether the
/// consent ack exists. All three are advisory — the org feature flag gate
/// + the backend consent gate remain the security boundaries.
#[tauri::command]
async fn screen_capture_status(app: AppHandle) -> ScreenCaptureStatus {
    let local = screen_capture_local_status(app.clone());
    // Best-effort consent probe — outer None on any error (offline / no
    // backend). The ack GET returns `Some({ accepted_at })` when a row
    // exists, so we surface both the boolean and the raw `accepted_at`
    // (ISO string) the Settings stamp renders as the acceptance date.
    let ack = match config::Config::load().ok().and_then(|c| c.backend) {
        Some(backend) => portal::get_screen_capture_ack(&backend).await.ok(),
        None => None,
    };
    let consented = ack.as_ref().map(|opt| opt.is_some());
    let consented_at = ack.flatten().and_then(|v| {
        v.get("accepted_at")
            .and_then(|a| a.as_str())
            .map(str::to_owned)
    });
    ScreenCaptureStatus {
        available: local.available,
        capturing: local.capturing,
        consented,
        consented_at,
        sources: local.sources,
        source_kind: local.source_kind,
    }
}

// ── #659 P4 — floating always-on-top co-pilot overlay ────────────────
//
// A second Tauri v2 webview window (label "overlay") created ON DEMAND
// from Rust behind `open_overlay` / `close_overlay`. It's opt-in
// (`config.overlay_enabled`, default OFF) + Call-mode + live-transcript-on;
// the SvelteKit layer opens it on record-start and closes it on stop. It
// loads the `/overlay` route — Tauri's asset resolver falls back to
// `index.html` for the (non-prerendered SPA) path, and SvelteKit's client
// router lands on the bare `/overlay` page (see `+layout.svelte`).
//
// OPAQUE v1 — deliberately NOT `.transparent(true)`. The prod/dev launcher
// forces `WEBKIT_DISABLE_COMPOSITING_MODE=1` (a real Wayland-stability
// workaround) which is fundamentally at odds with webkit transparency: a
// transparent window would render a black box. So the overlay is an opaque,
// decorationless, rounded-via-solid-background rectangle — safe on every
// platform. True alpha transparency + real rounded corners are a documented
// follow-up (also needs `macOSPrivateApi` on macOS).
//
// Creating the window from Rust needs no capability grant. The `/overlay`
// route gets only its two custom commands, close/drag, and event-listen access
// through the window-scoped `capabilities/overlay.json` ACL.

/// Cold-start hydration cache for the overlay window. The overlay is created
/// on demand and MISSES every `live-*` broadcast emitted before it existed
/// (coaching runs on a ~20s cadence, so a freshly-opened overlay could sit
/// blank up to 20s). This managed cache holds the latest coaching snapshot,
/// live-session status, and the current recording / session_uuid, updated at
/// each emit site (`emit_state`, `live::forward_incoming`, `live::emit_session`),
/// so `get_live_snapshot` can hydrate an overlay instantly. It rides the same
/// global broadcast thereafter — no main↔overlay IPC, no shared JS store.
#[derive(Default)]
pub(crate) struct LiveSnapshotCache {
    inner: std::sync::Mutex<LiveSnapshot>,
}

#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct LiveSnapshot {
    /// Last `live-coaching` payload (a FULL snapshot). None pre-first-frame /
    /// after a fresh session start.
    coaching: Option<serde_json::Value>,
    /// Last live-session status ("live" | "ended" | "error"). None when idle.
    status: Option<String>,
    /// Current live session_uuid — Some during a Call with the relay open, so
    /// the overlay's ask-chips can address the session on cold start.
    session_uuid: Option<String>,
    /// Whether a recording is currently in flight.
    recording: bool,
}

impl LiveSnapshotCache {
    fn snapshot(&self) -> LiveSnapshot {
        self.inner.lock().unwrap().clone()
    }
    pub(crate) fn set_coaching(&self, v: serde_json::Value) {
        self.inner.lock().unwrap().coaching = Some(v);
    }
    pub(crate) fn set_status(&self, s: &str) {
        self.inner.lock().unwrap().status = Some(s.to_string());
    }
    /// Record-start (Call): reset the coaching snapshot (fresh session),
    /// seed status optimistically to "live", stash the uuid, mark recording.
    /// Mirrors the JS `liveSession.resetForNewSession` + `setSessionUuid`.
    pub(crate) fn begin_session(&self, session_uuid: Option<String>) {
        let mut g = self.inner.lock().unwrap();
        g.coaching = None;
        g.status = Some("live".to_string());
        g.session_uuid = session_uuid;
        g.recording = true;
    }
    /// Record-stop: mark not-recording. Coaching + status + uuid are left
    /// intact so a grace-period overlay still shows the final snapshot; the
    /// next `begin_session` clears them.
    pub(crate) fn end_session(&self) {
        self.inner.lock().unwrap().recording = false;
    }
}

/// #659 P4 — hydrate a freshly-opened overlay with the latest live snapshot
/// (coaching + session status + recording / session_uuid) so it never sits
/// blank waiting for the next ~20s coaching frame.
#[tauri::command]
fn get_live_snapshot(cache: State<'_, LiveSnapshotCache>) -> LiveSnapshot {
    cache.snapshot()
}

fn open_overlay_window(app: &AppHandle) -> tauri::Result<()> {
    // Idempotent: if it's already open, re-show + re-assert on-top.
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.show();
        let _ = w.set_always_on_top(true);
        return Ok(());
    }

    let mut builder =
        WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("/overlay".into()))
            .title("aftercalls co-pilot")
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            // Never yank the rep off their call app when the overlay appears.
            .focused(false)
            .resizable(true)
            .inner_size(360.0, 240.0)
            .min_inner_size(240.0, 120.0);

    // Park it top-right of the primary monitor so it floats over a centered
    // call window. Fixed offset fallback if the monitor query fails.
    match app.primary_monitor() {
        Ok(Some(monitor)) => {
            let scale = monitor.scale_factor();
            let size = monitor.size().to_logical::<f64>(scale);
            let x = (size.width - 360.0 - 24.0).max(0.0);
            builder = builder.position(x, 56.0);
        }
        _ => {
            builder = builder.position(900.0, 56.0);
        }
    }

    builder.build()?;
    Ok(())
}

/// #659 P4 — open (or re-show) the floating co-pilot overlay window.
#[tauri::command]
fn open_overlay(app: AppHandle) -> Result<(), String> {
    open_overlay_window(&app).map_err(|e| e.to_string())
}

/// #659 P4 — close the overlay window if present. No-op when absent. Never
/// touches the recording — the overlay is a passive display surface.
#[tauri::command]
fn close_overlay(app: AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.close();
    }
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

fn selected_save_path(
    app: &AppHandle,
    suggested_filename: &str,
    filter_name: &str,
    extensions: &[&str],
) -> Result<Option<PathBuf>, String> {
    let picked = app
        .dialog()
        .file()
        .set_file_name(ipc_security::safe_suggested_filename(
            suggested_filename,
            "aftercalls-export",
        ))
        .add_filter(filter_name, extensions)
        .blocking_save_file();
    let Some(picked) = picked else {
        return Ok(None);
    };
    let picked = picked
        .into_path()
        .map_err(|e| format!("resolve save location: {e}"))?;
    if !picked.is_absolute() {
        return Err("save location must be absolute".into());
    }
    let filename = picked
        .file_name()
        .ok_or_else(|| "save location must include a filename".to_string())?;
    let parent = picked
        .parent()
        .ok_or_else(|| "save location must include a parent folder".to_string())?
        .canonicalize()
        .map_err(|e| format!("resolve save folder: {e}"))?;
    if !parent.is_dir() {
        return Err("save location parent is not a folder".into());
    }
    Ok(Some(parent.join(filename)))
}

/// Stream an authenticated backend-issued audio URL to a location selected
/// by the native save dialog. The webview receives neither arbitrary network
/// fetch authority nor arbitrary filesystem write authority.
#[tauri::command]
async fn download_audio(
    app: AppHandle,
    security: State<'_, ipc_security::IpcSecurity>,
    url: String,
    suggested_filename: String,
) -> Result<bool, String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    const MAX_AUDIO_EXPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

    let url = security.require_approved_audio_url(&url)?;
    let Some(dest) = selected_save_path(&app, &suggested_filename, "Opus audio", &["opus"])? else {
        return Ok(false);
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("build download client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fetch failed: HTTP {}", resp.status()));
    }
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_AUDIO_EXPORT_BYTES)
    {
        return Err("audio download exceeds the 2 GB safety limit".into());
    }

    let staged = dest.with_file_name(format!(
        ".aftercalls-download-{}.part",
        uuid::Uuid::new_v4().simple()
    ));
    let stage_guard = media_manifest::reserve_private_stage(&staged)
        .map_err(|e| format!("reserve download stage: {e:#}"))?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&staged)
        .map_err(|e| format!("open download stage: {e}"))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut received = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read audio download: {e}"))?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| "audio download size overflow".to_string())?;
        if received > MAX_AUDIO_EXPORT_BYTES {
            return Err("audio download exceeds the 2 GB safety limit".into());
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write audio download: {e}"))?;
    }
    file.sync_all()
        .await
        .map_err(|e| format!("sync audio download: {e}"))?;
    drop(file);
    media_manifest::atomic_replace_file(&staged, &dest)
        .map_err(|e| format!("publish audio download: {e:#}"))?;
    drop(stage_guard);
    Ok(true)
}

/// Save UTF-8 text through a native dialog. No destination path crosses IPC.
#[tauri::command]
fn save_text_file(
    app: AppHandle,
    suggested_filename: String,
    contents: String,
) -> Result<bool, String> {
    const MAX_TEXT_EXPORT_BYTES: usize = 10 * 1024 * 1024;
    if contents.len() > MAX_TEXT_EXPORT_BYTES {
        return Err("text export exceeds the 10 MB safety limit".into());
    }
    let Some(dest) = selected_save_path(&app, &suggested_filename, "Text", &["txt"])? else {
        return Ok(false);
    };
    let staged = dest.with_file_name(format!(
        ".aftercalls-text-{}.part",
        uuid::Uuid::new_v4().simple()
    ));
    let stage_guard = media_manifest::reserve_private_stage(&staged)
        .map_err(|e| format!("reserve text export stage: {e:#}"))?;
    std::fs::write(&staged, contents.as_bytes())
        .map_err(|e| format!("write text export: {e}"))?;
    media_manifest::enforce_private_file(&staged)
        .map_err(|e| format!("protect text export: {e:#}"))?;
    media_manifest::sync_staged_file(&staged)
        .map_err(|e| format!("sync text export: {e:#}"))?;
    media_manifest::atomic_replace_file(&staged, &dest)
        .map_err(|e| format!("publish text export: {e:#}"))?;
    drop(stage_guard);
    Ok(true)
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

// #661 (speaker-identity Phase A) — unresolved speaker-naming suggestions
// for a call. Read-only; org-scoped on the backend. The front-end renders
// these as never-silent confirm chips. Confirm reuses `rename_speaker`
// (no dedicated command); dismiss is the command below.
#[tauri::command]
async fn call_speaker_suggestions(id: String) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::speaker_suggestions(backend, &id).await
}

// #661 — dismiss a pending suggestion so it stops re-surfacing. The only
// net-new user action beyond confirm (confirm rides `rename_speaker`).
#[tauri::command]
async fn dismiss_speaker_suggestion(
    id: String,
    suggestion_id: String,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::dismiss_speaker_suggestion(backend, &id, &suggestion_id).await
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

// ── Call Questions manual-edit shims (Phase 4 follow-up) ─────────────
//
// Three IPC commands wrapping the backend's manual-edit CRUD on the durable
// `call_questions` rows. The after-call page invokes these; the body is
// forwarded verbatim so the TS side stays the authoritative shape. Errors flow
// as the structured `PortalError` shape (#124). A manual add is
// `source='manual'`; editing an auto row flips it to manual server-side so
// re-enrichment never wipes the user's edit.

/// POST /v1/calls/{id}/questions — manual-add. Body:
///   {question_text, asker_side?, asker_display?, status?, answer_text?}
/// Backend returns 201 with the created row.
#[tauri::command]
async fn add_call_question(
    call_id: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::add_call_question(backend, &call_id, &body).await
}

/// PATCH /v1/calls/{id}/questions/{qid} — edit wording / answer / status /
/// attribution. Returns the updated row. Errors arrive as `PortalError` (#124).
#[tauri::command]
async fn patch_call_question(
    call_id: String,
    qid: String,
    body: serde_json::Value,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::patch_call_question(backend, &call_id, &qid, &body).await
}

/// DELETE /v1/calls/{id}/questions/{qid}?revision=N. The revision is required
/// for optimistic concurrency; conflicts surface for a canonical refresh.
#[tauri::command]
async fn delete_call_question(
    call_id: String,
    qid: String,
    revision: i64,
) -> Result<(), error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::delete_call_question(backend, &call_id, &qid, revision).await
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

/// GET /v1/live/crm-context — #653 co-pilot CRM pull. Given the Zoho
/// contact id the user picked (`contactId`) and optionally the live
/// `sessionUuid`, returns the contact card + open-Deals envelope the
/// CrmContextLane renders. Decoupled from the audio WS on purpose; the
/// copilot flag gate 404s when off, and a Zoho hiccup degrades per-lane
/// (never errors the panel). Mirrors the `zoho_search_records` shim.
#[tauri::command]
async fn live_crm_context(
    contact_id: Option<String>,
    session_uuid: Option<String>,
    mode: Option<String>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_crm_context(
        backend,
        contact_id.as_deref(),
        session_uuid.as_deref(),
        mode.as_deref(),
    )
    .await
}

/// POST /v1/live/ask — #660 co-pilot ask-chip. Generates a plain-text
/// answer over the live-transcript window for one of the four presets
/// (`catch_me_up | summarize | what_did_they_ask | action_items`). The
/// backend degrades calm-200 (empty / no-key / failure all resolve to a
/// renderable answer line); a genuine gate/transport error surfaces as a
/// structured `PortalError` the lane renders as a calm degrade. Mirrors
/// the `live_crm_context` shim.
#[tauri::command]
async fn live_ask(
    session_uuid: String,
    chip: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_ask(backend, &session_uuid, &chip).await
}

/// POST /v1/live/knowledge — #659 P5b Support-mode cited knowledge answer.
/// Given the live `sessionUuid` and an optional manual `query` (else the
/// backend derives it from the counterpart's most recent turn), returns
/// `{ answer, sources }`. Grounding-first: a no-match returns a calm line with
/// empty sources, never a hallucination. Copilot- + impersonation-write-gated;
/// degrades calm-200. Mirrors the `live_ask` shim.
#[tauri::command]
async fn live_knowledge(
    session_uuid: String,
    query: Option<String>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_knowledge(backend, &session_uuid, query.as_deref()).await
}

/// POST /v1/live/highlight — #660 one-click star toggle on a live
/// transcript turn (keyed `channel + start_ms`). `starred=false`
/// un-marks. Returns `{ starred, count }`. Copilot- + impersonation-
/// write-gated. Mirrors the `live_crm_context` shim.
#[tauri::command]
async fn live_highlight(
    session_uuid: String,
    channel: String,
    start_ms: i64,
    end_ms: i64,
    speaker: Option<String>,
    text: String,
    starred: bool,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_highlight(
        backend,
        &session_uuid,
        &channel,
        start_ms,
        end_ms,
        speaker.as_deref(),
        &text,
        starred,
    )
    .await
}

/// POST /v1/live/speaker-identity — #663 Phase 2 per-speaker identity
/// assignment (live rename → after-call continuity). Assigns (or clears,
/// `clear=true`) an identity for one live speaker keyed by `channel +
/// speakerLabel`. `kind` is `"zoho_contact"` | `"internal_user"` | `"adhoc"`;
/// `contactId` / `userId` are set per source. A primary `zoho_contact`
/// mirrors into the session's scalar contact anchor. Returns `{ identities }`
/// (the full stored map) so the frontend reconciles its optimistic set.
/// Copilot- + impersonation-write-gated; best-effort (fire-and-forget).
/// Mirrors the `live_highlight` shim.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn live_speaker_identity(
    session_uuid: String,
    channel: String,
    speaker_label: String,
    kind: String,
    display_name: String,
    contact_id: Option<String>,
    user_id: Option<String>,
    // Optional on the wire: the TS `SpeakerIdentityAssignArgs` declares both as
    // `?:`, and every assign path omits at least one (a non-Zoho pick sends no
    // `isPrimary`, an assign sends no `clear`). A bare `bool` makes Tauri reject
    // the whole IPC call with "missing required key" BEFORE the command body
    // runs — no HTTP request, and the caller's empty `catch` swallows it, so the
    // optimistic label stands while nothing is ever persisted.
    is_primary: Option<bool>,
    clear: Option<bool>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_speaker_identity(
        backend,
        &session_uuid,
        &channel,
        &speaker_label,
        &kind,
        &display_name,
        contact_id.as_deref(),
        user_id.as_deref(),
        is_primary.unwrap_or(false),
        clear.unwrap_or(false),
    )
    .await
}

/// POST /v1/live/questions — apply ONE manual edit to the live Questions
/// ledger. `op` is `"add"` | `"edit"` | `"delete"` | `"answer"` | `"reopen"`;
/// `id` targets a ledger entry for every op but `"add"`. Returns the FULL
/// post-edit snapshot in the `{"type":"questions"}` frame shape so the frontend
/// swaps its list wholesale. Copilot- + impersonation-write-gated. NOT
/// fire-and-forget — the rep is watching their own edit, so the caller surfaces
/// failures. Mirrors the `live_speaker_identity` shim.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn live_question_edit(
    session_uuid: String,
    op: String,
    id: Option<String>,
    text: Option<String>,
    asker_side: Option<String>,
    asker_display: Option<String>,
    answer_text: Option<String>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_question_edit(
        backend,
        &session_uuid,
        &op,
        id.as_deref(),
        text.as_deref(),
        asker_side.as_deref(),
        asker_display.as_deref(),
        answer_text.as_deref(),
    )
    .await
}

/// POST /v1/live/linked-deal — Phase 3 link (or clear, `clear=true`) the
/// call's Zoho Deal mid-call. Writes the scalar `state.copilot.linked_deal`;
/// enrichment projects it onto `call_links` + the call-end auto-push reads
/// it. Returns `{ linked_deal }` (the stored object or JSON null) so the
/// frontend reconciles its optimistic set. Copilot- + impersonation-write-
/// gated; best-effort (fire-and-forget). Mirrors the `live_speaker_identity`
/// shim.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn live_linked_deal(
    session_uuid: String,
    module: String,
    record_id: String,
    record_name: String,
    stage: Option<String>,
    amount: Option<String>,
    // Optional on the wire — only the UNLINK path sends `clear: true`; linking a
    // Deal omits it entirely. A bare `bool` fails the IPC call outright (see
    // `live_speaker_identity`), so linking never reached the backend at all.
    clear: Option<bool>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_linked_deal(
        backend,
        &session_uuid,
        &module,
        &record_id,
        &record_name,
        stage.as_deref(),
        amount.as_deref(),
        clear.unwrap_or(false),
    )
    .await
}

/// POST /v1/live/linked-ticket — Zoho Desk link (or clear, `clear=true`) the
/// call's support Ticket mid-call. Writes the scalar
/// `state.copilot.linked_ticket`; enrichment projects it onto `call_links`
/// (`kind='desk_ticket'`) + the call-end auto-push reads it. Returns
/// `{ linked_ticket }` (the stored object or JSON null) so the frontend
/// reconciles its optimistic set. Coexists with a linked Deal. Desk- +
/// impersonation-write-gated; best-effort (fire-and-forget). Mirrors the
/// `live_linked_deal` shim.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn live_linked_ticket(
    session_uuid: String,
    ticket_id: String,
    ticket_number: Option<String>,
    subject: Option<String>,
    web_url: Option<String>,
    // Optional on the wire — only the UNLINK path sends `clear: true`. Same
    // IPC-rejection trap as `live_linked_deal`.
    clear: Option<bool>,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::live_linked_ticket(
        backend,
        &session_uuid,
        &ticket_id,
        ticket_number.as_deref(),
        subject.as_deref(),
        web_url.as_deref(),
        clear.unwrap_or(false),
    )
    .await
}

/// POST /v1/calls/{id}/zoho/push — push a call to Zoho as a Call
/// activity linked to the picked record (#186). Body is forwarded
/// verbatim from the SendToZohoModal Step-3 review state.
#[tauri::command]
async fn zoho_push_call(
    call_id: String,
    body: serde_json::Value,
    idempotency_key: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_push_call(backend, &call_id, &body, &idempotency_key).await
}

/// POST /v1/calls/{id}/zoho-desk/push — Zoho Desk push a finished call to a
/// linked support Ticket as a private internal note. Mirrors the `zoho_push_call`
/// shim; the body is the single `{ticket_id}` (the backend builds the note
/// text). Drives the ended-card "Add to ticket" [Add] action.
#[tauri::command]
async fn zoho_desk_push_call(
    call_id: String,
    ticket_id: String,
    idempotency_key: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_desk_push_call(backend, &call_id, &ticket_id, &idempotency_key).await
}

/// GET /v1/calls/{call_id}/external-pushes/{attempt_id} — read one durable
/// CRM/Desk push attempt. The portal module validates both UUIDs and constructs
/// the fixed call-scoped path instead of accepting an arbitrary status URL.
#[tauri::command]
async fn external_push_status(
    call_id: String,
    attempt_id: String,
) -> Result<portal::ExternalPushStatus, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::external_push_status(backend, &call_id, &attempt_id).await
}

/// GET /v1/calls/{id}/zoho/prior-push — the most-recent successful CRM push
/// for this call, or `null` when none (404 → calm absence). Drives the
/// auto-mode "Pushed to <Deal>" confirmation on the call-ended card + the
/// after-call detail surface. JSON is passed through as-is; the frontend
/// maps `{ pushed?, record_name, zoho_url, module, pushed_at } | null`.
#[tauri::command]
async fn zoho_prior_push(
    call_id: String,
) -> Result<serde_json::Value, error::PortalError> {
    let cfg = config::Config::load().map_err(error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    portal::zoho_prior_push(backend, &call_id).await
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
    // A local discard must not orphan a backend multipart upload. Keep the
    // durable checkpoint/source if the abort cannot be acknowledged so a
    // later retry can still release it.
    media_upload::abort_checkpointed_generations(&path, "user_discarded_local_session")
        .await
        .map_err(|error| format!("abort pending media upload before discard: {error:#}"))?;
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
            // CLI start has no co-pilot picker context → no contact hint.
            match do_start(&state, app, None, "manual", None) {
                Ok(_) => {}
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
        Ok(_) => {}
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
        // Hotkey/tray toggle has no co-pilot picker context → no contact hint.
        match do_start(&state, app, None, "manual", None) {
            Ok(_) => {}
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
    // Ask through the webview, not `tauri-plugin-dialog`.
    //
    // The native dialog never appears on Wayland under wlroots-derived
    // compositors (#605, the same thing that broke the settings "Forget"
    // confirm). Here the consequence was worse than a dead button: the close
    // handler calls `api.prevent_close()` first, so on those compositors the
    // window refused to close and no prompt ever explained why.
    //
    // The webview owns the prompt now — including its wording — and calls
    // `confirm_quit` if the user agrees.
    show_main_window(&app);
    match app.get_webview_window("main") {
        Some(win) => {
            if win
                .emit(
                    "quit-confirm-request",
                    serde_json::json!({
                        "recording": recorder_busy,
                        "processing": pipeline_busy,
                    }),
                )
                .is_err()
            {
                // Emit failed — there is no reachable UI to ask through, so
                // honour the quit rather than trapping the user in an app that
                // will not close.
                app.exit(0);
            }
        }
        // No main webview at all (already torn down): nothing to protect and
        // nothing to ask with.
        None => app.exit(0),
    }
}

/// Webview's answer to `quit-confirm-request`. The prompt itself lives in the
/// layout so it renders on every platform; this is only the commit step.
#[tauri::command]
fn confirm_quit(app: AppHandle) {
    app.exit(0);
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

/// #644 — sweep orphan `aftercalls` processes left behind by previous
/// auto-updates. The Tauri AppImage updater renames the running
/// AppImage (so its exe path resolves to `tauri_current_app*/...
/// (deleted)`) and writes the new bytes; `relaunch()` then exits the
/// inner extracted binary, but the AppImage runtime wrapper keeps
/// serving its FUSE mount and is reparented to `systemd --user`. One
/// orphan wrapper accumulates per update.
///
/// Strategy: at startup, scan processes; SIGTERM anything that looks
/// like a previous-launch leftover. Conservative — match only what
/// could not possibly be the current pair:
///
///   - exe path contains `tauri_current_app` (the updater's tempdir
///     name), OR
///   - exe path carries the `(deleted)` marker, OR
///   - exe path is under a `/tmp/.mount_*` prefix that differs from
///     ours (a different AppImage mount = a different launch).
///
/// And the candidate name starts with `aftercalls`. We never kill:
///   - our own PID,
///   - our parent PID (autostart / login-session wrapper),
///   - any process whose resolved exe path equals our own,
///   - any process whose mount prefix matches ours.
///
/// Linux-only — the orphan pattern is specific to the AppImage +
/// FUSE wrapper combo. macOS `.app` and Windows `.msi` don't have
/// this leak. Safe no-op on every launch when no orphans exist.
#[cfg(target_os = "linux")]
fn sweep_orphan_appimage_processes() {
    // Defensive: any unexpected error inside the sweep is logged and
    // swallowed so a panic in process introspection can't break
    // agent startup.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sweep_orphan_appimage_processes_inner();
    }));
    if result.is_err() {
        eprintln!("aftercalls: orphan-sweep panicked; continuing startup");
        telemetry::log(
            "warn",
            "agent::orphan_sweep",
            "orphan_sweep panicked; continuing",
            None,
            None,
        );
    }
}

#[cfg(target_os = "linux")]
fn sweep_orphan_appimage_processes_inner() {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    let our_pid = std::process::id();
    // SAFETY: getppid() is async-signal-safe and always succeeds.
    let our_ppid = unsafe { libc::getppid() } as u32;
    let our_exe = std::env::current_exe().ok();
    let our_exe_str = our_exe.as_ref().map(|p| p.to_string_lossy().into_owned());

    // Find our own `/tmp/.mount_*` prefix (if we're running from an
    // AppImage mount). Anything under the *same* prefix is part of
    // this launch and must not be touched.
    let our_mount_prefix: Option<String> = our_exe.as_deref().and_then(|p| {
        for ancestor in p.ancestors() {
            let s = ancestor.to_string_lossy();
            if s.starts_with("/tmp/.mount_") && s.len() > "/tmp/.mount_".len() {
                return Some(s.into_owned());
            }
        }
        None
    });

    // Minimum-fields refresh: name is populated unconditionally
    // (sysinfo-0.32 docs on `ProcessRefreshKind`), and we resolve the
    // exe path via `/proc/<pid>/exe` ourselves below, so we don't need
    // sysinfo's exe/cpu/disk/memory sampling — skipping CPU sampling
    // in particular keeps this startup path under its ~50ms budget.
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let mut found: u32 = 0;
    let mut killed: u32 = 0;
    let skipped_self: u32 = 0;
    let mut skipped_parent: u32 = 0;

    for (pid, proc_) in sys.processes() {
        let pid_u32 = pid.as_u32();

        // Self-protection: never our own PID, never our parent.
        if pid_u32 == our_pid {
            continue;
        }

        let name = proc_.name().to_string_lossy();
        // Match both `aftercalls` (inner binary) and `aftercalls.AppI[mage]`
        // (the truncated comm value sysinfo returns for the wrapper).
        if !name.starts_with("aftercalls") {
            continue;
        }

        // Resolve exe via `/proc/<pid>/exe` directly — that's the
        // only place where the `(deleted)` marker is visible (sysinfo
        // strips it when it builds `Process::exe()`).
        let exe_link = std::fs::read_link(format!("/proc/{pid_u32}/exe"));
        let exe_str = match &exe_link {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => String::new(), // process gone mid-scan; treat as no-match
        };

        // Defense in depth: same exe path as us → same launch.
        if let Some(ours) = &our_exe_str {
            if !exe_str.is_empty() && &exe_str == ours {
                continue;
            }
        }

        if !exe_path_looks_like_orphan(&exe_str, our_mount_prefix.as_deref()) {
            continue;
        }

        found += 1;

        // Self-protection #2: never the parent PID. Counted after the
        // orphan match so the telemetry event reflects "we saw it and
        // chose not to act."
        if pid_u32 == our_ppid {
            skipped_parent += 1;
            telemetry::log(
                "info",
                "agent::orphan_sweep",
                format!("orphan_sweep: skipped parent pid={pid_u32} exe={exe_str}"),
                Some(serde_json::json!({ "pid": pid_u32, "exe": exe_str, "reason": "parent" })),
                None,
            );
            continue;
        }

        // SIGTERM, not SIGKILL — orphans may flush telemetry via the
        // existing panic-hook flush pattern. We don't wait; systemd
        // --user reaps. Fire-and-forget at startup.
        let rc = unsafe { libc::kill(pid_u32 as libc::pid_t, libc::SIGTERM) };
        if rc == 0 {
            killed += 1;
            telemetry::log(
                "info",
                "agent::orphan_sweep",
                format!("orphan_sweep: terminated stale aftercalls pid={pid_u32} exe={exe_str}"),
                Some(serde_json::json!({ "pid": pid_u32, "exe": exe_str })),
                None,
            );
        } else {
            // ESRCH = process already gone; benign. Anything else is
            // worth a warn but not a startup failure.
            let errno = std::io::Error::last_os_error();
            telemetry::log(
                "warn",
                "agent::orphan_sweep",
                format!(
                    "orphan_sweep: kill SIGTERM failed pid={pid_u32} exe={exe_str} err={errno}"
                ),
                Some(serde_json::json!({ "pid": pid_u32, "exe": exe_str, "errno": errno.raw_os_error() })),
                None,
            );
        }
    }

    // Suppress unused-warning bookkeeping field on builds where the
    // skipped_self counter never fires (we early-continue on pid ==
    // our_pid before reaching the orphan-classification step, so it
    // stays at zero — kept in the summary payload for symmetry with
    // the spec).
    let _ = skipped_self;

    telemetry::log(
        "info",
        "agent::orphan_sweep",
        format!("orphan_sweep done: found={found} killed={killed} skipped_parent={skipped_parent}"),
        Some(serde_json::json!({
            "found": found,
            "killed": killed,
            "skipped_self": skipped_self,
            "skipped_parent": skipped_parent,
        })),
        None,
    );
}

/// Pure heuristic used by `sweep_orphan_appimage_processes_inner`,
/// factored out so the matching logic is unit-testable without
/// scanning real `/proc`. `our_mount_prefix` is the caller's
/// `/tmp/.mount_*` prefix if it lives inside an AppImage mount,
/// otherwise `None`.
#[cfg(target_os = "linux")]
fn exe_path_looks_like_orphan(exe_str: &str, our_mount_prefix: Option<&str>) -> bool {
    if exe_str.is_empty() {
        return false;
    }
    exe_str.contains("tauri_current_app")
        || exe_str.contains("(deleted)")
        || (exe_str.starts_with("/tmp/.mount_")
            && our_mount_prefix
                .map(|m| !exe_str.starts_with(m))
                .unwrap_or(false))
}

#[cfg(all(test, target_os = "linux"))]
mod orphan_sweep_tests {
    use super::exe_path_looks_like_orphan;

    /// Live evidence from #644 — the leaked v0.18.0 wrapper's exe
    /// path. Must be classified as an orphan.
    #[test]
    fn detects_deleted_tauri_current_app_path() {
        let stale = "/opt/aftercalls/tauri_current_app38awJr/current_app.AppImage (deleted)";
        assert!(exe_path_looks_like_orphan(stale, None));
        // Even when we *are* running from a mount, a stale path
        // outside that mount should still classify as an orphan.
        assert!(exe_path_looks_like_orphan(
            stale,
            Some("/tmp/.mount_aftercGGANpH"),
        ));
    }

    /// The current inner binary's path on the leak report — must
    /// NOT be classified as an orphan when we resolve our own mount
    /// prefix to the same root.
    #[test]
    fn rejects_current_process_exe_path() {
        let ours = "/tmp/.mount_aftercGGANpH/usr/bin/aftercalls";
        assert!(!exe_path_looks_like_orphan(
            ours,
            Some("/tmp/.mount_aftercGGANpH"),
        ));
    }

    /// A *different* `/tmp/.mount_*` prefix indicates a different
    /// launch — that wrapper IS an orphan.
    #[test]
    fn detects_other_mount_prefix() {
        let other_launch = "/tmp/.mount_afterckOeijl/usr/bin/aftercalls";
        assert!(exe_path_looks_like_orphan(
            other_launch,
            Some("/tmp/.mount_aftercGGANpH"),
        ));
        // With no mount of our own (dev build), don't false-positive
        // on a single mount path — the heuristic only fires when we
        // can prove "different mount."
        assert!(!exe_path_looks_like_orphan(other_launch, None));
    }

    /// Empty exe-string (read_link failed) is not an orphan — a
    /// transient failure shouldn't SIGTERM unrelated processes.
    #[test]
    fn empty_exe_path_is_not_orphan() {
        assert!(!exe_path_looks_like_orphan("", None));
        assert!(!exe_path_looks_like_orphan(
            "",
            Some("/tmp/.mount_aftercGGANpH"),
        ));
    }

    /// Regular `/opt/aftercalls/aftercalls` (the current-launch
    /// wrapper symlink target) must not match — it's neither in
    /// `tauri_current_app*` nor under `/tmp/.mount_*`.
    #[test]
    fn rejects_current_wrapper_symlink_target() {
        let wrapper = "/opt/aftercalls/aftercalls";
        assert!(!exe_path_looks_like_orphan(wrapper, None));
        assert!(!exe_path_looks_like_orphan(
            wrapper,
            Some("/tmp/.mount_aftercGGANpH"),
        ));
    }
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
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(ipc_security::IpcSecurity::default())
        .manage(RecordingLifecycle::new())
        .manage(Recorder::new())
        // #302 Slice B — screen-capture recorder. Process-scoped so
        // do_start / do_stop reach the same in-flight capture via
        // app.state, exactly like the audio Recorder.
        .manage(ScreenRecorder::new())
        // Correlates the secondary Windows region overlay with the exact
        // recording/request that opened it; late overlay events are ignored.
        .manage(RegionSelectState::default())
        // chunked-upload — rolling per-channel Opus encoder. Process-scoped
        // so do_start / do_stop reach the same in-flight encode via
        // app.state, exactly like the audio Recorder + ScreenRecorder.
        .manage(RollingEncoder::new())
        // #live — live-transcript relay controller (Phase 1). Process-scoped
        // so do_start / do_stop reach the same session via app.state.
        .manage(live::LiveRelay::new())
        // #659 P4 — cold-start hydration cache for the floating overlay
        // window. Updated at each live emit site; read by get_live_snapshot.
        .manage(LiveSnapshotCache::default())
        // #634 — tray unread-count counter. Lives at process scope so
        // every `apply_tray_state` site reads the same value. Webview
        // pumps updates via the `set_unread_badge` command.
        .manage(TrayUnreadCount::new())
        // #634 — last-applied tray state, mirrored on every
        // `apply_tray_state` call. Lets `tray_refresh_with_current_state`
        // (the unread-badge IPC repaint path) preserve SelfNote /
        // Processing visuals instead of collapsing to Recording / Idle.
        .manage(CurrentTrayState::new())
        .invoke_handler(tauri::generate_handler![
            get_autostart,
            set_autostart,
            start_recording,
            start_self_note,
            stop_recording,
            is_recording,
            is_processing,
            busy_detail,
            confirm_quit,
            select_import_file,
            process_imported_file,
            confirm_auto_start,
            dismiss_auto_start,
            confirm_auto_end,
            keep_auto_recording,
            // #596 — auto-record per-app whitelist surface. Six
            // commands + four events drive the Settings page + cancel
            // toast; see the AutoRecord module group above.
            auto_record_settings_get,
            auto_record_settings_set_master,
            auto_record_settings_toggle_app,
            auto_record_settings_set_app_mode,
            auto_record_settings_forget_app,
            confirm_auto_record_cancel,
            login,
            logout,
            current_user,
            // #659 — network refresh of cached org features (co-pilot,
            // live transcript) so a mid-session purchase surfaces
            // without a re-login. Reuses portal::refresh_me.
            refresh_current_user,
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
            save_text_file,
            get_app_prefs,
            set_app_prefs,
            list_input_devices,
            platform_os,
            // #623 — macOS capture-permission pre-flight. Status read
            // gates the Start path; the request + open-settings shims
            // back the onboarding permissions slide and the actionable
            // "Open System Settings" error affordance. No-op off macOS.
            // Mirrored in `permissions/app.toml` `main-commands`.
            check_capture_permissions,
            request_mic_permission,
            request_screen_capture_access,
            open_privacy_settings,
            // #302 Slice B — screen capture: monitor picker, per-user
            // prefs, the (distinct) consent ack, + a readiness probe.
            // The org feature flag gate lives on me.features.screen_capture.
            list_displays,
            get_screen_capture_prefs,
            set_screen_capture_prefs,
            screen_capture_ack,
            screen_capture_local_status,
            screen_capture_status,
            get_screen_recording,
            create_screen_playback_url,
            // #302 follow-up — per-call screen-source chooser: window list,
            // region drag-select (Linux slurp), the Windows region-overlay
            // result sink, and the start-on-chosen-source command.
            list_windows,
            pick_region,
            submit_region_selection,
            // #302 review — Rust-managed region overlay (mirrors open/close_overlay)
            // so the frontend drops the unscoped webview-create capability.
            open_region_select,
            close_region_select,
            start_screen_source,
            // #659 P4 — floating always-on-top co-pilot overlay: open/close
            // the second webview + hydrate it from the cold-start snapshot
            // cache. Its two custom commands and minimal core permissions live
            // in the window-scoped capabilities/overlay.json ACL.
            open_overlay,
            close_overlay,
            get_live_snapshot,
            telemetry::log_event,
            get_peaks,
            get_vault_settings,
            select_vault_directory,
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
            call_speaker_suggestions,
            dismiss_speaker_suggestion,
            org_members,
            update_call_tags,
            resummarize_call,
            patch_call,
            text_replace,
            patch_action_item,
            add_client_allowlist_entry,
            add_action_item,
            delete_action_item,
            // Call Questions manual-edit shims (Phase 4 follow-up).
            add_call_question,
            patch_call_question,
            delete_call_question,
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
            // #630 — per-user summary-style override. Backs the agent's
            // new "AI summary style" Settings card.
            me_summary_style_get,
            me_summary_style_patch,
            // Phase 3 — per-user Zoho call-end auto-push preference. Backs
            // the agent's "Push to CRM" Settings card (prompt | auto).
            me_zoho_autopush_get,
            me_zoho_autopush_patch,
            // #634 — per-user unread-call state + tray badge. Three
            // shims around the new backend endpoints + one helper that
            // pushes the live count into the tray tooltip from the
            // webview's poll callback. See `apply_tray_state` + the
            // `TrayUnreadCount` notes above for the per-OS strategy.
            mark_call_read,
            mark_calls_read_bulk,
            me_unread_count,
            me_subscription,
            set_unread_badge,
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
            // #646 Layer C — 5-min sweeper Tauri command. Returns a
            // Vec<AutoResumeResult> for diagnostic / test purposes; the
            // user-visible surface is the silent pipeline run itself
            // (topstrip indicator picks up the next stage).
            recovery::auto_resume_orphans,
            // #186 Zoho CRM: status probe + search + push.
            // Connect/disconnect happens on the portal admin page; the
            // agent links out via openUrl, so no connect command here.
            zoho_status,
            zoho_record_types,
            zoho_search_records,
            // #653 — in-call co-pilot CRM-context pull (contact card +
            // open Deals). Decoupled from the audio WS relay.
            live_crm_context,
            // #660 co-pilot P1 — on-demand ask-chip + one-click highlight
            // (star) toggle. REST, off the audio WS; copilot-gated.
            live_ask,
            live_highlight,
            // #663 Phase 2 — per-speaker identity assignment (live rename →
            // after-call continuity). REST, off the audio WS; copilot-gated.
            live_speaker_identity,
            live_question_edit,
            // Phase 3 — link a Zoho Deal to the live call (→ call_links +
            // call-end auto-push). REST, off the audio WS; copilot-gated.
            live_linked_deal,
            // Zoho Desk — link a support Ticket to the live call (→ call_links
            // `kind='desk_ticket'` + call-end auto-push). Coexists with a
            // linked Deal. REST, off the audio WS; desk-gated.
            live_linked_ticket,
            // #659 P5b — Support-mode cited knowledge answer over the org's
            // own knowledge base. REST, off the audio WS; copilot-gated.
            live_knowledge,
            zoho_push_call,
            // Durable external-push settlement: fixed call/attempt GET only.
            external_push_status,
            // Zoho Desk — push a finished call to a linked Ticket as a private
            // internal note. Drives the ended-card "Add to ticket" action.
            zoho_desk_push_call,
            // Phase 3 — most-recent successful CRM push for a call (404 →
            // null). Drives the ended-card confirmation + after-call detail.
            zoho_prior_push,
            notes::save_notes,
            notes::load_notes,
            notes::save_title,
            notes::update_call_notes,
            // #183 — in-agent support reports.
            // #203 — stage_support_video lands webview-produced webm
            // blobs on a temp path so the existing path-based submit
            // pipeline rides unchanged.
            support::select_support_attachments,
            support::stage_support_video,
            support::submit_support_report,
            // #628 — opt-in attachment of the user's most recent
            // recording session (audio + a session-meta.json)
            // to a support ticket. Returns a staged zip path that
            // rides the existing path-based upload pipeline.
            support::bundle_latest_session,
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

            // #644 — sweep orphan AppImage wrappers left by previous
            // auto-updates. See `sweep_orphan_appimage_processes`
            // above for the heuristic + safety rationale. Linux-only;
            // the helper is `#[cfg(target_os = "linux")]` so this is
            // a literal no-op on macOS / Windows builds.
            #[cfg(target_os = "linux")]
            sweep_orphan_appimage_processes();

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
                    } else if is_busy(&app) {
                        // Close-to-tray=false makes the window X a real
                        // quit path, so protect in-flight recording or
                        // post-call work the same way tray Quit does.
                        api.prevent_close();
                        quit_with_confirm(app);
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

#[cfg(test)]
mod ipc_acl_guard {
    /// Registering a command in `generate_handler!` is only half of exposing
    /// it — Tauri's ACL denies any command absent from a `commands.allow` list
    /// in `permissions/app.toml`, and it does so at the IPC boundary, BEFORE
    /// the command body runs. The frontend then sees a bare rejection with no
    /// HTTP request behind it, which reads exactly like a backend failure.
    ///
    /// `live_question_edit` shipped that way in v0.32.0: every Questions edit
    /// (delete / mark-answered / edit / add) failed with "Couldn't save that
    /// just now" while nothing ever reached the backend. This test is the
    /// cheap guard that keeps the two lists in step.
    #[test]
    fn every_registered_command_is_allowlisted() {
        const SRC: &str = include_str!("lib.rs");
        const ACL: &str = include_str!("../permissions/app.toml");

        let handler = SRC
            .split_once("generate_handler!")
            .expect("generate_handler! is present")
            .1;
        let start = handler.find('[').expect("handler list opens with [");
        let mut depth = 0usize;
        let mut end = start;
        for (i, c) in handler[start..].char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(end > start, "handler list never closed");

        let registered: Vec<String> = handler[start + 1..end]
            .lines()
            // Strip line comments — the list is heavily annotated.
            .map(|line| line.split("//").next().unwrap_or(""))
            .flat_map(|line| line.split(','))
            // `recovery::auto_resume_orphans` registers as `auto_resume_orphans`.
            .map(|tok| tok.trim().rsplit("::").next().unwrap_or("").trim().to_string())
            .filter(|tok| {
                !tok.is_empty() && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .collect();
        // Sanity floor: if the parse breaks, fail loudly rather than pass empty.
        assert!(
            registered.len() > 100,
            "parsed only {} commands — the handler-list parse is broken",
            registered.len()
        );

        let missing: Vec<&str> = registered
            .iter()
            .filter(|cmd| !ACL.contains(&format!("\"{cmd}\"")))
            .map(String::as_str)
            .collect();
        assert!(
            missing.is_empty(),
            "registered but absent from permissions/app.toml — Tauri will deny \
             these at the IPC boundary, so the frontend gets a rejection with no \
             request behind it: {missing:?}"
        );
    }
}
