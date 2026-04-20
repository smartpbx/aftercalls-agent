use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use hound::{SampleFormat as WavSampleFormat, WavSpec, WavWriter};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

type SharedWriter = Arc<Mutex<Option<WavWriter<BufWriter<File>>>>>;

pub struct Recorder {
    inner: Mutex<Inner>,
}

struct Inner {
    tx: Sender<Command>,
    _worker: JoinHandle<()>,
}

enum Command {
    Start {
        base_dir: PathBuf,
        reply: Sender<Result<PathBuf, String>>,
    },
    Stop {
        reply: Sender<Result<PathBuf, String>>,
    },
}

struct Active {
    _stream: cpal::Stream,
    writer: SharedWriter,
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
        }
    }

    pub fn start(&self, base_dir: PathBuf) -> Result<PathBuf, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap()
            .tx
            .send(Command::Start {
                base_dir,
                reply: reply_tx,
            })
            .map_err(|e| e.to_string())?;
        reply_rx.recv().map_err(|e| e.to_string())?
    }

    pub fn stop(&self) -> Result<PathBuf, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap()
            .tx
            .send(Command::Stop { reply: reply_tx })
            .map_err(|e| e.to_string())?;
        reply_rx.recv().map_err(|e| e.to_string())?
    }
}

fn worker_loop(rx: Receiver<Command>) {
    let mut active: Option<Active> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Start { base_dir, reply } => {
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
            Command::Stop { reply } => match active.take() {
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
    let mic_path = session_dir.join("mic.wav");

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
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
            SampleFormat::F32 => WavSampleFormat::Float,
            _ => WavSampleFormat::Int,
        },
    };

    let wav = WavWriter::create(&mic_path, spec).context("create wav")?;
    let writer: SharedWriter = Arc::new(Mutex::new(Some(wav)));
    let err_fn = |e| eprintln!("callscribe: input stream error: {e}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[f32], _: &_| write_f32(&w, d),
                err_fn,
                None,
            )?
        }
        SampleFormat::I16 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[i16], _: &_| write_i16(&w, d),
                err_fn,
                None,
            )?
        }
        SampleFormat::U16 => {
            let w = Arc::clone(&writer);
            device.build_input_stream(
                &stream_config,
                move |d: &[u16], _: &_| write_u16(&w, d),
                err_fn,
                None,
            )?
        }
        fmt => anyhow::bail!("unsupported sample format: {fmt:?}"),
    };
    stream.play()?;

    Ok(Active {
        _stream: stream,
        writer,
        session_dir,
    })
}

fn finish(rec: Active) -> Result<PathBuf> {
    drop(rec._stream);
    if let Some(w) = rec.writer.lock().unwrap().take() {
        w.finalize().context("finalize wav")?;
    }
    Ok(rec.session_dir)
}

fn write_f32(w: &SharedWriter, data: &[f32]) {
    if let Some(ref mut w) = *w.lock().unwrap() {
        for &s in data {
            let _ = w.write_sample(s);
        }
    }
}

fn write_i16(w: &SharedWriter, data: &[i16]) {
    if let Some(ref mut w) = *w.lock().unwrap() {
        for &s in data {
            let _ = w.write_sample(s);
        }
    }
}

fn write_u16(w: &SharedWriter, data: &[u16]) {
    if let Some(ref mut w) = *w.lock().unwrap() {
        for &s in data {
            let _ = w.write_sample(s as i16);
        }
    }
}
