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

use crate::app_observations::AppObservations;
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

/// Matches `CARGO_PKG_NAME` — the binary name our recorder shows up as
/// in `pactl`. Used as the recursion-guard name in `on_event`.
fn own_bundle_id() -> &'static str {
    env!("CARGO_PKG_NAME")
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

        // Check master pref + per-row enable. Default OFF for both, so
        // a fresh install never fires.
        let cfg = crate::config::Config::load().ok();
        let start_on = cfg.as_ref().map(|c| c.auto_record_start_enabled).unwrap_or(false);
        if !start_on {
            return;
        }
        let row_enabled = self.inner.store.is_enabled(&ev.bundle_id).unwrap_or(false);
        if !row_enabled {
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
        let should_stop = {
            let mut a = self.inner.active_auto.lock().unwrap();
            match a.as_ref() {
                Some(active) if active.bundle_id == ev.bundle_id => {
                    *a = None;
                    true
                }
                _ => false,
            }
        };
        if should_stop {
            let rec = app.state::<Recorder>();
            if rec.is_active() {
                if let Err(e) = crate::do_stop(&rec, app) {
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
        match crate::do_start(&rec, app) {
            Ok(path) => {
                crate::write_session_source(
                    std::path::Path::new(&path),
                    "auto_app",
                    Some(&pending.friendly_name),
                );
                *self.inner.active_auto.lock().unwrap() = Some(ActiveAuto {
                    bundle_id: pending.bundle_id.clone(),
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
    fn own_bundle_id_matches_cargo_pkg_name() {
        // Defends against an accidental rename of the agent crate that
        // would silently disable our self-recursion guard.
        assert_eq!(own_bundle_id(), env!("CARGO_PKG_NAME"));
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
