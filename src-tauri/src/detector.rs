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
    "aftercalls",             // our own cpal mic consumer shows up as "pipewire alsa [aftercalls]"
];

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long the mic consumer must be gone before we prompt to end.
const CONSUMER_GONE_BEFORE_END_PROMPT: Duration = Duration::from_secs(5);
/// Safety net for a crashed / force-quit softphone (#74). If the
/// consumer has been gone this long and the user hasn't answered the
/// end-prompt, the recording is force-stopped via the same code path
/// as manual Stop. Six consecutive polls at POLL_INTERVAL=5s.
const CONSUMER_GONE_FORCE_STOP: Duration = Duration::from_secs(30);

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
    /// `gone_since` is inherited from Recording so the 30s force-stop
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
    let auto_detect_on = crate::config::Config::load()
        .map(|c| c.auto_detect)
        .unwrap_or(true);
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

#[cfg(target_os = "windows")]
fn raw_mic_consumers() -> Vec<String> {
    match windows_mic_consumers() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("aftercalls: WASAPI session enumeration failed: {e}");
            Vec::new()
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_mic_consumers() -> Result<Vec<String>, String> {
    use windows::core::{Interface, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE};
    use windows::Win32::Media::Audio::{
        eCapture, AudioSessionStateInactive, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // Helper hoisted above the main body so the outer function's tail
    // expression is the `unsafe { ... }` block and its `Result<Vec<…>, …>`
    // value flows out directly. (Previously the inner `fn` declaration
    // followed the unsafe block, which made rustc treat the unsafe
    // block as a statement and the function's implicit return `()` —
    // Linux ignores this file via cfg but Windows CI caught it.)
    #[inline]
    unsafe fn process_exe_basename(pid: u32) -> Result<String, String> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let handle: HANDLE =
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .map_err(|e| format!("OpenProcess({pid}): {e}"))?;

        let mut buf: [u16; 1024] = [0; 1024];
        let mut size: u32 = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );

        let _ = CloseHandle(handle);

        result.map_err(|e| format!("QueryFullProcessImageNameW: {e}"))?;

        let slice = &buf[..size as usize];
        let full = OsString::from_wide(slice)
            .to_string_lossy()
            .into_owned();
        let base = full
            .rsplit(|c| c == '\\' || c == '/')
            .next()
            .unwrap_or(&full)
            .to_string();
        Ok(base)
    }

    unsafe {
        // COM init: per-thread. Ignore RPC_E_CHANGED_MODE (already inited
        // differently) and S_FALSE (already inited same mode).
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = hr.ok() {
            if e.code() != RPC_E_CHANGED_MODE {
                return Err(format!("CoInitializeEx: {e}"));
            }
        }

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;

        let devices = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;

        let device_count = devices
            .GetCount()
            .map_err(|e| format!("IMMDeviceCollection::GetCount: {e}"))?;

        let own_pid = std::process::id();
        let mut result: Vec<String> = Vec::new();
        let mut seen_pids: HashSet<u32> = HashSet::new();

        for i in 0..device_count {
            let device = match devices.Item(i) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("aftercalls: IMMDeviceCollection::Item({i}): {e}");
                    continue;
                }
            };

            let session_manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("aftercalls: IMMDevice::Activate IAudioSessionManager2: {e}");
                    continue;
                }
            };

            let session_enum = match session_manager.GetSessionEnumerator() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("aftercalls: GetSessionEnumerator: {e}");
                    continue;
                }
            };

            let session_count = match session_enum.GetCount() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("aftercalls: IAudioSessionEnumerator::GetCount: {e}");
                    continue;
                }
            };

            for s in 0..session_count {
                let control = match session_enum.GetSession(s) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let control2: IAudioSessionControl2 = match control.cast() {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let pid = match control2.GetProcessId() {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                if pid == 0 || pid == own_pid {
                    continue;
                }

                // Active use only — skip inactive/expired sessions.
                if let Ok(state) = control.GetState() {
                    if state == AudioSessionStateInactive {
                        continue;
                    }
                }

                if !seen_pids.insert(pid) {
                    continue;
                }

                match process_exe_basename(pid) {
                    Ok(name) if !name.is_empty() => result.push(name),
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("aftercalls: process_exe_basename({pid}): {e}");
                    }
                }
            }
        }

        Ok(result)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn raw_mic_consumers() -> Vec<String> {
    // macOS + other unsupported platforms: auto-detect is a no-op.
    Vec::new()
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
