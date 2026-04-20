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
