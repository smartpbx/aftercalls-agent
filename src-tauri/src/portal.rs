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
