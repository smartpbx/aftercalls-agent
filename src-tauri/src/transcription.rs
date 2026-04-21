use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::config::Config;
use crate::portal::OrgVocab;

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

#[derive(Clone, Copy)]
enum TrackKind {
    Mic,
    System,
}

#[derive(Clone, Debug)]
struct TaggedWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
    speaker: String,
}

pub async fn transcribe_session(
    session_dir: &Path,
    config: &Config,
    vocab: &OrgVocab,
) -> Result<MergedTranscript> {
    let mic = session_dir.join("mic.wav");
    let system = session_dir.join("system.wav");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let api_key = &config.api_keys.assemblyai;

    // Per-track chunking keeps a speaker's phrase whole even when the other
    // track interjects mid-utterance during overlapping speech. Merging at the
    // chunk level (instead of the word level) means "What's happening?" stays
    // one utterance even if the other speaker says "It doesn't" simultaneously.
    let mut timeline: Vec<Utterance> = Vec::new();
    if mic.exists() {
        let words = transcribe_track(&client, api_key, mic.clone(), TrackKind::Mic, vocab).await?;
        timeline.extend(chunk_into_utterances(&words));
    }
    if system.exists() {
        let words =
            transcribe_track(&client, api_key, system.clone(), TrackKind::System, vocab).await?;
        timeline.extend(chunk_into_utterances(&words));
    }
    if timeline.is_empty() {
        anyhow::bail!("no usable tracks in {}", session_dir.display());
    }

    // Stable sort preserves mic-before-system tiebreaker for chunks that
    // start at the same instant (rare but deterministic reads matter for tests).
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
    audio_path: PathBuf,
    kind: TrackKind,
    vocab: &OrgVocab,
) -> Result<Vec<TaggedWord>> {
    let compressed = compress_for_upload(&audio_path)
        .await
        .with_context(|| format!("compress {}", audio_path.display()))?;
    let bytes = fs::read(&compressed).with_context(|| format!("read {}", compressed.display()))?;

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

    let mut create_body = serde_json::json!({
        "audio_url": upload.upload_url,
        "speaker_labels": matches!(kind, TrackKind::System),
    });
    // AssemblyAI rejects empty arrays for these fields; only send when populated.
    if let Some(arr) = vocab.custom_spelling.as_array() {
        if !arr.is_empty() {
            create_body["custom_spelling"] = vocab.custom_spelling.clone();
        }
    }
    if !vocab.word_boost.is_empty() {
        create_body["word_boost"] = serde_json::json!(vocab.word_boost);
    }
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
            "completed" => return Ok(words_from_response(&poll, kind)),
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

async fn compress_for_upload(input: &Path) -> Result<PathBuf> {
    // 16kHz mono Opus is what ASR models consume internally; any more is waste.
    // ~15-20x size reduction vs raw WAV → proportionally faster upload.
    let output = input.with_extension("opus");
    let status = tokio::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("libopus")
        .arg("-b:a")
        .arg("32k")
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("run ffmpeg")?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with {status}");
    }
    Ok(output)
}

fn words_from_response(resp: &TranscriptResponse, kind: TrackKind) -> Vec<TaggedWord> {
    let Some(words) = resp.words.as_ref() else {
        return Vec::new();
    };
    words
        .iter()
        .map(|w| TaggedWord {
            text: w.text.clone(),
            start_ms: w.start,
            end_ms: w.end,
            speaker: match kind {
                TrackKind::Mic => "You".to_string(),
                TrackKind::System => match &w.speaker {
                    Some(s) => format!("Speaker {s}"),
                    None => "Speaker ?".to_string(),
                },
            },
        })
        .collect()
}

fn chunk_into_utterances(words: &[TaggedWord]) -> Vec<Utterance> {
    // Break on speaker change OR large gap OR max duration.
    // Because words from all tracks are merged and time-sorted before this
    // runs, an interjection from "You" in the middle of a "Speaker A"
    // monologue naturally splits A's chunk in half.
    const MAX_GAP_MS: u64 = 1500;
    const MAX_DURATION_MS: u64 = 20_000;

    if words.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut chunk_start = 0usize;

    for i in 1..words.len() {
        let prev = &words[i - 1];
        let cur = &words[i];
        let first = &words[chunk_start];
        let gap = cur.start_ms.saturating_sub(prev.end_ms);
        let duration = cur.end_ms.saturating_sub(first.start_ms);
        let speaker_changed = cur.speaker != first.speaker;

        if gap > MAX_GAP_MS || duration > MAX_DURATION_MS || speaker_changed {
            result.push(build_utterance(&words[chunk_start..i]));
            chunk_start = i;
        }
    }

    if chunk_start < words.len() {
        result.push(build_utterance(&words[chunk_start..]));
    }
    result
}

fn build_utterance(words: &[TaggedWord]) -> Utterance {
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Utterance {
        speaker: words[0].speaker.clone(),
        start_ms: words.first().unwrap().start_ms,
        end_ms: words.last().unwrap().end_ms,
        text,
    }
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
    words: Option<Vec<AssemblyWord>>,
}

#[derive(Deserialize)]
struct AssemblyWord {
    text: String,
    start: u64,
    end: u64,
    #[serde(default)]
    speaker: Option<String>,
}
