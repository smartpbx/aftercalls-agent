//! Auto-record state machine (#596).
//!
//! Owns the bridge between `audio_observer::McaEvent`s and the
//! existing `do_start` / `do_stop` recording entry points. Drops a
//! 5-second cancel toast when an enabled app's mic transition fires,
//! lands the recording (with the same audio source as a manual press),
//! and auto-stops on the matching `Stopped` event when auto-stop is
//! also on.
//!
//! Defence-in-depth around recursion (per Q1 of the architect's plan):
//!   1. `mic_consumers` filters our own PID upstream.
//!   2. The `Detector`'s `MIC_CONSUMER_BLACKLIST` (also imported here)
//!      filters our binary name.
//!   3. `auto_recorder::on_event` short-circuits if `bundle_id` matches
//!      `own_bundle_id()`.
//!   4. While `Recorder::is_active()` is true we never spawn a new
//!      pending — even a manual recording suppresses auto-start until
//!      the user stops.
//!
//! Public-copy posture: the toast string the layout listener renders
//! uses the friendly_name we ship over IPC; we never embed
//! "PipeWire" / "PulseAudio" / "WASAPI" anywhere a user could see.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::app_observations::{AppMode, AppObservations};
use crate::audio_observer::{McaEvent, McaEventKind};
use crate::recorder::Recorder;

/// User-visible cancel window. Matches the issue spec — the user has
/// 5s after the toast fires to click Cancel before `do_start` runs.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Throttle window for the `observed-apps-updated` event. A chatty mic
/// consumer (one that flips on/off rapidly) shouldn't be allowed to
/// hammer the IPC bus; the layout listener already debounces a refetch
/// internally, but capping the emit-rate here is a cheap belt.
const OBSERVED_UPDATED_THROTTLE: Duration = Duration::from_secs(2);

/// The bundled binary basename, NOT `CARGO_PKG_NAME`. The crate is
/// named `agent` (codepath neutrality) but the bundled binary on every
/// supported OS is `aftercalls` — `pactl` (Linux) and `process_exe_basename`
/// (Windows) report this name. Using `CARGO_PKG_NAME` here silently
/// disabled the recursion guard for two releases (#604) because every
/// `bundle_id == "agent"` check missed the real binary `aftercalls`.
///
/// If the bundle ever gets renamed — change `productName` in
/// `tauri.conf.json` — update this const in lockstep AND keep the
/// blacklist entry in `mic_consumers::MIC_CONSUMER_BLACKLIST` aligned
/// (the two are belt-and-braces against the same recursion failure).
const OWN_BUNDLE_ID: &str = "aftercalls";

fn own_bundle_id() -> &'static str {
    OWN_BUNDLE_ID
}

/// In-flight pending start. Lives behind a `Mutex<Option<…>>` on the
/// auto-recorder state so the cancel-IPC and the grace-tick can both
/// reach it.
#[derive(Debug, Clone)]
struct Pending {
    pending_id: String,
    bundle_id: String,
    friendly_name: String,
    deadline: Instant,
}

/// "We auto-started a recording for `bundle_id`; auto-stop should fire
/// when the matching Stopped event lands." Cleared on manual stop or
/// pipeline completion.
#[derive(Debug, Clone)]
struct ActiveAuto {
    bundle_id: String,
    session_dir: std::path::PathBuf,
}

/// Cloneable handle the IPC commands clutch. Internally an Arc around
/// the shared state; tauri `manage()`s the handle so the command fns
/// can pull it via `State<AutoRecorder>`.
#[derive(Clone)]
pub struct AutoRecorder {
    inner: Arc<Inner>,
}

struct Inner {
    store: AppObservations,
    pending: Mutex<Option<Pending>>,
    active_auto: Mutex<Option<ActiveAuto>>,
    last_observed_emit: Mutex<Option<Instant>>,
}

impl AutoRecorder {
    /// Open the on-disk store and return a handle. Errors propagate so
    /// `setup()` can log and continue without auto-record (the rest of
    /// the agent stays functional).
    pub fn open() -> anyhow::Result<Self> {
        let path = crate::app_observations::agent_db_path()?;
        let store = AppObservations::open(path)?;
        // Sweep stale blacklisted rows on every startup. Users who
        // upgraded from v0.14.0–v0.14.2 carried `aftercalls` / `parec` /
        // `Chromium input` rows in their store from before the source-
        // side filter landed (#604). Forgetting them via the UI used
        // to require an OS-native confirm that doesn't fire on
        // wlroots-based Wayland (#605); auto-purging at boot lets the
        // user open Settings and see a clean list without manual
        // intervention. Cheap (single SELECT + tiny DELETE loop) and
        // idempotent — harmless on a fresh install.
        match store.purge_blacklisted_rows() {
            Ok(0) => {}
            Ok(n) => eprintln!("aftercalls: purged {n} stale blacklisted observed_apps row(s)"),
            Err(e) => eprintln!("aftercalls: purge_blacklisted_rows failed: {e}"),
        }
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                pending: Mutex::new(None),
                active_auto: Mutex::new(None),
                last_observed_emit: Mutex::new(None),
            }),
        })
    }

    pub fn store(&self) -> &AppObservations {
        &self.inner.store
    }

    /// Wire the observer's event channel into the state machine. Spawn
    /// once at startup; the task lives for the agent's lifetime.
    pub fn run(&self, app: AppHandle, mut rx: mpsc::UnboundedReceiver<McaEvent>) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = rx.recv().await {
                me.on_event(&app, ev).await;
            }
        });
    }

    async fn on_event(&self, app: &AppHandle, ev: McaEvent) {
        // Recursion guard: never auto-record ourselves. Three independent
        // filters already block this upstream (the mic_consumers PID
        // filter, the detector blacklist, and the recorder's
        // `is_active`), but a literal name compare here is a cheap
        // belt + suspenders.
        if ev.bundle_id == own_bundle_id() {
            return;
        }

        match ev.kind {
            McaEventKind::Started => self.on_started(app, ev).await,
            McaEventKind::Stopped => self.on_stopped(app, ev).await,
        }
    }

    async fn on_started(&self, app: &AppHandle, ev: McaEvent) {
        // Always remember we saw it — even when auto-record is fully off.
        // Discoverability of the per-app list depends on the user being
        // able to see what was on the mic without having to first flip
        // the master toggle.
        let now = chrono::Utc::now();
        let inserted = match self.inner.store.upsert(&ev.bundle_id, &ev.friendly_name, now) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "aftercalls: auto_recorder upsert failed for {}: {e}",
                    ev.bundle_id
                );
                return;
            }
        };
        self.maybe_emit_observed_updated(app, inserted);

        // Initial-batch events populate the catalog only — never fire
        // the trigger. See `audio_observer.rs` §"Initial-tick semantics":
        // a softphone running since boot must not trigger an auto-
        // recording the moment the user opens the agent.
        if ev.initial {
            return;
        }

        // Check master pref + per-row enable. Default OFF for both, so
        // a fresh install never fires.
        let cfg = crate::config::Config::load().ok();
        let start_on = cfg.as_ref().map(|c| c.auto_record_start_enabled).unwrap_or(false);
        if !start_on {
            return;
        }
        // Per-row gate (#never-ask-app): only `mode == Auto` rows
        // arm a pending start. `Ask` and `Never` short-circuit the
        // same way an unticked row used to. A `Never` row reaching
        // this point is theoretically impossible — the detector's
        // `interesting_mic_consumers` filter drops silenced apps
        // before any phase transition runs — but the audio observer
        // feeds a SEPARATE event stream into auto_recorder, so the
        // belt-and-braces check here keeps the two filters honest.
        // A missing row (None) maps to Ask, matching today's safest-
        // default behaviour for an unobserved app.
        let row_mode = self
            .inner
            .store
            .mode_of(&ev.bundle_id)
            .unwrap_or(None)
            .unwrap_or(AppMode::Ask);
        if !matches!(row_mode, AppMode::Auto) {
            return;
        }
        // Already recording (manual OR auto) — never stack a second
        // start. Auto-stop logic will only fire for the bundle that
        // auto-started, so a manual session isn't disturbed.
        if app.state::<Recorder>().is_active() {
            return;
        }
        // Already a pending in flight — drop the second one. It'll be
        // re-picked up on the next idle→capturing edge.
        {
            let p = self.inner.pending.lock().unwrap();
            if p.is_some() {
                return;
            }
        }
        let pending_id = generate_pending_id();
        let pending = Pending {
            pending_id: pending_id.clone(),
            bundle_id: ev.bundle_id.clone(),
            friendly_name: ev.friendly_name.clone(),
            deadline: Instant::now() + CANCEL_GRACE,
        };
        *self.inner.pending.lock().unwrap() = Some(pending.clone());

        let _ = app.emit(
            "auto-record-pending",
            &PendingEvent {
                pending_id: pending_id.clone(),
                bundle_id: ev.bundle_id.clone(),
                friendly_name: ev.friendly_name.clone(),
            },
        );

        // Spawn the grace timer. Re-checks the pending slot on wakeup
        // — if it was wiped (cancel arrived, or stop event for the same
        // bundle), the start is silently skipped.
        let me = self.clone();
        let app_for_task = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(CANCEL_GRACE).await;
            me.fire_pending_if_still_armed(&app_for_task, pending_id).await;
        });
    }

    async fn on_stopped(&self, app: &AppHandle, ev: McaEvent) {
        // If a Stopped lands during the grace window for the same
        // bundle, treat that as a cancel. The user opened Zoom, then
        // closed it before the 5s elapsed — auto-recording an already-
        // gone app would be a privacy-noisy false start.
        let abort = {
            let mut p = self.inner.pending.lock().unwrap();
            if let Some(pending) = p.as_ref() {
                if pending.bundle_id == ev.bundle_id {
                    let id = pending.pending_id.clone();
                    *p = None;
                    Some(id)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(_id) = abort {
            let _ = app.emit(
                "auto-record-cancelled",
                &CancelEvent {
                    bundle_id: ev.bundle_id.clone(),
                    reason: "app_stopped",
                },
            );
            return;
        }

        // Auto-stop branch. Only fires when:
        //   • master `auto_record_stop_enabled` is on
        //   • the active recording WAS auto-started (active_auto present)
        //   • the bundle matches the one we auto-started
        let cfg = crate::config::Config::load().ok();
        let stop_on = cfg.as_ref().map(|c| c.auto_record_stop_enabled).unwrap_or(false);
        if !stop_on {
            return;
        }
        let stop_session = {
            let mut a = self.inner.active_auto.lock().unwrap();
            match a.as_ref() {
                Some(active) if active.bundle_id == ev.bundle_id => {
                    let session_dir = active.session_dir.clone();
                    *a = None;
                    Some(session_dir)
                }
                _ => None,
            }
        };
        if let Some(session_dir) = stop_session {
            let rec = app.state::<Recorder>();
            if rec.is_active() {
                if let Err(e) = crate::do_stop_session(&rec, app, &session_dir) {
                    eprintln!("aftercalls: auto-stop failed: {e}");
                }
            }
        }
    }

    /// Frontend Cancel-button click. Wipes the in-flight pending if it
    /// still matches the id we handed out; emits a `cancelled` event
    /// so the toast dismisses cleanly.
    pub fn cancel_pending(&self, app: &AppHandle, pending_id: &str) {
        let info = {
            let mut p = self.inner.pending.lock().unwrap();
            if let Some(pending) = p.as_ref() {
                if pending.pending_id == pending_id {
                    let bundle = pending.bundle_id.clone();
                    *p = None;
                    Some(bundle)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(bundle_id) = info {
            let _ = app.emit(
                "auto-record-cancelled",
                &CancelEvent {
                    bundle_id,
                    reason: "user",
                },
            );
        }
    }

    async fn fire_pending_if_still_armed(&self, app: &AppHandle, pending_id: String) {
        // Re-check the slot under lock; a cancel or matching Stopped
        // event during the grace will have wiped it.
        let pending = {
            let mut p = self.inner.pending.lock().unwrap();
            match p.as_ref() {
                Some(pending) if pending.pending_id == pending_id => {
                    let cloned = pending.clone();
                    *p = None;
                    Some(cloned)
                }
                _ => None,
            }
        };
        let Some(pending) = pending else {
            return;
        };
        // The grace might have been longer than expected (a paused
        // tokio runtime, sleep mode, etc.); bail if we're already
        // recording, to avoid stacking on top of a manual session.
        let rec = app.state::<Recorder>();
        if rec.is_active() {
            let _ = app.emit(
                "auto-record-cancelled",
                &CancelEvent {
                    bundle_id: pending.bundle_id.clone(),
                    reason: "error",
                },
            );
            return;
        }
        // Auto-app-triggered start has no co-pilot picker context → no hint.
        match crate::do_start(
            &rec,
            app,
            None,
            "auto_app",
            Some(&pending.friendly_name),
        ) {
            Ok(path) => {
                *self.inner.active_auto.lock().unwrap() = Some(ActiveAuto {
                    bundle_id: pending.bundle_id.clone(),
                    session_dir: std::path::PathBuf::from(path),
                });
                let _ = app.emit(
                    "auto-record-fired",
                    &FiredEvent {
                        bundle_id: pending.bundle_id.clone(),
                    },
                );
                // Squelch deadline-warning warnings about unused field
                let _ = pending.deadline;
            }
            Err(e) => {
                eprintln!("aftercalls: auto-record do_start failed: {e}");
                let _ = app.emit(
                    "auto-record-cancelled",
                    &CancelEvent {
                        bundle_id: pending.bundle_id.clone(),
                        reason: "error",
                    },
                );
            }
        }
    }

    fn maybe_emit_observed_updated(&self, app: &AppHandle, inserted: bool) {
        // Always emit on a fresh insert (the Settings list grew); for a
        // touch-update, throttle so a chatty consumer doesn't spam.
        let now = Instant::now();
        let mut last = self.inner.last_observed_emit.lock().unwrap();
        let should_emit = if inserted {
            true
        } else {
            match *last {
                Some(t) if now.duration_since(t) < OBSERVED_UPDATED_THROTTLE => false,
                _ => true,
            }
        };
        if should_emit {
            *last = Some(now);
            drop(last);
            let _ = app.emit("observed-apps-updated", &EmptyPayload {});
        }
    }
}

// ── Event payloads (serialize as JSON to the webview) ─────────────────

#[derive(Serialize)]
struct EmptyPayload {}

#[derive(Serialize)]
struct PendingEvent {
    pending_id: String,
    bundle_id: String,
    friendly_name: String,
}

#[derive(Serialize)]
struct FiredEvent {
    bundle_id: String,
}

#[derive(Serialize)]
struct CancelEvent {
    bundle_id: String,
    reason: &'static str,
}

/// 16-char alphanumeric token. Hand-rolled to avoid pulling in `nanoid`
/// for a single random-id call site; the existing `rand` transitive
/// (via reqwest's deps) already supplies the alphabet.
fn generate_pending_id() -> String {
    use rand::distributions::{Alphanumeric, DistString};
    Alphanumeric.sample_string(&mut rand::thread_rng(), 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn own_bundle_id_is_the_bundled_binary_basename() {
        // Defends against #604 regressing: the prior version of this
        // test asserted equality with `CARGO_PKG_NAME` ("agent"), but
        // the bundled binary on every supported OS is "aftercalls" and
        // pactl / WASAPI report THAT name. The recursion guard checks
        // `ev.bundle_id == own_bundle_id()`, so getting this wrong
        // means the agent's own cpal mic capture leaks into the
        // observed-apps list and (if a user toggled it on) recording
        // would loop into itself.
        assert_eq!(own_bundle_id(), "aftercalls");
        assert_ne!(own_bundle_id(), env!("CARGO_PKG_NAME"));
    }

    #[test]
    fn own_bundle_id_is_in_mic_consumer_blacklist() {
        // Belt-and-braces: even if the recursion guard fails, the
        // blacklist filter inside `mic_consumers::raw_mic_consumers`
        // should drop our binary at the source. Keep the two aligned —
        // a rename touching only one is a #604-class regression.
        assert!(crate::mic_consumers::is_blacklisted(own_bundle_id()));
    }

    #[test]
    fn generated_pending_ids_are_unique_and_ascii() {
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..256 {
            let id = generate_pending_id();
            assert_eq!(id.len(), 16);
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
            assert!(seen.insert(id), "pending_id collision on a 16-char alphanum");
        }
    }
}
