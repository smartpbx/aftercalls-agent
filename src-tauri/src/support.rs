//! In-agent "Report an issue" IPC commands (#183, #203).
//!
//! Three commands:
//!   * `select_support_attachments()` — opens a native image picker and
//!     returns small file-meta records. The webview cannot nominate a path.
//!   * `stage_support_video(bytes, filename)` — writes an in-memory
//!     webm blob (produced by the webview's MediaRecorder) to a temp
//!     file under `<temp>/aftercalls-support/<uuid>/`, returns the
//!     absolute path. Keeps the subsequent presigned-PUT pipeline
//!     path-based and unchanged between screenshots + videos.
//!   * `submit_support_report(title, body, metadata, attachments)` —
//!     orchestrates the whole submit flow:
//!       1. Flush telemetry so recent diagnostic logs land before the
//!          report row gets indexed.
//!       2. POST `/v1/support/reports` to mint the report id +
//!          presigned PUT URLs for each attachment.
//!       3. PUT each chosen file directly to storage using the signed
//!          content-type.
//!       4. POST `.../attachments/finalize` so the backend flips
//!          `uploaded=TRUE` after the HEAD-verification round-trip.
//!       5. Best-effort cleanup of any staged temp files.
//!
//! The dialog stays vendor-opaque on its own copy; this module talks
//! to the backend by name (HTTP + presigned URLs) but the user never
//! sees those mechanics.
//!
//! v2 NOTE (#203): screen-capture happens in the webview via
//! `navigator.mediaDevices.getDisplayMedia` + `MediaRecorder`. We
//! deliberately do NOT pull a native screen-capture crate (e.g.
//! `scap`) — the webview's standards-based APIs already handle the
//! per-OS permission flow, return a `MediaStream` ready for encoding,
//! and don't add to the native dependency graph. See the issue-203
//! plan for the full rationale.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::ipc_security::{IpcSecurity, PathPurpose};
use crate::portal::build_auth_header;

// ── Constants (mirror backend caps) ─────────────────────────────────

const MAX_SCREENSHOT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 100 * 1024 * 1024;
const MAX_LOG_BYTES: u64 = 250 * 1024 * 1024;
const ALLOWED_SCREENSHOT_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];
const ALLOWED_VIDEO_MIMES: &[&str] = &["video/webm"];
// `log` covers the auto-bundled recording-session zip (#628).
// Schema CHECK already permitted 'log' as a reserved kind; this is
// the first wired use of it.
const ALLOWED_LOG_MIMES: &[&str] = &["application/zip"];

/// Subdir name for staged video blobs under the OS temp dir.
/// Keeping the parent dir stable lets a crashed agent's next launch
/// sweep lingering bytes; the per-stage subdir is a random uuid so
/// two concurrent stages don't collide.
const STAGE_DIR_NAME: &str = "aftercalls-support";

fn resolve_stage_root(create: bool) -> Result<PathBuf> {
    let temp = std::env::temp_dir()
        .canonicalize()
        .context("resolve operating-system temp directory")?;
    let parent = temp.join(STAGE_DIR_NAME);
    match parent.symlink_metadata() {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!("support stage root is not a real directory");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            std::fs::create_dir(&parent)
                .with_context(|| format!("create support stage root {}", parent.display()))?;
        }
        Err(error) => return Err(error).context("inspect support stage root"),
    }
    crate::session_fs::enforce_private_dir(&parent)?;
    let canonical = parent
        .canonicalize()
        .context("resolve support stage root")?;
    if canonical.parent() != Some(temp.as_path()) {
        anyhow::bail!("support stage root escaped the operating-system temp directory");
    }
    Ok(canonical)
}

fn allocate_stage_dir() -> Result<PathBuf> {
    let parent = resolve_stage_root(true)?;
    for _ in 0..8 {
        let subdir = parent.join(uuid::Uuid::new_v4().simple().to_string());
        match std::fs::create_dir(&subdir) {
            Ok(()) => {
                crate::session_fs::enforce_private_dir(&subdir)?;
                return Ok(subdir);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create support stage {}", subdir.display()))
            }
        }
    }
    anyhow::bail!("could not allocate a unique support stage directory")
}

/// Resolve per-kind upload limits. Mirrors the backend's
/// `limits_for_kind` helper so a mime that passes the agent's
/// pre-flight passes the server too.
fn limits_for_kind(kind: &str) -> Option<(&'static [&'static str], u64)> {
    match kind {
        "screenshot" => Some((ALLOWED_SCREENSHOT_MIMES, MAX_SCREENSHOT_BYTES)),
        "video" => Some((ALLOWED_VIDEO_MIMES, MAX_VIDEO_BYTES)),
        "log" => Some((ALLOWED_LOG_MIMES, MAX_LOG_BYTES)),
        _ => None,
    }
}

// ── select_support_attachments ────────────────────────────────────

#[derive(Serialize)]
pub struct AttachmentInspect {
    pub path: String,
    pub filename: String,
    pub mime: String,
    pub size_bytes: i64,
    /// `data:<mime>;base64,<bytes>` preview, or None when the file is
    /// too big to inline preview. The chip falls back to a placeholder
    /// rectangle when this is None.
    pub preview_data_url: Option<String>,
}

fn mime_from_extension(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

fn inspect_inner(path: &Path) -> Result<AttachmentInspect> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        return Err(anyhow!("not a file: {}", path.display()));
    }
    let size_bytes = meta.len();
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment")
        .to_string();
    let mime = mime_from_extension(path);

    // Inline preview is intentionally None — generating one requires
    // either a base64 dep or piping bytes through the JS bridge, both
    // of which add weight for a 24×24 thumbnail. The chip falls back
    // to a placeholder rect with the filename visible.
    Ok(AttachmentInspect {
        path: path.to_string_lossy().into_owned(),
        filename,
        mime: mime.to_string(),
        size_bytes: size_bytes as i64,
        preview_data_url: None,
    })
}

#[tauri::command]
pub async fn select_support_attachments(
    app: tauri::AppHandle,
    security: State<'_, IpcSecurity>,
    max_files: u8,
) -> Result<Vec<AttachmentInspect>, String> {
    if max_files == 0 {
        return Ok(Vec::new());
    }
    let picked = app
        .dialog()
        .file()
        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"])
        .blocking_pick_files()
        .unwrap_or_default();
    let mut attachments = Vec::new();
    for picked in picked.into_iter().take(usize::from(max_files.min(5))) {
        let path = picked
            .into_path()
            .map_err(|e| format!("resolve selected attachment: {e}"))?;
        let canonical = crate::ipc_security::canonical_existing_file(&path.to_string_lossy())?;
        security.approve_path(PathPurpose::SupportAttachment, canonical.clone());
        attachments.push(inspect_inner(&canonical).map_err(|e| e.to_string())?);
    }
    Ok(attachments)
}

// ── stage_support_video ────────────────────────────────────────────

#[derive(Serialize)]
pub struct StagedAttachment {
    /// Absolute path of the temp file we just wrote. Pass this back
    /// into `submit_support_report` inside the attachment list —
    /// the submit path reads from disk, so a staged video rides the
    /// same pipeline as a file-picker screenshot.
    pub path: String,
    /// Actual bytes we wrote (for the chip size display).
    pub size_bytes: i64,
}

/// Write a webview-produced blob to a temp file so the existing
/// path-based upload pipeline can ride unchanged. Caller is
/// responsible for calling `cleanup_staged` after submit finishes
/// (success or failure); we also belt-and-braces sweep at startup
/// via `sweep_stage_dir()`.
#[tauri::command]
pub fn stage_support_video(
    bytes: Vec<u8>,
    filename: String,
) -> Result<StagedAttachment, String> {
    stage_support_video_inner(bytes, filename).map_err(|e| e.to_string())
}

fn stage_support_video_inner(
    bytes: Vec<u8>,
    filename: String,
) -> Result<StagedAttachment> {
    // Gate on the agent-side max cap so we don't write a megabyte-
    // heavy blob just to have the server reject it.
    if bytes.len() as u64 > MAX_VIDEO_BYTES {
        anyhow::bail!("recording exceeds 100 MB cap");
    }
    let safe = sanitise_filename(&filename);
    let subdir = allocate_stage_dir()?;
    let full = subdir.join(&safe);
    crate::session_fs::write_private_file(&full, &bytes)
        .with_context(|| format!("write staged video {}", full.display()))?;
    Ok(StagedAttachment {
        path: full.to_string_lossy().into_owned(),
        size_bytes: bytes.len() as i64,
    })
}

fn sanitise_filename(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    // Mirror the backend cap at 120 chars + fall back to a
    // deterministic default if the whole thing collapses to empty
    // (the backend would also normalise, but an empty filename
    // produces a weird-looking chip in the meantime).
    let trimmed: String = out.chars().take(120).collect();
    if trimmed.is_empty() { "recording.webm".into() } else { trimmed }
}

// ── bundle_latest_session ──────────────────────────────────────────
//
// Packages the most recent recording session under
// `<app_data>/recordings/<timestamp>/` into a single zip + a
// session-meta.json with system context (#628). Exposed as a Tauri
// command the Submit Issue dialog calls when the user opts to attach
// their last recording to a support ticket.
//
// The output zip is staged under the same `<temp>/aftercalls-support/`
// dir + uuid pattern as `stage_support_video`, so the existing
// `submit_support_report` upload path picks it up unchanged.

/// Static metadata bundled into `session-meta.json`. Strict-no-PII:
/// version, OS, mic device label, free disk, locale. NOT environment
/// vars, NOT clipboard, NOT user.email.
#[derive(Serialize)]
struct SessionMeta {
    agent_version: &'static str,
    platform: &'static str,
    os_version: String,
    bundled_at_utc: String,
    session_dir_name: String,
    session_files: Vec<SessionFileMeta>,
}

#[derive(Serialize)]
struct SessionFileMeta {
    name: String,
    size_bytes: u64,
}

#[tauri::command]
pub fn bundle_latest_session(app: tauri::AppHandle) -> Result<StagedAttachment, String> {
    bundle_latest_session_inner(&app).map_err(|e| format!("{e:#}"))
}

fn bundle_latest_session_inner(app: &tauri::AppHandle) -> Result<StagedAttachment> {
    use tauri::Manager;
    let recordings_root = app
        .path()
        .app_local_data_dir()
        .map_err(|e| anyhow!("resolve app data dir: {e}"))?
        .join("recordings");

    let session_dir = pick_latest_session(&recordings_root)
        .ok_or_else(|| anyhow!("no recording sessions on disk under {}", recordings_root.display()))?;

    let session_name = session_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();

    // Stage under the same temp tree the video pipeline uses so
    // sweep_stage_dir() and the existing submit cleanup catch it.
    let subdir = allocate_stage_dir()?;

    let zip_filename = format!("aftercalls-session-{session_name}.zip");
    let zip_path = subdir.join(&zip_filename);

    let session_files = write_session_zip(&session_dir, &zip_path)?;

    // Also add a session-meta.json with system context the user might
    // forget to mention. The metadata is written INTO the zip via a
    // second open + append; cheaper than re-writing the whole archive.
    append_meta_json(&zip_path, &session_name, session_files)?;

    let size_bytes = std::fs::metadata(&zip_path)
        .with_context(|| format!("stat {}", zip_path.display()))?
        .len();

    Ok(StagedAttachment {
        path: zip_path.to_string_lossy().into_owned(),
        size_bytes: size_bytes as i64,
    })
}

fn pick_latest_session(recordings_root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(recordings_root).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &best {
            Some((m, _)) if *m >= modified => {}
            _ => best = Some((modified, path)),
        }
    }
    best.map(|(_, p)| p)
}

/// Stream every regular file under `session_dir` into a fresh zip at
/// `zip_path`. Subdirectories are NOT recursed — recording sessions
/// are flat by convention and recursing would balloon the bundle if a
/// future feature drops large temp files into a session subdir. Emits
/// a per-file metadata vec the caller folds into session-meta.json.
fn write_session_zip(session_dir: &Path, zip_path: &Path) -> Result<Vec<SessionFileMeta>> {
    use std::io::{Read, Write};

    let file = crate::session_fs::create_private_file(zip_path)
        .with_context(|| format!("create {}", zip_path.display()))?;
    let mut zw = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(3));

    let mut metas = Vec::new();
    let read_dir = std::fs::read_dir(session_dir)
        .with_context(|| format!("read_dir {}", session_dir.display()))?;
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let meta = entry.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        metas.push(SessionFileMeta {
            name: name.clone(),
            size_bytes: size,
        });
        zw.start_file(&name, opts)
            .with_context(|| format!("zip start_file {name}"))?;
        let mut input = std::fs::File::open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zw.write_all(&buf[..n])?;
        }
    }
    zw.finish().context("zip finish")?;
    Ok(metas)
}

fn append_meta_json(
    zip_path: &Path,
    session_name: &str,
    session_files: Vec<SessionFileMeta>,
) -> Result<()> {
    use std::io::Write;
    let meta = SessionMeta {
        agent_version: env!("CARGO_PKG_VERSION"),
        platform: std::env::consts::OS,
        os_version: detect_os_version(),
        bundled_at_utc: chrono::Utc::now().to_rfc3339(),
        session_dir_name: session_name.to_string(),
        session_files,
    };
    let body = serde_json::to_vec_pretty(&meta).context("serialize session meta")?;

    // Re-open the zip in append mode. zip 2.x supports this via
    // ZipWriter::new_append over a seekable handle.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(zip_path)
        .with_context(|| format!("open zip for append {}", zip_path.display()))?;
    let mut zw = zip::ZipWriter::new_append(file).context("zip new_append")?;
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zw.start_file("session-meta.json", opts)
        .context("zip start_file meta")?;
    zw.write_all(&body)?;
    zw.finish().context("zip finish (meta)")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn detect_os_version() -> String {
    use std::process::Command;
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(target_os = "linux")]
fn detect_os_version() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(target_os = "windows")]
fn detect_os_version() -> String {
    // sysinfo carries this for free since we already depend on it for
    // memory/process metrics elsewhere in the agent.
    let mut sys = sysinfo::System::new();
    sys.refresh_all();
    sysinfo::System::long_os_version().unwrap_or_else(|| "windows".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn detect_os_version() -> String {
    "unknown".into()
}

/// Best-effort sweep of lingering staged uploads. Call this at agent
/// startup. Removes subdirs of `<temp>/aftercalls-support/` older
/// than 24 h so a crashed submit from yesterday doesn't leak bytes
/// forever.
pub fn sweep_stage_dir() {
    let Ok(parent) = resolve_stage_root(false) else { return };
    let Ok(read) = std::fs::read_dir(&parent) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
    for entry in read.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if uuid::Uuid::parse_str(&name).is_err() {
            continue;
        }
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if modified < cutoff {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ── submit_support_report ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct AttachmentSubmit {
    pub path: String,
    pub filename: String,
    pub mime: String,
    pub size_bytes: i64,
    /// `screenshot` (default) or `video`. Defaulted for backwards
    /// compat with any caller still passing the #183 shape.
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "screenshot".to_string()
}

fn trusted_staged_file(input: &str) -> Result<(PathBuf, PathBuf)> {
    let canonical = crate::ipc_security::canonical_existing_file(input)
        .map_err(|error| anyhow!(error))?;
    let root = resolve_stage_root(false)?;
    let relative = canonical
        .strip_prefix(&root)
        .map_err(|_| anyhow!("attachment is outside the private support stage"))?;
    let components: Vec<_> = relative.components().collect();
    if components.len() != 2 {
        anyhow::bail!("staged attachment must be one file under one stage directory");
    }
    let stage_id = match components[0] {
        std::path::Component::Normal(value) => value,
        _ => anyhow::bail!("invalid support stage path"),
    };
    let stage_id_text = stage_id
        .to_str()
        .ok_or_else(|| anyhow!("invalid support stage identifier"))?;
    uuid::Uuid::parse_str(stage_id_text).context("invalid support stage identifier")?;
    let stage_dir = root.join(stage_id);
    let stage_metadata = stage_dir
        .symlink_metadata()
        .context("inspect support stage directory")?;
    if stage_metadata.file_type().is_symlink() || !stage_metadata.is_dir() {
        anyhow::bail!("support stage directory is not a real directory");
    }
    let canonical_stage = stage_dir
        .canonicalize()
        .context("resolve support stage directory")?;
    if canonical.parent() != Some(canonical_stage.as_path()) {
        anyhow::bail!("staged attachment escaped its private directory");
    }
    Ok((canonical, canonical_stage))
}

fn expected_mime(path: &Path, kind: &str) -> &'static str {
    match kind {
        "screenshot" => mime_from_extension(path),
        "video" if path.extension().and_then(|value| value.to_str()) == Some("webm") => {
            "video/webm"
        }
        "log" if path.extension().and_then(|value| value.to_str()) == Some("zip") => {
            "application/zip"
        }
        _ => "application/octet-stream",
    }
}

fn validate_attachment_paths(
    security: &IpcSecurity,
    attachments: &mut [AttachmentSubmit],
) -> Result<Vec<PathBuf>> {
    let mut staged_dirs = Vec::new();
    for attachment in attachments {
        let canonical = if attachment.kind == "screenshot" {
            security
                .require_approved_file(PathPurpose::SupportAttachment, &attachment.path)
                .map_err(|error| anyhow!(error))?
        } else {
            let (canonical, stage_dir) = trusted_staged_file(&attachment.path)?;
            if !staged_dirs.contains(&stage_dir) {
                staged_dirs.push(stage_dir);
            }
            canonical
        };
        let filename = canonical
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("attachment filename is not valid UTF-8"))?;
        if filename != attachment.filename {
            anyhow::bail!("attachment filename does not match the selected file");
        }
        let actual_mime = expected_mime(&canonical, &attachment.kind);
        if actual_mime != attachment.mime {
            anyhow::bail!("attachment type does not match its file extension");
        }
        let actual_size = canonical
            .metadata()
            .context("inspect attachment size")?
            .len();
        if attachment.size_bytes < 0 || actual_size != attachment.size_bytes as u64 {
            anyhow::bail!("attachment size changed after it was selected");
        }
        attachment.path = canonical.to_string_lossy().into_owned();
    }
    Ok(staged_dirs)
}

#[derive(Deserialize, Serialize)]
struct CreateReportResponse {
    id: String,
    attachments: Vec<CreateReportAttachment>,
}

#[derive(Deserialize, Serialize)]
struct CreateReportAttachment {
    id: String,
    filename: String,
    presigned_put_url: String,
}

#[tauri::command]
pub async fn submit_support_report(
    security: State<'_, IpcSecurity>,
    title: String,
    body: String,
    metadata: Value,
    mut attachments: Vec<AttachmentSubmit>,
) -> Result<String, String> {
    // Resolve and fence every path before the network request. In particular,
    // cleanup directories are derived only from validated private stage paths;
    // an IPC string can never nominate an arbitrary directory for deletion.
    let staged_dirs = validate_attachment_paths(&security, &mut attachments)
        .map_err(|error| error.to_string())?;

    let result = submit_inner(title, body, metadata, attachments)
        .await
        .map_err(|e| e.to_string());

    // Best-effort cleanup. Failure to remove a staged dir is
    // non-fatal — `sweep_stage_dir` at next startup will get it.
    for dir in &staged_dirs {
        let _ = std::fs::remove_dir_all(dir);
    }

    result
}

async fn submit_inner(
    title: String,
    body: String,
    mut metadata: Value,
    attachments: Vec<AttachmentSubmit>,
) -> Result<String> {
    // ── Validation parity with the backend ─────────────────────
    let title = title.trim().to_string();
    let body = body.trim().to_string();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(anyhow!("subject must be 1-200 characters"));
    }
    if body.is_empty() || body.chars().count() > 8_000 {
        return Err(anyhow!("description must be 1-8000 characters"));
    }
    if attachments.len() > 5 {
        return Err(anyhow!("max 5 attachments per report"));
    }
    for a in &attachments {
        let Some((allowed_mimes, max_bytes)) = limits_for_kind(&a.kind) else {
            return Err(anyhow!("unsupported attachment kind: {}", a.kind));
        };
        if !allowed_mimes.contains(&a.mime.as_str()) {
            return Err(anyhow!("unsupported mime: {}", a.mime));
        }
        if a.size_bytes < 0 || a.size_bytes as u64 > max_bytes {
            let cap_mb = max_bytes / (1024 * 1024);
            return Err(anyhow!("attachment too large (max {cap_mb} MB)"));
        }
    }

    // ── Telemetry flush — push the live ring buffer first so the
    //    staff pivot from the report into agent_logs has the most
    //    recent context. Failure is non-fatal — never block the
    //    report on the diagnostics ship.
    if let Err(e) = crate::telemetry::flush_now().await {
        eprintln!("aftercalls support: telemetry flush failed (non-fatal): {e:#}");
    }

    // ── Enrich metadata with recent session ids if it's an object
    //    so the staff side has a pivot point into agent_logs without
    //    the user having to copy-paste a session id.
    if let Value::Object(map) = &mut metadata {
        let recent = crate::telemetry::recent_session_ids(8);
        map.insert("recent_session_ids".to_string(), Value::from(recent));
    }

    let cfg = crate::config::Config::load()?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| anyhow!("no backend configured"))?;

    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("http client build")?;

    // ── 1. POST /v1/support/reports ────────────────────────────
    let auth = build_auth_header(backend).await?;
    let create_url = format!(
        "{}/v1/support/reports",
        backend.url.trim_end_matches('/')
    );
    let create_body = serde_json::json!({
        "title": title,
        "body": body,
        "metadata": metadata,
        "attachments": attachments
            .iter()
            .map(|a| serde_json::json!({
                "kind": a.kind,
                "filename": a.filename,
                "size_bytes": a.size_bytes,
                "mime": a.mime,
            }))
            .collect::<Vec<_>>(),
    });
    let resp = client
        .post(&create_url)
        .header("authorization", &auth)
        .json(&create_body)
        .send()
        .await
        .with_context(|| format!("POST {create_url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("create report failed ({s}): {t}");
    }
    let created: CreateReportResponse =
        resp.json().await.context("decode create-report response")?;

    // ── 2. PUT each presigned URL ─────────────────────────────
    // Pair attachments by filename — the create response lists them
    // in the same order we submitted, but we match defensively in
    // case the backend ever permutes (it doesn't today).
    for (i, slot) in created.attachments.iter().enumerate() {
        crate::media_upload::validate_signed_url(&slot.presigned_put_url)
            .context("reject unsafe support upload URL")?;
        let local = attachments
            .get(i)
            .ok_or_else(|| anyhow!("create response had unexpected attachment count"))?;
        let bytes = tokio::fs::read(&local.path)
            .await
            .with_context(|| format!("read {}", local.path))?;
        let put = client
            .put(&slot.presigned_put_url)
            .header("content-type", &local.mime)
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("PUT {}", local.filename))?;
        if !put.status().is_success() {
            let s = put.status();
            let t = put.text().await.unwrap_or_default();
            anyhow::bail!("upload failed ({s}): {t}");
        }
    }

    // ── 3. POST .../attachments/finalize ──────────────────────
    if !created.attachments.is_empty() {
        let finalize_url = format!(
            "{}/v1/support/reports/{}/attachments/finalize",
            backend.url.trim_end_matches('/'),
            created.id,
        );
        let finalize_body = serde_json::json!({
            "attachment_ids": created
                .attachments
                .iter()
                .map(|a| a.id.clone())
                .collect::<Vec<_>>(),
        });
        let auth = build_auth_header(backend).await?;
        let resp = client
            .post(&finalize_url)
            .header("authorization", &auth)
            .json(&finalize_body)
            .send()
            .await
            .with_context(|| format!("POST {finalize_url}"))?;
        if !resp.status().is_success() {
            let s = resp.status();
            let t = resp.text().await.unwrap_or_default();
            // Soft-failure: the report row exists; the user's text
            // is on the backend. We still bubble up so the dialog
            // can show the partial-failure banner — staff will see
            // the report without the screenshots.
            anyhow::bail!("finalize failed ({s}): {t}");
        }
    }

    Ok(created.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aftercalls-support-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn screenshot_submit_requires_the_exact_native_selected_file() {
        let scratch = Scratch::new();
        let path = scratch.0.join("evidence.png");
        std::fs::write(&path, b"not-a-real-png-but-path-validation-is-exact").unwrap();
        let canonical = path.canonicalize().unwrap();
        let security = IpcSecurity::default();
        security.approve_path(PathPurpose::SupportAttachment, canonical.clone());

        let mut attachments = vec![AttachmentSubmit {
            path: canonical.to_string_lossy().into_owned(),
            filename: "evidence.png".into(),
            mime: "image/png".into(),
            size_bytes: canonical.metadata().unwrap().len() as i64,
            kind: "screenshot".into(),
        }];
        assert!(validate_attachment_paths(&security, &mut attachments).is_ok());

        attachments[0].filename = "other.png".into();
        assert!(validate_attachment_paths(&security, &mut attachments).is_err());
    }

    #[test]
    fn unapproved_screenshot_path_is_rejected() {
        let scratch = Scratch::new();
        let path = scratch.0.join("secret.png");
        std::fs::write(&path, b"private").unwrap();
        let mut attachments = vec![AttachmentSubmit {
            path: path.to_string_lossy().into_owned(),
            filename: "secret.png".into(),
            mime: "image/png".into(),
            size_bytes: 7,
            kind: "screenshot".into(),
        }];
        assert!(validate_attachment_paths(&IpcSecurity::default(), &mut attachments).is_err());
    }
}
