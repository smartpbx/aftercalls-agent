use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Backend;
use crate::summary::Summary;
use crate::transcription::MergedTranscript;

#[derive(Deserialize, Debug, Default)]
pub struct UploadUrls {
    pub mic: Option<String>,
    pub system: Option<String>,
    pub mixed: Option<String>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)] // call_id kept for future direct-lookup flows
pub struct CreateCallResponse {
    pub call_id: String,
    pub upload_urls: UploadUrls,
}

pub async fn post_call(
    backend: &Backend,
    transcript: &MergedTranscript,
    summary: &Summary,
    session_dir: &Path,
    note_path: &Path,
) -> Result<CreateCallResponse> {
    let session_id = session_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let recorded_at = parse_session_timestamp(&session_id);

    let body = CreateCall {
        session_id: session_id.clone(),
        recorded_at,
        duration_ms: transcript.duration_ms as i64,
        title: &summary.title,
        matched_client: summary.matched_client.as_deref(),
        summary_text: &summary.summary,
        action_items: &summary.action_items,
        participants: &summary.participants,
        note_markdown_path: note_path.to_string_lossy().into_owned(),
        utterances: transcript
            .timeline
            .iter()
            .map(|u| CreateUtterance {
                speaker: &u.speaker,
                start_ms: u.start_ms as i64,
                end_ms: u.end_ms as i64,
                text: &u.text,
            })
            .collect(),
    };

    let client = http_client()?;
    let url = format!("{}/v1/calls", backend.url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .bearer_auth(&backend.token)
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

/// PUTs the three track files (mic.opus, system.opus, mixed.wav) to the
/// presigned URLs returned by /v1/calls. Missing files are skipped silently;
/// individual upload failures are reported but don't abort the batch so one
/// broken track doesn't lose the others.
pub async fn upload_audio(session_dir: &Path, urls: &UploadUrls) -> Result<()> {
    let client = http_client()?;
    let tracks = [
        (urls.mic.as_deref(), session_dir.join("mic.opus"), "audio/ogg"),
        (
            urls.system.as_deref(),
            session_dir.join("system.opus"),
            "audio/ogg",
        ),
        (
            urls.mixed.as_deref(),
            session_dir.join("mixed.wav"),
            "audio/wav",
        ),
    ];
    for (url, path, content_type) in tracks {
        let Some(url) = url else { continue };
        if !path.exists() {
            continue;
        }
        if let Err(e) = put_file(&client, url, &path, content_type).await {
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
        // Uploads can be slow on hotel wifi; 10min is comfortably above worst case.
        .timeout(Duration::from_secs(600))
        .build()?)
}

fn parse_session_timestamp(session_id: &str) -> DateTime<Utc> {
    chrono::NaiveDateTime::parse_from_str(session_id, "%Y%m%dT%H%M%SZ")
        .map(|ndt| ndt.and_utc())
        .unwrap_or_else(|_| Utc::now())
}

#[derive(Serialize)]
struct CreateCall<'a> {
    session_id: String,
    recorded_at: DateTime<Utc>,
    duration_ms: i64,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_client: Option<&'a str>,
    summary_text: &'a str,
    action_items: &'a [String],
    participants: &'a [String],
    note_markdown_path: String,
    utterances: Vec<CreateUtterance<'a>>,
}

#[derive(Serialize)]
struct CreateUtterance<'a> {
    speaker: &'a str,
    start_ms: i64,
    end_ms: i64,
    text: &'a str,
}
