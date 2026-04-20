use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

use crate::config::Backend;
use crate::summary::Summary;
use crate::transcription::MergedTranscript;

pub async fn post_call(
    backend: &Backend,
    transcript: &MergedTranscript,
    summary: &Summary,
    session_dir: &Path,
    note_path: &Path,
) -> Result<()> {
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
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
    Ok(())
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
