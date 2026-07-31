//! Rolling Opus encode during the call (chunked-upload).
//!
//! The end-of-call wait used to be dominated by whole-file ffmpeg
//! encodes at stop: mix mic+system, then compress mic / system / mixed
//! to Opus — four serial passes over the entire recording. This module
//! moves the per-channel encode OFF the end of the call by running a
//! long-lived ffmpeg per track (`mic`, `system`) that tails the growing
//! WAV and streams it into a `.opus` DURING the call. At stop only a
//! tiny final tail + the Opus/Ogg flush remain, so the `.opus` is ready
//! almost immediately.
//!
//! ## Feed model — tail the WAV into ffmpeg's stdin
//! Each worker opens the recorder's growing `<track>.wav`, reads new
//! bytes as they land, and writes them to a
//! `ffmpeg -f wav -ignore_length 1 -i pipe:0 …` child that encodes
//! continuously to `<track>.opus`. `-ignore_length 1` is the canonical
//! way to stream a WAV whose header length field is a placeholder (the
//! recorder patches it only at finalize). Closing ffmpeg's stdin at stop
//! makes it flush the Opus stream + Ogg trailer and exit with a complete
//! file. If the agent is killed mid-call the pipe's write end closes,
//! ffmpeg sees EOF and finalizes on its own — and either way the raw WAV
//! is always on disk for orphan-resume.
//!
//! ## Best-effort, always — a pure optimization with a safety net
//! Nothing here can lose a recording or fail a call. A spawn failure, a
//! never-appearing track (mic-only self-note has no `system.wav`), a
//! wedged encoder, a crash — every path degrades to "no rolling `.opus`",
//! and `pipeline::run_inner` compresses the raw WAV at stop exactly as it
//! did before (the unchanged fallback). `mixed` is deliberately NOT
//! produced: the backend regenerates it server-side post-transcribe
//! (`mix::ensure_mixed_audio`) for playback + peaks.
//!
//! Managed at process scope in `lib.rs` (sibling of `Recorder` /
//! `ScreenRecorder`) so `do_start` / `do_stop` reach the same session.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::pipeline::ffmpeg_binary;

/// Per-channel tracks the rolling encoder drives. Filenames match the
/// recorder's WAV outputs and the pipeline's `.opus` expectations.
const TRACKS: [&str; 2] = ["mic", "system"];

/// Poll interval while tailing the growing WAV (and while waiting for it
/// to first appear).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long a worker waits for its `<track>.wav` to appear before giving
/// up (→ no rolling `.opus`, pipeline falls back). The recorder creates
/// the WAV before `start()` returns, so in practice this is instant;
/// a track that never records (mic-only self-note → no `system.wav`)
/// hits this ceiling or the finalize signal, whichever comes first.
const FILE_APPEAR_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounded wait for a track's ffmpeg to flush + exit after stdin close.
/// The tail is tiny by stop (we streamed all along), so this is ~instant;
/// the ceiling only guards a wedged encoder.
const FFMPEG_EXIT_TIMEOUT: Duration = Duration::from_secs(6);

/// Total time `stop_and_persist` will block waiting for every worker to
/// finalize. Typically well under a second; bounded so a wedged encoder
/// can never hang the stop path (the track just falls back to a
/// compress-at-stop).
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(8);

/// Read buffer for the WAV→ffmpeg pump.
const PUMP_BUF_BYTES: usize = 64 * 1024;

pub struct RollingEncoder {
    active: Mutex<Option<Active>>,
}

struct Active {
    session_dir: PathBuf,
    workers: Vec<Worker>,
}

struct Worker {
    track: &'static str,
    /// Flip → the worker drains the WAV to EOF, closes ffmpeg's stdin,
    /// waits for a clean exit, and reports via `done_rx`.
    finalize: Arc<AtomicBool>,
    /// `true` when the worker produced a usable `.opus`; `false` (or a
    /// recv timeout) means the pipeline should fall back for this track.
    done_rx: Receiver<bool>,
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

    /// Best-effort start of a rolling encode for `mic` + `system` in
    /// `session_dir`. No-op (logged) if a session is already active.
    /// Never fails — a track that can't be encoded simply won't have a
    /// rolling `.opus`, and the pipeline compresses its WAV at stop.
    pub fn start(&self, session_dir: &Path) {
        let mut guard = self.active.lock().unwrap();
        if guard.is_some() {
            eprintln!("aftercalls: rolling encoder already active — skipping");
            return;
        }

        let mut workers = Vec::with_capacity(TRACKS.len());
        for track in TRACKS {
            let wav = session_dir.join(format!("{track}.wav"));
            let opus = session_dir.join(format!("{track}.opus"));
            let finalize = Arc::new(AtomicBool::new(false));
            let (done_tx, done_rx) = mpsc::channel();
            let finalize_worker = Arc::clone(&finalize);
            thread::spawn(move || {
                let ok = tail_encode(&wav, &opus, &finalize_worker);
                // Receiver may be gone if stop_and_persist already timed
                // out and dropped Active — ignore the send error.
                let _ = done_tx.send(ok);
            });
            workers.push(Worker {
                track,
                finalize,
                done_rx,
            });
        }

        eprintln!(
            "aftercalls: rolling encode started for {}",
            session_dir.display()
        );
        *guard = Some(Active {
            session_dir: session_dir.to_path_buf(),
            workers,
        });
    }

    /// Signal every worker to finalize its `.opus`, then wait (bounded)
    /// for them to finish so the pipeline sees complete files. No-op when
    /// idle. Fully best-effort: a track that doesn't finalize in time is
    /// left for the pipeline's compress-at-stop fallback.
    pub fn stop_and_persist(&self, session_dir: &Path) {
        let active = {
            let mut guard = self.active.lock().unwrap();
            guard.take()
        };
        let Some(active) = active else {
            return;
        };
        if active.session_dir != session_dir {
            eprintln!(
                "aftercalls: rolling encode stop session mismatch (active {:?} vs {:?}); finalizing active",
                active.session_dir, session_dir
            );
        }

        // Signal all workers first, then collect — so both tracks drain
        // in parallel rather than serially.
        for w in &active.workers {
            w.finalize.store(true, Ordering::SeqCst);
        }

        let deadline = Instant::now() + FINALIZE_TIMEOUT;
        for w in &active.workers {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            match w.done_rx.recv_timeout(remaining) {
                Ok(true) => eprintln!("aftercalls: rolling {} encode ready", w.track),
                Ok(false) => eprintln!(
                    "aftercalls: rolling {} encode produced nothing — pipeline will compress the WAV",
                    w.track
                ),
                Err(_) => eprintln!(
                    "aftercalls: rolling {} encode didn't finalize in time — pipeline will compress the WAV",
                    w.track
                ),
            }
        }
    }
}

/// Tail `wav` into a long-lived ffmpeg encoding to `opus`. Returns `true`
/// only when ffmpeg exited cleanly and wrote a non-empty `.opus` without
/// a mid-stream write error. Any other outcome returns `false` (the
/// pipeline falls back to compressing the WAV at stop).
fn tail_encode(wav: &Path, opus: &Path, finalize: &AtomicBool) -> bool {
    // Wait for the recorder to create the WAV. A track that never
    // records (or an instant stop) bails without spawning ffmpeg.
    if !wait_for_file(wav, finalize) {
        return false;
    }

    let mut child = match spawn_encoder(opus) {
        Some(c) => c,
        None => return false,
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let mut file = match std::fs::File::open(wav) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("aftercalls: rolling encode open {} failed: {e}", wav.display());
            drop(stdin);
            let _ = wait_child_bounded(&mut child);
            return false;
        }
    };

    let mut buf = vec![0u8; PUMP_BUF_BYTES];
    let mut write_ok = true;
    loop {
        let pumped = drain_to_eof(&mut file, &mut stdin, &mut buf);
        if !pumped.ok {
            write_ok = false;
            break;
        }
        if finalize.load(Ordering::SeqCst) {
            // `stop_and_persist` sets the flag only AFTER the recorder has
            // fully finalized the WAV (all bytes flushed to disk), so one
            // more read-to-EOF captures the final tail deterministically.
            let tail = drain_to_eof(&mut file, &mut stdin, &mut buf);
            if !tail.ok {
                write_ok = false;
            }
            break;
        }
        if !pumped.moved_any {
            thread::sleep(POLL_INTERVAL);
        }
    }

    // EOF on ffmpeg's stdin → it flushes the Opus stream + Ogg trailer.
    drop(stdin);
    let exited_ok = wait_child_bounded(&mut child);

    let ok = write_ok && exited_ok && file_non_empty(opus);
    if !ok {
        // A failed encode can leave a NON-EMPTY but TRUNCATED `.opus`: the
        // live-flushed body is on disk, but the finalizing Ogg EOS/tail
        // page never got written (the pipe broke mid-stream, or ffmpeg was
        // SIGKILLed during the post-stop flush). `pipeline::ensure_track_opus`
        // judges usability by file size ALONE (`len() > 0`), so a truncated
        // file would sail past the whole-WAV compress-at-stop fallback and a
        // partial recording would be uploaded — then pipeline-success cleanup
        // deletes the raw WAV = lost audio. Remove the bad output so the
        // size check correctly sees it absent and the fallback fires.
        // Best-effort — ignore errors (nothing else to do here).
        let _ = std::fs::remove_file(opus);
    }
    ok
}

struct PumpResult {
    /// No read/write error occurred.
    ok: bool,
    /// At least one byte was moved this pass.
    moved_any: bool,
}

/// Read every currently-available byte from `file` and write it to
/// `stdin`, stopping at EOF (a growing file returns `Ok(0)` when the
/// reader has caught up). Never blocks waiting for more data.
fn drain_to_eof(file: &mut std::fs::File, stdin: &mut impl Write, buf: &mut [u8]) -> PumpResult {
    let mut moved_any = false;
    loop {
        match file.read(buf) {
            Ok(0) => return PumpResult { ok: true, moved_any },
            Ok(n) => {
                if stdin.write_all(&buf[..n]).is_err() {
                    // ffmpeg exited / pipe broke — stop feeding it.
                    return PumpResult { ok: false, moved_any };
                }
                moved_any = true;
            }
            Err(e) => {
                eprintln!("aftercalls: rolling encode read error: {e}");
                return PumpResult { ok: false, moved_any };
            }
        }
    }
}

/// Poll for `path` to exist until it appears, the finalize flag flips, or
/// the appear-timeout elapses. Returns whether the file exists at the end.
fn wait_for_file(path: &Path, finalize: &AtomicBool) -> bool {
    let deadline = Instant::now() + FILE_APPEAR_TIMEOUT;
    loop {
        if path.exists() {
            return true;
        }
        if finalize.load(Ordering::SeqCst) {
            // Stop requested before the file ever appeared (e.g. a
            // mic-only self-note has no system.wav).
            return path.exists();
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Spawn the long-lived Opus encoder reading a streamed WAV from stdin.
/// Output spec (mono 16 kHz Opus @ 32 kbps) matches
/// `pipeline::compress_for_upload` so the rolling and fallback `.opus`
/// are interchangeable.
fn spawn_encoder(opus: &Path) -> Option<Child> {
    let mut cmd = Command::new(ffmpeg_binary());
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        // Stream a WAV whose header length is a placeholder (patched only
        // at recorder finalize) — read PCM until stdin EOF.
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
        .arg("-y")
        .arg(opus)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_console_std(&mut cmd);
    match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("aftercalls: rolling encoder spawn failed: {e}");
            None
        }
    }
}

/// Wait for `child` to exit, bounded; hard-kill if it wedges. Returns
/// whether it exited with a success status.
fn wait_child_bounded(child: &mut Child) -> bool {
    let deadline = Instant::now() + FFMPEG_EXIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

fn file_non_empty(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

/// Suppress the transient Windows console window on a sync std child
/// (learning #91). No-op elsewhere. Mirrors
/// `screen_recorder::no_console_std`.
#[cfg(windows)]
fn no_console_std(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_std(_cmd: &mut Command) {}
