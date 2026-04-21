use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

use crate::config::Backend;

pub async fn list_calls(backend: &Backend) -> Result<Value> {
    let client = client()?;
    let url = format!("{}/v1/calls", backend.url.trim_end_matches('/'));
    client
        .get(&url)
        .bearer_auth(&backend.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("backend response")?
        .json::<Value>()
        .await
        .context("decode calls list")
}

pub async fn get_call(backend: &Backend, id: &str) -> Result<Value> {
    let client = client()?;
    let url = format!("{}/v1/calls/{}", backend.url.trim_end_matches('/'), id);
    client
        .get(&url)
        .bearer_auth(&backend.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("backend response")?
        .json::<Value>()
        .await
        .context("decode call detail")
}

pub async fn update_utterance(
    backend: &Backend,
    id: &str,
    idx: i32,
    speaker: &str,
) -> Result<()> {
    let client = client()?;
    let url = format!(
        "{}/v1/calls/{}/utterances/{}",
        backend.url.trim_end_matches('/'),
        id,
        idx
    );
    let resp = client
        .patch(&url)
        .bearer_auth(&backend.token)
        .json(&serde_json::json!({ "speaker": speaker }))
        .send()
        .await
        .with_context(|| format!("PATCH {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {s}: {t}");
    }
    Ok(())
}

pub async fn rename_speaker(
    backend: &Backend,
    id: &str,
    from: &str,
    to: &str,
) -> Result<u64> {
    let client = client()?;
    let url = format!(
        "{}/v1/calls/{}/rename-speaker",
        backend.url.trim_end_matches('/'),
        id
    );
    let resp = client
        .post(&url)
        .bearer_auth(&backend.token)
        .json(&serde_json::json!({ "from": from, "to": to }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("backend {status}: {body}");
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    Ok(parsed
        .get("updated")
        .and_then(|v| v.as_u64())
        .unwrap_or(0))
}

#[derive(Default, Clone, Debug)]
pub struct OrgVocab {
    pub custom_spelling: serde_json::Value,
    pub word_boost: Vec<String>,
}

/// Pulls the per-org AssemblyAI hints. Best-effort: on failure we log and
/// return defaults so transcription still runs without vocab.
pub async fn fetch_vocab(backend: &Backend) -> Result<OrgVocab> {
    let client = client()?;
    let url = format!("{}/v1/config", backend.url.trim_end_matches('/'));
    let body: Value = client
        .get(&url)
        .bearer_auth(&backend.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("backend config response")?
        .json()
        .await
        .context("decode backend config")?;

    let custom_spelling = body
        .get("custom_spelling")
        .cloned()
        .unwrap_or(serde_json::json!([]));
    let word_boost = body
        .get("word_boost")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(OrgVocab {
        custom_spelling,
        word_boost,
    })
}

pub async fn get_org_vocab(backend: &Backend) -> Result<Value> {
    let client = client()?;
    let url = format!("{}/v1/org/vocab", backend.url.trim_end_matches('/'));
    client
        .get(&url)
        .bearer_auth(&backend.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("backend response")?
        .json::<Value>()
        .await
        .context("decode org vocab")
}

pub async fn set_org_vocab(
    backend: &Backend,
    custom_spelling: &Value,
    word_boost: &[String],
) -> Result<()> {
    let client = client()?;
    let url = format!("{}/v1/org/vocab", backend.url.trim_end_matches('/'));
    let resp = client
        .put(&url)
        .bearer_auth(&backend.token)
        .json(&serde_json::json!({
            "custom_spelling": custom_spelling,
            "word_boost": word_boost,
        }))
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {status}: {text}");
    }
    Ok(())
}

pub async fn get_audio_urls(backend: &Backend, id: &str) -> Result<Value> {
    let client = client()?;
    let url = format!(
        "{}/v1/calls/{}/audio-urls",
        backend.url.trim_end_matches('/'),
        id
    );
    client
        .get(&url)
        .bearer_auth(&backend.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("backend response")?
        .json::<Value>()
        .await
        .context("decode audio-urls")
}

pub async fn delete_call(backend: &Backend, id: &str) -> Result<()> {
    let client = client()?;
    let url = format!("{}/v1/calls/{}", backend.url.trim_end_matches('/'), id);
    let resp = client
        .delete(&url)
        .bearer_auth(&backend.token)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {status}: {text}");
    }
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}
