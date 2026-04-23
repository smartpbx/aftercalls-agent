use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat};
use hound::{SampleFormat as WavSampleFormat, WavSpec, WavWriter};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

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

pub struct Recorder {
    inner: Mutex<Inner>,
    active: AtomicBool,
    // Unix-ms timestamp set when `start()` succeeds; 0 while idle. Lets
    // the UI rebuild the running timer after a webview remount (tray
    // hide+show, route nav) without persisting state to disk.
    started_at_ms: AtomicI64,
    // Monotonic session counter, bumped on every successful start().
    // The auto-stop watchdog (see lib.rs) captures the value at spawn
    // time and only fires if it still matches — that way a manual
    // stop followed by a new start can't be nuked by a stale timer
    // from the previous session. Cheaper + simpler than a Weak/Arc
    // cancellation token.
    session_seq: AtomicI64,
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
        saved_device: Option<String>,
        reply: Sender<Result<StartOk, String>>,
    },
    Stop {
        reply: Sender<Result<PathBuf, String>>,
    },
}

struct CpalTrack {
    _stream: cpal::Stream,
    writer: SharedWriter,
}

// System audio capture: either a subprocess (parec on Linux, output
// streamed to system.wav) or a cpal stream (Windows WASAPI loopback,
// data written via the normal CpalTrack writer). Kept as an enum so
// finish() can clean up either kind.
enum SystemCapture {
    Child(Child),
    Cpal(CpalTrack),
}

struct Active {
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
            session_seq: AtomicI64::new(0),
            fallback_seen: Mutex::new(HashSet::new()),
            pending_fallback: Mutex::new(None),
        }
    }

    /// Current session sequence. Bumped on every successful start().
    /// The watchdog spawned in `do_start` captures this value and
    /// aborts if it ever drifts (manual stop, then new start) — so a
    /// stale timer from a prior session can't kill a new one.
    pub fn session_seq(&self) -> i64 {
        self.session_seq.load(Ordering::Relaxed)
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

    pub fn start(
        &self,
        base_dir: PathBuf,
        saved_device: Option<String>,
    ) -> Result<PathBuf, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap()
            .tx
            .send(Command_::Start {
                base_dir,
                saved_device,
                reply: reply_tx,
            })
            .map_err(|e| e.to_string())?;
        let result = reply_rx.recv().map_err(|e| e.to_string())?;
        match result {
            Ok((path, fallback)) => {
                self.started_at_ms
                    .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
                self.active.store(true, Ordering::Relaxed);
                // Bump the session seq so any still-pending watchdog
                // from a prior session sees a mismatch and exits. The
                // new watchdog (spawned by lib.rs after start())
                // captures the updated value below.
                self.session_seq.fetch_add(1, Ordering::Relaxed);
                // Stash the fallback info (if any) for the caller to
                // consume via `take_last_fallback()`. Dedupe happens
                // there, not here, so `start()` stays a pure
                // "this-session-fell-back" signal.
                *self.pending_fallback.lock().unwrap() = fallback;
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

    pub fn stop(&self) -> Result<PathBuf, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap()
            .tx
            .send(Command_::Stop { reply: reply_tx })
            .map_err(|e| e.to_string())?;
        let result = reply_rx.recv().map_err(|e| e.to_string())?;
        if result.is_ok() {
            self.active.store(false, Ordering::Relaxed);
            self.started_at_ms.store(0, Ordering::Relaxed);
            // Bump on stop too so the currently-running watchdog's
            // captured seq goes stale and its next tick is a no-op.
            self.session_seq.fetch_add(1, Ordering::Relaxed);
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
                saved_device,
                reply,
            } => {
                if active.is_some() {
                    let _ = reply.send(Err("recording already in progress".into()));
                    continue;
                }
                match begin(&base_dir, saved_device.as_deref()) {
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
            Command_::Stop { reply } => match active.take() {
                Some(rec) => {
                    let result = finish(rec).map_err(|e| e.to_string());
                    let _ = reply.send(result);
                }
                None => {
                    let _ = reply.send(Err("no active recording".into()));
                }
            },
        }
    }
}

fn begin(base_dir: &Path, saved_device: Option<&str>) -> Result<Active> {
    let session_dir = base_dir.join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    fs::create_dir_all(&session_dir).context("create session dir")?;

    let host = cpal::default_host();
    let (mic_device, fallback) = resolve_input_device(&host, saved_device)?;
    let mic_track = build_cpal_track(&mic_device, session_dir.join("mic.wav"))
        .context("build mic track")?;
    eprintln!(
        "aftercalls: recording mic from {:?}",
        mic_device.name().unwrap_or_default()
    );

    let system = match start_system_loopback(&session_dir.join("system.wav")) {
        Ok((cap, target)) => {
            eprintln!("aftercalls: recording system audio from {target}");
            Some(cap)
        }
        Err(e) => {
            eprintln!("aftercalls: skipping system loopback: {e:#}");
            None
        }
    };

    mic_track._stream.play().context("start mic stream")?;
    if let Some(SystemCapture::Cpal(t)) = &system {
        t._stream.play().context("start system loopback stream")?;
    }

    Ok(Active {
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

fn finish(rec: Active) -> Result<PathBuf> {
    for t in rec.cpal_tracks {
        drop(t._stream);
        if let Some(w) = t.writer.lock().unwrap().take() {
            w.finalize().context("finalize wav")?;
        }
    }
    match rec.system {
        Some(SystemCapture::Child(mut child)) => {
            stop_child_gracefully(&mut child).context("stop system loopback")?;
        }
        Some(SystemCapture::Cpal(t)) => {
            drop(t._stream);
            if let Some(w) = t.writer.lock().unwrap().take() {
                w.finalize().context("finalize system wav")?;
            }
        }
        None => {}
    }
    Ok(rec.session_dir)
}

fn build_cpal_track(device: &Device, output_path: PathBuf) -> Result<CpalTrack> {
    let config = device
        .default_input_config()
        .context("default input config")?;
    build_cpal_track_from_config(device, output_path, config)
}

fn build_cpal_track_from_config(
    device: &Device,
    output_path: PathBuf,
    config: cpal::SupportedStreamConfig,
) -> Result<CpalTrack> {
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.clone().into();

    let spec = WavSpec {
        channels: config.channels(),
        sample_rate: config.sample_rate().0,
        bits_per_sample: (sample_format.sample_size() * 8) as u16,
        sample_format: match sample_format {
            SampleFormat::F32 | SampleFormat::F64 => WavSampleFormat::Float,
            _ => WavSampleFormat::Int,
        },
    };

    let wav = WavWriter::create(&output_path, spec).context("create wav")?;
    let writer: SharedWriter = Arc::new(Mutex::new(Some(wav)));
    let err_fn = |e| eprintln!("aftercalls: input stream error: {e}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[f32], _: &_| write_samples(&w, d, |v| v),
                err_fn,
                None,
            )?
        }
        SampleFormat::I16 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[i16], _: &_| write_samples(&w, d, |v| v),
                err_fn,
                None,
            )?
        }
        SampleFormat::I32 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[i32], _: &_| write_samples(&w, d, |v| v),
                err_fn,
                None,
            )?
        }
        SampleFormat::U16 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[u16], _: &_| {
                    write_samples(&w, d, |v| (v as i32 - i16::MAX as i32 - 1) as i16)
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
fn start_system_loopback(output_path: &Path) -> Result<(SystemCapture, String)> {
    use std::os::unix::process::CommandExt;
    let default_sink = default_sink_name().context("get default sink")?;
    let monitor = format!("{default_sink}.monitor");
    // parec (PulseAudio API) respects the `.monitor` source name; pw-cat's
    // --target resolves to the wrong node because PipeWire exposes the monitor
    // as a port of the sink node, not as a separate node.
    //
    // PR_SET_PDEATHSIG ties parec's lifetime to the agent process — if the
    // agent is SIGKILL'd (e.g. binary swap, crash, user force-quit) parec
    // gets SIGINT instead of being reparented to systemd and silently
    // writing to a stale session dir forever.
    let mut cmd = Command::new("parec");
    cmd.arg("--device")
        .arg(&monitor)
        .arg("--file-format=wav")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT);
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn parec")?;
    Ok((SystemCapture::Child(child), monitor))
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
fn start_system_loopback(output_path: &Path) -> Result<(SystemCapture, String)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device for loopback"))?;
    let label = device.name().unwrap_or_else(|_| "default output".into());
    let config = device
        .default_output_config()
        .context("default output config (loopback)")?;
    let track = build_cpal_track_from_config(&device, output_path.to_path_buf(), config)
        .context("build system loopback track")?;
    Ok((SystemCapture::Cpal(track), label))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn start_system_loopback(_output_path: &Path) -> Result<(SystemCapture, String)> {
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
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    child.wait().context("wait child")?;
    Ok(())
}

#[cfg(not(unix))]
fn stop_child_gracefully(child: &mut Child) -> Result<()> {
    child.kill().ok();
    child.wait().context("wait child")?;
    Ok(())
}
