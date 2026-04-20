use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::transcription::{MergedTranscript, Utterance};

const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Summary {
    pub title: String,
    pub matched_client: Option<String>,
    pub summary: String,
    pub action_items: Vec<String>,
    #[serde(default)]
    pub participants: Vec<String>,
}

pub async fn generate(
    session_dir: &Path,
    transcript: &MergedTranscript,
    config: &Config,
    candidate_clients: &[String],
) -> Result<Summary> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;

    let transcript_text = format_transcript(&transcript.timeline);
    let system_prompt = r#"You are analyzing a call transcript. The speaker labeled "You" is the user recording the call; other speakers ("Speaker A", "Speaker B", etc.) are other participants.

Output a single JSON object with these fields:
- "title": a short (< 60 chars) descriptive title for the call
- "matched_client": the single best matching client name from the provided candidate list (exact string), or null if no candidate fits
- "summary": a 2-3 paragraph summary of what was discussed
- "action_items": array of concrete action-item strings (empty array if none)
- "participants": array of detected participant names from the transcript (empty if unknown)

Respond with ONLY the JSON object, no surrounding text."#;

    let user_prompt = format!(
        "Candidate clients:\n{}\n\nTranscript:\n{}",
        if candidate_clients.is_empty() {
            "(none provided)".to_string()
        } else {
            candidate_clients.join("\n")
        },
        transcript_text
    );

    let body = serde_json::json!({
        "model": config.openai.model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt}
        ],
        "response_format": {"type": "json_object"}
    });

    let resp = client
        .post(OPENAI_URL)
        .bearer_auth(&config.api_keys.openai)
        .json(&body)
        .send()
        .await
        .context("openai request")?;

    let status = resp.status();
    let text = resp.text().await.context("read openai response")?;
    if !status.is_success() {
        return Err(anyhow!("openai {}: {}", status, text));
    }

    let parsed: OpenAiResponse =
        serde_json::from_str(&text).with_context(|| format!("parse openai envelope: {text}"))?;
    let content = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| anyhow!("no choices in openai response"))?;

    let summary: Summary = serde_json::from_str(&content)
        .with_context(|| format!("parse summary json: {content}"))?;

    fs::write(
        session_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )
    .context("write summary.json")?;
    Ok(summary)
}

fn format_transcript(timeline: &[Utterance]) -> String {
    timeline
        .iter()
        .map(|u| {
            format!(
                "[{}] {}: {}",
                format_ts(u.start_ms),
                u.speaker,
                u.text.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_ts(ms: u64) -> String {
    let total_sec = ms / 1000;
    format!("{:02}:{:02}", total_sec / 60, total_sec % 60)
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}
