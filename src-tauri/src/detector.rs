use std::collections::HashSet;
use std::time::{Duration, Instant};

use sysinfo::{ProcessesToUpdate, System};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::recorder::Recorder;

/// Case-insensitive substring matches against process names. Short patterns
/// catch variants: "teams" → teams-for-linux / msteams / Teams.exe / ms-teams.
/// Browser-based meetings are intentionally excluded (process-watching can't
/// distinguish an active meeting tab from an idle browser).
const MEETING_APP_PATTERNS: &[&str] = &[
    "zoom",
    "teams",
    "discord",
    "slack",
    "webex",
];

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long the mic must stay idle before we prompt the user to end.
const MIC_IDLE_BEFORE_END_PROMPT: Duration = Duration::from_secs(20);

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

#[derive(Debug)]
enum Phase {
    Idle,
    AwaitingStartConfirm { app: String },
    Recording { app: String, idle_since: Option<Instant> },
    AwaitingEndConfirm { app: String },
    Suppressed { app: String },
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AutoDetectEvent {
    PromptStart { app: String },
    PromptEnd { app: String },
    Cleared,
}

async fn run(app: AppHandle, mut rx: mpsc::UnboundedReceiver<UserDecision>) {
    let mut sys = System::new();
    let mut phase = Phase::Idle;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                phase = tick(&app, &mut sys, phase);
            }
            Some(decision) = rx.recv() => {
                phase = handle_decision(&app, phase, decision).await;
            }
        }
    }
}

fn tick(app: &AppHandle, sys: &mut System, phase: Phase) -> Phase {
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let apps: HashSet<String> = sys
        .processes()
        .values()
        .filter_map(|p| matched_app(&p.name().to_string_lossy()))
        .collect();
    let mic_busy = default_mic_is_active();
    let state = app.state::<Recorder>();
    let is_recording = state.is_active();

    // User changed recording state from outside (UI button, hotkey, tray).
    // Reconcile so we don't re-prompt / re-start against their will.
    let phase = reconcile_external(phase, is_recording, app);

    match phase {
        Phase::Idle => {
            if let Some(hit) = apps.iter().next() {
                if mic_busy {
                    let app_name = hit.clone();
                    eprintln!("callscribe: '{app_name}' + mic busy, prompting to start");
                    emit(app, AutoDetectEvent::PromptStart { app: app_name.clone() });
                    show_window(app);
                    return Phase::AwaitingStartConfirm { app: app_name };
                }
            }
            Phase::Idle
        }
        Phase::AwaitingStartConfirm { app: name } => {
            if !apps.contains(&name) {
                emit(app, AutoDetectEvent::Cleared);
                Phase::Idle
            } else {
                Phase::AwaitingStartConfirm { app: name }
            }
        }
        Phase::Recording { app: name, idle_since } => {
            if !apps.contains(&name) {
                // App exited — stop without asking.
                if is_recording {
                    let _ = crate::do_stop(&state, app);
                }
                emit(app, AutoDetectEvent::Cleared);
                return Phase::Idle;
            }
            if mic_busy {
                Phase::Recording { app: name, idle_since: None }
            } else {
                let since = idle_since.unwrap_or_else(Instant::now);
                if since.elapsed() >= MIC_IDLE_BEFORE_END_PROMPT {
                    eprintln!("callscribe: mic idle, prompting to end");
                    emit(app, AutoDetectEvent::PromptEnd { app: name.clone() });
                    show_window(app);
                    Phase::AwaitingEndConfirm { app: name }
                } else {
                    Phase::Recording { app: name, idle_since: Some(since) }
                }
            }
        }
        Phase::AwaitingEndConfirm { app: name } => {
            if !apps.contains(&name) {
                if is_recording {
                    let _ = crate::do_stop(&state, app);
                }
                emit(app, AutoDetectEvent::Cleared);
                Phase::Idle
            } else if mic_busy {
                // User rejoined; cancel the end prompt.
                emit(app, AutoDetectEvent::Cleared);
                Phase::Recording { app: name, idle_since: None }
            } else {
                Phase::AwaitingEndConfirm { app: name }
            }
        }
        Phase::Suppressed { app: name } => {
            if !apps.contains(&name) {
                Phase::Idle
            } else {
                Phase::Suppressed { app: name }
            }
        }
    }
}

async fn handle_decision(app: &AppHandle, phase: Phase, decision: UserDecision) -> Phase {
    let state = app.state::<Recorder>();
    match (phase, decision) {
        (Phase::AwaitingStartConfirm { app: name }, UserDecision::ConfirmStart) => {
            match crate::do_start(&state, app) {
                Ok(_) => {
                    emit(app, AutoDetectEvent::Cleared);
                    Phase::Recording { app: name, idle_since: None }
                }
                Err(e) => {
                    eprintln!("callscribe: auto-start failed: {e}");
                    emit(app, AutoDetectEvent::Cleared);
                    Phase::Idle
                }
            }
        }
        (Phase::AwaitingStartConfirm { app: name }, UserDecision::DismissStart) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Suppressed { app: name }
        }
        (Phase::AwaitingEndConfirm { app: _ }, UserDecision::ConfirmEnd) => {
            if state.is_active() {
                let _ = crate::do_stop(&state, app);
            }
            emit(app, AutoDetectEvent::Cleared);
            Phase::Idle
        }
        (Phase::AwaitingEndConfirm { app: name }, UserDecision::KeepRecording) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Recording { app: name, idle_since: None }
        }
        (phase, _) => phase,
    }
}

fn reconcile_external(phase: Phase, is_recording: bool, app: &AppHandle) -> Phase {
    match (phase, is_recording) {
        // We believed we were recording, but the user stopped. Back off until
        // the meeting app exits.
        (Phase::Recording { app: name, .. }, false) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Suppressed { app: name }
        }
        (Phase::AwaitingEndConfirm { app: name }, false) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Suppressed { app: name }
        }
        // We were prompting to start, but the user hit Start via the UI first.
        (Phase::AwaitingStartConfirm { app: name }, true) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Recording { app: name, idle_since: None }
        }
        (other, _) => other,
    }
}

fn matched_app(process_name: &str) -> Option<String> {
    let lower = process_name.to_lowercase();
    for pat in MEETING_APP_PATTERNS {
        if lower.contains(pat) {
            return Some(process_name.to_string());
        }
    }
    None
}

/// True if a non-monitor audio source is currently being consumed — i.e. some
/// app has the mic actively open (not just holding a handle). On PulseAudio /
/// PipeWire, a source flips to RUNNING only while data is flowing.
#[cfg(target_os = "linux")]
fn default_mic_is_active() -> bool {
    let output = std::process::Command::new("pactl")
        .arg("list")
        .arg("sources")
        .arg("short")
        .output();
    let Ok(output) = output else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .any(|line| line.contains("RUNNING") && !line.contains(".monitor"))
}

#[cfg(not(target_os = "linux"))]
fn default_mic_is_active() -> bool {
    // TODO(windows): WASAPI IAudioSessionManager2 for active capture streams.
    true
}

fn emit(app: &AppHandle, event: AutoDetectEvent) {
    let _ = app.emit("auto-detect", event);
}

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}
