use std::collections::HashSet;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::mic_consumers::raw_mic_consumers;
use crate::notify_actions::{self, ActionSpec, NotifyError};
use crate::recorder::Recorder;

/// Apps we *don't* want to treat as "a call." Matched case-insensitively as
/// substrings. Kept tight: WEBRTC VoiceEngine is the generic Chromium/Electron
/// audio stack, so Discord hits it — we rely on `application.process.binary`
/// to identify the real app (Discord / teams-for-linux / etc.) and only
/// blacklist things that are *never* an app-meeting (our own capture and
/// accessibility noise).
const MIC_CONSUMER_BLACKLIST: &[&str] = &[
    "pipewire alsa [client]", // generic ALSA client (our cpal mic path lands here)
    "pacat",                  // parec's binary
    "parec",
    "pw-cat",
    "pw-record",
    "speech-dispatcher",
    "aftercalls",             // our own cpal mic consumer shows up as "pipewire alsa [aftercalls]"
];

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long the mic consumer must be gone before we prompt to end.
const CONSUMER_GONE_BEFORE_END_PROMPT: Duration = Duration::from_secs(5);
/// Safety net for a crashed / force-quit softphone (#74). If the
/// consumer has been gone this long and the user hasn't answered the
/// end-prompt, the recording is force-stopped via the same code path
/// as manual Stop. Three consecutive polls at POLL_INTERVAL=5s — fast
/// enough that a SIGKILL'd softphone can't keep the recorder pinned
/// open for more than ~15s while still tolerating a brief restart.
const CONSUMER_GONE_FORCE_STOP: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug)]
pub enum UserDecision {
    ConfirmStart,
    DismissStart,
    ConfirmEnd,
    KeepRecording,
}

pub struct Detector {
    tx: mpsc::UnboundedSender<UserDecision>,
}

impl Detector {
    pub fn spawn(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tauri::async_runtime::spawn(run(app, rx));
        Self { tx }
    }

    pub fn decide(&self, d: UserDecision) {
        let _ = self.tx.send(d);
    }
}

#[derive(Debug, Clone)]
enum Phase {
    Idle,
    AwaitingStartConfirm { consumer: String },
    Recording { consumer: String, gone_since: Option<Instant> },
    /// `gone_since` is inherited from Recording so the 15s force-stop
    /// watchdog (#74) measures total consumer-absence, not just time
    /// spent waiting on the user. Without this a user who walks away
    /// from an end-prompt would keep the recorder running until they
    /// returned.
    AwaitingEndConfirm { consumer: String, gone_since: Instant },
    /// User explicitly said no to recording this consumer. Release when the
    /// consumer stops using the mic — they may come back for another call.
    Suppressed { consumer: String },
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AutoDetectEvent {
    PromptStart { app: String },
    PromptEnd { app: String },
    Cleared,
}

async fn run(app: AppHandle, mut rx: mpsc::UnboundedReceiver<UserDecision>) {
    let mut phase = Phase::Idle;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                phase = tick(&app, phase);
            }
            Some(decision) = rx.recv() => {
                phase = handle_decision(&app, phase, decision).await;
            }
        }
    }
}

fn tick(app: &AppHandle, phase: Phase) -> Phase {
    // Cheap per-tick config read (the file is tiny) so toggling
    // auto-detect off in Settings takes effect immediately without
    // having to restart the app. When off, we clear any in-flight
    // prompt and return to Idle so the user isn't stuck on an
    // already-fired banner.
    //
    // #180 — the popup flag is read on the same tick so flipping
    // "show system popup" in Settings takes effect before the next
    // detection without a restart. Defaults to true on read failure
    // to preserve historical "always-focus" behavior when the
    // config can't be loaded.
    let cfg = crate::config::Config::load().ok();
    let auto_detect_on = cfg.as_ref().map(|c| c.auto_detect).unwrap_or(true);
    let popup_on = cfg.as_ref().map(|c| c.auto_detect_popup).unwrap_or(true);
    if !auto_detect_on {
        if !matches!(phase, Phase::Idle) {
            emit(app, AutoDetectEvent::Cleared);
        }
        return Phase::Idle;
    }

    let consumers = interesting_mic_consumers();
    let state = app.state::<Recorder>();
    let is_recording = state.is_active();

    let phase = reconcile_external(phase, is_recording, app);

    match phase {
        Phase::Idle => {
            if let Some(consumer) = consumers.iter().next() {
                eprintln!("aftercalls: '{consumer}' is using the mic — prompting");
                emit(app, AutoDetectEvent::PromptStart { app: consumer.clone() });
                maybe_show_window(app, popup_on, PromptKind::Start { consumer });
                Phase::AwaitingStartConfirm { consumer: consumer.clone() }
            } else {
                Phase::Idle
            }
        }
        Phase::AwaitingStartConfirm { consumer } => {
            if !consumers.contains(&consumer) {
                emit(app, AutoDetectEvent::Cleared);
                Phase::Idle
            } else {
                Phase::AwaitingStartConfirm { consumer }
            }
        }
        Phase::Recording { consumer, gone_since } => {
            if consumers.contains(&consumer) {
                Phase::Recording { consumer, gone_since: None }
            } else {
                let since = gone_since.unwrap_or_else(Instant::now);
                if since.elapsed() >= CONSUMER_GONE_BEFORE_END_PROMPT {
                    eprintln!("aftercalls: '{consumer}' stopped using mic — prompting to end");
                    emit(app, AutoDetectEvent::PromptEnd { app: consumer.clone() });
                    maybe_show_window(app, popup_on, PromptKind::End);
                    Phase::AwaitingEndConfirm { consumer, gone_since: since }
                } else {
                    Phase::Recording { consumer, gone_since: Some(since) }
                }
            }
        }
        Phase::AwaitingEndConfirm { consumer, gone_since } => {
            if consumers.contains(&consumer) {
                // Consumer came back — user rejoined. Cancel the end prompt.
                emit(app, AutoDetectEvent::Cleared);
                Phase::Recording { consumer, gone_since: None }
            } else if gone_since.elapsed() >= CONSUMER_GONE_FORCE_STOP {
                // Safety net (#74): consumer has been dead for
                // CONSUMER_GONE_FORCE_STOP and the user never answered
                // the prompt. Force-stop via the same path as manual
                // Stop so a crashed softphone can't pin the recorder
                // open indefinitely.
                eprintln!(
                    "aftercalls: '{consumer}' gone for {}s and no user response — force-stopping",
                    gone_since.elapsed().as_secs()
                );
                let state = app.state::<Recorder>();
                if state.is_active() {
                    let _ = crate::do_stop(&state, app);
                }
                emit(app, AutoDetectEvent::Cleared);
                Phase::Idle
            } else {
                Phase::AwaitingEndConfirm { consumer, gone_since }
            }
        }
        Phase::Suppressed { consumer } => {
            // Release suppression once the old consumer is gone.
            let still_suppressing = consumers.contains(&consumer);
            // If a different consumer is now on the mic, prompt for it.
            if let Some(other) = consumers.iter().find(|c| **c != consumer) {
                eprintln!("aftercalls: new mic consumer '{other}' — prompting");
                emit(app, AutoDetectEvent::PromptStart { app: other.clone() });
                maybe_show_window(app, popup_on, PromptKind::Start { consumer: other });
                Phase::AwaitingStartConfirm { consumer: other.clone() }
            } else if still_suppressing {
                Phase::Suppressed { consumer }
            } else {
                Phase::Idle
            }
        }
    }
}

fn reconcile_external(phase: Phase, is_recording: bool, app: &AppHandle) -> Phase {
    match (phase, is_recording) {
        (Phase::Recording { consumer, .. }, false) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Suppressed { consumer }
        }
        (Phase::AwaitingEndConfirm { consumer, .. }, false) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Suppressed { consumer }
        }
        (Phase::AwaitingStartConfirm { consumer }, true) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Recording { consumer, gone_since: None }
        }
        (other, _) => other,
    }
}

async fn handle_decision(app: &AppHandle, phase: Phase, decision: UserDecision) -> Phase {
    let state = app.state::<Recorder>();
    match (phase, decision) {
        (Phase::AwaitingStartConfirm { consumer }, UserDecision::ConfirmStart) => {
            match crate::do_start(&state, app) {
                Ok(path) => {
                    crate::write_session_source(
                        std::path::Path::new(&path),
                        "auto_detected",
                        Some(&consumer),
                    );
                    // Confirmed record — wipe the per-consumer snooze
                    // so a future mic-detect for the same app (after
                    // a Stop) can re-toast immediately. The snooze is
                    // designed to suppress repeat *prompts*, not to
                    // gate recording.
                    notify_actions::clear_snooze(&consumer);
                    emit(app, AutoDetectEvent::Cleared);
                    Phase::Recording { consumer, gone_since: None }
                }
                Err(e) => {
                    eprintln!("aftercalls: auto-start failed: {e}");
                    emit(app, AutoDetectEvent::Cleared);
                    Phase::Idle
                }
            }
        }
        (Phase::AwaitingStartConfirm { consumer }, UserDecision::DismissStart) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Suppressed { consumer }
        }
        (Phase::AwaitingEndConfirm { .. }, UserDecision::ConfirmEnd) => {
            if state.is_active() {
                let _ = crate::do_stop(&state, app);
            }
            emit(app, AutoDetectEvent::Cleared);
            Phase::Idle
        }
        (Phase::AwaitingEndConfirm { consumer, .. }, UserDecision::KeepRecording) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Recording { consumer, gone_since: None }
        }
        (phase, _) => phase,
    }
}

/// Apps currently holding a source-output on *any* mic source, filtered
/// against the blacklist. Deduplicated so a multi-stream app (e.g. WebRTC)
/// only shows up once.
///
/// The detector renders the "consumer" string in user-visible prompts,
/// so we keep the historical behaviour: prefer `application.process.binary`
/// (RawMicConsumer::bundle_id) and fall back to `application.name`
/// (`friendly_name`) if the binary lookup ever produced an empty key.
/// In practice the two are the same for non-empty rows; the wrapper
/// just keeps the same shape callers expect.
fn interesting_mic_consumers() -> Vec<String> {
    let consumers = raw_mic_consumers();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for c in consumers {
        let display = if c.bundle_id.is_empty() {
            c.friendly_name.clone()
        } else {
            c.bundle_id.clone()
        };
        let lower = display.to_lowercase();
        if MIC_CONSUMER_BLACKLIST
            .iter()
            .any(|b| lower.contains(&b.to_lowercase()))
        {
            continue;
        }
        if seen.insert(display.clone()) {
            result.push(display);
        }
    }
    result
}

fn emit(app: &AppHandle, event: AutoDetectEvent) {
    let _ = app.emit("auto-detect", event);
}

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        // See show_main_window in lib.rs — redundant show() on wlroots
        // compositors can spawn a phantom second surface.
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

/// Which prompt the detector wants to surface — controls whether we
/// route through the OS-native actionable toast (start) or the
/// existing focus-steal `show_window` (end). Carrying the consumer
/// name on `Start` lets the toast snooze key match the detector's
/// own per-consumer state.
enum PromptKind<'a> {
    Start { consumer: &'a str },
    /// Mid-recording end-prompt. Kept on the focus-steal path because
    /// the in-app slide-out for end-prompts lives on the Record page
    /// and the user is almost always there during a live recording.
    /// The toast UX (#89) covers start-prompts only.
    End,
}

/// #180 — gate the window-show / focus-steal behind the
/// `auto_detect_popup` pref. When the user has it off, neither the
/// focus-steal NOR the OS toast fires (the in-app `auto-detect` event
/// still emits separately so the slide-out renders).
///
/// #89 — start-prompts route through the OS-native actionable toast
/// (`notify_actions`) instead of stealing window focus. The toast
/// emits `notify-action` on user click, which the layout-level
/// listener routes back through `confirm_auto_start` /
/// `dismiss_auto_start` IPC — landing as a `UserDecision` here. If
/// the toast backend errors (DBus daemon down, Win OS notifications
/// off, mac bundle missing), we fall through to the legacy
/// `show_window` so the user is never trapped without a way to act
/// on the detection.
fn maybe_show_window(app: &AppHandle, popup_on: bool, kind: PromptKind<'_>) {
    if !popup_on {
        return;
    }
    match kind {
        PromptKind::Start { consumer } => {
            // 30s timeout matches the spec; "Not now" + auto-dismiss
            // resolve to the same dismiss handler on the frontend so
            // the detector's Suppressed phase fires either way.
            let res = notify_actions::show_actionable_notification(
                app.clone(),
                "aftercalls",
                "Record this call?",
                vec![
                    ActionSpec { id: "record".into(),  label: "Record".into() },
                    ActionSpec { id: "dismiss".into(), label: "Not now".into() },
                ],
                "notify-action",
                Duration::from_secs(30),
                Some(consumer),
            );
            match res {
                Ok(()) => {} // toast posted; wait for the user's choice via IPC
                Err(NotifyError::PermissionDenied) => {
                    eprintln!(
                        "aftercalls: notifications denied by OS — falling back to focus-steal"
                    );
                    show_window(app);
                }
                Err(e) => {
                    eprintln!(
                        "aftercalls: actionable notification failed ({e}) — falling back to focus-steal"
                    );
                    show_window(app);
                }
            }
        }
        PromptKind::End => {
            // End-prompt path keeps the legacy focus-steal — the
            // Record page's inline end-banner is the canonical
            // surface and the user is typically focused on /record
            // already, so stealing focus is the right behavior.
            show_window(app);
        }
    }
}
