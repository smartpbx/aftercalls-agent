use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat};
use hound::{SampleFormat as WavSampleFormat, WavSpec, WavWriter};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::live::{LiveTap, CHANNEL_MIC};

type SharedWriter = Arc<Mutex<Option<WavWriter<BufWriter<File>>>>>;

/// Reason we fell back to the system-default mic instead of using the
/// saved-name preference. Serialized as the `reason` field on the
/// `mic-fallback` Tauri event the Record page listens for. Kept as a
/// string enum (not a free-form error message) so the frontend can
/// branch on it if we ever grow different copy per reason.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicFallbackReason {
    /// Saved name did not match any currently-enumerated input device.
    NotFound,
    /// `host.input_devices()` itself failed — rare, but we still want
    /// the user to know the saved pref couldn't even be checked.
    EnumerationFailed,
}

#[derive(Clone, Debug, Serialize)]
pub struct MicFallback {
    pub saved: String,
    pub reason: MicFallbackReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawTrackState {
    Finalized,
    NotPresent,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct RawTrackFinalization {
    pub track: &'static str,
    pub state: RawTrackState,
    pub path: PathBuf,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeardownIssue {
    pub component: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecorderStopReport {
    pub session_dir: PathBuf,
    pub tracks: Vec<RawTrackFinalization>,
    pub issues: Vec<TeardownIssue>,
}

pub struct Recorder {
    inner: Mutex<Inner>,
    active: AtomicBool,
    // Unix-ms timestamp set when `start()` succeeds; 0 while idle. Lets
    // the UI rebuild the running timer after a webview remount (tray
    // hide+show, route nav) without persisting state to disk.
    started_at_ms: AtomicI64,
    // The process-wide lifecycle generation shared with screen, rolling, and
    // live capture. Zero means idle. Stop commands carry the generation they
    // intend to tear down, so a delayed caller cannot stop a newer session.
    active_generation: AtomicI64,
    // Per-session dedupe for `mic-fallback` toasts (#3, decisions Q3).
    // Holds the set of saved device names we've already surfaced a
    // fallback toast for THIS process run. First recording with a
    // missing saved mic fires the toast; subsequent recordings in the
    // same session with the same stale name stay quiet. Restart
    // re-arms. A different stale name (user edited the pref mid-run)
    // is a new entry and fires once.
    fallback_seen: Mutex<HashSet<String>>,
    // Captured at `begin()` time and drained by `take_last_fallback()`
    // after a successful `start()` resolves. The worker thread writes
    // it; `lib.rs::do_start` reads it once, and the "once per session
    // per stale name" dedupe in `fallback_seen` gates whether it's
    // actually emitted. Mutex (not atomic) because the value isn't
    // Copy.
    pending_fallback: Mutex<Option<MicFallback>>,
    // Mirror of the active session's directory, populated on a
    // successful `start()` and cleared on `stop()`. Lets the
    // `is_recording` command expose the current session_dir to the
    // webview so a route-nav remount mid-recording can rehydrate
    // the manual-notes panel's session id (#185). Authoritative
    // ownership of the path still lives on the worker thread's
    // `Active` struct; this is a read-only snapshot kept in sync
    // with `started_at_ms`.
    active_session_dir: Mutex<Option<PathBuf>>,
}

struct Inner {
    tx: Sender<Command_>,
    _worker: JoinHandle<()>,
}

// Reply shape for a Start command. On success we carry the session
// directory AND an optional `MicFallback` describing whether the
// recorder had to fall back from a saved-name preference. `lib.rs`
// consumes the fallback via `take_last_fallback()` after `start()`
// returns; it's surfaced to the webview as a `mic-fallback` event
// (deduped per-session-per-name in `fallback_seen`).
type StartOk = (PathBuf, Option<MicFallback>);

enum Command_ {
    Start {
        base_dir: PathBuf,
        generation: u64,
        saved_device: Option<String>,
        // #142 · v0.4.5 — note-to-self mode. When true `begin()`
        // skips `start_system_loopback` entirely so even a
        // compositor / WASAPI monitor that would otherwise succeed
        // stays closed. The self-note session_dir contains only
        // mic.wav (+ source.json written by `lib.rs`).
        mic_only: bool,
        // #live — live-transcript tap (Phase 1). `Some` when the org has the
        // live_transcript feature on; each cpal callback pushes a lossy COPY
        // of its samples here for the relay. `None` disables the tap entirely
        // so there's zero extra work on the audio thread.
        live_tap: Option<LiveTap>,
        reply: Sender<Result<StartOk, String>>,
    },
    Stop {
        expected_generation: u64,
        expected_session_dir: PathBuf,
        reply: Sender<Result<RecorderStopReport, String>>,
    },
}

struct CpalTrack {
    _stream: cpal::Stream,
    writer: SharedWriter,
}

// System audio capture: either a subprocess (parec on Linux, output
// streamed to system.wav), a cpal stream (Windows WASAPI loopback,
// data written via the normal CpalTrack writer), or a
// ScreenCaptureKit-backed Swift shim on macOS that writes the WAV
// directly from the Swift side. Kept as an enum so finish() can
// clean up whichever flavor is active.
enum SystemCapture {
    // Linux: (batch parec → system.wav, optional live-tap parec → "Them" lane).
    Child(Child, Option<Child>),
    Cpal(CpalTrack),
    #[cfg(target_os = "macos")]
    Mac(crate::macos_loopback::MacLoopback),
}

struct Active {
    generation: u64,
    cpal_tracks: Vec<CpalTrack>,
    system: Option<SystemCapture>,
    session_dir: PathBuf,
    // Non-None when the saved-name preference didn't resolve and we
    // recorded from `host.default_input_device()` instead. Threaded
    // back to `lib.rs::do_start` via the Start reply so it can emit a
    // `mic-fallback` event (deduped per-session).
    fallback: Option<MicFallback>,
}

impl Recorder {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || worker_loop(rx));
        Self {
            inner: Mutex::new(Inner {
                tx,
                _worker: worker,
            }),
            active: AtomicBool::new(false),
            started_at_ms: AtomicI64::new(0),
            active_generation: AtomicI64::new(0),
            fallback_seen: Mutex::new(HashSet::new()),
            pending_fallback: Mutex::new(None),
            active_session_dir: Mutex::new(None),
        }
    }

    pub fn active_generation(&self) -> Option<u64> {
        u64::try_from(self.active_generation.load(Ordering::Acquire))
            .ok()
            .filter(|generation| *generation != 0)
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn started_at_ms(&self) -> Option<i64> {
        match self.started_at_ms.load(Ordering::Relaxed) {
            0 => None,
            ts => Some(ts),
        }
    }

    /// Current active session directory, or None when idle. Mirrors the
    /// `started_at_ms()` accessor so the `is_recording` command can
    /// expose both fields together for remount rehydration (#185).
    pub fn session_dir(&self) -> Option<PathBuf> {
        self.active_session_dir.lock().unwrap().clone()
    }

    pub fn start(
        &self,
        base_dir: PathBuf,
        generation: u64,
        saved_device: Option<String>,
        mic_only: bool,
        live_tap: Option<LiveTap>,
    ) -> Result<PathBuf, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if generation == 0 || generation > i64::MAX as u64 {
            return Err("invalid recording generation".into());
        }
        // Hold the caller-side lock through the worker reply AND mirror
        // update. Previously Start/Stop commands were serialized only in the
        // worker queue, allowing an older Stop caller to clear mirrors after a
        // newer Start caller had set them.
        let inner = self.inner.lock().unwrap();
        inner
            .tx
            .send(Command_::Start {
                base_dir,
                generation,
                saved_device,
                mic_only,
                live_tap,
                reply: reply_tx,
            })
            .map_err(|e| e.to_string())?;
        let result = reply_rx.recv().map_err(|e| e.to_string())?;
        match result {
            Ok((path, fallback)) => {
                self.started_at_ms
                    .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
                self.active_generation
                    .store(generation as i64, Ordering::Release);
                self.active.store(true, Ordering::Release);
                // Stash the fallback info (if any) for the caller to
                // consume via `take_last_fallback()`. Dedupe happens
                // there, not here, so `start()` stays a pure
                // "this-session-fell-back" signal.
                *self.pending_fallback.lock().unwrap() = fallback;
                // Snapshot the session_dir so `is_recording` can hand
                // it back to a route-nav remount (#185). Cleared in
                // `stop()`; paired with `started_at_ms`.
                *self.active_session_dir.lock().unwrap() = Some(path.clone());
                Ok(path)
            }
            Err(e) => Err(e),
        }
    }

    /// Pop the most recent successful start's fallback info, gated by
    /// the per-session HashSet<String> so the same stale name only
    /// fires a toast once per process run (decisions.md Q3). Returns
    /// None when there was no fallback OR when this saved name has
    /// already been surfaced this session.
    pub fn take_last_fallback(&self) -> Option<MicFallback> {
        let pending = self.pending_fallback.lock().unwrap().take()?;
        let mut seen = self.fallback_seen.lock().unwrap();
        if seen.contains(&pending.saved) {
            return None;
        }
        seen.insert(pending.saved.clone());
        Some(pending)
    }

    pub fn stop_generation(
        &self,
        expected_generation: u64,
        expected_session_dir: &Path,
    ) -> Result<RecorderStopReport, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let inner = self.inner.lock().unwrap();
        inner
            .tx
            .send(Command_::Stop {
                expected_generation,
                expected_session_dir: expected_session_dir.to_path_buf(),
                reply: reply_tx,
            })
            .map_err(|e| e.to_string())?;
        let result = reply_rx.recv().map_err(|e| e.to_string())?;
        if result.is_ok() {
            self.active.store(false, Ordering::Release);
            self.active_generation.store(0, Ordering::Release);
            self.started_at_ms.store(0, Ordering::Relaxed);
            // Drop the session_dir snapshot so a post-stop
            // `is_recording` read doesn't hand the webview a stale
            // path. Paired with the set in `start()`.
            *self.active_session_dir.lock().unwrap() = None;
        }
        result
    }
}

fn worker_loop(rx: Receiver<Command_>) {
    let mut active: Option<Active> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command_::Start {
                base_dir,
                generation,
                saved_device,
                mic_only,
                live_tap,
                reply,
            } => {
                if active.is_some() {
                    let _ = reply.send(Err("recording already in progress".into()));
                    continue;
                }
                match begin(
                    &base_dir,
                    generation,
                    saved_device.as_deref(),
                    mic_only,
                    live_tap,
                ) {
                    Ok(rec) => {
                        let path = rec.session_dir.clone();
                        let fallback = rec.fallback.clone();
                        active = Some(rec);
                        let _ = reply.send(Ok((path, fallback)));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            Command_::Stop {
                expected_generation,
                expected_session_dir,
                reply,
            } => match active.as_ref() {
                Some(rec)
                    if rec.generation != expected_generation
                        || rec.session_dir != expected_session_dir =>
                {
                    let _ = reply.send(Err(format!(
                        "stale recorder stop rejected: expected generation {expected_generation} for {}, active generation is {} for {}",
                        expected_session_dir.display(),
                        rec.generation,
                        rec.session_dir.display()
                    )));
                }
                Some(_) => {
                    let rec = active.take().expect("checked active recorder");
                    let _ = reply.send(Ok(finish(rec)));
                }
                None => {
                    let _ = reply.send(Err("no active recording".into()));
                }
            },
        }
    }
}

fn begin(
    base_dir: &Path,
    generation: u64,
    saved_device: Option<&str>,
    mic_only: bool,
    live_tap: Option<LiveTap>,
) -> Result<Active> {
    let session_dir = crate::session_fs::allocate(
        base_dir,
        crate::session_fs::SessionKind::Recording,
    )
    .context("allocate recording session")?;

    let host = cpal::default_host();
    let (mic_device, fallback) = resolve_input_device(&host, saved_device)?;
    // Mic → live channel 0 ("You"). The tap is a lossy COPY; the WAV path
    // below is untouched.
    let mic_track = build_cpal_track(
        &mic_device,
        session_dir.join("mic.wav"),
        CHANNEL_MIC,
        live_tap.clone(),
    )
    .context("build mic track")?;
    eprintln!(
        "aftercalls: recording mic from {:?}",
        mic_device.name().unwrap_or_default()
    );

    // #142 · v0.4.5 — note-to-self sessions skip system loopback
    // entirely so a silently-succeeding compositor monitor can't
    // capture "the other side" of a conversation we deliberately
    // declared mic-only. Privacy + storage are both positively
    // affected; the pipeline already handles a missing system.wav
    // cleanly.
    let system_path = session_dir.join("system.wav");
    let system = if mic_only {
        eprintln!("aftercalls: mic-only session — skipping system loopback");
        None
    } else {
        match start_system_loopback(&system_path, live_tap.clone()) {
            Ok((cap, target)) => {
                eprintln!("aftercalls: recording system audio from {target}");
                Some(cap)
            }
            Err(e) => {
                // A failed constructor may have written only a WAV header.
                // Remove it so the pipeline distinguishes "not recorded"
                // from a track that captured data and later failed.
                let _ = std::fs::remove_file(&system_path);
                eprintln!("aftercalls: skipping system loopback: {e:#}");
                None
            }
        }
    };

    mic_track._stream.play().context("start mic stream")?;
    if let Some(SystemCapture::Cpal(t)) = &system {
        t._stream.play().context("start system loopback stream")?;
    }

    Ok(Active {
        generation,
        cpal_tracks: vec![mic_track],
        system,
        session_dir,
        fallback,
    })
}

/// Resolves the mic device to open. Saved name → walk
/// `host.input_devices()` for a first-match; on miss OR on
/// enumeration failure, fall back to `host.default_input_device()`.
/// Returns the `Option<MicFallback>` alongside the device so
/// `begin()` can thread it out to `lib.rs` for the `mic-fallback`
/// event (#3). Fresh installs (saved == None) never produce a
/// fallback.
fn resolve_input_device(
    host: &Host,
    saved: Option<&str>,
) -> Result<(Device, Option<MicFallback>)> {
    let Some(saved_name) = saved else {
        let dev = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?;
        return Ok((dev, None));
    };

    // Try to match the saved name first. `input_devices()` can fail
    // on some hosts (WASAPI endpoint enumeration, PipeWire registry
    // churn); treat that as a fallback with a distinct reason so the
    // UI copy can branch on it.
    match host.input_devices() {
        Ok(iter) => {
            for dev in iter {
                if dev.name().ok().as_deref() == Some(saved_name) {
                    return Ok((dev, None));
                }
            }
            // Saved name no longer enumerates — most common: USB mic
            // unplugged, or PipeWire graph renumbered it on reboot.
            let dev = host
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?;
            Ok((
                dev,
                Some(MicFallback {
                    saved: saved_name.to_string(),
                    reason: MicFallbackReason::NotFound,
                }),
            ))
        }
        Err(e) => {
            eprintln!("aftercalls: input_devices() failed: {e}");
            let dev = host
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?;
            Ok((
                dev,
                Some(MicFallback {
                    saved: saved_name.to_string(),
                    reason: MicFallbackReason::EnumerationFailed,
                }),
            ))
        }
    }
}

/// One row of the Settings → Input microphone dropdown. `name` is the
/// cpal-reported device name (the persisted-pref primitive);
/// `is_default` marks the currently-defaulted input so the UI can
/// display "currently: X" under "System default" without a second
/// enumeration roundtrip. Duplicate-name disambiguation (decisions.md
/// Q1) is computed client-side from enumeration order.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceEntry {
    pub name: String,
    pub is_default: bool,
}

/// Walks `host.input_devices()`, filters out PipeWire monitor sources
/// (and equivalents on other backends) via two layers:
///   1. Primary: `device.default_input_config().is_ok()` — monitor
///      nodes fail this on PipeWire because they expose an output
///      format, not an input one. Real mics pass.
///   2. Backup: suffix blocklist against the device name
///      (`.monitor`, `.monitor.*`, ALSA-naming edge cases). Belt-and-
///      -suspenders for hosts where the config probe surprises us.
///
/// Returns early on `host.input_devices()` failure — callers surface
/// this as the "Couldn't load devices" state. A Some-but-empty list
/// is the "no mics connected" state, also surfaced by the UI.
pub fn enumerate_input_devices() -> Result<Vec<DeviceEntry>, String> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok());
    let iter = host.input_devices().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for dev in iter {
        let Ok(name) = dev.name() else {
            continue;
        };
        // Filter monitor sources. PipeWire exposes the default sink's
        // monitor (e.g. `alsa_output.*.monitor`) via input_devices()
        // because it IS a capturable node, but it's not a microphone
        // and we don't want it in the mic dropdown.
        if is_monitor_source_name(&name) {
            continue;
        }
        if dev.default_input_config().is_err() {
            continue;
        }
        let is_default = default_name.as_deref() == Some(name.as_str());
        out.push(DeviceEntry { name, is_default });
    }
    Ok(out)
}

/// Name-suffix backup filter used in tandem with the
/// `default_input_config` probe. Covers the common PipeWire /
/// PulseAudio monitor-source naming conventions. Kept generous so an
/// unexpected `.monitor.something` variant still gets filtered.
fn is_monitor_source_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".monitor")
        || lower.contains(".monitor.")
        || lower.ends_with(" monitor")
        || lower.ends_with(" monitor of built-in audio")
}

#[derive(Default)]
struct TeardownAccumulator {
    issues: Vec<TeardownIssue>,
}

impl TeardownAccumulator {
    fn attempt(&mut self, component: impl Into<String>, step: impl FnOnce() -> Result<()>) {
        let component = component.into();
        if let Err(error) = step() {
            self.issues.push(TeardownIssue {
                component,
                error: format!("{error:#}"),
            });
        }
    }
}

fn finish(rec: Active) -> RecorderStopReport {
    let Active {
        cpal_tracks,
        system,
        session_dir,
        ..
    } = rec;
    let mut teardown = TeardownAccumulator::default();

    // Drop every live stream first, then attempt every writer finalizer.
    // `attempt` never short-circuits, so one broken mic writer cannot skip
    // system capture teardown.
    for (idx, track) in cpal_tracks.into_iter().enumerate() {
        drop(track._stream);
        let component = format!("mic_writer_{idx}");
        let writer = match track.writer.lock() {
            Ok(mut writer) => writer.take(),
            Err(poisoned) => {
                teardown.issues.push(TeardownIssue {
                    component: component.clone(),
                    error: "WAV writer mutex was poisoned; recovering owned writer".into(),
                });
                poisoned.into_inner().take()
            }
        };
        teardown.attempt(component, move || {
            if let Some(writer) = writer {
                writer.finalize().context("finalize mic WAV")?;
            }
            Ok(())
        });
    }

    match system {
        Some(SystemCapture::Child(mut child, tap)) => {
            teardown.attempt("system_loopback", || {
                stop_child_gracefully(&mut child).context("stop system loopback")
            });
            if let Some(mut tap_child) = tap {
                teardown.attempt("system_live_tap", || {
                    stop_child_now(&mut tap_child).context("stop system live tap")
                });
            }
        }
        Some(SystemCapture::Cpal(track)) => {
            drop(track._stream);
            let writer = match track.writer.lock() {
                Ok(mut writer) => writer.take(),
                Err(poisoned) => {
                    teardown.issues.push(TeardownIssue {
                        component: "system_writer".into(),
                        error: "WAV writer mutex was poisoned; recovering owned writer".into(),
                    });
                    poisoned.into_inner().take()
                }
            };
            teardown.attempt("system_writer", move || {
                if let Some(writer) = writer {
                    writer.finalize().context("finalize system WAV")?;
                }
                Ok(())
            });
        }
        #[cfg(target_os = "macos")]
        Some(SystemCapture::Mac(mut loopback)) => {
            teardown.attempt("system_loopback", || {
                loopback.stop().context("stop system loopback")
            });
        }
        None => {}
    }

    let mic = inspect_finalized_wav("mic", session_dir.join("mic.wav"), true);
    let system = inspect_finalized_wav("system", session_dir.join("system.wav"), false);
    for track in [&mic, &system] {
        if track.state == RawTrackState::Failed {
            teardown.issues.push(TeardownIssue {
                component: format!("{}_wav", track.track),
                error: track
                    .error
                    .clone()
                    .unwrap_or_else(|| "raw WAV validation failed".into()),
            });
        }
    }

    RecorderStopReport {
        session_dir,
        tracks: vec![mic, system],
        issues: teardown.issues,
    }
}

fn inspect_finalized_wav(
    track: &'static str,
    path: PathBuf,
    required: bool,
) -> RawTrackFinalization {
    if !path.exists() {
        return RawTrackFinalization {
            track,
            state: if required {
                RawTrackState::Failed
            } else {
                RawTrackState::NotPresent
            },
            path,
            error: required.then(|| "required WAV is missing".into()),
        };
    }
    if let Err(error) = crate::media_manifest::enforce_private_file(&path) {
        return RawTrackFinalization {
            track,
            state: RawTrackState::Failed,
            path,
            error: Some(format!("protect finalized WAV: {error:#}")),
        };
    }
    match hound::WavReader::open(&path) {
        Ok(reader) if reader.spec().sample_rate > 0 && reader.duration() > 0 => {
            RawTrackFinalization {
                track,
                state: RawTrackState::Finalized,
                path,
                error: None,
            }
        }
        Ok(_) => RawTrackFinalization {
            track,
            state: RawTrackState::Failed,
            path,
            error: Some("WAV has zero sample rate or duration".into()),
        },
        Err(error) => RawTrackFinalization {
            track,
            state: RawTrackState::Failed,
            path,
            error: Some(format!("open finalized WAV: {error}")),
        },
    }
}

fn build_cpal_track(
    device: &Device,
    output_path: PathBuf,
    channel_tag: u8,
    live_tap: Option<LiveTap>,
) -> Result<CpalTrack> {
    let config = device
        .default_input_config()
        .context("default input config")?;
    build_cpal_track_from_config(device, output_path, config, channel_tag, live_tap)
}

fn build_cpal_track_from_config(
    device: &Device,
    output_path: PathBuf,
    config: cpal::SupportedStreamConfig,
    channel_tag: u8,
    live_tap: Option<LiveTap>,
) -> Result<CpalTrack> {
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.clone().into();
    // Native rate + channel count for the live tap's resampler (16 kHz mono
    // downmix happens off the audio thread, in the live module).
    let tap_rate = config.sample_rate().0;
    let tap_channels = config.channels();

    let spec = WavSpec {
        channels: config.channels(),
        sample_rate: config.sample_rate().0,
        bits_per_sample: (sample_format.sample_size() * 8) as u16,
        sample_format: match sample_format {
            SampleFormat::F32 | SampleFormat::F64 => WavSampleFormat::Float,
            _ => WavSampleFormat::Int,
        },
    };

    let file = crate::session_fs::create_private_file(&output_path)
        .with_context(|| format!("create private WAV {}", output_path.display()))?;
    let wav = WavWriter::new(BufWriter::new(file), spec).context("write WAV header")?;
    let writer: SharedWriter = Arc::new(Mutex::new(Some(wav)));
    let err_fn = |e| eprintln!("aftercalls: input stream error: {e}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let w = Arc::clone(&writer);
            let tap = live_tap.clone();
            device.build_input_stream(
                &stream_config,
                move |d: &[f32], _: &_| {
                    write_samples(&w, d, |v| v);
                    if let Some(t) = &tap {
                        t.push(channel_tag, d.to_vec(), tap_rate, tap_channels);
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I16 => {
            let w = Arc::clone(&writer);
            let tap = live_tap.clone();
            device.build_input_stream(
                &stream_config,
                move |d: &[i16], _: &_| {
                    write_samples(&w, d, |v| v);
                    if let Some(t) = &tap {
                        t.push(
                            channel_tag,
                            d.iter().map(|&v| v as f32 / 32768.0).collect(),
                            tap_rate,
                            tap_channels,
                        );
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I32 => {
            let w = Arc::clone(&writer);
            let tap = live_tap.clone();
            device.build_input_stream(
                &stream_config,
                move |d: &[i32], _: &_| {
                    write_samples(&w, d, |v| v);
                    if let Some(t) = &tap {
                        t.push(
                            channel_tag,
                            d.iter().map(|&v| v as f32 / 2_147_483_648.0).collect(),
                            tap_rate,
                            tap_channels,
                        );
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::U16 => {
            let w = Arc::clone(&writer);
            let tap = live_tap.clone();
            device.build_input_stream(
                &stream_config,
                move |d: &[u16], _: &_| {
                    write_samples(&w, d, |v| (v as i32 - i16::MAX as i32 - 1) as i16);
                    if let Some(t) = &tap {
                        t.push(
                            channel_tag,
                            d.iter().map(|&v| (v as f32 - 32768.0) / 32768.0).collect(),
                            tap_rate,
                            tap_channels,
                        );
                    }
                },
                err_fn,
                None,
            )?
        }
        fmt => anyhow::bail!("unsupported sample format: {fmt:?}"),
    };

    Ok(CpalTrack {
        _stream: stream,
        writer,
    })
}

fn write_samples<S, T>(writer: &SharedWriter, data: &[S], convert: impl Fn(S) -> T)
where
    S: Copy,
    T: hound::Sample,
{
    if let Some(ref mut w) = *writer.lock().unwrap() {
        for &s in data {
            let _ = w.write_sample(convert(s));
        }
    }
}

#[cfg(target_os = "linux")]
fn start_system_loopback(
    output_path: &Path,
    live_tap: Option<LiveTap>,
) -> Result<(SystemCapture, String)> {
    // Linux system audio: a `parec` subprocess writes system.wav directly
    // (the lossless batch track — unchanged). When a live tap is present, a
    // SECOND parec streams the same monitor as raw PCM to a detached reader
    // thread that feeds the "Them" live lane. The batch WAV path stays
    // untouched, and a tap failure only costs the live far-end draft.
    use std::os::unix::process::CommandExt;
    let default_sink = default_sink_name().context("get default sink")?;
    let monitor = format!("{default_sink}.monitor");

    // Ties a parec child's lifetime to the agent — if the agent is SIGKILL'd
    // (binary swap, crash, force-quit) parec gets SIGINT instead of leaking
    // and writing to a stale session dir. Applied to both parecs.
    let parent_pid = std::process::id() as libc::pid_t;
    let tie = move |cmd: &mut Command| {
        unsafe {
            cmd.pre_exec(move || {
                // Media children inherit the process umask by default. Make
                // every file they create private even on hosts configured
                // with a permissive 0022 umask.
                libc::umask(0o077);
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "recorder parent exited before child exec",
                    ));
                }
                Ok(())
            });
        }
    };

    // Batch track → system.wav. parec respects the `.monitor` source name;
    // pw-cat's --target resolves to the wrong node (PipeWire exposes the
    // monitor as a port of the sink node, not a separate node).
    // Reserve the inode as 0600 before handing it to the external producer;
    // libsndfile truncates the existing file without widening its mode.
    drop(crate::session_fs::create_private_file(output_path)?);
    let mut batch = Command::new("parec");
    batch
        .arg("--device")
        .arg(&monitor)
        .arg("--file-format=wav")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Do not leave an unread pipe that can fill and deadlock Stop.
        .stderr(Stdio::null());
    tie(&mut batch);
    let batch_child = match batch.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(output_path);
            return Err(error).context("spawn parec");
        }
    };

    // Live-tap track → raw s16le PCM on stdout (48 kHz stereo; the live module
    // downmixes + resamples to 16 kHz mono off-thread). A detached reader
    // thread pushes each chunk to the "Them" lane and exits on parec's EOF
    // (the tap parec is killed at stop). Best-effort spawn.
    let tap_child = live_tap.and_then(|tap| {
        let mut cmd = Command::new("parec");
        cmd.arg("--device")
            .arg(&monitor)
            .arg("--format=s16le")
            .arg("--rate=48000")
            .arg("--channels=2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        tie(&mut cmd);
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(mut out) = child.stdout.take() {
                    thread::spawn(move || {
                        use std::io::Read;
                        let mut buf = [0u8; 8192];
                        while let Ok(n) = out.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            let end = n - (n % 2);
                            let samples: Vec<f32> = buf[..end]
                                .chunks_exact(2)
                                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                                .collect();
                            tap.push(crate::live::CHANNEL_SYSTEM, samples, 48_000, 2);
                        }
                    });
                }
                Some(child)
            }
            Err(e) => {
                eprintln!("aftercalls: live far-end (system) tap unavailable: {e:#}");
                None
            }
        }
    });

    Ok((SystemCapture::Child(batch_child, tap_child), monitor))
}

// Windows WASAPI loopback capture. cpal's WASAPI backend uses
// AUDCLNT_STREAMFLAGS_LOOPBACK internally when build_input_stream is
// called on a device that exposes a render endpoint — i.e. the
// default output device. Stream format comes from the device's
// default OUTPUT config (loopback captures what's being rendered,
// not what the output's input pair expects). Same CpalTrack writer
// plumbing as the mic; the resulting system.wav is interchangeable
// with Linux's parec output from the pipeline's perspective.
#[cfg(target_os = "windows")]
fn start_system_loopback(
    output_path: &Path,
    live_tap: Option<LiveTap>,
) -> Result<(SystemCapture, String)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device for loopback"))?;
    let label = device.name().unwrap_or_else(|_| "default output".into());
    let config = device
        .default_output_config()
        .context("default output config (loopback)")?;
    // System loopback → live channel 1 ("Them"). WASAPI loopback IS a cpal
    // stream, so it can be tapped like the mic.
    let track = build_cpal_track_from_config(
        &device,
        output_path.to_path_buf(),
        config,
        crate::live::CHANNEL_SYSTEM,
        live_tap,
    )
    .context("build system loopback track")?;
    Ok((SystemCapture::Cpal(track), label))
}

// macOS loopback via ScreenCaptureKit (#621 / Phase 2). Audio capture
// is owned by a Swift shim (`macos/AftercallsLoopback.swift`) bridged
// through `crate::macos_loopback`. The Swift side writes the WAV
// file directly; we hand it the same `system.wav` path Linux/Windows
// use so the rest of the pipeline (mix, transcribe, summarize) is
// platform-agnostic. The user-facing target string is intentionally
// vendor-opaque per repo policy.
#[cfg(target_os = "macos")]
fn start_system_loopback(
    output_path: &Path,
    _live_tap: Option<LiveTap>,
) -> Result<(SystemCapture, String)> {
    // macOS system audio is written by the ScreenCaptureKit Swift shim, not
    // a cpal stream, so the Phase-1 live tap covers the mic ("You") lane only.
    let loopback = crate::macos_loopback::MacLoopback::new(output_path)
        .context("start system audio loopback")?;
    Ok((SystemCapture::Mac(loopback), "system audio".to_string()))
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn start_system_loopback(
    _output_path: &Path,
    _live_tap: Option<LiveTap>,
) -> Result<(SystemCapture, String)> {
    anyhow::bail!("system loopback not implemented on this platform yet")
}

#[cfg(target_os = "linux")]
fn default_sink_name() -> Result<String> {
    let output = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .context("run pactl")?;
    if !output.status.success() {
        anyhow::bail!(
            "pactl: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(unix)]
fn stop_child_gracefully(child: &mut Child) -> Result<()> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(poll_error) => {
            let _ = child.kill();
            let reap = child.wait();
            return match reap {
                Ok(_) => Err(poll_error)
                    .context("poll child before stop (child killed and reaped)"),
                Err(reap_error) => anyhow::bail!(
                    "poll child before stop failed: {poll_error}; kill/reap also failed: {reap_error}"
                ),
            };
        }
    }
    let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    if signal_result != 0 {
        let signal_error = std::io::Error::last_os_error();
        // Even when signalling races an exit, collect the child. A signal
        // error must never turn into a dropped, unreaped process handle.
        let reap_result = stop_child_now(child);
        return match reap_result {
            Ok(()) => Err(signal_error).context("signal child (child reaped)"),
            Err(reap_error) => anyhow::bail!(
                "signal child failed: {signal_error}; kill/reap also failed: {reap_error:#}"
            ),
        };
    }
    wait_child_bounded(child, Duration::from_secs(5))
}

#[cfg(not(unix))]
fn stop_child_gracefully(child: &mut Child) -> Result<()> {
    stop_child_now(child)
}

fn stop_child_now(child: &mut Child) -> Result<()> {
    let _ = child.kill();
    child.wait().context("reap child")?;
    Ok(())
}

fn wait_child_bounded(child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                child.wait().context("reap timed-out child")?;
                anyhow::bail!(
                    "child did not stop within {}s; killed and reaped",
                    timeout.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(poll_error) => {
                let _ = child.kill();
                let reap = child.wait();
                return match reap {
                    Ok(_) => Err(poll_error).context("poll child (child killed and reaped)"),
                    Err(reap_error) => anyhow::bail!(
                        "poll child failed: {poll_error}; kill/reap also failed: {reap_error}"
                    ),
                };
            }
        }
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn independent_finalizer_failure_does_not_short_circuit_later_steps() {
        let ran = AtomicUsize::new(0);
        let mut teardown = TeardownAccumulator::default();
        teardown.attempt("mic", || {
            ran.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("injected mic finalize failure")
        });
        teardown.attempt("system", || {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        teardown.attempt("live", || {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(ran.load(Ordering::SeqCst), 3);
        assert_eq!(teardown.issues.len(), 1);
        assert_eq!(teardown.issues[0].component, "mic");
    }
}
