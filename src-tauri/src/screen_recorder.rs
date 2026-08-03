//! Screen-capture subprocess (#302 — per-call screen / window / area capture).
//!
//! Optionally records the screen as ONE mp4 video for a Call's duration,
//! alongside the existing mic + system audio. It is a **stored media
//! asset only** — the video NEVER goes to transcription / summarization /
//! co-pilot (mirrors the notes-never-to-AI rule).
//!
//! ## Best-effort, always
//! A capture start/stop failure — a missing capture binary, a spawn
//! error, a monitor that vanished, a finalize hiccup — must NEVER block
//! or fail the audio recording. Every entry point here degrades to
//! "no video captured" and the call records exactly as it does today.
//!
//! ## Per-call source, capability seam
//! [`ScreenRecorder::start`] takes a [`CaptureSource`] (Screen / Window /
//! Region) instead of resolving a fixed monitor from prefs. Each platform
//! backend advertises which source kinds it can drive via
//! [`supported_source_kinds`]; the chooser hides the rest.
//!
//! ## Subprocess, not in-process
//! * **Linux/wlroots** shells out to the system `gpu-screen-recorder`
//!   binary (Screen via `-w <monitor>`, Window via the native picker
//!   handoff `-w portal`, Region via `-w region -region <geo>` after a
//!   `slurp` drag-select). Its lifetime is tied to the agent with
//!   `PR_SET_PDEATHSIG` exactly like the `parec` children in `recorder.rs`.
//! * **Windows** drives the bundled media sidecar (`pipeline::ffmpeg_binary`)
//!   with the `gdigrab` input (Screen/Region via a monitor-rect crop of
//!   `-i desktop`, Window via `-i title=<title>`). The child is bound to a
//!   Job Object (`KILL_ON_JOB_CLOSE`) — the PDEATHSIG analog — so it dies
//!   with the agent. gdigrab title capture can render **black** for
//!   GPU-accelerated / occluded / minimized windows (documented v1 caveat;
//!   Windows.Graphics.Capture is the robust future backend behind this same
//!   seam — NOT built here).
//! * **macOS** is a stub that advertises zero source kinds so the whole
//!   surface stays hidden until a native ScreenCaptureKit path lands.
//!   // TODO(#302 mac): native ScreenCaptureKit capture backend.
//!
//! ## Runtime-detected, gracefully absent
//! The capture backend is detected at runtime; when unavailable the feature
//! is silently unavailable (logged + skipped) — never a hard failure. On
//! Windows availability additionally probes the sidecar for `gdigrab` and
//! an H.264 encoder (see [`capture_available`]).
//!
//! ## moov-at-end
//! Both backends write the mp4 with the `moov` atom at the END, so the raw
//! file does not seek in a `<video>` element until it is remuxed with
//! `-movflags +faststart` (done best-effort in the uploader, not here).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Relative filename of the raw capture inside `<session_dir>/screen/`.
pub const RECORDING_FILENAME: &str = "recording.mp4";
/// Sidecar metadata the uploader reads to remux + multipart-upload.
pub const META_FILENAME: &str = "recording.json";
/// Subdirectory under the session dir that holds the screen assets.
pub const SCREEN_SUBDIR: &str = "screen";

/// The one codec v1 captures with. Kept as a constant so the arg builder,
/// the persisted metadata, and the upload `codec` field can't drift.
const VIDEO_CODEC: &str = "h264";

// ── AppPrefs-derived start config ────────────────────────────────────

/// The per-user capture knobs resolved from `AppPrefs` at record-start.
/// Kept backend-agnostic so every platform backend consumes the same
/// struct. The capture *target* is a separate [`CaptureSource`] arg (it
/// became per-call in the #302 follow-up).
#[derive(Clone, Debug)]
pub struct StartConfig {
    /// Frames per second; clamped to [10, 30].
    pub fps: u32,
    /// `"720p" | "1080p" | "native"` — a fit-within resolution cap.
    pub resolution: Option<String>,
    /// CBR bitrate ceiling in kbps (bounds the storage cost of a shared
    /// 4K video playback — the dominant cost lever).
    pub bitrate_kbps: u32,
}

/// The per-call capture target. Built from the chooser's `kind` + `target`
/// in `lib.rs::start_screen_source`; interpreted per-platform in
/// [`spawn_capture`].
#[derive(Clone, Debug)]
pub enum CaptureSource {
    /// A whole monitor. `None` = the focused/primary monitor.
    Screen { monitor: Option<String> },
    /// A single application window. `None` = the native picker (Linux
    /// portal handoff); `Some(title)` = an exact window title (Windows).
    Window { target: Option<String> },
    /// A drag-selected rectangle, canonical `"WxH+X+Y"` geometry.
    Region { geometry: String },
}

/// The advertised kind string for a source (matches `supported_source_kinds`
/// entries + the chooser's button ids).
pub fn source_kind_str(source: &CaptureSource) -> &'static str {
    match source {
        CaptureSource::Screen { .. } => "screen",
        CaptureSource::Window { .. } => "window",
        CaptureSource::Region { .. } => "region",
    }
}

// ── Persisted metadata (session_dir/screen/recording.json) ───────────

/// Written at capture-stop; read by `upload::upload_screen_recording`.
/// Carries everything the multipart `init` / `complete` calls need so the
/// upload step is decoupled from the capture subprocess entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenRecordingMeta {
    /// Filename of the raw mp4 inside the same `screen/` dir.
    pub file: String,
    /// video t=0 minus audio t=0, in ms (subprocess spawn latency).
    pub start_offset_ms: i64,
    /// Wall-clock capture duration in ms.
    pub duration_ms: i64,
    pub fps: i32,
    #[serde(default)]
    pub width: Option<i32>,
    #[serde(default)]
    pub height: Option<i32>,
    pub codec: String,
}

impl ScreenRecordingMeta {
    /// Read the sidecar from a session dir, if present + parseable.
    pub fn read(session_dir: &Path) -> Option<Self> {
        let path = session_dir.join(SCREEN_SUBDIR).join(META_FILENAME);
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

// ── Display / window enumeration (for the chooser + Settings) ─────────

/// One selectable monitor. `name` is the exact capture target string (the
/// gsr `-w` target on Linux, the device name on Windows); the resolution
/// feeds the picker's "3840×2160" hint.
#[derive(Clone, Debug, Serialize)]
pub struct DisplayInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// True for the compositor's focused/primary output when known.
    pub is_primary: bool,
    /// Virtual-desktop rect origin. 0 on Linux (gsr targets by name); the
    /// real offset on Windows where gdigrab needs `-offset_x/-offset_y`.
    /// Additive + `serde(default)` — old callers / payloads decode as 0.
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
}

/// One selectable top-level window (Windows). Empty everywhere else — Linux
/// uses the compositor's native window picker instead of an in-app list.
#[derive(Clone, Debug, Serialize)]
pub struct WindowInfo {
    /// The exact window title, used as the `gdigrab -i title=<t>` target.
    pub title: String,
}

// ── The recorder handle (managed in app state, sibling of Recorder) ───

/// Owns the single in-flight capture, if any. Managed at process scope in
/// `lib.rs` so `do_start` / `do_stop` reach the same session via
/// `app.state::<ScreenRecorder>()`.
pub struct ScreenRecorder {
    active: Mutex<Option<Active>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenStopReport {
    pub attempted: bool,
    pub published: bool,
    pub path: Option<PathBuf>,
    pub error: Option<String>,
}

impl ScreenStopReport {
    fn idle() -> Self {
        Self {
            attempted: false,
            published: false,
            path: None,
            error: None,
        }
    }
}

struct Active {
    generation: u64,
    backend: Box<dyn CaptureBackend>,
    session_dir: PathBuf,
    output_path: PathBuf,
    started_at: Instant,
    audio_started_at_ms: i64,
    start_offset_ms: i64,
    fps: u32,
    dims: Option<(u32, u32)>,
    /// The advertised kind ("screen"/"window"/"region") the floater reads.
    source_kind: &'static str,
}

impl Default for ScreenRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenRecorder {
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

    /// Whether the platform capture backend is available on this machine
    /// right now. Drives the Settings UI + the chooser's "capture
    /// unavailable" state.
    pub fn is_available(&self) -> bool {
        capture_available()
    }

    /// The active capture's kind ("screen"/"window"/"region") IFF a capture
    /// is running. Returns `None` when idle OR when the capture subprocess
    /// has died (denied / closed / gdigrab black-window exit) so the floater
    /// drops the cue mid-call. Uses a non-blocking `try_wait`.
    pub fn active_source_kind(&self) -> Option<String> {
        let mut guard = self.active.lock().unwrap();
        let active = guard.as_mut()?;
        if active.backend.is_running() {
            Some(active.source_kind.to_string())
        } else {
            None
        }
    }

    /// Best-effort start of a screen capture into
    /// `<session_dir>/screen/recording.mp4`. Returns `true` when a capture
    /// subprocess was spawned; `false` when capture is gracefully
    /// unavailable (binary absent, no display, spawn failed, already
    /// active). The caller MUST treat `false` as "no video" and carry on
    /// — it is NEVER a recording failure.
    pub fn start(
        &self,
        session_dir: &Path,
        generation: u64,
        source: CaptureSource,
        cfg: &StartConfig,
        audio_started_at_ms: i64,
    ) -> bool {
        if generation == 0 {
            eprintln!("aftercalls: screen capture skipped — invalid lifecycle generation");
            return false;
        }
        let mut guard = self.active.lock().unwrap();
        if guard.is_some() {
            eprintln!("aftercalls: screen capture already active — skipping");
            return false;
        }

        let screen_dir = session_dir.join(SCREEN_SUBDIR);
        if let Err(e) = crate::session_fs::ensure_private_dir(&screen_dir) {
            eprintln!("aftercalls: screen capture skipped — mkdir failed: {e}");
            return false;
        }
        let output_path = screen_dir.join(RECORDING_FILENAME);
        let kind = source_kind_str(&source);

        match spawn_capture(&source, cfg, &output_path) {
            Ok((backend, dims)) => {
                // Capture start moment vs the audio recorder's start. On the
                // ask-each-call path this also absorbs the user's pick
                // latency, which the player's frame-0-park + caption state
                // already handles.
                let video_started_at_ms = chrono::Utc::now().timestamp_millis();
                let start_offset_ms =
                    compute_start_offset_ms(audio_started_at_ms, video_started_at_ms);
                eprintln!(
                    "aftercalls: screen capture started ({kind}, offset {start_offset_ms}ms) → {}",
                    output_path.display()
                );
                *guard = Some(Active {
                    generation,
                    backend,
                    session_dir: session_dir.to_path_buf(),
                    output_path,
                    started_at: Instant::now(),
                    audio_started_at_ms,
                    start_offset_ms,
                    fps: clamp_fps(cfg.fps),
                    dims,
                    source_kind: kind,
                });
                true
            }
            Err(e) => {
                // Graceful degrade: log + skip. The audio recording is
                // completely unaffected.
                eprintln!("aftercalls: screen capture unavailable: {e:#}");
                false
            }
        }
    }

    /// Stop + finalize the active capture and persist metadata only after a
    /// clean producer exit. The call remains audio-usable on failure, but the
    /// failure is explicit so the aggregate Stop report can retain/retry it.
    pub fn stop_and_persist(
        &self,
        expected_generation: Option<u64>,
        expected_session_dir: Option<&Path>,
    ) -> ScreenStopReport {
        let mut guard = self.active.lock().unwrap();
        if let Some(active) = guard.as_ref() {
            let generation_mismatch = expected_generation
                .map(|expected| expected != active.generation)
                .unwrap_or(false);
            let session_mismatch = expected_session_dir
                .map(|expected| expected != active.session_dir)
                .unwrap_or(false);
            if generation_mismatch || session_mismatch {
                return ScreenStopReport {
                    attempted: false,
                    published: false,
                    path: None,
                    error: Some(format!(
                        "stale screen stop rejected (active generation {} at {}, requested generation {:?} at {:?})",
                        active.generation,
                        active.session_dir.display(),
                        expected_generation,
                        expected_session_dir
                    )),
                };
            }
        }
        let active = guard.take();
        drop(guard);
        let Some(mut active) = active else {
            return ScreenStopReport::idle();
        };

        let stop_requested_at_ms = chrono::Utc::now().timestamp_millis();
        let fallback_duration_ms = active.started_at.elapsed().as_millis() as i64;
        if let Err(e) = active.backend.finalize() {
            let error = format!("screen capture finalize failed: {e:#}");
            eprintln!("aftercalls: {error}");
            return ScreenStopReport {
                attempted: true,
                published: false,
                path: Some(active.output_path),
                error: Some(error),
            };
        }

        let byte_size = std::fs::metadata(&active.output_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if byte_size == 0 {
            let error = format!(
                "screen capture produced no usable file at {}",
                active.output_path.display()
            );
            eprintln!("aftercalls: {error}");
            return ScreenStopReport {
                attempted: true,
                published: false,
                path: Some(active.output_path),
                error: Some(error),
            };
        }

        if let Err(error) = crate::media_manifest::enforce_private_file(&active.output_path) {
            let error = format!("protect finalized screen capture: {error:#}");
            eprintln!("aftercalls: {error}");
            return ScreenStopReport {
                attempted: true,
                published: false,
                path: Some(active.output_path),
                error: Some(error),
            };
        }

        // Portal window selection occurs inside the child after spawn. Use
        // the finalized container duration to recover the true first-frame
        // offset instead of counting the user's picker delay as video time.
        let duration_ms = probe_media_duration_ms(&active.output_path)
            .ok()
            .filter(|duration| *duration > 0)
            .unwrap_or(fallback_duration_ms);
        let corrected_offset = corrected_start_offset_ms(
            active.audio_started_at_ms,
            stop_requested_at_ms,
            duration_ms,
        );
        let start_offset_ms = active.start_offset_ms.max(corrected_offset);

        let (width, height) = match active.dims {
            Some((w, h)) => (Some(w as i32), Some(h as i32)),
            None => (None, None),
        };
        let meta = ScreenRecordingMeta {
            file: RECORDING_FILENAME.to_string(),
            start_offset_ms,
            duration_ms,
            fps: active.fps as i32,
            width,
            height,
            codec: VIDEO_CODEC.to_string(),
        };
        let meta_path = active.session_dir.join(SCREEN_SUBDIR).join(META_FILENAME);
        let staged_meta =
            meta_path.with_file_name(format!("{META_FILENAME}.part.{}", uuid::Uuid::new_v4()));
        let persisted = (|| -> Result<()> {
            let json =
                serde_json::to_vec_pretty(&meta).context("serialize screen recording metadata")?;
            let _stage_guard = crate::media_manifest::reserve_private_stage(&staged_meta)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&staged_meta)
                .with_context(|| format!("create {}", staged_meta.display()))?;
            file.write_all(&json)
                .with_context(|| format!("write {}", staged_meta.display()))?;
            file.write_all(b"\n")
                .with_context(|| format!("finish {}", staged_meta.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", staged_meta.display()))?;
            drop(file);
            crate::media_manifest::atomic_replace_file(&staged_meta, &meta_path)?;
            crate::media_manifest::mark_screen_published(&active.session_dir, &active.output_path)?;
            Ok(())
        })();
        if let Err(e) = persisted {
            let error = format!("persist screen capture checkpoint: {e:#}");
            eprintln!("aftercalls: {error}");
            return ScreenStopReport {
                attempted: true,
                published: false,
                path: Some(active.output_path),
                error: Some(error),
            };
        }
        ScreenStopReport {
            attempted: true,
            published: true,
            path: Some(active.output_path),
            error: None,
        }
    }
}

// ── Platform capture backend ─────────────────────────────────────────

/// A running platform screen-capture session. Each platform impl drives its
/// own subprocess; `ScreenRecorder` + the `lib.rs` lifecycle need no
/// rewrite to add a backend.
trait CaptureBackend: Send {
    /// Signal a clean stop and block until the output mp4 is fully
    /// written (finalized).
    fn finalize(&mut self) -> Result<()>;
    /// Non-blocking liveness probe (`try_wait`). Lets `status` drop the cue
    /// for a capture that died mid-call (denied / closed / gdigrab exit).
    fn is_running(&mut self) -> bool;
}

fn wait_capture_child(child: &mut std::process::Child, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => anyhow::bail!("screen capture child exited with {status}"),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                child.wait().context("reap timed-out screen capture")?;
                anyhow::bail!(
                    "screen capture did not finalize within {}s; killed and reaped",
                    timeout.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(poll_error) => {
                let _ = child.kill();
                let reap = child.wait();
                return match reap {
                    Ok(_) => Err(poll_error)
                        .context("poll screen capture child (child killed and reaped)"),
                    Err(reap_error) => anyhow::bail!(
                        "poll screen capture child failed: {poll_error}; kill/reap also failed: {reap_error}"
                    ),
                };
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_capture_stderr_drain<R>(mut stderr: R) -> JoinHandle<String>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        const CAP: usize = crate::media_process::STDERR_LIMIT_BYTES;
        let mut kept = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        let mut truncated = false;
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = CAP.saturating_sub(kept.len());
                    let take = room.min(n);
                    kept.extend_from_slice(&buf[..take]);
                    truncated |= take < n;
                }
                Err(_) => break,
            }
        }
        let mut message = String::from_utf8_lossy(&kept).trim().to_string();
        if truncated {
            message.push_str(" [truncated]");
        }
        message
    })
}

// ── Linux backend — gpu-screen-recorder subprocess ───────────────────

/// Resolve the source + fps/resolution/bitrate → spawn the platform
/// capture subprocess. Returns the running backend plus the best-effort
/// output dimensions (for the stored metadata). Errors here are the
/// graceful-unavailable path — the caller degrades to "no video".
#[cfg(target_os = "linux")]
fn spawn_capture(
    source: &CaptureSource,
    cfg: &StartConfig,
    output_path: &Path,
) -> Result<(Box<dyn CaptureBackend>, Option<(u32, u32)>)> {
    use std::process::{Command, Stdio};

    let bin = locate_gsr().context("gpu-screen-recorder not found on PATH")?;
    let out_str = output_path.to_string_lossy();
    let fps = clamp_fps(cfg.fps);
    let cap = resolution_cap_box(cfg.resolution.as_deref());

    // Resolve the source → the gsr `-w` target (+ optional `-region`) and
    // best-effort output dims for the stored metadata.
    let (w_target, region, out_dims): (String, Option<String>, Option<(u32, u32)>) = match source {
        CaptureSource::Screen { monitor } => {
            let displays = enumerate_displays();
            let mon = resolve_monitor(monitor.as_deref(), &displays)
                .context("no capturable display found")?;
            let native = displays
                .iter()
                .find(|d| d.name == mon)
                .map(|d| (d.width, d.height));
            let dims = native.map(|n| fit_within(n, cap));
            (mon, None, dims)
        }
        // Window: hand off to the compositor's native picker (Wayland's
        // reliable route). `target` is ignored on Linux — the picker owns
        // the choice, and follows the window if it moves.
        CaptureSource::Window { .. } => ("portal".to_string(), None, None),
        CaptureSource::Region { geometry } => {
            // #302 security review — REQUIRE a valid geometry; never hand
            // gpu-screen-recorder the unvalidated client string. Parse it
            // and, on failure, abort the capture start (the caller degrades
            // to audio-only — the call is never broken). Rebuild the
            // canonical `WxH+X+Y` from the parsed ints so only sanitized
            // values reach the `-region` arg. Mirrors the Windows gdigrab arm.
            let (rw, rh, rx, ry) =
                parse_region_geometry(geometry).context("invalid region geometry")?;
            let canonical = format!("{rw}x{rh}+{rx}+{ry}");
            (
                "region".to_string(),
                Some(canonical),
                Some(fit_within((rw, rh), cap)),
            )
        }
    };

    let args = build_gsr_args(
        &w_target,
        fps,
        cfg.bitrate_kbps,
        cap,
        region.as_deref(),
        &out_str,
    );

    let mut command = Command::new(&bin);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Drain concurrently and retain a bounded diagnostic. An unread pipe
        // can fill and deadlock finalization; /dev/null hid the root cause.
        .stderr(Stdio::piped());

    // Tie the recorder's lifetime to the agent: if the agent is SIGKILL'd
    // (binary swap, crash, force-quit) the recorder gets SIGINT and
    // finalizes its mp4 instead of leaking + writing a stale session dir.
    // Identical to the `parec` children in recorder.rs.
    unsafe {
        use std::os::unix::process::CommandExt;
        let parent_pid = std::process::id() as libc::pid_t;
        command.pre_exec(move || {
            libc::umask(0o077);
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "screen recorder parent exited before child exec",
                ));
            }
            Ok(())
        });
    }

    let mut child = command.spawn().context("spawn gpu-screen-recorder")?;
    let stderr_join = child.stderr.take().map(spawn_capture_stderr_drain);
    Ok((
        Box::new(GpuScreenRecorder { child, stderr_join }),
        out_dims,
    ))
}

#[cfg(target_os = "linux")]
struct GpuScreenRecorder {
    child: std::process::Child,
    stderr_join: Option<JoinHandle<String>>,
}

#[cfg(target_os = "linux")]
impl Drop for GpuScreenRecorder {
    fn drop(&mut self) {
        // A backend dropped outside the normal Stop path must not leave a
        // privacy-sensitive capture process behind. Normal finalization has
        // already reaped the child, making this branch a cheap no-op.
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        if let Some(join) = self.stderr_join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(target_os = "linux")]
impl CaptureBackend for GpuScreenRecorder {
    fn finalize(&mut self) -> Result<()> {
        // `status()` may already have reaped a child that exited early.
        // Never signal a stale/reusable PID in that case.
        match self.child.try_wait() {
            Ok(Some(status)) if status.success() => return self.finish_with_stderr(Ok(())),
            Ok(Some(status)) => {
                return self.finish_with_stderr(Err(anyhow::anyhow!(
                    "screen capture child exited with {status}"
                )))
            }
            Ok(None) => {}
            Err(poll_error) => {
                let _ = self.child.kill();
                let reap = self.child.wait();
                let result = match reap {
                    Ok(_) => Err(poll_error)
                        .context("poll screen capture before stop (child killed and reaped)"),
                    Err(reap_error) => Err(anyhow::anyhow!(
                        "poll screen capture before stop failed: {poll_error}; kill/reap also failed: {reap_error}"
                    )),
                };
                return self.finish_with_stderr(result);
            }
        }
        // SIGINT → gpu-screen-recorder writes the trailing moov atom and
        // exits cleanly. `kill`+`wait` mirrors `recorder.rs::stop_child_gracefully`.
        if unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) } != 0 {
            let signal_error = std::io::Error::last_os_error();
            // The process may have exited between try_wait and kill. Polling
            // through the bounded helper safely distinguishes that race from
            // a genuinely wedged producer and still guarantees collection.
            let result = wait_capture_child(&mut self.child, Duration::from_secs(8)).with_context(
                || format!("signal screen capture child failed first: {signal_error}"),
            );
            return self.finish_with_stderr(result);
        }
        let result = wait_capture_child(&mut self.child, Duration::from_secs(8));
        self.finish_with_stderr(result)
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(target_os = "linux")]
impl GpuScreenRecorder {
    fn finish_with_stderr(&mut self, result: Result<()>) -> Result<()> {
        let diagnostic = self
            .stderr_join
            .take()
            .and_then(|join| join.join().ok())
            .unwrap_or_default();
        match (result, diagnostic.is_empty()) {
            (Ok(()), _) => Ok(()),
            (Err(error), true) => Err(error),
            (Err(error), false) => {
                Err(error).context(format!("screen capture stderr: {diagnostic}"))
            }
        }
    }
}

// ── Windows backend — bundled ffmpeg sidecar + gdigrab ───────────────

/// Windows capture: the bundled media sidecar with the `gdigrab` input.
/// The child is bound to a Job Object with `KILL_ON_JOB_CLOSE` (the
/// PDEATHSIG analog) so it dies with the agent. Stopped gracefully by
/// writing `q` to stdin (ffmpeg finalizes the mp4) then waiting.
#[cfg(windows)]
struct FfmpegGdigrabRecorder {
    child: std::process::Child,
    /// Raw Job Object handle value (0 = none). Kept OPEN for the child's
    /// lifetime so `KILL_ON_JOB_CLOSE` fires if the agent dies; closed
    /// AFTER the graceful stop in `finalize`.
    job: isize,
}

#[cfg(windows)]
impl FfmpegGdigrabRecorder {
    fn close_job(&mut self) {
        if self.job != 0 {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(
                    self.job as *mut core::ffi::c_void,
                ));
            }
            self.job = 0;
        }
    }
}

#[cfg(windows)]
impl Drop for FfmpegGdigrabRecorder {
    fn drop(&mut self) {
        // Safety net if we were dropped without an explicit finalize —
        // closing the job kills the child (KILL_ON_JOB_CLOSE).
        self.close_job();
    }
}

#[cfg(windows)]
impl CaptureBackend for FfmpegGdigrabRecorder {
    fn finalize(&mut self) -> Result<()> {
        use std::io::Write;
        // Graceful ffmpeg stop: `q` on stdin → finalize the mp4 (trailing
        // moov atom, same faststart-remux fixup as the Linux path).
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = stdin.write_all(b"q\n");
            let _ = stdin.flush();
        }

        // Bounded wait, then hard-kill so a wedged encoder never blocks the
        // stop path. MUST finish (or kill) BEFORE closing the job handle —
        // closing it early would kill the child mid-finalize and truncate
        // the mp4.
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(status)) => {
                    self.close_job();
                    anyhow::bail!("screen ffmpeg exited with {status}");
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let kill_error = self.child.kill().err();
                        // Closing the kill-on-close job is a second, exact
                        // termination mechanism if Child::kill itself fails.
                        self.close_job();
                        self.child.wait().context("reap timed-out screen ffmpeg")?;
                        match kill_error {
                            Some(error) => anyhow::bail!(
                                "screen ffmpeg did not finalize within 8s; job-close killed and reaped it after Child::kill failed: {error}"
                            ),
                            None => anyhow::bail!(
                                "screen ffmpeg did not finalize within 8s; killed and reaped"
                            ),
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(poll_error) => {
                    let _ = self.child.kill();
                    self.close_job();
                    let reap = self.child.wait();
                    return match reap {
                        Ok(_) => Err(poll_error)
                            .context("poll screen ffmpeg (child killed and reaped)"),
                        Err(reap_error) => anyhow::bail!(
                            "poll screen ffmpeg failed: {poll_error}; kill/reap also failed: {reap_error}"
                        ),
                    };
                }
            }
        }
        self.close_job();
        Ok(())
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(windows)]
fn spawn_capture(
    source: &CaptureSource,
    cfg: &StartConfig,
    output_path: &Path,
) -> Result<(Box<dyn CaptureBackend>, Option<(u32, u32)>)> {
    use std::process::{Command, Stdio};

    let bin = crate::pipeline::ffmpeg_binary();
    let encoders = probed_h264_encoders();
    if encoders.is_empty() {
        anyhow::bail!("no runtime-usable H.264 encoder available in the media sidecar");
    }

    // A compiled encoder can still fail against the real gdigrab input
    // (driver reset, device contention, unsupported frame shape). Try every
    // runtime-proven candidate and require it to survive the bounded startup
    // window before publishing the backend.
    let mut failures = Vec::new();
    for &encoder in encoders {
        let (args, dims) =
            build_gdigrab_capture(source, cfg, encoder, &output_path.to_string_lossy())?;
        let mut command = Command::new(&bin);
        command
            .args(&args)
            // stdin piped so `finalize` can write `q` for a graceful mp4 finish.
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // learning #91 — suppress the transient console window Windows would
        // otherwise flash for a console-subsystem child.
        no_console_std(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                failures.push(format!("{encoder}: spawn failed: {error}"));
                continue;
            }
        };
        // Tie the child's lifetime to the agent (PDEATHSIG analog). This is a
        // privacy boundary, not an optimization: if the Job Object cannot be
        // configured and assigned, kill/reap the child and reject capture.
        let job = match assign_kill_on_close_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                failures.push(format!("{encoder}: bind kill-on-close job failed: {error:#}"));
                let _ = std::fs::remove_file(output_path);
                continue;
            }
        };
        let mut recorder = FfmpegGdigrabRecorder { child, job };
        match require_capture_startup(&mut recorder.child, Duration::from_millis(900)) {
            Ok(()) => return Ok((Box::new(recorder), dims)),
            Err(error) => {
                failures.push(format!("{encoder}: {error:#}"));
                recorder.close_job();
                let _ = std::fs::remove_file(output_path);
            }
        }
    }
    anyhow::bail!(
        "all runtime-usable H.264 encoders failed capture startup: {}",
        failures.join("; ")
    )
}

#[cfg(windows)]
fn require_capture_startup(
    child: &mut std::process::Child,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => anyhow::bail!("capture exited during startup with {status}"),
            Ok(None) if Instant::now() >= deadline => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("poll capture during startup");
            }
        }
    }
}

/// Resolve a [`CaptureSource`] → the gdigrab argv + best-effort dims.
#[cfg(windows)]
fn build_gdigrab_capture(
    source: &CaptureSource,
    cfg: &StartConfig,
    encoder: &str,
    output: &str,
) -> Result<(Vec<String>, Option<(u32, u32)>)> {
    let fps = clamp_fps(cfg.fps);
    let cap = resolution_cap_box(cfg.resolution.as_deref());
    match source {
        CaptureSource::Window { target } => {
            let title = target
                .clone()
                .filter(|t| !t.trim().is_empty())
                .context("window capture requires a window title on Windows")?;
            let input = GdigrabInput::Window { title };
            Ok((
                build_gdigrab_args(
                    &input,
                    fps,
                    cfg.bitrate_kbps,
                    encoder,
                    Some(match cap {
                        Some((width, height)) => {
                            GdigrabScale::FitWithin { width, height }
                        }
                        // A dynamically-sized window can be odd. Native still
                        // needs even yuv420p dimensions.
                        None => GdigrabScale::Even,
                    }),
                    output,
                ),
                // Window size is dynamic (gdigrab captures the current
                // rect); the filter caps each frame but exact dims remain
                // unknown until capture.
                None,
            ))
        }
        CaptureSource::Screen { monitor } => {
            let displays = enumerate_displays();
            let d = resolve_display(monitor.as_deref(), &displays)
                .context("no capturable display found")?;
            let (w, h) = (even(d.width), even(d.height));
            let input = GdigrabInput::Desktop {
                x: d.x,
                y: d.y,
                width: w,
                height: h,
            };
            let output_dims = fit_within((w, h), cap);
            let scale = (output_dims != (w, h)).then_some(GdigrabScale::Exact {
                width: output_dims.0,
                height: output_dims.1,
            });
            Ok((
                build_gdigrab_args(
                    &input,
                    fps,
                    cfg.bitrate_kbps,
                    encoder,
                    scale,
                    output,
                ),
                Some(output_dims),
            ))
        }
        CaptureSource::Region { geometry } => {
            let (rw, rh, x, y) =
                parse_region_geometry(geometry).context("invalid region geometry")?;
            let (w, h) = (even(rw), even(rh));
            let input = GdigrabInput::Desktop {
                x,
                y,
                width: w,
                height: h,
            };
            let output_dims = fit_within((w, h), cap);
            let scale = (output_dims != (w, h)).then_some(GdigrabScale::Exact {
                width: output_dims.0,
                height: output_dims.1,
            });
            Ok((
                build_gdigrab_args(
                    &input,
                    fps,
                    cfg.bitrate_kbps,
                    encoder,
                    scale,
                    output,
                ),
                Some(output_dims),
            ))
        }
    }
}

/// Suppress the transient Windows console window on a sync std child.
#[cfg(windows)]
fn no_console_std(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Create a Job Object with `KILL_ON_JOB_CLOSE`, assign the child to it, and
/// return the raw handle value kept open for the child's lifetime. Any failure
/// is fatal to this capture attempt: running a screen recorder without a
/// parent-death boundary could continue recording after the agent exits.
#[cfg(windows)]
fn assign_kill_on_close_job(child: &std::process::Child) -> Result<isize> {
    use std::os::windows::io::AsRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).context("CreateJobObjectW")?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set_result = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if let Err(error) = set_result {
            let _ = CloseHandle(job);
            return Err(error).context("SetInformationJobObject(KILL_ON_JOB_CLOSE)");
        }
        let child_handle = HANDLE(child.as_raw_handle());
        if let Err(error) = AssignProcessToJobObject(job, child_handle) {
            let _ = CloseHandle(job);
            return Err(error).context("AssignProcessToJobObject");
        }
        Ok(job.0 as isize)
    }
}

// ── macOS / other — stub backend (surface hidden) ────────────────────

/// No capture backend on this platform yet → always the graceful-
/// unavailable path (`supported_source_kinds` is empty, so the surface
/// never renders — this arm is a defensive backstop).
// TODO(#302 mac): native ScreenCaptureKit capture backend.
#[cfg(not(any(target_os = "linux", windows)))]
fn spawn_capture(
    _source: &CaptureSource,
    _cfg: &StartConfig,
    _output_path: &Path,
) -> Result<(Box<dyn CaptureBackend>, Option<(u32, u32)>)> {
    anyhow::bail!("screen capture not implemented on this platform yet")
}

// ── Pure helpers (unit-tested) ───────────────────────────────────────

fn probe_media_duration_ms(path: &Path) -> Result<i64> {
    let mut command = std::process::Command::new(crate::pipeline::ffmpeg_binary());
    command.arg("-hide_banner").arg("-i").arg(path);
    let output = crate::media_process::run_bounded(
        command,
        Duration::from_secs(5),
        crate::media_process::STDERR_LIMIT_BYTES,
    )
    .context("probe finalized screen duration")?;
    parse_ffmpeg_duration_ms(&String::from_utf8_lossy(&output.stderr))
        .context("media probe did not report a duration")
}

fn parse_ffmpeg_duration_ms(stderr: &str) -> Option<i64> {
    let marker = "Duration: ";
    let value = stderr.split(marker).nth(1)?.split(',').next()?.trim();
    if value == "N/A" {
        return None;
    }
    let mut parts = value.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || !hours.is_finite()
        || !minutes.is_finite()
        || !seconds.is_finite()
        || hours < 0.0
        || !(0.0..60.0).contains(&minutes)
        || !(0.0..60.0).contains(&seconds)
    {
        return None;
    }
    Some(((hours * 3600.0 + minutes * 60.0 + seconds) * 1000.0).round() as i64)
}

fn corrected_start_offset_ms(
    audio_started_at_ms: i64,
    stop_requested_at_ms: i64,
    media_duration_ms: i64,
) -> i64 {
    (stop_requested_at_ms - audio_started_at_ms - media_duration_ms).max(0)
}

/// video-start minus audio-start, floored at 0 (the video subprocess
/// always spawns AFTER the audio recorder, so the delta is a small
/// positive value; a clock hiccup can't produce a negative offset).
pub fn compute_start_offset_ms(audio_started_at_ms: i64, video_started_at_ms: i64) -> i64 {
    (video_started_at_ms - audio_started_at_ms).max(0)
}

/// Clamp fps to the supported capture window [10, 30].
pub fn clamp_fps(fps: u32) -> u32 {
    fps.clamp(10, 30)
}

/// Map a resolution keyword to a fit-within bounding box. `native` (or an
/// unknown/empty keyword defaulting conservatively to 1080p) → the caller
/// omits `-s` for `None`.
pub fn resolution_cap_box(res: Option<&str>) -> Option<(u32, u32)> {
    match res.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("720p") => Some((1280, 720)),
        Some("1080p") => Some((1920, 1080)),
        Some("native") | Some("") | None => None,
        // Unknown value: default to the 1080p cap rather than trusting an
        // unbounded native capture on a possibly-4K panel.
        Some(_) => Some((1920, 1080)),
    }
}

/// Fit `native` within `cap` preserving aspect ratio (integer floor).
/// `cap = None` returns `native` unchanged. Matches gpu-screen-recorder's
/// `-s` "output resolution limit" semantics so the stored width/height
/// track the file the recorder actually writes.
pub fn fit_within(native: (u32, u32), cap: Option<(u32, u32)>) -> (u32, u32) {
    let (nw, nh) = native;
    let Some((cw, ch)) = cap else {
        return native;
    };
    if nw == 0 || nh == 0 {
        return native;
    }
    if nw <= cw && nh <= ch {
        return native;
    }
    // Scale to the tighter of the two axis ratios.
    let scale = (cw as f64 / nw as f64).min(ch as f64 / nh as f64);
    let w = ((nw as f64 * scale).round() as u32).max(2) & !1; // keep even
    let h = ((nh as f64 * scale).round() as u32).max(2) & !1;
    (w, h)
}

/// Round down to the nearest even value (min 2). H.264 + yuv420p require
/// even dimensions; a drag-selected region can be odd.
pub fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

/// Parse a canonical `"WxH+X+Y"` geometry (as `slurp` emits and the Windows
/// region overlay returns). `X`/`Y` may be negative on a multi-monitor
/// virtual desktop. `None` on any malformed input or a zero dimension.
pub fn parse_region_geometry(geo: &str) -> Option<(u32, u32, i32, i32)> {
    let g = geo.trim();
    let (w_str, rest) = g.split_once('x')?;
    let mut parts = rest.split('+');
    let h_str = parts.next()?;
    let x_str = parts.next()?;
    let y_str = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let w: u32 = w_str.trim().parse().ok()?;
    let h: u32 = h_str.trim().parse().ok()?;
    let x: i32 = x_str.trim().parse().ok()?;
    let y: i32 = y_str.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h, x, y))
}

/// Require a region to fit wholly inside one currently enumerated physical
/// monitor. Used at the Windows IPC boundary so a forged geometry cannot ask
/// gdigrab to read an arbitrary/overflowing virtual-desktop rectangle.
pub fn region_within_any_display(geo: &str, displays: &[DisplayInfo]) -> bool {
    let Some((width, height, x, y)) = parse_region_geometry(geo) else {
        return false;
    };
    let left = i64::from(x);
    let top = i64::from(y);
    let right = left + i64::from(width);
    let bottom = top + i64::from(height);
    displays.iter().any(|display| {
        if display.width == 0 || display.height == 0 {
            return false;
        }
        let display_left = i64::from(display.x);
        let display_top = i64::from(display.y);
        let display_right = display_left + i64::from(display.width);
        let display_bottom = display_top + i64::from(display.height);
        left >= display_left
            && top >= display_top
            && right <= display_right
            && bottom <= display_bottom
    })
}

/// Construct the `gpu-screen-recorder` argv (excluding argv[0]). Pure so
/// the exact flag set is unit-testable.
///
/// * `-w <target>`    the capture target: a monitor name (Screen),
///                    `portal` (Window → native picker), or `region`.
/// * `-restore-portal-session no`  (portal only) re-prompt the window
///                    picker each call instead of reusing a saved session.
/// * `-region <geo>`  (region only) the `WxH+X+Y` rectangle.
/// * `-c mp4`         mp4 container (H.264 for broad `<video>` compat).
/// * `-f <fps>`       frame rate (already clamped by the caller).
/// * `-k h264`        video codec.
/// * `-bm cbr -q N`   constant-bitrate cap at N kbps — bounds worst-case
///                    storage (a shared 4K playback can't blow the budget).
/// * `-s WxH`         fit-within resolution cap (omitted for native).
/// * `-cursor yes`    include the cursor in the capture.
/// * `-o <path>`      output mp4.
pub fn build_gsr_args(
    w_target: &str,
    fps: u32,
    bitrate_kbps: u32,
    cap: Option<(u32, u32)>,
    region: Option<&str>,
    output: &str,
) -> Vec<String> {
    let mut a: Vec<String> = vec!["-w".into(), w_target.to_string()];
    if w_target == "portal" {
        a.push("-restore-portal-session".into());
        a.push("no".into());
    }
    if let Some(geo) = region {
        a.push("-region".into());
        a.push(geo.to_string());
    }
    a.extend([
        "-c".into(),
        "mp4".into(),
        "-f".into(),
        fps.to_string(),
        "-k".into(),
        VIDEO_CODEC.to_string(),
        "-bm".into(),
        "cbr".into(),
        "-q".into(),
        bitrate_kbps.to_string(),
        "-cursor".into(),
        "yes".into(),
    ]);
    if let Some((w, h)) = cap {
        a.push("-s".into());
        a.push(format!("{w}x{h}"));
    }
    a.push("-o".into());
    a.push(output.to_string());
    a
}

/// The gdigrab capture input — a monitor/region crop of the desktop, or a
/// single window by title.
#[derive(Clone, Debug)]
pub enum GdigrabInput {
    /// `-i desktop` cropped to `-offset_x/-offset_y -video_size`.
    Desktop {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// `-i title=<title>`.
    Window { title: String },
}

/// Optional Windows output scaling. Fixed desktop/region inputs use `Exact`
/// after `fit_within` so persisted metadata matches the encoded dimensions.
/// Window capture is dynamic, so `FitWithin` bounds every frame without
/// stretching or upscaling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GdigrabScale {
    Exact { width: u32, height: u32 },
    FitWithin { width: u32, height: u32 },
    Even,
}

/// Construct the Windows `gdigrab` ffmpeg argv (excluding argv[0]). Pure so
/// the exact flag set is unit-testable on any host.
///
/// The encoder is probed at preflight (see [`choose_h264_encoder`]) — do NOT
/// assume `libx264`, the bundled Windows sidecar may be an LGPL build. Only
/// `libx264` gets a `-preset`; the hardware / Media-Foundation encoders
/// reject that flag, so it is omitted for them (their defaults are fine).
/// moov-at-end is fixed by the existing faststart remux in the uploader.
pub fn build_gdigrab_args(
    input: &GdigrabInput,
    fps: u32,
    bitrate_kbps: u32,
    encoder: &str,
    scale: Option<GdigrabScale>,
    output: &str,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-f".into(),
        "gdigrab".into(),
        "-framerate".into(),
        fps.to_string(),
    ];
    match input {
        GdigrabInput::Desktop {
            x,
            y,
            width,
            height,
        } => {
            a.push("-offset_x".into());
            a.push(x.to_string());
            a.push("-offset_y".into());
            a.push(y.to_string());
            a.push("-video_size".into());
            a.push(format!("{width}x{height}"));
            a.push("-i".into());
            a.push("desktop".into());
        }
        GdigrabInput::Window { title } => {
            a.push("-i".into());
            a.push(format!("title={title}"));
        }
    }
    if let Some(scale) = scale {
        let filter = match scale {
            GdigrabScale::Exact { width, height } => {
                format!("scale={width}:{height}")
            }
            GdigrabScale::FitWithin { width, height } => format!(
                concat!(
                    "scale=w=min({}\\,iw):h=min({}\\,ih):",
                    "force_original_aspect_ratio=decrease:force_divisible_by=2"
                ),
                width,
                height,
            ),
            GdigrabScale::Even => {
                "scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string()
            }
        };
        a.push("-vf".into());
        a.push(filter);
    }
    a.push("-c:v".into());
    a.push(encoder.to_string());
    if encoder == "libx264" {
        a.push("-preset".into());
        a.push("veryfast".into());
    }
    a.push("-pix_fmt".into());
    a.push("yuv420p".into());
    a.push("-b:v".into());
    a.push(format!("{bitrate_kbps}k"));
    a.push("-maxrate".into());
    a.push(format!("{bitrate_kbps}k"));
    a.push("-bufsize".into());
    a.push(format!("{}k", bitrate_kbps.saturating_mul(2)));
    a.push("-y".into());
    a.push(output.to_string());
    a
}

/// Pick the best available H.264 encoder from an `ffmpeg -encoders` dump, in
/// preference order (hardware first, then Media Foundation which is always
/// present on Windows, then libx264 if the build has it). `None` when the
/// build ships no usable H.264 encoder → Windows advertises unavailable.
/// Pure so the ordering is unit-tested without a Windows host.
pub fn choose_h264_encoder(encoders_output: &str) -> Option<&'static str> {
    listed_h264_encoders(encoders_output).into_iter().next()
}

/// Return listed candidates in deterministic preference order. The listing is
/// only discovery; production separately runtime-probes every returned codec.
pub fn listed_h264_encoders(encoders_output: &str) -> Vec<&'static str> {
    const PREFERRED: [&str; 5] =
        ["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"];
    let mut listed = Vec::new();
    for name in PREFERRED {
        if encoders_output
            .split(|c: char| c.is_whitespace())
            .any(|tok| tok == name)
        {
            listed.push(name);
        }
    }
    listed
}

/// Filter the listing through an injected runtime probe. Kept pure/injectable
/// so fallback ordering is deterministic in non-Windows unit tests.
pub fn usable_h264_encoders_from<F>(
    encoders_output: &str,
    mut usable: F,
) -> Vec<&'static str>
where
    F: FnMut(&str) -> bool,
{
    listed_h264_encoders(encoders_output)
        .into_iter()
        .filter(|encoder| usable(encoder))
        .collect()
}

/// Pick the capture-target monitor name. Prefers the user's saved name when
/// it still enumerates; else the focused/primary monitor; else the first
/// listed. `None` when there are no displays to capture.
pub fn resolve_monitor(preferred: Option<&str>, displays: &[DisplayInfo]) -> Option<String> {
    if displays.is_empty() {
        return None;
    }
    if let Some(name) = preferred.map(str::trim).filter(|s| !s.is_empty()) {
        if displays.iter().any(|d| d.name == name) {
            return Some(name.to_string());
        }
        // Saved monitor unplugged / renamed → fall through to a default.
    }
    displays
        .iter()
        .find(|d| d.is_primary)
        .or_else(|| displays.first())
        .map(|d| d.name.clone())
}

/// Parse one `gpu-screen-recorder --list-monitors` line (`NAME|WIDTHxHEIGHT`).
fn parse_gsr_monitor_line(line: &str) -> Option<(String, u32, u32)> {
    let (name, res) = line.trim().split_once('|')?;
    let (w, h) = res.trim().split_once('x')?;
    Some((
        name.trim().to_string(),
        w.trim().parse().ok()?,
        h.trim().parse().ok()?,
    ))
}

// ── Windows enumeration + preflight probe ────────────────────────────

/// Resolve the full [`DisplayInfo`] (rect origin included) for a Windows
/// capture: saved name → primary → first.
#[cfg(windows)]
fn resolve_display(preferred: Option<&str>, displays: &[DisplayInfo]) -> Option<DisplayInfo> {
    if displays.is_empty() {
        return None;
    }
    if let Some(name) = preferred.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(d) = displays.iter().find(|d| d.name == name) {
            return Some(d.clone());
        }
    }
    displays
        .iter()
        .find(|d| d.is_primary)
        .or_else(|| displays.first())
        .cloned()
}

/// The chosen H.264 encoder for this machine, probed once + cached. `None`
/// when the sidecar ships no usable encoder.
#[cfg(windows)]
pub fn pick_h264_encoder() -> Option<&'static str> {
    probed_h264_encoders().first().copied()
}

#[cfg(windows)]
fn probed_h264_encoders() -> &'static [&'static str] {
    static ENCODERS: std::sync::OnceLock<Vec<&'static str>> =
        std::sync::OnceLock::new();
    ENCODERS
        .get_or_init(|| {
            win_encoders_text()
                .map(|text| usable_h264_encoders_from(&text, probe_h264_encoder))
                .unwrap_or_default()
        })
        .as_slice()
}

/// A codec listed by `-encoders` may only be compiled in; hardware backends
/// commonly fail at initialization on machines without the matching driver.
/// Encode one synthetic frame with a hard deadline to prove initialization.
#[cfg(windows)]
fn probe_h264_encoder(encoder: &str) -> bool {
    use std::process::{Command, Stdio};

    let bin = crate::pipeline::ffmpeg_binary();
    let mut command = Command::new(&bin);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:r=1",
            "-frames:v",
            "1",
            "-an",
            "-c:v",
            encoder,
            "-pix_fmt",
            "yuv420p",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_console_std(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    // One 64×64 frame should initialize almost immediately; two seconds is
    // deliberately generous while keeping the five-candidate preflight
    // bounded to a tolerable worst case.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Run `ffmpeg -hide_banner -encoders` on the sidecar, returning stdout.
#[cfg(windows)]
fn win_encoders_text() -> Option<String> {
    use std::process::Command;
    let bin = crate::pipeline::ffmpeg_binary();
    let mut cmd = Command::new(&bin);
    cmd.args(["-hide_banner", "-encoders"]);
    no_console_std(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether the sidecar exposes the `gdigrab` input device. Probed once + cached.
#[cfg(windows)]
fn win_has_gdigrab() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        use std::process::Command;
        let bin = crate::pipeline::ffmpeg_binary();
        let mut cmd = Command::new(&bin);
        cmd.args(["-hide_banner", "-devices"]);
        no_console_std(&mut cmd);
        match cmd.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                stdout.contains("gdigrab") || stderr.contains("gdigrab")
            }
            Err(_) => false,
        }
    })
}

/// Enumerate visible top-level windows (title, filtering empty titles +
/// tool windows) via `EnumWindows`.
#[cfg(windows)]
pub fn enumerate_windows() -> Vec<WindowInfo> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
        GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<WindowInfo>);
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (exstyle & WS_EX_TOOLWINDOW.0) != 0 {
            return TRUE;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return TRUE;
        }
        let mut buf = vec![0u16; (len as usize) + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        if got <= 0 {
            return TRUE;
        }
        let title = String::from_utf16_lossy(&buf[..got as usize]);
        if title.trim().is_empty() {
            return TRUE;
        }
        out.push(WindowInfo { title });
        TRUE
    }

    let mut windows_list: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_cb),
            LPARAM(&mut windows_list as *mut Vec<WindowInfo> as isize),
        );
    }
    windows_list
}

#[cfg(not(windows))]
pub fn enumerate_windows() -> Vec<WindowInfo> {
    // Linux uses the compositor's native window picker; macOS has no
    // capture surface. No in-app window list.
    Vec::new()
}

// ── Runtime detection + capability seam ──────────────────────────────

/// Advertise which of Screen / Window / Region this platform can capture.
/// The chooser renders only these; an empty list hides the whole surface.
#[cfg(target_os = "linux")]
pub fn supported_source_kinds() -> Vec<&'static str> {
    // Screen (always) + Window (native picker, always) + Region iff `slurp`
    // is installed (the proven wlroots region tool).
    let mut kinds = vec!["screen", "window"];
    if locate_slurp().is_some() {
        kinds.push("region");
    }
    kinds
}

#[cfg(windows)]
pub fn supported_source_kinds() -> Vec<&'static str> {
    // Screen + Window + Region. (Region rides the de-risk clause — if the
    // in-app overlay proves flaky on real hardware, drop "region" here and
    // it ships as a Windows fast-follow with no other change.)
    vec!["screen", "window", "region"]
}

// macOS / other: no capture surface (stub backend).
// TODO(#302 mac): advertise ScreenCaptureKit kinds when the backend lands.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn supported_source_kinds() -> Vec<&'static str> {
    Vec::new()
}

/// Locate an executable by bare name on `$PATH` (plus a couple of common
/// absolute locations). Pure over the process env so a bogus name is a
/// deterministic `None` in tests.
#[cfg(target_os = "linux")]
pub fn find_executable(name: &str) -> Option<PathBuf> {
    // Absolute path passed through verbatim.
    let direct = Path::new(name);
    if direct.is_absolute() && is_executable_file(direct) {
        return Some(direct.to_path_buf());
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|s| !s.is_empty()) {
            let cand = Path::new(dir).join(name);
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    // Common absolute fallbacks in case PATH is minimal (systemd/.desktop
    // launch environments frequently are).
    for base in ["/usr/bin", "/usr/local/bin", "/bin"] {
        let cand = Path::new(base).join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Locate the `gpu-screen-recorder` binary, or `None` when it isn't
/// installed (→ capture gracefully unavailable).
#[cfg(target_os = "linux")]
pub fn locate_gsr() -> Option<PathBuf> {
    find_executable("gpu-screen-recorder")
}

/// Locate the `slurp` region-select tool (gates the Region source kind).
#[cfg(target_os = "linux")]
pub fn locate_slurp() -> Option<PathBuf> {
    find_executable("slurp")
}

/// Drive a `slurp` drag-select → canonical `"WxH+X+Y"` geometry. `None` on
/// ESC / cancel (slurp exits non-zero) or when `slurp` is absent. Best-
/// effort: any spawn error is a graceful `None`.
#[cfg(target_os = "linux")]
pub fn resolve_region_via_slurp() -> Option<String> {
    use std::process::Command;
    let bin = locate_slurp()?;
    let out = Command::new(&bin)
        .arg("-f")
        .arg("%wx%h+%x+%y")
        .output()
        .ok()?;
    if !out.status.success() {
        // ESC / right-click cancel → non-zero exit.
        return None;
    }
    let geo = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if geo.is_empty() {
        None
    } else {
        Some(geo)
    }
}

/// Whether a capture backend can run right now.
#[cfg(target_os = "linux")]
pub fn capture_available() -> bool {
    // The binary is present AND at least one display enumerates.
    locate_gsr().is_some() && !enumerate_displays().is_empty()
}

#[cfg(windows)]
pub fn capture_available() -> bool {
    // The sidecar exposes gdigrab AND ships a usable H.264 encoder. Both
    // probes are cached on first use. Encoder discovery runtime-initializes
    // each listed candidate with a bounded one-frame encode; a failure hides
    // the whole surface exactly like the Linux binary-absent path.
    win_has_gdigrab() && pick_h264_encoder().is_some()
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn capture_available() -> bool {
    false
}

/// Enumerate selectable monitors for the chooser + Settings picker.
#[cfg(target_os = "linux")]
pub fn enumerate_displays() -> Vec<DisplayInfo> {
    use std::process::Command;

    // Focused monitor name from Hyprland, if we're in a Hypr session.
    let focused = hyprctl_focused_monitor();

    // Primary: gpu-screen-recorder --list-monitors (NAME|WxH per line).
    if let Some(bin) = locate_gsr() {
        if let Ok(out) = Command::new(&bin).arg("--list-monitors").output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                let mut list: Vec<DisplayInfo> = text
                    .lines()
                    .filter_map(parse_gsr_monitor_line)
                    .map(|(name, width, height)| {
                        let is_primary = focused.as_deref() == Some(name.as_str());
                        DisplayInfo {
                            name,
                            width,
                            height,
                            is_primary,
                            // gsr targets by name on Linux; rect origin is
                            // unused (0). Windows carries the real offset.
                            x: 0,
                            y: 0,
                        }
                    })
                    .collect();
                // If nothing was flagged primary (non-Hypr compositor),
                // mark the first as a sensible default.
                if !list.is_empty() && !list.iter().any(|d| d.is_primary) {
                    list[0].is_primary = true;
                }
                if !list.is_empty() {
                    return list;
                }
            }
        }
    }

    // Fallback: Hyprland's own monitor list.
    hyprctl_monitors()
}

/// Enumerate monitors on Windows via `EnumDisplayMonitors` + `GetMonitorInfoW`
/// (rect + device name + primary flag). The rect origin feeds gdigrab's
/// `-offset_x/-offset_y`.
#[cfg(windows)]
pub fn enumerate_displays() -> Vec<DisplayInfo> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    unsafe extern "system" fn monitor_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<DisplayInfo>);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut mi.monitorInfo as *mut MONITORINFO).as_bool() {
            let r = mi.monitorInfo.rcMonitor;
            let width = (r.right - r.left).max(0) as u32;
            let height = (r.bottom - r.top).max(0) as u32;
            let is_primary = (mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;
            let name = {
                let raw = &mi.szDevice;
                let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                String::from_utf16_lossy(&raw[..end])
            };
            out.push(DisplayInfo {
                name,
                width,
                height,
                is_primary,
                x: r.left,
                y: r.top,
            });
        }
        TRUE
    }

    let mut displays: Vec<DisplayInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(monitor_cb),
            LPARAM(&mut displays as *mut Vec<DisplayInfo> as isize),
        );
    }
    if !displays.is_empty() && !displays.iter().any(|d| d.is_primary) {
        displays[0].is_primary = true;
    }
    displays
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn enumerate_displays() -> Vec<DisplayInfo> {
    Vec::new()
}

/// Name of Hyprland's focused monitor, if any.
#[cfg(target_os = "linux")]
fn hyprctl_focused_monitor() -> Option<String> {
    hyprctl_monitors()
        .into_iter()
        .find(|d| d.is_primary)
        .map(|d| d.name)
}

/// Parse `hyprctl monitors -j` → the monitor list (focused flag included).
/// Empty when hyprctl is absent / not a Hypr session / the output doesn't
/// parse. No serde struct — a tiny hand-parse over `serde_json::Value`
/// keeps this dependency-free and tolerant of Hypr's field churn.
#[cfg(target_os = "linux")]
fn hyprctl_monitors() -> Vec<DisplayInfo> {
    use std::process::Command;
    let Ok(out) = Command::new("hyprctl").arg("monitors").arg("-j").output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|m| {
            let name = m.get("name")?.as_str()?.to_string();
            let width = m.get("width")?.as_u64()? as u32;
            let height = m.get("height")?.as_u64()? as u32;
            let is_primary = m.get("focused").and_then(|v| v.as_bool()).unwrap_or(false);
            Some(DisplayInfo {
                name,
                width,
                height,
                is_primary,
                x: 0,
                y: 0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NeverFinalize;

    impl CaptureBackend for NeverFinalize {
        fn finalize(&mut self) -> Result<()> {
            panic!("stale stop must not finalize the active backend")
        }

        fn is_running(&mut self) -> bool {
            true
        }
    }

    #[test]
    fn start_offset_is_video_minus_audio() {
        // Video spawns 420 ms after audio → offset 420.
        assert_eq!(compute_start_offset_ms(1_000, 1_420), 420);
    }

    #[test]
    fn start_offset_floors_at_zero() {
        // A clock hiccup can't produce a negative offset.
        assert_eq!(compute_start_offset_ms(2_000, 1_500), 0);
        assert_eq!(compute_start_offset_ms(1_000, 1_000), 0);
    }

    #[test]
    fn finalized_duration_recovers_portal_picker_delay() {
        // Audio ran 30s, but the selected window produced only 20s of video.
        assert_eq!(corrected_start_offset_ms(1_000, 31_000, 20_000), 10_000);
    }

    #[test]
    fn ffmpeg_duration_parser_is_bounded_and_precise() {
        assert_eq!(
            parse_ffmpeg_duration_ms("Duration: 01:02:03.45, start: 0.0"),
            Some(3_723_450)
        );
        assert_eq!(parse_ffmpeg_duration_ms("Duration: N/A, start: 0.0"), None);
        assert_eq!(parse_ffmpeg_duration_ms("Duration: 00:99:00.00"), None);
    }

    #[test]
    fn stale_stop_does_not_take_new_screen_generation() {
        let recorder = ScreenRecorder::new();
        *recorder.active.lock().unwrap() = Some(Active {
            generation: 2,
            backend: Box::new(NeverFinalize),
            session_dir: PathBuf::from("/recordings/new"),
            output_path: PathBuf::from("/recordings/new/screen/recording.mp4"),
            started_at: Instant::now(),
            audio_started_at_ms: 0,
            start_offset_ms: 0,
            fps: 15,
            dims: None,
            source_kind: "screen",
        });
        let report = recorder.stop_and_persist(
            Some(1),
            Some(Path::new("/recordings/old")),
        );
        assert!(report.error.unwrap().contains("stale screen stop rejected"));
        assert_eq!(recorder.active_generation(), Some(2));
    }

    #[test]
    fn fps_clamped_to_capture_window() {
        assert_eq!(clamp_fps(5), 10);
        assert_eq!(clamp_fps(15), 15);
        assert_eq!(clamp_fps(60), 30);
    }

    #[test]
    fn resolution_keyword_maps_to_cap_box() {
        assert_eq!(resolution_cap_box(Some("720p")), Some((1280, 720)));
        assert_eq!(resolution_cap_box(Some("1080p")), Some((1920, 1080)));
        assert_eq!(resolution_cap_box(Some("native")), None);
        assert_eq!(resolution_cap_box(None), None);
        // Unknown keyword defaults to the 1080p cap.
        assert_eq!(resolution_cap_box(Some("4k")), Some((1920, 1080)));
    }

    #[test]
    fn fit_within_downscales_preserving_aspect() {
        // 4K → fit within 1080p keeps 16:9 (even dims).
        assert_eq!(fit_within((3840, 2160), Some((1920, 1080))), (1920, 1080));
        // The same source at the 720p preference.
        assert_eq!(fit_within((3840, 2160), Some((1280, 720))), (1280, 720));
        // Ultrawide 3840x1080 fit within 1920x1080 clamps on width.
        assert_eq!(fit_within((3840, 1080), Some((1920, 1080))), (1920, 540));
        assert_eq!(fit_within((3840, 1080), Some((1280, 720))), (1280, 360));
    }

    #[test]
    fn fit_within_leaves_small_and_native_untouched() {
        // Already within the cap → unchanged.
        assert_eq!(fit_within((1600, 900), Some((1920, 1080))), (1600, 900));
        // No cap → native.
        assert_eq!(fit_within((3840, 2160), None), (3840, 2160));
    }

    #[test]
    fn even_rounds_down_min_two() {
        assert_eq!(even(1920), 1920);
        assert_eq!(even(1921), 1920);
        assert_eq!(even(1), 2);
        assert_eq!(even(0), 2);
    }

    // ── Linux gsr arg builder ──────────────────────────────────────

    #[test]
    fn gsr_args_native_omits_scale() {
        let args = build_gsr_args("DP-1", 15, 3000, None, None, "/tmp/x/recording.mp4");
        assert_eq!(
            args,
            vec![
                "-w", "DP-1", "-c", "mp4", "-f", "15", "-k", "h264", "-bm", "cbr", "-q", "3000",
                "-cursor", "yes", "-o", "/tmp/x/recording.mp4",
            ]
        );
        assert!(!args.iter().any(|a| a == "-s"));
    }

    #[test]
    fn gsr_args_capped_includes_scale() {
        let args = build_gsr_args("HDMI-A-1", 30, 6000, Some((1920, 1080)), None, "/o.mp4");
        // -s <box> appears right before -o.
        let s_idx = args.iter().position(|a| a == "-s").expect("has -s");
        assert_eq!(args[s_idx + 1], "1920x1080");
        assert_eq!(args[args.len() - 2], "-o");
        assert_eq!(args[args.len() - 1], "/o.mp4");
        // fps + bitrate threaded through.
        let f_idx = args.iter().position(|a| a == "-f").unwrap();
        assert_eq!(args[f_idx + 1], "30");
        let q_idx = args.iter().position(|a| a == "-q").unwrap();
        assert_eq!(args[q_idx + 1], "6000");
    }

    #[test]
    fn gsr_args_window_uses_portal_and_reprompts() {
        let args = build_gsr_args("portal", 15, 3000, None, None, "/o.mp4");
        // Head: -w portal -restore-portal-session no ...
        assert_eq!(&args[0..4], &["-w", "portal", "-restore-portal-session", "no"]);
        assert!(!args.iter().any(|a| a == "-region"));
    }

    #[test]
    fn gsr_args_region_threads_geometry() {
        let args = build_gsr_args("region", 15, 3000, None, Some("800x600+10+20"), "/o.mp4");
        assert_eq!(&args[0..4], &["-w", "region", "-region", "800x600+10+20"]);
        // Not the portal path.
        assert!(!args.iter().any(|a| a == "-restore-portal-session"));
    }

    // ── Windows gdigrab arg builder ────────────────────────────────

    #[test]
    fn gdigrab_desktop_threads_offset_and_size() {
        let input = GdigrabInput::Desktop {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let args =
            build_gdigrab_args(&input, 15, 3000, "h264_mf", None, "/o.mp4");
        // gdigrab input with the monitor crop.
        assert_eq!(&args[0..4], &["-f", "gdigrab", "-framerate", "15"]);
        let ox = args.iter().position(|a| a == "-offset_x").unwrap();
        assert_eq!(args[ox + 1], "-1920");
        let oy = args.iter().position(|a| a == "-offset_y").unwrap();
        assert_eq!(args[oy + 1], "0");
        let vs = args.iter().position(|a| a == "-video_size").unwrap();
        assert_eq!(args[vs + 1], "1920x1080");
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "desktop");
        // Encoder threaded through; h264_mf gets NO -preset (would error).
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "h264_mf");
        assert!(!args.iter().any(|a| a == "-preset"));
        // CBR-ish rate control bounds storage.
        let bv = args.iter().position(|a| a == "-b:v").unwrap();
        assert_eq!(args[bv + 1], "3000k");
        let bufsize = args.iter().position(|a| a == "-bufsize").unwrap();
        assert_eq!(args[bufsize + 1], "6000k");
        assert_eq!(args[args.len() - 1], "/o.mp4");
    }

    #[test]
    fn gdigrab_window_uses_title_input() {
        let input = GdigrabInput::Window {
            title: "Zoom Meeting".to_string(),
        };
        let args =
            build_gdigrab_args(&input, 24, 4000, "libx264", None, "/o.mp4");
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "title=Zoom Meeting");
        // No desktop-crop flags on the window path.
        assert!(!args.iter().any(|a| a == "-video_size"));
        assert!(!args.iter().any(|a| a == "-offset_x"));
        // libx264 DOES get a -preset.
        assert!(args.iter().any(|a| a == "-preset"));
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "libx264");
    }

    #[test]
    fn h264_encoder_prefers_hardware_then_mf_then_libx264() {
        // Realistic-ish -encoders dump lines (leading flags + name + desc).
        let full = " V....D h264_nvenc  NVIDIA\n V....D h264_mf  MediaFoundation\n V....D libx264  x264";
        assert_eq!(choose_h264_encoder(full), Some("h264_nvenc"));
        let mf_only = " V....D h264_mf  MediaFoundation\n V....D libx264  x264";
        assert_eq!(choose_h264_encoder(mf_only), Some("h264_mf"));
        let x264_only = " V..... libx264  H.264 / AVC";
        assert_eq!(choose_h264_encoder(x264_only), Some("libx264"));
        // No H.264 encoder at all → None (Windows advertises unavailable).
        assert_eq!(choose_h264_encoder(" V..... vp9  VP9\n A..... aac  AAC"), None);
        // Partial token must not false-match (libx264rgb is not libx264).
        assert_eq!(choose_h264_encoder(" V..... libx264rgb  rgb"), None);
    }

    #[test]
    fn h264_encoder_runtime_probe_falls_back_in_listing_order() {
        let full = " V....D h264_nvenc NVIDIA\n V....D h264_qsv Intel\n V....D h264_mf MF\n V..... libx264 x264";
        let usable =
            usable_h264_encoders_from(full, |name| name == "h264_mf" || name == "libx264");
        assert_eq!(usable, vec!["h264_mf", "libx264"]);
    }

    #[test]
    fn gdigrab_exact_scale_caps_desktop_without_changing_input_crop() {
        let input = GdigrabInput::Desktop {
            x: 0,
            y: 0,
            width: 3840,
            height: 2160,
        };
        let args = build_gdigrab_args(
            &input,
            15,
            3000,
            "h264_mf",
            Some(GdigrabScale::Exact {
                width: 1280,
                height: 720,
            }),
            "/o.mp4",
        );
        let input_size = args.iter().position(|a| a == "-video_size").unwrap();
        assert_eq!(args[input_size + 1], "3840x2160");
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(args[vf + 1], "scale=1280:720");
    }

    #[test]
    fn gdigrab_window_scale_is_fit_within_and_no_upscale() {
        let input = GdigrabInput::Window {
            title: "Support call".to_string(),
        };
        let args = build_gdigrab_args(
            &input,
            15,
            3000,
            "h264_mf",
            Some(GdigrabScale::FitWithin {
                width: 1920,
                height: 1080,
            }),
            "/o.mp4",
        );
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(
            args[vf + 1],
            "scale=w=min(1920\\,iw):h=min(1080\\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2"
        );
    }

    #[test]
    fn gdigrab_native_window_still_normalizes_odd_dimensions() {
        let input = GdigrabInput::Window {
            title: "Odd-sized window".to_string(),
        };
        let args = build_gdigrab_args(
            &input,
            15,
            3000,
            "h264_mf",
            Some(GdigrabScale::Even),
            "/o.mp4",
        );
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(args[vf + 1], "scale=trunc(iw/2)*2:trunc(ih/2)*2");
    }

    // ── Region geometry parsing ────────────────────────────────────

    #[test]
    fn region_geometry_parses_wxh_plus_xy() {
        assert_eq!(parse_region_geometry("800x600+100+200"), Some((800, 600, 100, 200)));
        // Negative virtual-desktop origin (monitor left of / above primary).
        assert_eq!(
            parse_region_geometry("1920x1080+-1920+0"),
            Some((1920, 1080, -1920, 0))
        );
        // Whitespace tolerated.
        assert_eq!(parse_region_geometry("  640x480+0+0  "), Some((640, 480, 0, 0)));
    }

    #[test]
    fn region_geometry_rejects_malformed() {
        assert_eq!(parse_region_geometry("garbage"), None);
        assert_eq!(parse_region_geometry("800x600+100"), None); // missing Y
        assert_eq!(parse_region_geometry("800x600+1+2+3"), None); // trailing
        assert_eq!(parse_region_geometry("0x600+0+0"), None); // zero dim
    }

    #[test]
    fn region_geometry_must_fit_one_physical_display() {
        let displays = vec![
            DisplayInfo {
                name: "left".to_string(),
                width: 1920,
                height: 1080,
                is_primary: false,
                x: -1920,
                y: 0,
            },
            DisplayInfo {
                name: "main".to_string(),
                width: 2560,
                height: 1440,
                is_primary: true,
                x: 0,
                y: 0,
            },
        ];
        assert!(region_within_any_display(
            "800x600+-1800+100",
            &displays
        ));
        assert!(region_within_any_display(
            "2560x1440+0+0",
            &displays
        ));
        // Spans the monitor seam rather than fitting wholly in either.
        assert!(!region_within_any_display(
            "400x500+-200+100",
            &displays
        ));
        assert!(!region_within_any_display(
            "100x100+2500+1400",
            &displays
        ));
        assert!(!region_within_any_display("malformed", &displays));
    }

    #[test]
    fn resolve_monitor_prefers_saved_then_primary_then_first() {
        let displays = vec![
            DisplayInfo { name: "DP-1".into(), width: 2560, height: 1440, is_primary: false, x: 0, y: 0 },
            DisplayInfo { name: "DP-2".into(), width: 1920, height: 1080, is_primary: true, x: 0, y: 0 },
        ];
        // Saved name that still enumerates wins.
        assert_eq!(resolve_monitor(Some("DP-1"), &displays).as_deref(), Some("DP-1"));
        // Saved name gone → focused/primary.
        assert_eq!(resolve_monitor(Some("HDMI-A-9"), &displays).as_deref(), Some("DP-2"));
        // No preference → primary.
        assert_eq!(resolve_monitor(None, &displays).as_deref(), Some("DP-2"));
        // No displays → None.
        assert_eq!(resolve_monitor(Some("DP-1"), &[]), None);
    }

    #[test]
    fn parse_gsr_monitor_line_reads_name_and_res() {
        assert_eq!(
            parse_gsr_monitor_line("DP-2|3840x1080"),
            Some(("DP-2".to_string(), 3840, 1080))
        );
        assert_eq!(parse_gsr_monitor_line("garbage"), None);
        assert_eq!(parse_gsr_monitor_line("NoRes|"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_executable_absent_binary_is_none() {
        // A name that cannot exist on PATH → deterministic None (the
        // graceful-absent path capture detection relies on).
        assert!(find_executable("aftercalls-nonexistent-binary-xyz-123").is_none());
    }

    #[test]
    fn source_kind_str_maps_variants() {
        assert_eq!(source_kind_str(&CaptureSource::Screen { monitor: None }), "screen");
        assert_eq!(source_kind_str(&CaptureSource::Window { target: None }), "window");
        assert_eq!(
            source_kind_str(&CaptureSource::Region { geometry: "1x1+0+0".into() }),
            "region"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_finalize_handles_an_already_reaped_clean_child() {
        let child = std::process::Command::new("/bin/true").spawn().unwrap();
        let mut recorder = GpuScreenRecorder {
            child,
            stderr_join: None,
        };
        recorder.child.wait().unwrap();
        recorder
            .finalize()
            .expect("must not signal a stale PID after status reaped the child");
    }
}
