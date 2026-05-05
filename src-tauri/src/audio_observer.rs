//! Edge-triggered observer for "which apps are currently capturing the
//! microphone". Drives the auto-record state machine in
//! `auto_recorder.rs` (#596).
//!
//! Linux: piggy-backs on the existing `pactl list source-outputs` poll
//! (the same enumeration the call-detect prompt uses) via the shared
//! `mic_consumers` ticker. The detector and observer subscribe to one
//! 5s snapshot stream, so there is one `pactl` process per cycle while
//! each consumer keeps its own state machine.
//!
//! Windows: WASAPI session enumeration via `IAudioSessionManager2` /
//! `IAudioSessionControl2`. See `mic_consumers::raw_mic_consumers`
//! under `cfg(target_os = "windows")`. Same 5s snapshot cadence,
//! same observer/detector fan-out. Untested on real hardware as of
//! this writing — tracking in #598.
//!
//! macOS 14.2+: CoreAudio HAL via `kAudioHardwarePropertyProcessObjectList`
//! (process-list API, requires macOS 14.2). Same 5s snapshot cadence
//! as Linux/Windows — see `mic_consumers::raw_mic_consumers` under
//! `cfg(target_os = "macos")`. Older macOS reports
//! `is_supported() == false` and the Settings UI shows the same
//! "App detection isn't supported on this OS yet" banner. Tracked in #597.
//!
//! Public-copy posture: we never name PipeWire / Pulse / WASAPI in any
//! string the webview can render. The IPC layer just ships
//! `bundle_id` / `friendly_name` strings.
//!
//! ## Initial-tick semantics (read this before changing the spawn loop)
//!
//! Two competing requirements:
//! - **Catalog discovery** wants to surface apps that were already
//!   running when the agent / observer woke up, so the user can pick
//!   them in the auto-record list. Without this, a softphone that
//!   stays open 24/7 (e.g. Cliq) never reaches the catalog and can't
//!   be whitelisted (#604).
//! - **Trigger semantics** must NOT auto-start a recording for an app
//!   that was already running at observer-wake — that'd be a surprise
//!   recording every time the user opens the agent. Auto-record fires
//!   on a real "user just engaged the mic" transition, not on a
//!   process that's been running since boot.
//!
//! Resolution: the spawn loop seeds `active` with the current set
//! BEFORE the diff loop, and emits a synthetic `Started` event with
//! `initial = true` for each. `auto_recorder::on_started` always
//! upserts into `observed_apps` (catalog), then short-circuits before
//! the trigger logic when `initial` is true. Subsequent ticks emit
//! transition-only `Started` / `Stopped` with `initial = false`.

use std::collections::HashSet;
use tokio::sync::mpsc;

use crate::mic_consumers::{subscribe_mic_consumers, RawMicConsumer};

/// Event surfaced to the auto-recorder. Two flavours:
///   - Transition events fire when an app appears/disappears between
///     consecutive ticks (`initial = false`).
///   - Initial-batch events fire once on observer wake for every app
///     already capturing the mic at that moment (`initial = true`).
///     The auto-recorder uses this to populate the catalog without
///     firing the auto-record trigger — see the module docstring's
///     "Initial-tick semantics" section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McaEvent {
    pub bundle_id: String,
    pub friendly_name: String,
    pub kind: McaEventKind,
    /// True for the synthetic batch emitted on observer wake (catalog-
    /// only). False for real idle→active / active→idle transitions.
    pub initial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McaEventKind {
    /// Idle → capturing transition.
    Started,
    /// Capturing → idle transition.
    Stopped,
}

/// Whether the host OS has a working mic-consumer enumerator. The
/// Settings UI reads this to decide between rendering the per-app list
/// vs the "not supported on this OS yet" banner.
///
/// macOS is conditionally supported: the `cfg!` arm is true on every
/// macOS host (the agent ships with a 13.0 minimum-system-version),
/// but the runtime helper additionally checks for the macOS 14.2
/// process-list API. macOS 13.0–14.1 hosts evaluate to `false` here
/// and get the same "not supported" banner Linux/Windows users with
/// old hosts would hit.
pub fn is_supported() -> bool {
    cfg!(target_os = "linux")
        || cfg!(target_os = "windows")
        || (cfg!(target_os = "macos") && macos_observer_available())
}

#[cfg(target_os = "macos")]
fn macos_observer_available() -> bool {
    // Run-once cached probe — see `mic_consumers` for the
    // `sysctlbyname("kern.osproductversion")` implementation.
    crate::mic_consumers::macos_kernel_supports_process_list()
}

#[cfg(not(target_os = "macos"))]
fn macos_observer_available() -> bool {
    false
}

/// Spawn the platform-specific observer. Long-lived for the agent's
/// lifetime; the JoinHandle is intentionally not surfaced (callers
/// have nothing to await on shutdown).
///
/// Events flow over `tx`. The receiver end belongs to
/// `auto_recorder::run` which fans them out to its decision loop.
///
/// Uses `tauri::async_runtime::spawn` — the `setup` callback runs
/// before any Tokio runtime is in scope on the calling thread, so a
/// raw `tokio::spawn` here panics with "no reactor running" the moment
/// the function is called. Every other long-lived task in the agent
/// (detector, auto_recorder, updater poll) goes through the same
/// helper for the same reason.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn spawn(tx: mpsc::UnboundedSender<McaEvent>) {
    tauri::async_runtime::spawn(async move {
        let mut consumers_rx = subscribe_mic_consumers();
        // Seed `active` with the current set BEFORE the diff loop so
        // we don't auto-trigger a recording for apps that were already
        // running at observer-wake. Emit a catalog-only `initial = true`
        // batch for each so the auto-record list still discovers them.
        // See module docstring §"Initial-tick semantics" for why both
        // halves are needed.
        let initial_active: HashSet<RawMicConsumer> =
            consumers_rx.borrow().iter().cloned().collect();
        for c in &initial_active {
            let ev = McaEvent {
                bundle_id: c.bundle_id.clone(),
                friendly_name: c.friendly_name.clone(),
                kind: McaEventKind::Started,
                initial: true,
            };
            if tx.send(ev).is_err() {
                return; // receiver dropped — bail
            }
        }
        let mut active = initial_active;
        loop {
            if consumers_rx.changed().await.is_err() {
                return;
            }
            let next: HashSet<RawMicConsumer> = consumers_rx.borrow().iter().cloned().collect();
            for ev in diff(&active, &next) {
                if tx.send(ev).is_err() {
                    return; // receiver dropped — bail
                }
            }
            active = next;
        }
    });
}

/// Other unsupported targets compile to a no-op spawner — the
/// caller's setup() code stays uniform, but the spawned task
/// immediately drops the channel and exits.
///
/// macOS is now in the supported arm above; the runtime helper inside
/// `mic_consumers::raw_mic_consumers` short-circuits to an empty Vec
/// on pre-14.2 hosts, so the spawn loop runs but never emits events.
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn spawn(tx: mpsc::UnboundedSender<McaEvent>) {
    tauri::async_runtime::spawn(async move {
        // Drop tx to release the receiver immediately; the auto-recorder's
        // `recv()` loop turns into a no-op without any further work.
        drop(tx);
    });
}

/// Pure transition-diff. Public so the unit tests in `tests` (and the
/// auto_recorder unit tests, indirectly) can drive it from synthetic
/// inputs.
pub fn diff(prev: &HashSet<RawMicConsumer>, next: &HashSet<RawMicConsumer>) -> Vec<McaEvent> {
    let mut events = Vec::new();
    let prev_keys: HashSet<&str> = prev.iter().map(|c| c.bundle_id.as_str()).collect();
    let next_keys: HashSet<&str> = next.iter().map(|c| c.bundle_id.as_str()).collect();
    // Started: in next but not prev.
    for c in next {
        if !prev_keys.contains(c.bundle_id.as_str()) {
            events.push(McaEvent {
                bundle_id: c.bundle_id.clone(),
                friendly_name: c.friendly_name.clone(),
                kind: McaEventKind::Started,
                initial: false,
            });
        }
    }
    // Stopped: in prev but not next.
    for c in prev {
        if !next_keys.contains(c.bundle_id.as_str()) {
            events.push(McaEvent {
                bundle_id: c.bundle_id.clone(),
                friendly_name: c.friendly_name.clone(),
                kind: McaEventKind::Stopped,
                initial: false,
            });
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(bundle_id: &str, friendly: &str) -> RawMicConsumer {
        RawMicConsumer::new(bundle_id, friendly)
    }

    fn set(items: &[(&str, &str)]) -> HashSet<RawMicConsumer> {
        items.iter().map(|(b, f)| rc(b, f)).collect()
    }

    #[test]
    fn empty_to_zoom_emits_started() {
        let events = diff(&set(&[]), &set(&[("zoom", "Zoom")]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bundle_id, "zoom");
        assert_eq!(events[0].kind, McaEventKind::Started);
        // Real transition events MUST be `initial: false` so the
        // auto-recorder's trigger logic runs (the initial-batch
        // short-circuit only suppresses observer-wake events).
        assert!(!events[0].initial);
    }

    #[test]
    fn no_change_emits_nothing() {
        let s = set(&[("zoom", "Zoom")]);
        assert!(diff(&s, &s).is_empty());
    }

    #[test]
    fn add_slack_to_active_emits_started_for_slack() {
        let prev = set(&[("zoom", "Zoom")]);
        let next = set(&[("zoom", "Zoom"), ("slack", "Slack")]);
        let events = diff(&prev, &next);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bundle_id, "slack");
        assert_eq!(events[0].kind, McaEventKind::Started);
    }

    #[test]
    fn remove_slack_emits_stopped() {
        let prev = set(&[("zoom", "Zoom"), ("slack", "Slack")]);
        let next = set(&[("zoom", "Zoom")]);
        let events = diff(&prev, &next);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bundle_id, "slack");
        assert_eq!(events[0].kind, McaEventKind::Stopped);
    }

    #[test]
    fn drain_to_empty_emits_stopped() {
        let prev = set(&[("zoom", "Zoom")]);
        let next = set(&[]);
        let events = diff(&prev, &next);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bundle_id, "zoom");
        assert_eq!(events[0].kind, McaEventKind::Stopped);
    }

    #[test]
    fn friendly_name_change_alone_emits_nothing() {
        // The diff is keyed on bundle_id, so an Electron app whose
        // application.name string improved between ticks should NOT
        // flicker as Stopped + Started — that would re-fire the toast.
        let prev = set(&[("firefox", "Firefox")]);
        let next = set(&[("firefox", "Firefox: Zoom")]);
        let events = diff(&prev, &next);
        assert!(events.is_empty());
    }

    #[test]
    fn simultaneous_start_and_stop() {
        let prev = set(&[("zoom", "Zoom")]);
        let next = set(&[("slack", "Slack")]);
        let events = diff(&prev, &next);
        assert_eq!(events.len(), 2);
        // No ordering guarantee across HashSet iteration; check by kind.
        let started: Vec<_> = events
            .iter()
            .filter(|e| e.kind == McaEventKind::Started)
            .map(|e| e.bundle_id.as_str())
            .collect();
        let stopped: Vec<_> = events
            .iter()
            .filter(|e| e.kind == McaEventKind::Stopped)
            .map(|e| e.bundle_id.as_str())
            .collect();
        assert_eq!(started, vec!["slack"]);
        assert_eq!(stopped, vec!["zoom"]);
    }
}
