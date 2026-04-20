use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;

const ASSEMBLY_BASE: &str = "https://api.assemblyai.com/v2";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Utterance {
    pub speaker: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MergedTranscript {
    pub session_dir: String,
    pub duration_ms: u64,
    pub timeline: Vec<Utterance>,
}

pub async fn transcribe_session(session_dir: &Path, config: &Config) -> Result<MergedTranscript> {
    let mic = session_dir.join("mic.wav");
    let system = session_dir.join("system.wav");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let api_key = &config.api_keys.assemblyai;

    let mut tasks = Vec::new();
    if mic.exists() {
        tasks.push(transcribe_track(&client, api_key, mic.clone(), "You".to_string()));
    }
    if system.exists() {
        tasks.push(transcribe_track(
            &client,
            api_key,
            system.clone(),
            "System".to_string(),
        ));
    }
    if tasks.is_empty() {
        anyhow::bail!("no track files to transcribe in {}", session_dir.display());
    }

    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await?);
    }

    let mut timeline: Vec<Utterance> = results.into_iter().flatten().collect();
    timeline.sort_by_key(|u| u.start_ms);
    let duration_ms = timeline.last().map(|u| u.end_ms).unwrap_or(0);

    let merged = MergedTranscript {
        session_dir: session_dir.to_string_lossy().into_owned(),
        duration_ms,
        timeline,
    };

    let json = serde_json::to_string_pretty(&merged)?;
    fs::write(session_dir.join("transcript.json"), json).context("write transcript.json")?;
    Ok(merged)
}

async fn transcribe_track(
    client: &reqwest::Client,
    api_key: &str,
    audio_path: std::path::PathBuf,
    speaker_label_for_mic: String,
) -> Result<Vec<Utterance>> {
    let is_mic = speaker_label_for_mic == "You";
    let bytes = fs::read(&audio_path).with_context(|| format!("read {}", audio_path.display()))?;

    let upload: UploadResponse = client
        .post(format!("{ASSEMBLY_BASE}/upload"))
        .header("authorization", api_key)
        .body(bytes)
        .send()
        .await
        .context("upload request")?
        .error_for_status()
        .context("upload status")?
        .json()
        .await
        .context("upload json")?;

    let create_body = serde_json::json!({
        "audio_url": upload.upload_url,
        "speaker_labels": true,
    });
    let created: CreatedResponse = client
        .post(format!("{ASSEMBLY_BASE}/transcript"))
        .header("authorization", api_key)
        .json(&create_body)
        .send()
        .await
        .context("create transcript")?
        .error_for_status()
        .context("create status")?
        .json()
        .await
        .context("create json")?;

    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let poll: TranscriptResponse = client
            .get(format!("{ASSEMBLY_BASE}/transcript/{}", created.id))
            .header("authorization", api_key)
            .send()
            .await
            .context("poll transcript")?
            .error_for_status()
            .context("poll status")?
            .json()
            .await
            .context("poll json")?;

        match poll.status.as_str() {
            "completed" => {
                return Ok(extract_utterances(&poll, is_mic, &speaker_label_for_mic));
            }
            "error" => {
                return Err(anyhow!(
                    "assemblyai transcription error: {}",
                    poll.error.unwrap_or_default()
                ));
            }
            _ => continue,
        }
    }
}

fn extract_utterances(resp: &TranscriptResponse, is_mic: bool, mic_label: &str) -> Vec<Utterance> {
    resp.utterances
        .as_ref()
        .map(|utts| {
            utts.iter()
                .map(|u| Utterance {
                    speaker: if is_mic {
                        mic_label.to_string()
                    } else {
                        format!("Speaker {}", u.speaker)
                    },
                    start_ms: u.start,
                    end_ms: u.end,
                    text: u.text.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct UploadResponse {
    upload_url: String,
}

#[derive(Deserialize)]
struct CreatedResponse {
    id: String,
}

#[derive(Deserialize)]
struct TranscriptResponse {
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    utterances: Option<Vec<AssemblyUtterance>>,
}

#[derive(Deserialize)]
struct AssemblyUtterance {
    speaker: String,
    start: u64,
    end: u64,
    text: String,
}
