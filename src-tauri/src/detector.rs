use std::collections::HashSet;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Listener, Manager};
use tokio::sync::mpsc;

use crate::app_observations::AppObservations;
use crate::auto_recorder::AutoRecorder;
use crate::mic_consumers::{is_blacklisted, subscribe_mic_consumers, RawMicConsumer};

use crate::notify_actions::{self, ActionSpec, NotifyError};
use crate::recorder::Recorder;

// Apps we *don't* want to treat as "a call" — vendor helpers we spawn,
// generic framework labels, accessibility noise. Lives in
// `mic_consumers::MIC_CONSUMER_BLACKLIST` so the detector AND the
// auto-record observer share one filter (#604: the observer used to
// skip this list, leaking `parec` + `Chromium input` into the
// user-visible auto-record catalog). New entries land there, not here.

/// How long the mic consumer must be gone before we prompt to end.
const CONSUMER_GONE_BEFORE_END_PROMPT: Duration = Duration::from_secs(5);
/// Safety net for a crashed / force-quit softphone (#74). If the
/// consumer has been gone this long and the user hasn't answered the
/// end-prompt, the recording is force-stopped via the same code path
/// as manual Stop. Three shared mic-consumer ticks at 5s — fast enough
/// that a SIGKILL'd softphone can't keep the recorder pinned open for
/// more than ~15s while still tolerating a brief restart.
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

        // #606 — Rust-side `notify-action` listener as a redundant
        // path for the OS-toast confirm. The webview's listener at
        // +layout.svelte:851 still routes clicks through the
        // confirm_auto_start IPC, but on Windows the toast click can
        // arrive BEFORE the webview's listener mounts (agent launched
        // by AUMID activation, agent waking from tray, etc.). When the
        // webview misses the event, the toast click was a no-op:
        // user clicks "Record this call" → app opens → no recording
        // starts → user has to click Record manually. That's the user
        // report on #606. Routing the decision through `decide()`
        // here as well makes the action robust to webview-mount race;
        // the second decision arriving from the webview later is a
        // phase-transition no-op (Detector::run drains both via the
        // same `rx` and already handles re-emit guards).
        let tx_for_listener = tx.clone();
        app.listen("notify-action", move |event| {
            #[derive(serde::Deserialize)]
            struct Payload {
                action_id: String,
                #[serde(default)]
                auto_dismissed: bool,
            }
            let Ok(p) = serde_json::from_str::<Payload>(event.payload()) else {
                return;
            };
            // Only the start-prompt toast routes through this event
            // (end-prompts use the in-app banner only — see
            // maybe_show_window). action_id values come from
            // detector.rs's PromptKind::Start: "record" / "dismiss".
            // The auto-dismiss case (timeout) is treated as Dismiss
            // so the detector's Suppressed phase fires consistently.
            let decision = match (p.action_id.as_str(), p.auto_dismissed) {
                ("record", false) => Some(UserDecision::ConfirmStart),
                ("dismiss", _) | (_, true) => Some(UserDecision::DismissStart),
                _ => None,
            };
            if let Some(d) = decision {
                let _ = tx_for_listener.send(d);
            }
        });

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
    AwaitingStartConfirm {
        consumer: String,
    },
    Recording {
        consumer: String,
        gone_since: Option<Instant>,
    },
    /// `gone_since` is inherited from Recording so the 15s force-stop
    /// watchdog (#74) measures total consumer-absence, not just time
    /// spent waiting on the user. Without this a user who walks away
    /// from an end-prompt would keep the recorder running until they
    /// returned.
    AwaitingEndConfirm {
        consumer: String,
        gone_since: Instant,
    },
    /// User explicitly said no to recording this consumer. Release when the
    /// consumer stops using the mic — they may come back for another call.
    Suppressed {
        consumer: String,
    },
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
    let mut consumers_rx = subscribe_mic_consumers();
    loop {
        tokio::select! {
            changed = consumers_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                phase = tick(&app, phase, consumers_rx.borrow().as_slice());
            }
            Some(decision) = rx.recv() => {
                phase = handle_decision(&app, phase, decision).await;
            }
        }
    }
}

fn tick(app: &AppHandle, phase: Phase, mic_consumers: &[RawMicConsumer]) -> Phase {
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

    // Source-side silence filter (#never-ask-app). Apps the user set
    // to `mode = Never` in Settings are dropped before any phase
    // transition runs — kills the toast, the in-app slide-out, AND
    // the PIPEDA modal in one cut without touching any downstream
    // surface. The store may not be managed during a degraded
    // startup (auto_recorder::open failed); when it isn't, we fall
    // back to today's no-silence behaviour.
    let consumers = if let Some(auto) = app.try_state::<AutoRecorder>() {
        interesting_mic_consumers(mic_consumers, Some(auto.store()))
    } else {
        interesting_mic_consumers(mic_consumers, None)
    };
    let state = app.state::<Recorder>();
    let is_recording = state.is_active();

    let phase = reconcile_external(phase, is_recording, app);

    match phase {
        Phase::Idle => {
            if let Some(consumer) = consumers.iter().next() {
                eprintln!("aftercalls: '{consumer}' is using the mic — prompting");
                emit(
                    app,
                    AutoDetectEvent::PromptStart {
                        app: consumer.clone(),
                    },
                );
                maybe_show_window(app, popup_on, PromptKind::Start { consumer });
                Phase::AwaitingStartConfirm {
                    consumer: consumer.clone(),
                }
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
        Phase::Recording {
            consumer,
            gone_since,
        } => {
            if consumers.contains(&consumer) {
                Phase::Recording {
                    consumer,
                    gone_since: None,
                }
            } else {
                let since = gone_since.unwrap_or_else(Instant::now);
                if since.elapsed() >= CONSUMER_GONE_BEFORE_END_PROMPT {
                    eprintln!("aftercalls: '{consumer}' stopped using mic — prompting to end");
                    emit(
                        app,
                        AutoDetectEvent::PromptEnd {
                            app: consumer.clone(),
                        },
                    );
                    maybe_show_window(app, popup_on, PromptKind::End);
                    Phase::AwaitingEndConfirm {
                        consumer,
                        gone_since: since,
                    }
                } else {
                    Phase::Recording {
                        consumer,
                        gone_since: Some(since),
                    }
                }
            }
        }
        Phase::AwaitingEndConfirm {
            consumer,
            gone_since,
        } => {
            if consumers.contains(&consumer) {
                // Consumer came back — user rejoined. Cancel the end prompt.
                emit(app, AutoDetectEvent::Cleared);
                Phase::Recording {
                    consumer,
                    gone_since: None,
                }
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
                Phase::AwaitingEndConfirm {
                    consumer,
                    gone_since,
                }
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
                Phase::AwaitingStartConfirm {
                    consumer: other.clone(),
                }
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
            Phase::Recording {
                consumer,
                gone_since: None,
            }
        }
        (other, _) => other,
    }
}

async fn handle_decision(app: &AppHandle, phase: Phase, decision: UserDecision) -> Phase {
    let state = app.state::<Recorder>();
    match (phase, decision) {
        (Phase::AwaitingStartConfirm { consumer }, UserDecision::ConfirmStart) => {
            // Auto-detect start has no co-pilot picker context → no contact hint.
            match crate::do_start(&state, app, None) {
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
                    Phase::Recording {
                        consumer,
                        gone_since: None,
                    }
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
            Phase::Recording {
                consumer,
                gone_since: None,
            }
        }
        (phase, _) => phase,
    }
}

/// Apps currently holding a source-output on *any* mic source, filtered
/// against the blacklist AND the per-row `mode = Never` silence list.
/// Deduplicated so a multi-stream app (e.g. WebRTC) only shows up once.
///
/// The detector renders the "consumer" string in user-visible prompts,
/// so we keep the historical behaviour: prefer `application.process.binary`
/// (RawMicConsumer::bundle_id) and fall back to `application.name`
/// (`friendly_name`) if the binary lookup ever produced an empty key.
/// In practice the two are the same for non-empty rows; the wrapper
/// just keeps the same shape callers expect.
///
/// `observations` is `Option` so unit tests (and a degraded startup
/// where AutoRecorder failed to open the store) can pass `None` and
/// get the today's-no-silence behaviour. When `Some`, a row with
/// `mode = Never` causes the bundle to be filtered out before any
/// phase transition runs.
fn interesting_mic_consumers(
    consumers: &[RawMicConsumer],
    observations: Option<&AppObservations>,
) -> Vec<String> {
    // `raw_mic_consumers` already applies the blacklist + helper-proc
    // filters at the source (#604), so this wrapper is just dedup +
    // bundle_id-preferred display. Belt-and-braces re-check via
    // `is_blacklisted` in case a future refactor lets a row slip past.
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for c in consumers {
        let display = if c.bundle_id.is_empty() {
            c.friendly_name.clone()
        } else {
            c.bundle_id.clone()
        };
        if is_blacklisted(&display) {
            continue;
        }
        if let Some(store) = observations {
            // A missing row OR a sqlite error both fall through to
            // "not silenced" — the safest default: the user gets the
            // prompt and can decide. A new mic-using app the
            // observer hasn't catalogued yet hits the missing-row
            // path on every tick until `auto_recorder::on_started`
            // upserts it, which is correct: silence is opt-in.
            if store.is_silenced(&display).unwrap_or(false) {
                continue;
            }
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
    Start {
        consumer: &'a str,
    },
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
                    ActionSpec {
                        id: "record".into(),
                        label: "Record".into(),
                    },
                    ActionSpec {
                        id: "dismiss".into(),
                        label: "Not now".into(),
                    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_observations::{AppMode, AppObservations};
    use chrono::Utc;

    fn raw(bundle: &str) -> RawMicConsumer {
        RawMicConsumer::new(bundle, bundle)
    }

    #[test]
    fn interesting_mic_consumers_passes_through_without_a_store() {
        // None means "no AppObservations available" — the detector
        // degrades to today's behaviour (no per-app silencing). A
        // bundle named "discord" is NOT in the source-side blacklist,
        // so it survives the wrapper.
        let consumers = vec![raw("discord"), raw("zoom")];
        let out = interesting_mic_consumers(&consumers, None);
        assert_eq!(out, vec!["discord".to_string(), "zoom".to_string()]);
    }

    #[test]
    fn interesting_mic_consumers_filters_rows_with_mode_never() {
        // The whole point of #never-ask-app: a row with `mode=Never`
        // gets dropped at the source, before any Phase transition
        // runs. Other rows in the same tick are unaffected.
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("discord", "Discord", Utc::now()).unwrap();
        store.set_mode("discord", AppMode::Never).unwrap();
        store.upsert("zoom", "Zoom", Utc::now()).unwrap();

        let consumers = vec![raw("discord"), raw("zoom")];
        let out = interesting_mic_consumers(&consumers, Some(&store));
        assert_eq!(out, vec!["zoom".to_string()]);
    }

    #[test]
    fn interesting_mic_consumers_does_not_silence_ask_or_auto_rows() {
        // Regression check: only `Never` rows get dropped. An Ask row
        // is the default for newly-observed apps and must still show
        // up so the detector can prompt; an Auto row must show up so
        // it can fire.
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("zoom", "Zoom", Utc::now()).unwrap();
        store.set_mode("zoom", AppMode::Auto).unwrap();
        store.upsert("slack", "Slack", Utc::now()).unwrap();
        // slack stays at the default mode (Ask).

        let consumers = vec![raw("zoom"), raw("slack")];
        let out = interesting_mic_consumers(&consumers, Some(&store));
        assert!(out.contains(&"zoom".to_string()));
        assert!(out.contains(&"slack".to_string()));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn interesting_mic_consumers_treats_unobserved_rows_as_not_silenced() {
        // A brand-new bundle the observer hasn't catalogued yet shows
        // up in the tick BEFORE its on_started upsert lands. The
        // detector must still surface it (today's safe default —
        // user can always pick Never afterward).
        let store = AppObservations::open_in_memory().unwrap();
        let consumers = vec![raw("brand-new-app")];
        let out = interesting_mic_consumers(&consumers, Some(&store));
        assert_eq!(out, vec!["brand-new-app".to_string()]);
    }
}

