use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use hound::{SampleFormat as WavSampleFormat, WavSpec, WavWriter};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

type SharedWriter = Arc<Mutex<Option<WavWriter<BufWriter<File>>>>>;

pub struct Recorder {
    inner: Mutex<Inner>,
    active: AtomicBool,
}

struct Inner {
    tx: Sender<Command_>,
    _worker: JoinHandle<()>,
}

enum Command_ {
    Start {
        base_dir: PathBuf,
        reply: Sender<Result<PathBuf, String>>,
    },
    Stop {
        reply: Sender<Result<PathBuf, String>>,
    },
}

struct CpalTrack {
    _stream: cpal::Stream,
    writer: SharedWriter,
}

struct Active {
    cpal_tracks: Vec<CpalTrack>,
    system_child: Option<Child>,
    session_dir: PathBuf,
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
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn start(&self, base_dir: PathBuf) -> Result<PathBuf, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap()
            .tx
            .send(Command_::Start {
                base_dir,
                reply: reply_tx,
            })
            .map_err(|e| e.to_string())?;
        let result = reply_rx.recv().map_err(|e| e.to_string())?;
        if result.is_ok() {
            self.active.store(true, Ordering::Relaxed);
        }
        result
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
        }
        result
    }
}

fn worker_loop(rx: Receiver<Command_>) {
    let mut active: Option<Active> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command_::Start { base_dir, reply } => {
                if active.is_some() {
                    let _ = reply.send(Err("recording already in progress".into()));
                    continue;
                }
                match begin(&base_dir) {
                    Ok(rec) => {
                        let path = rec.session_dir.clone();
                        active = Some(rec);
                        let _ = reply.send(Ok(path));
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

fn begin(base_dir: &Path) -> Result<Active> {
    let session_dir = base_dir.join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    fs::create_dir_all(&session_dir).context("create session dir")?;

    let host = cpal::default_host();
    let mic_device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let mic_track = build_cpal_track(&mic_device, session_dir.join("mic.wav"))
        .context("build mic track")?;
    eprintln!(
        "callscribe: recording mic from {:?}",
        mic_device.name().unwrap_or_default()
    );

    let system_child = match start_system_loopback(&session_dir.join("system.wav")) {
        Ok((child, target)) => {
            eprintln!("callscribe: recording system audio from {target}");
            Some(child)
        }
        Err(e) => {
            eprintln!("callscribe: skipping system loopback: {e:#}");
            None
        }
    };

    mic_track._stream.play().context("start mic stream")?;

    Ok(Active {
        cpal_tracks: vec![mic_track],
        system_child,
        session_dir,
    })
}

fn finish(rec: Active) -> Result<PathBuf> {
    for t in rec.cpal_tracks {
        drop(t._stream);
        if let Some(w) = t.writer.lock().unwrap().take() {
            w.finalize().context("finalize wav")?;
        }
    }
    if let Some(mut child) = rec.system_child {
        stop_child_gracefully(&mut child).context("stop system loopback")?;
    }
    Ok(rec.session_dir)
}

fn build_cpal_track(device: &Device, output_path: PathBuf) -> Result<CpalTrack> {
    let config = device
        .default_input_config()
        .context("default input config")?;
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
    let err_fn = |e| eprintln!("callscribe: input stream error: {e}");

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
fn start_system_loopback(output_path: &Path) -> Result<(Child, String)> {
    let default_sink = default_sink_name().context("get default sink")?;
    let monitor = format!("{default_sink}.monitor");
    // parec (PulseAudio API) respects the `.monitor` source name; pw-cat's
    // --target resolves to the wrong node because PipeWire exposes the monitor
    // as a port of the sink node, not as a separate node.
    let child = Command::new("parec")
        .arg("--device")
        .arg(&monitor)
        .arg("--file-format=wav")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn parec")?;
    Ok((child, monitor))
}

#[cfg(not(target_os = "linux"))]
fn start_system_loopback(_output_path: &Path) -> Result<(Child, String)> {
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
