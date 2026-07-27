//! Screen-capture subprocess (#302 Slice B — agent-Rust capture).
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
//! ## Subprocess, not in-process
//! The Linux/wlroots v1 backend shells out to the system
//! `gpu-screen-recorder` binary — a sibling of the `parec` system-audio
//! subprocess in `recorder.rs`. It talks native Wayland (via the
//! compositor's capture) regardless of the agent's `GDK_BACKEND=x11`, and
//! its lifetime is tied to the agent with `PR_SET_PDEATHSIG` exactly like
//! the `parec` children — if the agent is SIGKILL'd the recorder gets a
//! SIGINT and finalizes its mp4 instead of leaking.
//!
//! ## Runtime-detected, gracefully absent
//! `gpu-screen-recorder` is a system-installed binary present on some
//! machines and not others. It is detected at runtime; when absent the
//! feature is silently unavailable (logged + skipped) — never a hard
//! failure. Bundling it is out of scope (its GPU-driver deps make a
//! portable bundle fragile); the Phase-2 portal/PipeWire backend is the
//! future no-external-binary path and slots in behind the same
//! [`CaptureBackend`] trait without touching the lifecycle in `lib.rs`.
//!
//! ## moov-at-end
//! `gpu-screen-recorder` writes the mp4 with the `moov` atom at the END,
//! so the raw file does not seek in a `<video>` element until it is
//! remuxed with `-movflags +faststart` (done best-effort in the uploader,
//! not here).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

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
/// Kept backend-agnostic so the Phase-2 portal/PipeWire path consumes the
/// same struct.
#[derive(Clone, Debug)]
pub struct StartConfig {
    /// Chosen monitor name (`-w` target, as printed by
    /// `gpu-screen-recorder --list-monitors`). `None` = pick the
    /// focused/primary monitor.
    pub display: Option<String>,
    /// Frames per second; clamped to [10, 30].
    pub fps: u32,
    /// `"720p" | "1080p" | "native"` — a fit-within resolution cap.
    pub resolution: Option<String>,
    /// CBR bitrate ceiling in kbps (bounds the storage cost of a shared
    /// 4K video playback — the dominant cost lever).
    pub bitrate_kbps: u32,
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

// ── Display enumeration (for the Settings picker) ────────────────────

/// One selectable monitor. `name` is the exact `-w` target string; the
/// resolution feeds the picker's "3840×2160" hint.
#[derive(Clone, Debug, Serialize)]
pub struct DisplayInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// True for the compositor's focused/primary output when known.
    pub is_primary: bool,
}

// ── The recorder handle (managed in app state, sibling of Recorder) ───

/// Owns the single in-flight capture, if any. Managed at process scope in
/// `lib.rs` so `do_start` / `do_stop` reach the same session via
/// `app.state::<ScreenRecorder>()`.
pub struct ScreenRecorder {
    active: Mutex<Option<Active>>,
}

struct Active {
    backend: Box<dyn CaptureBackend>,
    session_dir: PathBuf,
    output_path: PathBuf,
    started_at: Instant,
    start_offset_ms: i64,
    fps: u32,
    dims: Option<(u32, u32)>,
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

    /// Whether a capture is currently in flight.
    pub fn is_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// Whether the platform capture backend is available on this machine
    /// right now (binary present + at least one enumerable display).
    /// Drives the Settings UI's "capture unavailable" state.
    pub fn is_available(&self) -> bool {
        capture_available()
    }

    /// Best-effort start of a screen capture into
    /// `<session_dir>/screen/recording.mp4`. Returns `true` when a capture
    /// subprocess was spawned; `false` when capture is gracefully
    /// unavailable (binary absent, no display, spawn failed, already
    /// active). The caller MUST treat `false` as "no video" and carry on
    /// — it is NEVER a recording failure.
    pub fn start(&self, session_dir: &Path, cfg: &StartConfig, audio_started_at_ms: i64) -> bool {
        let mut guard = self.active.lock().unwrap();
        if guard.is_some() {
            eprintln!("aftercalls: screen capture already active — skipping");
            return false;
        }

        let screen_dir = session_dir.join(SCREEN_SUBDIR);
        if let Err(e) = std::fs::create_dir_all(&screen_dir) {
            eprintln!("aftercalls: screen capture skipped — mkdir failed: {e}");
            return false;
        }
        let output_path = screen_dir.join(RECORDING_FILENAME);

        match spawn_capture(cfg, &output_path) {
            Ok((backend, dims)) => {
                // Capture start moment vs the audio recorder's start. The
                // subprocess spawns a beat after the audio, so this is a
                // small positive offset the player maps
                // `video_time = transcript_ms − start_offset_ms` with.
                let video_started_at_ms = chrono::Utc::now().timestamp_millis();
                let start_offset_ms = compute_start_offset_ms(audio_started_at_ms, video_started_at_ms);
                eprintln!(
                    "aftercalls: screen capture started (offset {start_offset_ms}ms) → {}",
                    output_path.display()
                );
                *guard = Some(Active {
                    backend,
                    session_dir: session_dir.to_path_buf(),
                    output_path,
                    started_at: Instant::now(),
                    start_offset_ms,
                    fps: clamp_fps(cfg.fps),
                    dims,
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

    /// Stop + finalize the active capture and persist
    /// `screen/recording.json` for the uploader. No-op when idle. Fully
    /// best-effort: a finalize error is logged, never surfaced — the call
    /// still saves.
    pub fn stop_and_persist(&self, session_dir: &Path) {
        let active = {
            let mut guard = self.active.lock().unwrap();
            guard.take()
        };
        let Some(mut active) = active else {
            return;
        };
        // Defensive: if a session mismatch somehow occurs (a stop for a
        // different dir than the active capture) we still finalize the
        // capture we actually own, and persist next to it.
        if active.session_dir != session_dir {
            eprintln!(
                "aftercalls: screen stop session mismatch (active {:?} vs {:?}); finalizing active",
                active.session_dir, session_dir
            );
        }

        let duration_ms = active.started_at.elapsed().as_millis() as i64;
        if let Err(e) = active.backend.finalize() {
            eprintln!("aftercalls: screen capture finalize failed: {e:#}");
            // Fall through and still write the sidecar — the mp4 may be
            // partially usable and the uploader remux is best-effort too.
        }

        // Only persist metadata if the capture actually produced a file.
        if !active.output_path.exists() {
            eprintln!(
                "aftercalls: screen capture produced no file at {} — nothing to upload",
                active.output_path.display()
            );
            return;
        }

        let (width, height) = match active.dims {
            Some((w, h)) => (Some(w as i32), Some(h as i32)),
            None => (None, None),
        };
        let meta = ScreenRecordingMeta {
            file: RECORDING_FILENAME.to_string(),
            start_offset_ms: active.start_offset_ms,
            duration_ms,
            fps: active.fps as i32,
            width,
            height,
            codec: VIDEO_CODEC.to_string(),
        };
        let meta_path = active.session_dir.join(SCREEN_SUBDIR).join(META_FILENAME);
        match serde_json::to_string_pretty(&meta) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&meta_path, json) {
                    eprintln!("aftercalls: write {} failed: {e}", meta_path.display());
                }
            }
            Err(e) => eprintln!("aftercalls: serialize screen recording meta failed: {e}"),
        }
    }
}

// ── Platform capture backend ─────────────────────────────────────────

/// A running platform screen-capture session. Phase-1 impl is the
/// `gpu-screen-recorder` subprocess; the Phase-2 portal/PipeWire backend
/// implements the same trait so `ScreenRecorder` + the `lib.rs` lifecycle
/// need no rewrite.
trait CaptureBackend: Send {
    /// Signal a clean stop and block until the output mp4 is fully
    /// written (finalized).
    fn finalize(&mut self) -> Result<()>;
}

/// Resolve the display/fps/resolution/bitrate → spawn the platform
/// capture subprocess. Returns the running backend plus the best-effort
/// output dimensions (for the stored metadata). Errors here are the
/// graceful-unavailable path — the caller degrades to "no video".
#[cfg(target_os = "linux")]
fn spawn_capture(
    cfg: &StartConfig,
    output_path: &Path,
) -> Result<(Box<dyn CaptureBackend>, Option<(u32, u32)>)> {
    use std::process::{Command, Stdio};

    let bin = locate_gsr().context("gpu-screen-recorder not found on PATH")?;

    // Resolve the capture target monitor + its native resolution.
    let displays = enumerate_displays();
    let monitor = resolve_monitor(cfg.display.as_deref(), &displays)
        .context("no capturable display found")?;
    let native = displays
        .iter()
        .find(|d| d.name == monitor)
        .map(|d| (d.width, d.height));

    let cap = resolution_cap_box(cfg.resolution.as_deref());
    let out_dims = native.map(|n| fit_within(n, cap));

    let out_str = output_path.to_string_lossy();
    let args = build_gsr_args(&monitor, clamp_fps(cfg.fps), cfg.bitrate_kbps, cap, &out_str);

    let mut command = Command::new(&bin);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // Tie the recorder's lifetime to the agent: if the agent is SIGKILL'd
    // (binary swap, crash, force-quit) the recorder gets SIGINT and
    // finalizes its mp4 instead of leaking + writing a stale session dir.
    // Identical to the `parec` children in recorder.rs.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().context("spawn gpu-screen-recorder")?;
    Ok((Box::new(GpuScreenRecorder { child }), out_dims))
}

/// Non-Linux: no capture backend yet (macOS ScreenCaptureKit / Windows
/// WGC are Phase-2). Always the graceful-unavailable path.
#[cfg(not(target_os = "linux"))]
fn spawn_capture(
    _cfg: &StartConfig,
    _output_path: &Path,
) -> Result<(Box<dyn CaptureBackend>, Option<(u32, u32)>)> {
    anyhow::bail!("screen capture not implemented on this platform yet")
}

#[cfg(target_os = "linux")]
struct GpuScreenRecorder {
    child: std::process::Child,
}

#[cfg(target_os = "linux")]
impl CaptureBackend for GpuScreenRecorder {
    fn finalize(&mut self) -> Result<()> {
        // SIGINT → gpu-screen-recorder writes the trailing moov atom and
        // exits cleanly. `kill`+`wait` mirrors `recorder.rs::stop_child_gracefully`.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
        }
        self.child.wait().context("wait gpu-screen-recorder")?;
        Ok(())
    }
}

// ── Pure helpers (unit-tested) ───────────────────────────────────────

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

/// Construct the `gpu-screen-recorder` argv (excluding argv[0]). Pure so
/// the exact flag set is unit-testable.
///
/// * `-w <monitor>`   capture the chosen output natively (Wayland).
/// * `-c mp4`         mp4 container (H.264 for broad `<video>` compat).
/// * `-f <fps>`       frame rate (already clamped by the caller).
/// * `-k h264`        video codec.
/// * `-bm cbr -q N`   constant-bitrate cap at N kbps — bounds worst-case
///                    storage (a shared 4K playback can't blow the budget).
/// * `-s WxH`         fit-within resolution cap (omitted for native).
/// * `-cursor yes`    include the cursor in the capture.
/// * `-o <path>`      output mp4.
pub fn build_gsr_args(
    monitor: &str,
    fps: u32,
    bitrate_kbps: u32,
    cap: Option<(u32, u32)>,
    output: &str,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-w".into(),
        monitor.to_string(),
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
    ];
    if let Some((w, h)) = cap {
        a.push("-s".into());
        a.push(format!("{w}x{h}"));
    }
    a.push("-o".into());
    a.push(output.to_string());
    a
}

/// Pick the capture-target monitor. Prefers the user's saved name when it
/// still enumerates; else the focused/primary monitor; else the first
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

// ── Runtime detection + enumeration ──────────────────────────────────

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

/// Whether a capture backend can run right now: the binary is present AND
/// at least one display enumerates.
#[cfg(target_os = "linux")]
pub fn capture_available() -> bool {
    locate_gsr().is_some() && !enumerate_displays().is_empty()
}

#[cfg(not(target_os = "linux"))]
pub fn capture_available() -> bool {
    false
}

/// Enumerate selectable monitors for the Settings picker. Primary source
/// is `gpu-screen-recorder --list-monitors` (its names are exactly the
/// `-w` targets the capture uses); Hyprland's `hyprctl monitors -j` fills
/// in the focused flag (and is the fallback when the recorder binary is
/// absent, so the picker can still render on a Hypr box). Empty on any
/// non-wlroots / no-tool machine → the UI shows "capture unavailable".
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

#[cfg(not(target_os = "linux"))]
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
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Ultrawide 3840x1080 fit within 1920x1080 clamps on width.
        assert_eq!(fit_within((3840, 1080), Some((1920, 1080))), (1920, 540));
    }

    #[test]
    fn fit_within_leaves_small_and_native_untouched() {
        // Already within the cap → unchanged.
        assert_eq!(fit_within((1600, 900), Some((1920, 1080))), (1600, 900));
        // No cap → native.
        assert_eq!(fit_within((3840, 2160), None), (3840, 2160));
    }

    #[test]
    fn gsr_args_native_omits_scale() {
        let args = build_gsr_args("DP-1", 15, 3000, None, "/tmp/x/recording.mp4");
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
        let args = build_gsr_args("HDMI-A-1", 30, 6000, Some((1920, 1080)), "/o.mp4");
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
    fn resolve_monitor_prefers_saved_then_primary_then_first() {
        let displays = vec![
            DisplayInfo { name: "DP-1".into(), width: 2560, height: 1440, is_primary: false },
            DisplayInfo { name: "DP-2".into(), width: 1920, height: 1080, is_primary: true },
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
}
