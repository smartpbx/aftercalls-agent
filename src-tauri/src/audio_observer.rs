//! Edge-triggered observer for "which apps are currently capturing the
//! microphone". Drives the auto-record state machine in
//! `auto_recorder.rs` (#596).
//!
//! Linux: piggy-backs on the existing `pactl list source-outputs` poll
//! (the same enumeration the call-detect prompt uses) via the shared
//! `mic_consumers` module. Polls every 5s on its own tick — one extra
//! `pactl` invocation per cycle is well below the noise floor for
//! typical desktop CPU load, and it lets the observer compute its
//! transition diff independently of the prompt detector's phase
//! machine. (The detector's poll cadence is matched but not coupled —
//! a future broadcast-channel refactor can collapse the two into one
//! invocation if pactl ever becomes a hot spot.)
//!
//! macOS / Windows v1: stub `UnsupportedObserver`. `is_supported()`
//! returns false so the Settings UI renders the "App detection isn't
//! supported on this OS yet" banner; no `pactl` / WASAPI work happens.
//!
//! Public-copy posture: we never name PipeWire / Pulse / WASAPI in any
//! string the webview can render. The IPC layer just ships
//! `bundle_id` / `friendly_name` strings.

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::mic_consumers::{raw_mic_consumers, RawMicConsumer};

/// Edge-trigger event surfaced to the auto-recorder. Fires only on a
/// transition between consecutive ticks — apps that were already
/// capturing when the observer first wakes do NOT fire (matches Q4 of
/// the architect's plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McaEvent {
    pub bundle_id: String,
    pub friendly_name: String,
    pub kind: McaEventKind,
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
pub fn is_supported() -> bool {
    cfg!(target_os = "linux") || cfg!(target_os = "windows")
}

/// 5s tick — matches the call-detect prompt's existing cadence and is
/// good enough for the 1s "user perception" target the spec asks for.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn the platform-specific observer. Returns a JoinHandle the
/// caller may await on shutdown; in practice the observer is
/// long-lived for the agent's lifetime so the handle is dropped.
///
/// Events flow over `tx`. The receiver end belongs to
/// `auto_recorder::run` which fans them out to its decision loop.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn spawn(tx: mpsc::UnboundedSender<McaEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut active: HashSet<RawMicConsumer> = HashSet::new();
        loop {
            let next: HashSet<RawMicConsumer> = raw_mic_consumers().into_iter().collect();
            for ev in diff(&active, &next) {
                if tx.send(ev).is_err() {
                    return; // receiver dropped — bail
                }
            }
            active = next;
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
}

/// macOS + other unsupported targets compile to a no-op spawner — the
/// caller still gets a JoinHandle so its setup() code stays uniform,
/// but the task immediately drops the channel and exits.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn spawn(tx: mpsc::UnboundedSender<McaEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Drop tx to release the receiver immediately; the auto-recorder's
        // `recv()` loop turns into a no-op without any further work.
        drop(tx);
    })
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
