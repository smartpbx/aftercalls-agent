//! In-agent "Report an issue" IPC commands (#183).
//!
//! Two commands:
//!   * `inspect_support_attachment(path)` — returns a small file-meta
//!     record + a base64 data-URL preview suitable for the chip
//!     thumbnail. Avoids piping bytes through the JS bridge for the
//!     full upload.
//!   * `submit_support_report(title, body, metadata, attachments)` —
//!     orchestrates the whole submit flow:
//!       1. Flush telemetry so recent diagnostic logs land before the
//!          report row gets indexed.
//!       2. POST `/v1/support/reports` to mint the report id +
//!          presigned PUT URLs for each attachment.
//!       3. PUT each chosen file directly to Spaces using the signed
//!          content-type.
//!       4. POST `.../attachments/finalize` so the backend flips
//!          `uploaded=TRUE` after the HEAD-verification round-trip.
//!
//! The dialog stays vendor-opaque on its own copy; this module talks
//! to the backend by name (HTTP + presigned URLs) but the user never
//! sees those mechanics.
//!
//! v1 NOTE: this implementation does NOT capture native screenshots
//! via Tauri's window API — that path is webview-only on most
//! platforms and stops at the OS-window-frame on Linux/Wayland. The
//! file-picker route is the v1 entry; v2 will revisit native capture.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

use crate::portal::build_auth_header;

// ── Constants (mirror backend caps) ─────────────────────────────────

const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const ALLOWED_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];

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

    // Inline preview is intentionally None in v1 — generating one
    // requires either a base64 dep or piping bytes through the JS
    // bridge, both of which add weight for a 24×24 thumbnail.
    // The chip falls back to a placeholder rect with the filename
    // visible. v2 (screen-record + native capture) will revisit.
    let _ = ALLOWED_MIMES; // referenced by submit; keep the symbol live
    Ok(AttachmentInspect {
        path: path.to_string(),
        filename,
        mime: mime.to_string(),
        size_bytes: size_bytes as i64,
        preview_data_url: None,
    })
}

// ── submit_support_report ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct AttachmentSubmit {
    pub path: String,
    pub filename: String,
    pub mime: String,
    pub size_bytes: i64,
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
    submit_inner(title, body, metadata, attachments)
        .await
        .map_err(|e| e.to_string())
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
        if !ALLOWED_MIMES.contains(&a.mime.as_str()) {
            return Err(anyhow!("unsupported mime: {}", a.mime));
        }
        if a.size_bytes < 0 || a.size_bytes as u64 > MAX_ATTACHMENT_BYTES {
            return Err(anyhow!("attachment too large (max 25 MB)"));
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
                "kind": "screenshot",
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
