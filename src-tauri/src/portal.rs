//! Thin HTTP wrappers around the backend API, plus an auth-aware header
//! builder that prefers the JWT stored in auth.json and falls back to the
//! legacy static bearer token in config.toml for transitional dev setups.
//!
//! Every request here:
//! - resolves the auth header once per call
//! - if the access token has expired we attempt a refresh in-process and
//!   persist the new auth.json before making the request, so a freshly
//!   launched agent doesn't require the user to re-login on every boot

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use crate::config::{read_auth_file, write_auth_file, AuthFile, Backend};

#[derive(Deserialize)]
struct AuthResponsePayload {
    access_token: String,
    access_expires_at: DateTime<Utc>,
    refresh_token: String,
    refresh_expires_at: DateTime<Utc>,
    user: MeResponse,
}

#[derive(Deserialize)]
struct MeResponse {
    id: String,
    email: String,
    display_name: String,
    role: String,
    org_id: String,
    org_slug: String,
    org_display_name: String,
}

/// Returns an `Authorization: Bearer …` value, refreshing the JWT if it's
/// within a minute of expiry. Falls back to the legacy static bearer
/// token only if no auth.json is on disk.
///
/// Shared with upload.rs so post-call pipeline HTTP doesn't skip the
/// refresh and 401 on a stale token that expired mid-recording.
pub async fn build_auth_header(backend: &Backend) -> Result<String> {
    if let Some(mut auth) = read_auth_file()? {
        // Treat any token with <60s left as expired so we don't hand out
        // an access token that's about to fail mid-request.
        let needs_refresh =
            auth.access_expires_at <= Utc::now() + chrono::Duration::seconds(60);
        if needs_refresh {
            if auth.refresh_expires_at <= Utc::now() {
                return Err(anyhow!("refresh token expired — please log in again"));
            }
            let refreshed = do_refresh(backend, &auth.refresh_token).await?;
            auth = merge_auth(refreshed);
            write_auth_file(&auth).ok();
        }
        return Ok(format!("Bearer {}", auth.access_token));
    }
    // Legacy path — a config.toml-supplied static token.
    if let Some(tok) = &backend.token {
        if !tok.is_empty() {
            return Ok(format!("Bearer {tok}"));
        }
    }
    Err(anyhow!("not logged in — please sign in to aftercalls"))
}

async fn do_refresh(backend: &Backend, refresh_token: &str) -> Result<AuthResponsePayload> {
    let client = client()?;
    let url = format!(
        "{}/v1/auth/refresh",
        backend.url.trim_end_matches('/')
    );
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("refresh failed ({s}): {t}");
    }
    resp.json::<AuthResponsePayload>()
        .await
        .context("decode refresh response")
}

fn merge_auth(p: AuthResponsePayload) -> AuthFile {
    AuthFile {
        access_token: p.access_token,
        access_expires_at: p.access_expires_at,
        refresh_token: p.refresh_token,
        refresh_expires_at: p.refresh_expires_at,
        user_id: p.user.id,
        email: p.user.email,
        display_name: p.user.display_name,
        role: p.user.role,
        org_id: p.user.org_id,
        org_slug: p.user.org_slug,
        org_display_name: p.user.org_display_name,
    }
}

/// Run a login; on success persist auth.json + return the freshly
/// landed credentials so the caller can also surface them to the UI.
pub async fn login(
    backend: &Backend,
    email: &str,
    password: &str,
) -> Result<AuthFile> {
    let client = client()?;
    let url = format!("{}/v1/auth/login", backend.url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("login failed ({s}): {t}");
    }
    let payload: AuthResponsePayload =
        resp.json().await.context("decode login")?;
    let auth = merge_auth(payload);
    write_auth_file(&auth)?;
    Ok(auth)
}

/// Revoke the refresh token on the server and wipe auth.json locally.
pub async fn logout(backend: &Backend) -> Result<()> {
    if let Some(auth) = read_auth_file()? {
        let client = client()?;
        let url = format!(
            "{}/v1/auth/logout",
            backend.url.trim_end_matches('/')
        );
        let _ = client
            .post(&url)
            .json(&serde_json::json!({ "refresh_token": auth.refresh_token }))
            .send()
            .await;
    }
    crate::config::delete_auth_file()?;
    Ok(())
}

// ── Existing routes, migrated to the auth-aware header ───────────────

pub async fn list_calls(backend: &Backend) -> Result<Value> {
    get_json(backend, "/v1/calls").await
}

pub async fn get_call(backend: &Backend, id: &str) -> Result<Value> {
    get_json(backend, &format!("/v1/calls/{id}")).await
}

pub async fn update_utterance(
    backend: &Backend,
    id: &str,
    idx: i32,
    speaker: &str,
) -> Result<()> {
    patch_nop(
        backend,
        &format!("/v1/calls/{id}/utterances/{idx}"),
        serde_json::json!({ "speaker": speaker }),
    )
    .await
}

pub async fn rename_speaker(
    backend: &Backend,
    id: &str,
    from: &str,
    to: &str,
) -> Result<u64> {
    let body: Value = post_json(
        backend,
        &format!("/v1/calls/{id}/rename-speaker"),
        serde_json::json!({ "from": from, "to": to }),
    )
    .await?;
    Ok(body.get("updated").and_then(|v| v.as_u64()).unwrap_or(0))
}

#[allow(dead_code)]
#[derive(Default, Clone, Debug)]
pub struct OrgVocab {
    pub custom_spelling: serde_json::Value,
    pub word_boost: Vec<String>,
}

#[allow(dead_code)]
pub async fn fetch_vocab(backend: &Backend) -> Result<OrgVocab> {
    let body: Value = get_json(backend, "/v1/config").await?;
    Ok(OrgVocab {
        custom_spelling: body
            .get("custom_spelling")
            .cloned()
            .unwrap_or(serde_json::json!([])),
        word_boost: body
            .get("word_boost")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

pub async fn get_org_vocab(backend: &Backend) -> Result<Value> {
    get_json(backend, "/v1/org/vocab").await
}

pub async fn set_org_vocab(
    backend: &Backend,
    custom_spelling: &Value,
    word_boost: &[String],
) -> Result<()> {
    put_nop(
        backend,
        "/v1/org/vocab",
        serde_json::json!({
            "custom_spelling": custom_spelling,
            "word_boost": word_boost,
        }),
    )
    .await
}

pub async fn list_highlights(backend: &Backend, call_id: &str) -> Result<Value> {
    get_json(backend, &format!("/v1/calls/{call_id}/highlights")).await
}

pub async fn create_highlight(
    backend: &Backend,
    call_id: &str,
    body: &Value,
) -> Result<Value> {
    post_json(backend, &format!("/v1/calls/{call_id}/highlights"), body.clone()).await
}

pub async fn update_highlight(backend: &Backend, id: &str, body: &Value) -> Result<()> {
    patch_nop(backend, &format!("/v1/highlights/{id}"), body.clone()).await
}

pub async fn delete_highlight(backend: &Backend, id: &str) -> Result<()> {
    delete_nop(backend, &format!("/v1/highlights/{id}")).await
}

pub async fn auto_highlight(backend: &Backend, call_id: &str) -> Result<Value> {
    // LLM call can run a minute+ on a long transcript; bump the timeout.
    post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/auto-highlight"),
        serde_json::Value::Null,
        Duration::from_secs(240),
    )
    .await
}

pub async fn get_audio_urls(backend: &Backend, id: &str) -> Result<Value> {
    get_json(backend, &format!("/v1/calls/{id}/audio-urls")).await
}

pub async fn get_peaks(backend: &Backend, id: &str) -> Result<Value> {
    get_json(backend, &format!("/v1/calls/{id}/peaks.json")).await
}

pub async fn delete_call(backend: &Backend, id: &str) -> Result<()> {
    delete_nop(backend, &format!("/v1/calls/{id}")).await
}

// ── Pipeline (new; the transcription + summary work used to run on the
//    agent against the user's own keys). ─────────────────────────────

pub async fn transcribe(backend: &Backend, call_id: &str) -> Result<Value> {
    // AssemblyAI job + poll can take a couple minutes on a real call.
    post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/transcribe"),
        serde_json::Value::Null,
        Duration::from_secs(600),
    )
    .await
}

pub async fn summarize(
    backend: &Backend,
    call_id: &str,
    transcript: &Value,
    candidate_clients: &[String],
) -> Result<Value> {
    post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/summarize"),
        serde_json::json!({
            "transcript": transcript,
            "candidate_clients": candidate_clients,
        }),
        Duration::from_secs(240),
    )
    .await
}

pub async fn generate_peaks(backend: &Backend, call_id: &str) -> Result<Value> {
    post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/peaks"),
        serde_json::Value::Null,
        Duration::from_secs(180),
    )
    .await
}

// ── HTTP primitives ──────────────────────────────────────────────────

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn get_json(backend: &Backend, path: &str) -> Result<Value> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    c.get(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("backend response")?
        .json::<Value>()
        .await
        .context("decode")
}

async fn post_json(backend: &Backend, path: &str, body: Value) -> Result<Value> {
    post_with_timeout(backend, path, body, Duration::from_secs(60)).await
}

async fn post_with_timeout(
    backend: &Backend,
    path: &str,
    body: Value,
    timeout: Duration,
) -> Result<Value> {
    let auth = build_auth_header(backend).await?;
    let c = reqwest::Client::builder().timeout(timeout).build()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
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
    // Tolerate empty responses — a few endpoints return 204.
    let text = resp.text().await.unwrap_or_default();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("decode")
}

async fn put_nop(backend: &Backend, path: &str, body: Value) -> Result<()> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .put(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {s}: {t}");
    }
    Ok(())
}

async fn patch_nop(backend: &Backend, path: &str, body: Value) -> Result<()> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .patch(&url)
        .header("authorization", auth)
        .json(&body)
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

async fn delete_nop(backend: &Backend, path: &str) -> Result<()> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .delete(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {s}: {t}");
    }
    Ok(())
}
