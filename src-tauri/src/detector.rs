use std::collections::HashSet;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

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
];

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long the mic consumer must be gone before we prompt to end.
const CONSUMER_GONE_BEFORE_END_PROMPT: Duration = Duration::from_secs(20);

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
    AwaitingEndConfirm { consumer: String },
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
    let consumers = interesting_mic_consumers();
    let state = app.state::<Recorder>();
    let is_recording = state.is_active();

    let phase = reconcile_external(phase, is_recording, app);

    match phase {
        Phase::Idle => {
            if let Some(consumer) = consumers.iter().next() {
                eprintln!("aftercalls: '{consumer}' is using the mic — prompting");
                emit(app, AutoDetectEvent::PromptStart { app: consumer.clone() });
                show_window(app);
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
                    show_window(app);
                    Phase::AwaitingEndConfirm { consumer }
                } else {
                    Phase::Recording { consumer, gone_since: Some(since) }
                }
            }
        }
        Phase::AwaitingEndConfirm { consumer } => {
            if consumers.contains(&consumer) {
                // Consumer came back — user rejoined. Cancel the end prompt.
                emit(app, AutoDetectEvent::Cleared);
                Phase::Recording { consumer, gone_since: None }
            } else {
                Phase::AwaitingEndConfirm { consumer }
            }
        }
        Phase::Suppressed { consumer } => {
            // Release suppression once the old consumer is gone.
            let still_suppressing = consumers.contains(&consumer);
            // If a different consumer is now on the mic, prompt for it.
            if let Some(other) = consumers.iter().find(|c| **c != consumer) {
                eprintln!("aftercalls: new mic consumer '{other}' — prompting");
                emit(app, AutoDetectEvent::PromptStart { app: other.clone() });
                show_window(app);
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
        (Phase::AwaitingEndConfirm { consumer }, false) => {
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
                Ok(_) => {
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
        (Phase::AwaitingEndConfirm { consumer }, UserDecision::KeepRecording) => {
            emit(app, AutoDetectEvent::Cleared);
            Phase::Recording { consumer, gone_since: None }
        }
        (phase, _) => phase,
    }
}

/// Apps currently holding a source-output on *any* mic source, filtered
/// against the blacklist. Deduplicated so a multi-stream app (e.g. WebRTC)
/// only shows up once.
fn interesting_mic_consumers() -> Vec<String> {
    let names = raw_mic_consumers();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for name in names {
        let lower = name.to_lowercase();
        if MIC_CONSUMER_BLACKLIST
            .iter()
            .any(|b| lower.contains(&b.to_lowercase()))
        {
            continue;
        }
        if seen.insert(name.clone()) {
            result.push(name);
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn raw_mic_consumers() -> Vec<String> {
    let output = std::process::Command::new("pactl")
        .arg("list")
        .arg("source-outputs")
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let own_pid = std::process::id().to_string();

    let mut result = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_binary: Option<String> = None;
    let mut cur_pid: Option<String> = None;

    let flush = |result: &mut Vec<String>,
                 name: &mut Option<String>,
                 binary: &mut Option<String>,
                 pid: &mut Option<String>,
                 own_pid: &str| {
        let is_ours = pid.as_deref() == Some(own_pid);
        if !is_ours {
            if let Some(app) = binary.take().or_else(|| name.take()) {
                result.push(app);
            }
        }
        *name = None;
        *binary = None;
        *pid = None;
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Source Output #") {
            flush(&mut result, &mut cur_name, &mut cur_binary, &mut cur_pid, &own_pid);
        } else if let Some(rest) = trimmed.strip_prefix("application.name = \"") {
            if let Some(end) = rest.find('"') {
                cur_name = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("application.process.binary = \"") {
            if let Some(end) = rest.find('"') {
                cur_binary = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("application.process.id = \"") {
            if let Some(end) = rest.find('"') {
                cur_pid = Some(rest[..end].to_string());
            }
        }
    }
    flush(&mut result, &mut cur_name, &mut cur_binary, &mut cur_pid, &own_pid);
    result
}

#[cfg(not(target_os = "linux"))]
fn raw_mic_consumers() -> Vec<String> {
    // TODO(windows): enumerate active WASAPI capture sessions and map each
    // session's ProcessId to its executable name.
    Vec::new()
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
