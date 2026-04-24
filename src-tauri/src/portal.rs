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
    // Structured names (#96). Serde defaults cover backend responses
    // that predate the column (shouldn't happen once deployed, but
    // keeps local dev against a stale backend from panicking).
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    display_name: String,
    role: String,
    /// #86 — aftercalls-staff capability, orthogonal to role. Default
    /// covers a backend ≤ step-2 that hasn't yet started emitting the
    /// field.
    #[serde(default)]
    is_platform_staff: bool,
    org_id: String,
    org_slug: String,
    org_display_name: String,
    // Backend added this alongside `pending_tos` for #44 so the agent
    // can cache the one-time recording-ack state at login time and
    // avoid a roundtrip every time Start Recording is clicked.
    // Defaulted to false so older server responses don't break decode.
    #[serde(default)]
    recording_acknowledged: bool,
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
        first_name: p.user.first_name,
        last_name: p.user.last_name,
        display_name: p.user.display_name,
        role: p.user.role,
        is_platform_staff: p.user.is_platform_staff,
        org_id: p.user.org_id,
        org_slug: p.user.org_slug,
        org_display_name: p.user.org_display_name,
        recording_acknowledged: p.user.recording_acknowledged,
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

/// #96: PATCH /v1/auth/me with structured first/last name. On success
/// the backend returns the updated MeResponse; we merge it into the
/// cached auth.json so `current_user` and any downstream reads pick up
/// the new first/last/display_name without requiring a re-login.
/// Returns the refreshed AuthFile (UI renders its `display_name`).
pub async fn update_me(
    backend: &Backend,
    first_name: &str,
    last_name: &str,
) -> Result<AuthFile> {
    let body = serde_json::json!({
        "first_name": first_name,
        "last_name": last_name,
    });
    let resp = patch_json(backend, "/v1/auth/me", body).await?;
    // The endpoint returns MeResponse — same shape as AuthResponsePayload.user.
    let me: MeResponse = serde_json::from_value(resp).context("decode update_me")?;
    let existing = read_auth_file()?
        .ok_or_else(|| anyhow!("no auth on disk — please sign in again"))?;
    // Preserve the tokens (the backend doesn't reissue them on a
    // profile edit); just merge the renamed identity fields so the
    // next `current_user` / autofill sees the new values.
    let merged = AuthFile {
        access_token: existing.access_token,
        access_expires_at: existing.access_expires_at,
        refresh_token: existing.refresh_token,
        refresh_expires_at: existing.refresh_expires_at,
        user_id: me.id,
        email: me.email,
        first_name: me.first_name,
        last_name: me.last_name,
        display_name: me.display_name,
        role: me.role,
        is_platform_staff: me.is_platform_staff,
        org_id: me.org_id,
        org_slug: me.org_slug,
        org_display_name: me.org_display_name,
        recording_acknowledged: me.recording_acknowledged,
    };
    write_auth_file(&merged)?;
    Ok(merged)
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

pub async fn list_calls(
    backend: &Backend,
    scope: Option<&str>,
    user_id: Option<&str>,
    tags: &[String],
    // #146 — optional RFC3339 bounds (`YYYY-MM-DDTHH:MM:SSZ`). Empty
    // or `None` leaves the param off the wire, which matches the
    // backend's `serde(default) = None` default.
    from_date: Option<&str>,
    to_date: Option<&str>,
) -> Result<Value> {
    // Tag filters are passed as repeated ?tag= params; missing = no
    // filter. scope=all restricts to admin/superadmin; user= narrows
    // scope=all to one member's calls. `from_date` / `to_date` add
    // an open-ended date filter — either may be absent.
    let mut path = String::from("/v1/calls");
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(s) = scope {
        if !s.is_empty() {
            params.push(("scope".into(), s.into()));
        }
    }
    if let Some(u) = user_id {
        if !u.is_empty() {
            params.push(("user".into(), u.into()));
        }
    }
    for t in tags {
        params.push(("tag".into(), t.clone()));
    }
    if let Some(f) = from_date {
        if !f.is_empty() {
            params.push(("from_date".into(), f.into()));
        }
    }
    if let Some(t) = to_date {
        if !t.is_empty() {
            params.push(("to_date".into(), t.into()));
        }
    }
    if !params.is_empty() {
        path.push('?');
        let mut first = true;
        for (k, v) in &params {
            if !first {
                path.push('&');
            }
            first = false;
            path.push_str(k);
            path.push('=');
            path.push_str(&urlencoding_minimal(v));
        }
    }
    get_json(backend, &path).await
}

pub async fn list_trashed(
    backend: &Backend,
    scope: Option<&str>,
    // #163 (v0.5.2) — optional date-range filter on the trash list,
    // passed through to the backend which narrows by `recorded_at`.
    from_date: Option<&str>,
    to_date: Option<&str>,
) -> Result<Value> {
    let mut path = String::from("/v1/calls/trashed");
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(s) = scope {
        if !s.is_empty() {
            params.push(("scope".into(), s.into()));
        }
    }
    if let Some(f) = from_date {
        if !f.is_empty() {
            params.push(("from_date".into(), f.into()));
        }
    }
    if let Some(t) = to_date {
        if !t.is_empty() {
            params.push(("to_date".into(), t.into()));
        }
    }
    if !params.is_empty() {
        path.push('?');
        let mut first = true;
        for (k, v) in &params {
            if !first {
                path.push('&');
            }
            first = false;
            path.push_str(k);
            path.push('=');
            path.push_str(&urlencoding_minimal(v));
        }
    }
    get_json(backend, &path).await
}

pub async fn restore_call(backend: &Backend, id: &str) -> Result<()> {
    post_json(backend, &format!("/v1/calls/{id}/restore"), serde_json::json!({})).await?;
    Ok(())
}

pub async fn permadelete_call(backend: &Backend, id: &str) -> Result<()> {
    // Matches the same auth-header wiring delete_call uses.
    let client = reqwest::Client::new();
    let url = format!(
        "{}/v1/calls/{}?permanent=true",
        backend.url.trim_end_matches('/'),
        id,
    );
    let resp = client
        .delete(&url)
        .header("Authorization", build_auth_header(backend).await?)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("permadelete call returned {}", resp.status());
    }
    Ok(())
}

pub async fn get_call(backend: &Backend, id: &str) -> Result<Value> {
    get_json(backend, &format!("/v1/calls/{id}")).await
}

/// Narrow lookup used by the orphan-recovery scanner (#63). Returns
/// Some({id, status}) when the backend has a row for this session,
/// None on 404 (no row — the session never finished the create_call
/// step or was deleted). Any other status is an error.
pub async fn get_call_by_session(
    backend: &Backend,
    session_id: &str,
) -> Result<Option<Value>> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!(
        "{}/v1/calls/by-session/{}",
        backend.url.trim_end_matches('/'),
        session_id,
    );
    let resp = c
        .get(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {s}: {t}");
    }
    let v: Value = resp.json().await.context("decode call-by-session")?;
    Ok(Some(v))
}

pub async fn update_utterance(
    backend: &Backend,
    id: &str,
    idx: i32,
    speaker: &str,
    speaker_user_id: Option<&str>,
) -> Result<()> {
    // #82: forward the optional FK. When `None` the field is still
    // emitted as `null` so the backend's `Option<Uuid>` deserializer
    // keeps the current (speaker-only) behaviour. An older backend
    // ignores the unknown field via serde's default-deny.
    patch_nop(
        backend,
        &format!("/v1/calls/{id}/utterances/{idx}"),
        serde_json::json!({
            "speaker": speaker,
            "speaker_user_id": speaker_user_id,
        }),
    )
    .await
}

pub async fn rename_speaker(
    backend: &Backend,
    id: &str,
    from: &str,
    to: &str,
    to_user_id: Option<&str>,
) -> Result<u64> {
    // Only include `to_user_id` in the payload when we have one.
    // Omitting it on the wire leaves existing FKs on matching rows
    // untouched (backend `Option<Uuid>` with `#[serde(default)]`) —
    // important for text-only renames of already-linked speakers.
    let mut payload = serde_json::json!({ "from": from, "to": to });
    if let Some(uid) = to_user_id {
        payload["to_user_id"] = serde_json::Value::String(uid.to_string());
    }
    let body: Value = post_json(
        backend,
        &format!("/v1/calls/{id}/rename-speaker"),
        payload,
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

// ── Tags (#57) ───────────────────────────────────────────────────────

/// Replace the notes body on a call. Used by the call-detail page
/// for post-pipeline edits (#73). Separate from the recording-screen
/// notes save path, which writes to the session_dir on disk and lets
/// the pipeline's create_call ship the initial value.
pub async fn update_call_notes(
    backend: &Backend,
    id: &str,
    notes: &str,
) -> Result<()> {
    patch_nop(
        backend,
        &format!("/v1/calls/{id}/notes"),
        serde_json::json!({ "notes": notes }),
    )
    .await
}

/// Replace the whole tag array on a call. Backend validates kind +
/// non-empty value; bad input surfaces as a 400 the UI displays inline.
pub async fn update_call_tags(
    backend: &Backend,
    id: &str,
    tags: &Value,
) -> Result<()> {
    patch_nop(
        backend,
        &format!("/v1/calls/{id}/tags"),
        serde_json::json!({ "tags": tags }),
    )
    .await
}

// ── Phase 2 (#19): resummarize + edit-in-place ────────────────────────

/// POST /v1/calls/{id}/resummarize — regenerate summary + AI action
/// items against the call's stored transcript. Backend enforces a
/// 30s per-call cooldown; when we hit it the backend returns 429
/// with `{error: "cooldown", retry_after_seconds: N}`. Surface the
/// retry window by prefixing the error message with "cooldown:{N}"
/// so the Tauri IPC layer can parse it and render a countdown,
/// matching the portal's Error.retryAfterSeconds shape.
pub async fn resummarize_call(backend: &Backend, id: &str) -> Result<Value> {
    // Same 600s ceiling as transcribe/summarize: the LLM call can
    // take a minute or two on a real call, and an agent user that
    // triggered the regenerate shouldn't see an IPC timeout before
    // the backend finishes.
    let auth = build_auth_header(backend).await?;
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let url = format!(
        "{}/v1/calls/{id}/resummarize",
        backend.url.trim_end_matches('/'),
    );
    let resp = c
        .post(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return serde_json::from_str(&text).context("decode resummarize");
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Parse the retry window from the body first, then the
        // Retry-After header as a fallback. Fold it into the error
        // message using a prefix the Tauri command can split back
        // out on the Rust side; ts-side error messages flow through
        // `e.toString()` which only carries the string.
        let retry_header = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();
        let retry_body: Option<u64> = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("retry_after_seconds").and_then(|n| n.as_u64()));
        let retry = retry_body.or(retry_header).unwrap_or(0);
        anyhow::bail!("cooldown:{retry}");
    }
    let t = resp.text().await.unwrap_or_default();
    anyhow::bail!("backend {status}: {t}");
}

/// PATCH /v1/calls/{id} — partial update of summary_text / title /
/// matched_client. Tri-state server-side: each field is either
/// absent, null (clear), or a string (set). The Tauri front-end
/// passes pre-shaped JSON so we forward verbatim.
pub async fn patch_call(backend: &Backend, id: &str, body: &Value) -> Result<Value> {
    patch_json(backend, &format!("/v1/calls/{id}"), body.clone()).await
}

/// PATCH /v1/calls/{id}/action-items/{item_id} — edit one action
/// item row. Cross-org assignee writes surface as 400 per #82, which
/// the UI renders as an inline error beneath the assignee picker.
pub async fn patch_action_item(
    backend: &Backend,
    call_id: &str,
    item_id: &str,
    body: &Value,
) -> Result<Value> {
    patch_json(
        backend,
        &format!("/v1/calls/{call_id}/action-items/{item_id}"),
        body.clone(),
    )
    .await
}

/// POST /v1/calls/{id}/action-items/manual — manual-add (#104).
/// `body` carries `{description, assignee_user_id?}`. Backend returns
/// 201 with the fully-stitched row. Cross-org assignee FK surfaces
/// as 400 with the usual "teammate isn't in your workspace" message
/// shape.
pub async fn add_action_item(
    backend: &Backend,
    call_id: &str,
    body: &Value,
) -> Result<Value> {
    post_json(
        backend,
        &format!("/v1/calls/{call_id}/action-items/manual"),
        body.clone(),
    )
    .await
}

/// GET /v1/me/action-items — Phase 4 (#105) me-scoped list for the
/// /actions page. Query params are passed opaquely so the shape stays
/// in sync with the portal helper automatically; the frontend builds
/// a normalized string (`?status=…&limit=…&cursor=…`) before
/// invoking.
pub async fn list_me_action_items(
    backend: &Backend,
    status: &str,
    cursor: Option<&str>,
    limit: i64,
) -> Result<Value> {
    let mut path = String::from("/v1/me/action-items?");
    path.push_str("status=");
    path.push_str(&urlencoding_minimal(status));
    path.push_str("&limit=");
    path.push_str(&limit.to_string());
    if let Some(c) = cursor {
        if !c.is_empty() {
            path.push_str("&cursor=");
            path.push_str(&urlencoding_minimal(c));
        }
    }
    get_json(backend, &path).await
}

/// DELETE /v1/calls/{id}/action-items/{item_id} — hard delete (#104).
/// Backend returns 204 on success, 404 when the row is already gone;
/// we map 404 to Ok(()) so the frontend's flow matches the portal's
/// `calls.deleteActionItem` (ui-phase-3 §G "404 is silent success").
/// Every other non-2xx is a real error.
pub async fn delete_action_item(
    backend: &Backend,
    call_id: &str,
    item_id: &str,
) -> Result<()> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!(
        "{}/v1/calls/{call_id}/action-items/{item_id}",
        backend.url.trim_end_matches('/'),
    );
    let resp = c
        .delete(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    let t = resp.text().await.unwrap_or_default();
    anyhow::bail!("backend {status}: {t}");
}

/// Prefix-match tag suggestions for the Add-tag popover autocomplete.
/// `kind` + `q` are optional but the UI always passes both.
pub async fn tag_suggestions(
    backend: &Backend,
    kind: Option<&str>,
    q: Option<&str>,
) -> Result<Value> {
    let mut path = String::from("/v1/calls/tag-suggestions");
    let mut first = true;
    let mut push = |k: &str, v: &str| {
        let sep = if first { '?' } else { '&' };
        // Percent-encoding via url::form_urlencoded would be cleaner
        // but pulling a new dep for two known-safe params isn't worth
        // it. Kind + q both reach the backend via Axum's query parser
        // which URL-decodes, so a simple replace of special chars
        // keeps us safe.
        let ev = urlencoding_minimal(v);
        path.push_str(&format!("{sep}{k}={ev}"));
        first = false;
    };
    if let Some(k) = kind {
        push("kind", k);
    }
    if let Some(v) = q {
        push("q", v);
    }
    get_json(backend, &path).await
}

/// Tiny URL-encoder for the handful of characters we need to escape
/// in a tag kind / query string. Full form-urlencoded would need a
/// dep; the backend only sees ASCII + short unicode here so this is
/// enough for safety.
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '+' => out.push_str("%2B"),
            _ => out.push(c),
        }
    }
    out
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

// ── PIPEDA recording-ack + notice prefs (#44, #45, #48) ──────────────

/// Returns Some(Value) with `accepted_at` when the user has accepted,
/// None on 404 (not yet accepted). Any other status is an error so
/// callers don't mistake a network blip for an un-accepted user.
pub async fn get_recording_ack(backend: &Backend) -> Result<Option<Value>> {
    let auth = build_auth_header(backend).await?;
    let c = client()?;
    let url = format!(
        "{}/v1/me/recording-ack",
        backend.url.trim_end_matches('/')
    );
    let resp = c
        .get(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {s}: {t}");
    }
    let v: Value = resp.json().await.context("decode recording-ack")?;
    Ok(Some(v))
}

pub async fn post_recording_ack(
    backend: &Backend,
    agent_version: &str,
    platform: &str,
) -> Result<()> {
    post_json(
        backend,
        "/v1/me/recording-ack",
        serde_json::json!({
            "agent_version": agent_version,
            "platform": platform,
        }),
    )
    .await?;
    Ok(())
}

pub async fn get_recording_prefs(backend: &Backend) -> Result<Value> {
    get_json(backend, "/v1/org/recording-prefs").await
}

// ── Org member roster (#65) ──────────────────────────────────────────

/// Slim `[{id, display_name, email}]` roster of active org members.
/// Used by the speaker-rename picker on the call-detail page. Backend
/// endpoint is readable by any authed user (not admin-gated), so the
/// normal `build_auth_header` flow suffices.
pub async fn list_org_members(backend: &Backend) -> Result<Value> {
    get_json(backend, "/v1/org/members").await
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

async fn patch_json(backend: &Backend, path: &str, body: Value) -> Result<Value> {
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
    let text = resp.text().await.unwrap_or_default();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("decode patch")
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
