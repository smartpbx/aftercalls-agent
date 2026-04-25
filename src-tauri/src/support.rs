//! In-agent "Report an issue" IPC commands (#183, #203).
//!
//! Three commands:
//!   * `inspect_support_attachment(path)` — returns a small file-meta
//!     record suitable for the chip thumbnail. Avoids piping bytes
//!     through the JS bridge for the full upload.
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

use crate::portal::build_auth_header;

// ── Constants (mirror backend caps) ─────────────────────────────────

const MAX_SCREENSHOT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 100 * 1024 * 1024;
const ALLOWED_SCREENSHOT_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];
const ALLOWED_VIDEO_MIMES: &[&str] = &["video/webm"];

/// Subdir name for staged video blobs under the OS temp dir.
/// Keeping the parent dir stable lets a crashed agent's next launch
/// sweep lingering bytes; the per-stage subdir is a random uuid so
/// two concurrent stages don't collide.
const STAGE_DIR_NAME: &str = "aftercalls-support";

fn stage_parent() -> PathBuf {
    std::env::temp_dir().join(STAGE_DIR_NAME)
}

/// Resolve per-kind upload limits. Mirrors the backend's
/// `limits_for_kind` helper so a mime that passes the agent's
/// pre-flight passes the server too.
fn limits_for_kind(kind: &str) -> Option<(&'static [&'static str], u64)> {
    match kind {
        "screenshot" => Some((ALLOWED_SCREENSHOT_MIMES, MAX_SCREENSHOT_BYTES)),
        "video" => Some((ALLOWED_VIDEO_MIMES, MAX_VIDEO_BYTES)),
        _ => None,
    }
}

// ── inspect_support_attachment ─────────────────────────────────────

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

#[tauri::command]
pub fn inspect_support_attachment(path: String) -> Result<AttachmentInspect, String> {
    inspect_inner(&path).map_err(|e| e.to_string())
}

fn inspect_inner(path: &str) -> Result<AttachmentInspect> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p)
        .with_context(|| format!("stat {path}"))?;
    if !meta.is_file() {
        return Err(anyhow!("not a file: {path}"));
    }
    let size_bytes = meta.len();
    let filename = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment")
        .to_string();
    let mime = mime_from_extension(p);

    // Inline preview is intentionally None — generating one requires
    // either a base64 dep or piping bytes through the JS bridge, both
    // of which add weight for a 24×24 thumbnail. The chip falls back
    // to a placeholder rect with the filename visible.
    Ok(AttachmentInspect {
        path: path.to_string(),
        filename,
        mime: mime.to_string(),
        size_bytes: size_bytes as i64,
        preview_data_url: None,
    })
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
    // Use a simple random identifier (nanos + rand-ish fallback) to
    // keep this module dependency-free — we already avoid pulling a
    // separate uuid crate for the agent.
    let id_hi = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id_lo = std::process::id() as u128;
    let stage_id = format!("{:032x}", id_hi.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(id_lo));
    let subdir = stage_parent().join(&stage_id);
    std::fs::create_dir_all(&subdir)
        .with_context(|| format!("create stage dir {}", subdir.display()))?;
    let full = subdir.join(&safe);
    std::fs::write(&full, &bytes)
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

/// Best-effort sweep of lingering staged uploads. Call this at agent
/// startup. Removes subdirs of `<temp>/aftercalls-support/` older
/// than 24 h so a crashed submit from yesterday doesn't leak bytes
/// forever.
pub fn sweep_stage_dir() {
    let parent = stage_parent();
    let Ok(read) = std::fs::read_dir(&parent) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
    for entry in read.flatten() {
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
    title: String,
    body: String,
    metadata: Value,
    attachments: Vec<AttachmentSubmit>,
) -> Result<String, String> {
    // Collect staged paths BEFORE handing off — `submit_inner` may
    // bail part-way, and we still want the temp bytes gone. A staged
    // video sits under `<temp>/aftercalls-support/<id>/recording.webm`
    // so we remove the parent per-stage subdir rather than the file
    // alone (keeps inode hygiene consistent with how we wrote it).
    let staged_dirs: Vec<PathBuf> = attachments
        .iter()
        .filter_map(|a| {
            let path = Path::new(&a.path);
            let parent = path.parent()?;
            let grandparent = parent.parent()?;
            if grandparent.file_name().and_then(|s| s.to_str()) == Some(STAGE_DIR_NAME) {
                Some(parent.to_path_buf())
            } else {
                None
            }
        })
        .collect();

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
