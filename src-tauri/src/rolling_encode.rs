//! Rolling per-track Opus encoding with an explicit publication boundary.
//!
//! ffmpeg never writes `mic.opus` / `system.opus` directly. It writes a
//! private `.part.<nonce>`, exits cleanly, then a second full decode proves
//! that the candidate reaches the finalized WAV duration. Only then is the
//! candidate atomically published. Every other result keeps the WAV as the
//! recovery source and forces the pipeline fallback encoder.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::media_manifest::{atomic_replace_file, enforce_private_file, reserve_private_stage};
use crate::media_process::{
    no_console_std, run_bounded, run_bounded_in_slot, BoundedProcessOutput, ChildSlot,
    ProcessTermination, STDERR_LIMIT_BYTES,
};
use crate::pipeline::ffmpeg_binary;

const TRACKS: [&str; 2] = ["mic", "system"];
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILE_APPEAR_TIMEOUT: Duration = Duration::from_secs(10);
const ENCODER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_FINALIZE_TIMEOUT: Duration = Duration::from_secs(12);
const ROLLING_VALIDATION_TIMEOUT: Duration = Duration::from_secs(6);
const FALLBACK_ENCODE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const FALLBACK_VALIDATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PUMP_BUF_BYTES: usize = 64 * 1024;
const DURATION_TOLERANCE_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPublicationState {
    Published,
    FallbackRequired,
    NotRecorded,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackFinalization {
    pub track: String,
    pub state: TrackPublicationState,
    pub wav_path: PathBuf,
    pub opus_path: Option<PathBuf>,
    pub error: Option<String>,
}

impl TrackFinalization {
    fn published(track: &str, wav_path: PathBuf, opus_path: PathBuf) -> Self {
        Self {
            track: track.into(),
            state: TrackPublicationState::Published,
            wav_path,
            opus_path: Some(opus_path),
            error: None,
        }
    }

    fn fallback(track: &str, wav_path: PathBuf, error: impl Into<String>) -> Self {
        let state = if wav_path.exists() {
            TrackPublicationState::FallbackRequired
        } else {
            TrackPublicationState::NotRecorded
        };
        Self {
            track: track.into(),
            state,
            wav_path,
            opus_path: None,
            error: Some(error.into()),
        }
    }

    #[cfg(test)]
    fn is_published(&self) -> bool {
        self.state == TrackPublicationState::Published && self.opus_path.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RollingFinalizationReport {
    pub session_dir: PathBuf,
    pub tracks: Vec<TrackFinalization>,
}

impl RollingFinalizationReport {
    /// Recovery/import runs have no in-memory producer proof. They must
    /// re-encode from WAV rather than trusting a leftover nonempty `.opus`.
    pub fn conservative(session_dir: &Path) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            tracks: TRACKS
                .iter()
                .map(|track| {
                    TrackFinalization::fallback(
                        track,
                        session_dir.join(format!("{track}.wav")),
                        "no current-process rolling publication report",
                    )
                })
                .collect(),
        }
    }

    pub fn track(&self, track: &str) -> Option<&TrackFinalization> {
        self.tracks.iter().find(|result| result.track == track)
    }
}

pub struct RollingEncoder {
    active: Mutex<Option<Active>>,
}

struct Active {
    generation: u64,
    session_dir: PathBuf,
    workers: Vec<Worker>,
}

struct Worker {
    track: &'static str,
    finalize: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    child: ChildSlot,
    done_rx: Receiver<TrackFinalization>,
    join: Option<JoinHandle<TrackFinalization>>,
}

impl Default for RollingEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RollingEncoder {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    pub fn active_generation(&self) -> Option<u64> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|active| active.generation)
    }

    pub fn start(&self, session_dir: &Path, generation: u64) -> Result<(), String> {
        let mut guard = self.active.lock().unwrap();
        if let Some(active) = guard.as_ref() {
            return Err(format!(
                "rolling encoder already active for generation {} at {}",
                active.generation,
                active.session_dir.display()
            ));
        }

        let mut workers = Vec::with_capacity(TRACKS.len());
        for track in TRACKS {
            let wav = session_dir.join(format!("{track}.wav"));
            let final_opus = session_dir.join(format!("{track}.opus"));
            let staged_opus = unique_stage_path(&final_opus);
            let finalize = Arc::new(AtomicBool::new(false));
            let cancel = Arc::new(AtomicBool::new(false));
            let child = ChildSlot::default();
            let (done_tx, done_rx) = mpsc::channel();
            let worker_finalize = Arc::clone(&finalize);
            let worker_cancel = Arc::clone(&cancel);
            let worker_child = child.clone();
            let join = thread::spawn(move || {
                let result = tail_encode(
                    track,
                    &wav,
                    &staged_opus,
                    &final_opus,
                    &worker_finalize,
                    &worker_cancel,
                    &worker_child,
                );
                let _ = done_tx.send(result.clone());
                result
            });
            workers.push(Worker {
                track,
                finalize,
                cancel,
                child,
                done_rx,
                join: Some(join),
            });
        }

        *guard = Some(Active {
            generation,
            session_dir: session_dir.to_path_buf(),
            workers,
        });
        Ok(())
    }

    /// Finalize all tracks and return the only proof the pipeline may use for
    /// a rolling artifact. Timeout cancels and kills/reaps each current child,
    /// then joins every worker; dropping a detached worker is not an option.
    pub fn stop_and_persist(
        &self,
        expected_generation: u64,
        expected_session_dir: Option<&Path>,
    ) -> Option<RollingFinalizationReport> {
        let mut guard = self.active.lock().unwrap();
        if let Some(active) = guard.as_ref() {
            let session_mismatch = expected_session_dir
                .map(|expected| expected != active.session_dir)
                .unwrap_or(false);
            if active.generation != expected_generation || session_mismatch {
                eprintln!(
                    "aftercalls: stale rolling stop rejected (active generation {} at {:?}, requested generation {} at {:?})",
                    active.generation,
                    active.session_dir,
                    expected_generation,
                    expected_session_dir
                );
                return expected_session_dir.map(RollingFinalizationReport::conservative);
            }
        }
        let active = guard.take();
        drop(guard);
        let Some(mut active) = active else {
            return expected_session_dir.map(RollingFinalizationReport::conservative);
        };

        for worker in &active.workers {
            worker.finalize.store(true, Ordering::SeqCst);
        }

        let deadline = Instant::now() + STOP_FINALIZE_TIMEOUT;
        let mut received: BTreeMap<&'static str, TrackFinalization> = BTreeMap::new();
        for worker in &active.workers {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if let Ok(result) = worker.done_rx.recv_timeout(remaining) {
                received.insert(worker.track, result);
            }
        }

        // Every worker that missed the deadline gets an explicit cancellation
        // and its exact encoder/validator child is killed + reaped.
        for worker in &active.workers {
            if !received.contains_key(worker.track) {
                worker.cancel.store(true, Ordering::SeqCst);
                let _ = worker.child.kill_and_reap();
            }
        }

        let mut tracks = Vec::with_capacity(active.workers.len());
        for mut worker in active.workers.drain(..) {
            let joined = match worker.join.take().expect("rolling join present").join() {
                Ok(result) => result,
                Err(_) => TrackFinalization::fallback(
                    worker.track,
                    active.session_dir.join(format!("{}.wav", worker.track)),
                    "rolling worker panicked",
                ),
            };
            tracks.push(received.remove(worker.track).unwrap_or(joined));
        }

        Some(RollingFinalizationReport {
            session_dir: active.session_dir,
            tracks,
        })
    }
}

fn tail_encode(
    track: &str,
    wav: &Path,
    staged_opus: &Path,
    final_opus: &Path,
    finalize: &AtomicBool,
    cancel: &AtomicBool,
    child_slot: &ChildSlot,
) -> TrackFinalization {
    if !wait_for_file(wav, finalize, cancel) {
        return TrackFinalization::fallback(track, wav.to_path_buf(), "WAV never appeared");
    }
    let _stage_guard = match reserve_private_stage(staged_opus) {
        Ok(guard) => guard,
        Err(e) => {
            return TrackFinalization::fallback(
                track,
                wav.to_path_buf(),
                format!("reserve rolling Opus stage: {e:#}"),
            )
        }
    };

    let mut command = encoder_command(staged_opus);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return TrackFinalization::fallback(
                track,
                wav.to_path_buf(),
                format!("spawn rolling ffmpeg: {e}"),
            )
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return TrackFinalization::fallback(track, wav.to_path_buf(), "ffmpeg stdin unavailable");
    };
    let stderr_join = child.stderr.take().map(spawn_bounded_stderr_drain);
    if let Err(mut child) = child_slot.install(child) {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(join) = stderr_join {
            let _ = join.join();
        }
        return TrackFinalization::fallback(
            track,
            wav.to_path_buf(),
            "rolling child slot was occupied",
        );
    }

    let mut file = match std::fs::File::open(wav) {
        Ok(file) => file,
        Err(e) => {
            drop(stdin);
            let _ = child_slot.kill_and_reap();
            if let Some(join) = stderr_join {
                let _ = join.join();
            }
            return TrackFinalization::fallback(track, wav.to_path_buf(), format!("open WAV: {e}"));
        }
    };

    let mut buf = vec![0u8; PUMP_BUF_BYTES];
    let write_error = loop {
        if cancel.load(Ordering::SeqCst) {
            break Some("rolling encode cancelled".to_string());
        }
        let pumped = drain_to_eof(&mut file, &mut stdin, &mut buf);
        if let Some(error) = pumped.error {
            break Some(error);
        }
        if finalize.load(Ordering::SeqCst) {
            let tail = drain_to_eof(&mut file, &mut stdin, &mut buf);
            break tail.error;
        }
        if !pumped.moved_any {
            thread::sleep(POLL_INTERVAL);
        }
    };

    drop(stdin);
    let termination = child_slot.wait(ENCODER_EXIT_TIMEOUT, Some(cancel));
    let stderr = stderr_join
        .and_then(|join| join.join().ok())
        .unwrap_or_default();

    if let Some(error) = write_error {
        let _ = child_slot.kill_and_reap();
        return TrackFinalization::fallback(track, wav.to_path_buf(), error);
    }
    match termination {
        ProcessTermination::Exited(status) if status.success() => {}
        other => {
            let detail = if stderr.is_empty() {
                format!("rolling ffmpeg did not exit cleanly: {other:?}")
            } else {
                format!("rolling ffmpeg did not exit cleanly: {other:?}: {stderr}")
            };
            return TrackFinalization::fallback(track, wav.to_path_buf(), detail);
        }
    }

    match validate_and_publish(
        staged_opus,
        final_opus,
        wav,
        ROLLING_VALIDATION_TIMEOUT,
        child_slot,
        Some(cancel),
    ) {
        Ok(()) => TrackFinalization::published(track, wav.to_path_buf(), final_opus.to_path_buf()),
        Err(e) => TrackFinalization::fallback(
            track,
            wav.to_path_buf(),
            format!("rolling Opus validation failed: {e:#}"),
        ),
    }
}

struct PumpResult {
    moved_any: bool,
    error: Option<String>,
}

fn drain_to_eof(file: &mut std::fs::File, stdin: &mut impl Write, buf: &mut [u8]) -> PumpResult {
    let mut moved_any = false;
    loop {
        match file.read(buf) {
            Ok(0) => {
                return PumpResult {
                    moved_any,
                    error: None,
                }
            }
            Ok(n) => {
                if let Err(e) = stdin.write_all(&buf[..n]) {
                    return PumpResult {
                        moved_any,
                        error: Some(format!("write rolling ffmpeg stdin: {e}")),
                    };
                }
                moved_any = true;
            }
            Err(e) => {
                return PumpResult {
                    moved_any,
                    error: Some(format!("read growing WAV: {e}")),
                }
            }
        }
    }
}

fn wait_for_file(path: &Path, finalize: &AtomicBool, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + FILE_APPEAR_TIMEOUT;
    loop {
        if path.exists() {
            return true;
        }
        if cancel.load(Ordering::SeqCst) || finalize.load(Ordering::SeqCst) {
            return path.exists();
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn encoder_command(staged_opus: &Path) -> Command {
    let mut command = Command::new(ffmpeg_binary());
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("wav")
        .arg("-ignore_length")
        .arg("1")
        .arg("-i")
        .arg("pipe:0")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("libopus")
        .arg("-b:a")
        .arg("32k")
        // The private stage suffix is intentionally not `.opus`; declare the
        // muxer explicitly so ffmpeg never guesses from `.part.<uuid>`.
        .arg("-f")
        .arg("opus")
        .arg("-y")
        .arg(staged_opus)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    no_console_std(&mut command);
    command
}

fn spawn_bounded_stderr_drain<R>(mut stderr: R) -> JoinHandle<String>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut kept = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        let mut truncated = false;
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = STDERR_LIMIT_BYTES.saturating_sub(kept.len());
                    let take = room.min(n);
                    kept.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        let mut text = String::from_utf8_lossy(&kept).trim().to_string();
        if truncated {
            text.push_str(" [truncated]");
        }
        text
    })
}

#[derive(Debug)]
struct ValidationEvidence {
    exit_success: bool,
    timed_out: bool,
    progress_end: bool,
    decoded_duration_ms: Option<u64>,
}

fn validate_and_publish(
    staged_opus: &Path,
    final_opus: &Path,
    wav: &Path,
    timeout: Duration,
    child_slot: &ChildSlot,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let expected_duration_ms = wav_duration_ms(wav)?;
    let evidence = decode_validation(staged_opus, timeout, child_slot, cancel)?;
    assess_validation(expected_duration_ms, &evidence)?;

    // Flush the completed candidate itself before publishing its directory
    // entry. ffmpeg has closed it by this point.
    enforce_private_file(staged_opus)?;
    crate::media_manifest::sync_staged_file(staged_opus)
        .with_context(|| format!("sync staged Opus {}", staged_opus.display()))?;
    atomic_replace_file(staged_opus, final_opus).with_context(|| {
        format!(
            "publish validated Opus {} -> {}",
            staged_opus.display(),
            final_opus.display()
        )
    })
}

fn decode_validation(
    staged_opus: &Path,
    timeout: Duration,
    child_slot: &ChildSlot,
    cancel: Option<&AtomicBool>,
) -> Result<ValidationEvidence> {
    let mut command = Command::new(ffmpeg_binary());
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(staged_opus)
        .arg("-map")
        .arg("0:a:0")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .arg("-f")
        .arg("null")
        .arg("-");
    let out = run_bounded_in_slot(command, timeout, STDERR_LIMIT_BYTES, child_slot, cancel)
        .context("run full Opus decode validation")?;
    Ok(validation_evidence(&out))
}

fn validation_evidence(out: &BoundedProcessOutput) -> ValidationEvidence {
    let progress = String::from_utf8_lossy(&out.stdout);
    let mut decoded_duration_ms = None;
    let mut progress_end = false;
    for line in progress.lines() {
        if line == "progress=end" {
            progress_end = true;
        } else if let Some(value) = line.strip_prefix("out_time_us=") {
            if let Ok(micros) = value.parse::<u64>() {
                decoded_duration_ms = Some(micros / 1000);
            }
        }
    }
    ValidationEvidence {
        exit_success: out.success(),
        timed_out: matches!(out.termination, ProcessTermination::TimedOut),
        progress_end,
        decoded_duration_ms,
    }
}

fn assess_validation(expected_duration_ms: u64, evidence: &ValidationEvidence) -> Result<()> {
    if evidence.timed_out {
        anyhow::bail!("full decode timed out");
    }
    if !evidence.exit_success {
        anyhow::bail!("full decode exited nonzero");
    }
    if !evidence.progress_end {
        anyhow::bail!("full decode did not reach progress=end");
    }
    let decoded = evidence
        .decoded_duration_ms
        .context("full decode did not report a duration")?;
    if expected_duration_ms.abs_diff(decoded) > DURATION_TOLERANCE_MS {
        anyhow::bail!(
            "decoded duration {decoded}ms differs from finalized WAV {expected_duration_ms}ms by more than {DURATION_TOLERANCE_MS}ms"
        );
    }
    Ok(())
}

fn wav_duration_ms(wav: &Path) -> Result<u64> {
    let reader = hound::WavReader::open(wav)
        .with_context(|| format!("open finalized WAV {}", wav.display()))?;
    let sample_rate = reader.spec().sample_rate as u64;
    if sample_rate == 0 {
        anyhow::bail!("finalized WAV has zero sample rate");
    }
    let duration_ms = reader.duration() as u64 * 1000 / sample_rate;
    if duration_ms == 0 {
        anyhow::bail!("finalized WAV has zero duration");
    }
    Ok(duration_ms)
}

fn unique_stage_path(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "track.opus".into());
    final_path.with_file_name(format!("{name}.part.{}", uuid::Uuid::new_v4()))
}

/// Safe whole-WAV fallback used by the pipeline. It has the same private-stage,
/// clean-exit, full-decode, duration-match, and atomic-publication boundary as
/// the rolling producer.
pub fn fallback_encode_track(session_dir: &Path, track: &str) -> TrackFinalization {
    let wav = session_dir.join(format!("{track}.wav"));
    if !wav.exists() {
        return TrackFinalization::fallback(track, wav, "finalized WAV is not present");
    }
    let final_opus = session_dir.join(format!("{track}.opus"));
    let staged_opus = unique_stage_path(&final_opus);
    let _stage_guard = match reserve_private_stage(&staged_opus) {
        Ok(guard) => guard,
        Err(e) => {
            return TrackFinalization::fallback(
                track,
                wav,
                format!("reserve fallback Opus stage: {e:#}"),
            )
        }
    };
    let mut command = Command::new(ffmpeg_binary());
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(&wav)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("libopus")
        .arg("-b:a")
        .arg("32k")
        .arg("-f")
        .arg("opus")
        .arg(&staged_opus);
    let encoded = match run_bounded(command, FALLBACK_ENCODE_TIMEOUT, STDERR_LIMIT_BYTES) {
        Ok(out) if out.success() => out,
        Ok(out) => {
            return TrackFinalization::fallback(
                track,
                wav,
                format!("fallback ffmpeg failed: {}", out.diagnostic()),
            )
        }
        Err(e) => {
            return TrackFinalization::fallback(track, wav, format!("spawn fallback ffmpeg: {e:#}"))
        }
    };
    drop(encoded);

    let slot = ChildSlot::default();
    match validate_and_publish(
        &staged_opus,
        &final_opus,
        &wav,
        FALLBACK_VALIDATION_TIMEOUT,
        &slot,
        None,
    ) {
        Ok(()) => TrackFinalization::published(track, wav, final_opus),
        Err(e) => TrackFinalization::fallback(
            track,
            wav,
            format!("fallback Opus validation failed: {e:#}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "aftercalls-rolling-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn evidence(
        exit_success: bool,
        timed_out: bool,
        progress_end: bool,
        decoded_duration_ms: Option<u64>,
    ) -> ValidationEvidence {
        ValidationEvidence {
            exit_success,
            timed_out,
            progress_end,
            decoded_duration_ms,
        }
    }

    #[test]
    fn validation_rejects_nonzero_exit() {
        let err =
            assess_validation(60_000, &evidence(false, false, true, Some(60_000))).unwrap_err();
        assert!(err.to_string().contains("nonzero"));
    }

    #[test]
    fn validation_rejects_truncated_decode_without_end_marker() {
        let err =
            assess_validation(60_000, &evidence(true, false, false, Some(20_000))).unwrap_err();
        assert!(err.to_string().contains("progress=end"));
    }

    #[test]
    fn validation_rejects_duration_mismatch() {
        let err =
            assess_validation(60_000, &evidence(true, false, true, Some(40_000))).unwrap_err();
        assert!(err.to_string().contains("differs"));
    }

    #[test]
    fn validation_rejects_short_recording_beyond_codec_scale_tolerance() {
        let err = assess_validation(2_000, &evidence(true, false, true, Some(1_499)))
            .unwrap_err();
        assert!(err.to_string().contains("differs"));
        assess_validation(2_000, &evidence(true, false, true, Some(1_500)))
            .expect("500ms boundary remains accepted");
    }

    #[test]
    fn private_stage_command_declares_opus_muxer() {
        let command = encoder_command(Path::new("mic.opus.part.test"));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["-f", "opus"]));
    }

    #[test]
    #[ignore = "requires an ffmpeg binary with libopus"]
    fn fallback_round_trip_publishes_only_after_full_decode() {
        let scratch = Scratch::new();
        let wav = scratch.0.join("mic.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
        for sample in 0..16_000 {
            let value =
                ((sample as f32 / 16_000.0 * std::f32::consts::TAU * 440.0).sin() * 8_000.0) as i16;
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();

        let result = fallback_encode_track(&scratch.0, "mic");
        assert_eq!(
            result.state,
            TrackPublicationState::Published,
            "{:?}",
            result.error
        );
        assert!(scratch.0.join("mic.opus").exists());
        assert!(!std::fs::read_dir(&scratch.0)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".part.")));
    }

    #[test]
    fn conservative_report_never_trusts_existing_nonempty_opus() {
        let dir = PathBuf::from("/tmp/aftercalls-conservative-report");
        let report = RollingFinalizationReport::conservative(&dir);
        assert!(report.tracks.iter().all(|track| !track.is_published()));
    }

    #[test]
    fn stale_stop_does_not_take_new_generation_workers() {
        let encoder = RollingEncoder::new();
        *encoder.active.lock().unwrap() = Some(Active {
            generation: 2,
            session_dir: PathBuf::from("/recordings/new"),
            workers: Vec::new(),
        });
        let _ = encoder.stop_and_persist(1, Some(Path::new("/recordings/old")));
        assert_eq!(encoder.active_generation(), Some(2));
    }
}
