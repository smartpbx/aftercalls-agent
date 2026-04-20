use std::collections::HashSet;
use std::time::Duration;

use sysinfo::{ProcessesToUpdate, System};
use tauri::{AppHandle, Emitter, Manager};

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

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut sys = System::new();
        let mut managed: Option<String> = None;
        let mut suppress_restart_for: Option<String> = None;

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            sys.refresh_processes(ProcessesToUpdate::All, true);

            let apps: HashSet<String> = sys
                .processes()
                .values()
                .filter_map(|p| matched_app(&p.name().to_string_lossy()))
                .collect();

            let state = app.state::<Recorder>();
            let is_recording = state.is_active();

            // Recording session we were managing ended — figure out how.
            if let Some(m) = managed.clone() {
                if !apps.contains(&m) {
                    // App exited. Stop if still recording, clear all state.
                    if is_recording {
                        eprintln!("callscribe: meeting app '{m}' exited, auto-stopping");
                        emit_event(&app, AutoDetectEvent::Stopped { app: m.clone() });
                        if let Err(e) = crate::do_stop(&state, &app) {
                            eprintln!("callscribe: auto-stop failed: {e}");
                        }
                    }
                    managed = None;
                    suppress_restart_for = None;
                } else if !is_recording {
                    // User manually stopped while the app is still running.
                    // Don't auto-restart while the same app is present.
                    managed = None;
                    suppress_restart_for = Some(m);
                }
            }

            // If the suppressed app is gone, allow future auto-starts.
            if let Some(sup) = suppress_restart_for.clone() {
                if !apps.contains(&sup) {
                    suppress_restart_for = None;
                }
            }

            // Consider an auto-start.
            if !is_recording && managed.is_none() {
                let candidate = apps
                    .iter()
                    .find(|a| suppress_restart_for.as_ref() != Some(*a))
                    .cloned();
                if let Some(app_name) = candidate {
                    if default_mic_is_active() {
                        eprintln!(
                            "callscribe: '{app_name}' + mic busy, auto-starting"
                        );
                        emit_event(&app, AutoDetectEvent::Started { app: app_name.clone() });
                        match crate::do_start(&state, &app) {
                            Ok(_) => managed = Some(app_name),
                            Err(e) => eprintln!("callscribe: auto-start failed: {e}"),
                        }
                    }
                }
            }
        }
    });
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
    text.lines().any(|line| {
        line.contains("RUNNING") && !line.contains(".monitor")
    })
}

#[cfg(not(target_os = "linux"))]
fn default_mic_is_active() -> bool {
    // TODO(windows): check WASAPI IAudioSessionManager2 for active capture streams.
    // For now, fall through and let process presence alone trigger recording.
    true
}

fn emit_event(app: &AppHandle, event: AutoDetectEvent) {
    let _ = app.emit("auto-detect", event);
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "event", rename_all = "snake_case")]
enum AutoDetectEvent {
    Started { app: String },
    Stopped { app: String },
}
