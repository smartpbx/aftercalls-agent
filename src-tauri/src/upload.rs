//! Backend interaction for creating the call row, uploading audio, and
//! attaching the local vault-note path after processing lands. The
//! transcription + summarization work is now a separate backend
//! pipeline (see portal::transcribe / portal::summarize).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{read_auth_file, AuthFile, Backend};
use crate::portal::build_auth_header;

#[derive(Deserialize, Debug, Default)]
pub struct UploadUrls {
    pub mic: Option<String>,
    pub system: Option<String>,
    pub mixed: Option<String>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct CreateCallResponse {
    pub call_id: String,
    pub upload_urls: UploadUrls,
}

/// Create (or upsert) the call row on the backend with the minimal
/// metadata we know pre-pipeline: session_id, recorded_at, duration
/// estimate (zero is fine — transcribe backfills the real duration),
/// the source descriptor, and an empty utterances array. The response
/// carries the presigned PUT URLs the agent needs for the audio.
pub async fn create_call(
    backend: &Backend,
    session_dir: &Path,
    duration_ms_hint: u64,
) -> Result<CreateCallResponse> {
    let session_id = session_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let recorded_at = parse_session_timestamp(&session_id);
    let source = read_source_json(session_dir);

    let body = CreateCall {
        session_id: session_id.clone(),
        recorded_at,
        duration_ms: duration_ms_hint as i64,
        title: None,
        matched_client: None,
        summary_text: None,
        action_items: Vec::new(),
        participants: Vec::new(),
        note_markdown_path: None,
        source_kind: source.kind,
        source_app: source.app,
        utterances: Vec::new(),
    };

    let url = format!("{}/v1/calls", backend.url.trim_end_matches('/'));
    let auth = build_auth_header(backend).await?;
    let client = http_client()?;
    let resp = client
        .post(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {status}: {text}");
    }
    resp.json::<CreateCallResponse>()
        .await
        .context("decode create-call response")
}

/// After transcribe + summarize land server-side the row already has
/// the transcript + title + summary persisted. This call just attaches
/// the local vault note path so the portal can link out to it.
///
/// History: previously POSTed back to /v1/calls with a sparse body,
/// trusting a comment that claimed ON CONFLICT DO UPDATE preserved
/// absent fields. It doesn't — the backend overwrites every column
/// from EXCLUDED and re-DELETEs utterances. The narrow /note-path
/// endpoint on the backend only touches the column named.
pub async fn attach_note_path(
    backend: &Backend,
    call_id: &str,
    note_path: &Path,
) -> Result<()> {
    let body = serde_json::json!({
        "note_markdown_path": note_path.to_string_lossy(),
    });

    let url = format!(
        "{}/v1/calls/{}/note-path",
        backend.url.trim_end_matches('/'),
        call_id
    );
    let auth = build_auth_header(backend).await?;
    let client = http_client()?;
    let resp = client
        .post(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {s}: {t}");
    }
    Ok(())
}

/// PUTs the three track files to their presigned URLs. Prefers the
/// `.opus` (compressed by pipeline.rs via ffmpeg) and falls back to
/// the raw `.wav` when ffmpeg isn't available — notably on stock
/// Windows. The consuming services (AssemblyAI, ffmpeg on the
/// backend, the browser's `<audio>`) sniff the container on
/// download, so the bytes drive decoding regardless of the
/// advertised content type.
///
/// Critically, the `content-type` we send on the PUT must match the
/// one the backend baked into the presigned URL's signature — S3
/// SigV4 signs that header. If we send `audio/wav` for a WAV fallback
/// to a URL signed as `audio/ogg`, Spaces returns 403 and the upload
/// silently drops. We use the signed-for content-type for the PUT
/// header and let the consumer sniff the actual bytes.
///
/// Missing files are skipped silently; individual upload failures are
/// logged but don't abort the batch so one broken track doesn't lose
/// the others.
pub async fn upload_audio(session_dir: &Path, urls: &UploadUrls) -> Result<()> {
    let client = http_client()?;

    // Each entry: (presigned URL, the content-type the backend signed
    // the URL with, preferred-then-fallback source paths). The first
    // existing source file gets uploaded with the signed content-type.
    let candidates: [(Option<&str>, &str, &[PathBuf]); 3] = [
        (
            urls.mic.as_deref(),
            "audio/ogg",
            &[
                session_dir.join("mic.opus"),
                session_dir.join("mic.wav"),
            ],
        ),
        (
            urls.system.as_deref(),
            "audio/ogg",
            &[
                session_dir.join("system.opus"),
                session_dir.join("system.wav"),
            ],
        ),
        (
            urls.mixed.as_deref(),
            "audio/ogg",
            &[
                session_dir.join("mixed.opus"),
                session_dir.join("mixed.wav"),
            ],
        ),
    ];

    for (url, content_type, sources) in candidates {
        let Some(url) = url else { continue };
        let Some(path) = sources.iter().find(|p| p.exists()) else {
            continue;
        };
        if let Err(e) = put_file(&client, url, path, content_type).await {
            eprintln!(
                "aftercalls: audio upload failed for {}: {e:#}",
                path.display()
            );
        }
    }
    Ok(())
}

async fn put_file(
    client: &reqwest::Client,
    url: &str,
    path: &PathBuf,
    content_type: &str,
) -> Result<()> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let resp = client
        .put(url)
        .header("content-type", content_type)
        .body(bytes)
        .send()
        .await
        .with_context(|| format!("PUT {}", path.display()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("PUT returned {status}: {text}");
    }
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?)
}

fn parse_session_timestamp(session_id: &str) -> DateTime<Utc> {
    chrono::NaiveDateTime::parse_from_str(session_id, "%Y%m%dT%H%M%SZ")
        .map(|ndt| ndt.and_utc())
        .unwrap_or_else(|_| Utc::now())
}

#[derive(Serialize)]
struct CreateCall {
    session_id: String,
    recorded_at: DateTime<Utc>,
    duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_text: Option<String>,
    action_items: Vec<String>,
    participants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_markdown_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_app: Option<String>,
    utterances: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct SourceDescriptor {
    kind: Option<String>,
    app: Option<String>,
}

fn read_source_json(session_dir: &Path) -> SourceDescriptor {
    let path = session_dir.join("source.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => SourceDescriptor::default(),
    }
}

// AuthFile needs to be reachable for pipeline.rs's peeks at the current
// user (e.g. to surface org_display_name in UI bits). Re-export here
// so callers don't need to reach into config directly.
#[allow(dead_code)]
pub fn current_auth() -> Option<AuthFile> {
    read_auth_file().ok().flatten()
}
