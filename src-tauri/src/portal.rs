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
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use crate::config::{read_auth_file, write_auth_file, AuthFile, Backend, FeatureFlags, PendingTos};
use crate::error::{from_status, parse_retry_after, PortalError};

/// #179 — explicit User-Agent so backend logs can attribute requests to
/// a specific agent build + OS without relying on reqwest's default
/// (`reqwest/0.x.y`). Format: `aftercalls/<version> (<os>)`. Backend's
/// `parse_agent_version_from_headers` looks for the `aftercalls/` token
/// and falls back to `"unknown"` for legacy clients. Computed once per
/// process — `CARGO_PKG_VERSION` and `std::env::consts::OS` are both
/// compile-time / process-constant.
///
/// `pub(crate)` so `upload.rs` (#293) can stamp the same UA on its
/// reqwest builder without duplicating the format string. Other agent
/// modules that fire HTTP at the backend should reuse this rather than
/// re-deriving the format.
pub(crate) fn user_agent() -> String {
    format!(
        "aftercalls/{} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}

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
    /// #215 — per-org feature flags. Default = all-false so a backend
    /// response from before this field landed lands as "feature OFF",
    /// matching the absent-row default.
    #[serde(default)]
    features: FeatureFlags,
    /// #320 — outstanding ToS / privacy versions the user has yet to
    /// accept. Mirrors the portal's `PendingTos[]`. The agent's layout
    /// gates on `len() > 0` and routes the user to `/accept-terms`,
    /// matching the portal flow. Serde-defaulted to an empty Vec so a
    /// backend response from before this field landed (or one that
    /// inadvertently omits it) decodes cleanly with the empty/no-gate
    /// semantics.
    #[serde(default)]
    pending_tos: Vec<PendingTos>,
    // #329 — `org_has_agent_recording` from the backend MeResponse is
    // intentionally NOT mirrored here. The agent has no /calls empty-
    // state CTA today, so the field would be parsed and dropped (dead-
    // code warning). Serde's default behavior ignores unknown JSON
    // fields, so this struct remains forward-compatible. If the agent
    // later adds an equivalent empty-state, plumb it through here +
    // the AuthFile in `config.rs` at the same time.
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
        features: p.user.features,
        pending_tos: p.user.pending_tos,
    }
}

/// Run a login; on success persist auth.json + return the freshly
/// landed credentials so the caller can also surface them to the UI.
pub async fn login(
    backend: &Backend,
    email: &str,
    password: &str,
) -> std::result::Result<AuthFile, PortalError> {
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}/v1/auth/login", backend.url.trim_end_matches('/'));
    let resp = c
        .post(&url)
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let retry = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(from_status(status, body, retry));
    }
    let payload: AuthResponsePayload = resp
        .json()
        .await
        .map_err(PortalError::from)?;
    let auth = merge_auth(payload);
    write_auth_file(&auth).map_err(PortalError::from)?;
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
) -> std::result::Result<AuthFile, PortalError> {
    let body = serde_json::json!({
        "first_name": first_name,
        "last_name": last_name,
    });
    let resp = patch_json_typed(backend, "/v1/auth/me", body).await?;
    // The endpoint returns MeResponse — same shape as AuthResponsePayload.user.
    let me: MeResponse =
        serde_json::from_value(resp).map_err(PortalError::from)?;
    let existing = read_auth_file()
        .map_err(PortalError::from)?
        .ok_or_else(|| PortalError::Unauthorized)?;
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
        features: me.features,
        pending_tos: me.pending_tos,
    };
    write_auth_file(&merged).map_err(PortalError::from)?;
    Ok(merged)
}

/// #34 — mint a single-use, 60s session-handoff token. The user-menu
/// "Open web app" handler calls this, then opens the system browser at
/// `<portal_base>/handoff?token=<t>` so the launched browser lands
/// already-authenticated instead of bouncing through /login. We only
/// need the raw token from the response — the backend's `expires_at`
/// is informational (the server is the only authority on TTL).
pub async fn mint_handoff_token(backend: &Backend) -> std::result::Result<String, PortalError> {
    let resp = post_json_typed(backend, "/v1/auth/handoff/mint", serde_json::json!({})).await?;
    let token = resp
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PortalError::Other {
            message: "mint response missing token".into(),
        })?;
    Ok(token.to_string())
}

/// Revoke the refresh token on the server and wipe auth.json locally.
pub async fn logout(backend: &Backend) -> std::result::Result<(), PortalError> {
    if let Some(auth) = read_auth_file().map_err(PortalError::from)? {
        let c = client().map_err(PortalError::from)?;
        let url = format!(
            "{}/v1/auth/logout",
            backend.url.trim_end_matches('/')
        );
        // Best-effort: a server-side revoke failure shouldn't block
        // the local wipe (mirrors the previous `let _ = …` shape). If
        // the user is offline we still want auth.json cleared so the
        // next launch lands on the login screen.
        let _ = c
            .post(&url)
            .json(&serde_json::json!({ "refresh_token": auth.refresh_token }))
            .send()
            .await;
    }
    crate::config::delete_auth_file().map_err(PortalError::from)?;
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
    // #386 — keyset pagination. `cursor` is whatever opaque RFC-3339
    // string the previous response returned in `next_cursor`; `limit`
    // is 1..=200 with backend default 50 when None. Empty / None on
    // either falls through to the backend default.
    cursor: Option<&str>,
    limit: Option<i64>,
    q: Option<&str>,
    view: Option<&str>,
) -> std::result::Result<Value, PortalError> {
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
    if let Some(c) = cursor {
        if !c.is_empty() {
            params.push(("cursor".into(), c.into()));
        }
    }
    if let Some(n) = limit {
        params.push(("limit".into(), n.to_string()));
    }
    if let Some(search) = q {
        if !search.trim().is_empty() {
            params.push(("q".into(), search.trim().into()));
        }
    }
    if let Some(v) = view {
        if !v.is_empty() && v != "active" {
            params.push(("view".into(), v.into()));
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
    get_json_typed(backend, &path).await
}

pub async fn list_trashed(
    backend: &Backend,
    scope: Option<&str>,
    // #163 (v0.5.2) — optional date-range filter on the trash list,
    // passed through to the backend which narrows by `recorded_at`.
    from_date: Option<&str>,
    to_date: Option<&str>,
    // #386 — keyset pagination, mirror of list_calls. Trash cursors
    // anchor on `deleted_at` server-side; the agent layer just passes
    // the opaque token along.
    cursor: Option<&str>,
    limit: Option<i64>,
) -> std::result::Result<Value, PortalError> {
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
    if let Some(c) = cursor {
        if !c.is_empty() {
            params.push(("cursor".into(), c.into()));
        }
    }
    if let Some(n) = limit {
        params.push(("limit".into(), n.to_string()));
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
    get_json_typed(backend, &path).await
}

pub async fn restore_call(backend: &Backend, id: &str) -> std::result::Result<(), PortalError> {
    post_nop_typed(backend, &format!("/v1/calls/{id}/restore"), serde_json::json!({})).await
}

/// #303 — hydrate a placeholder external recording on demand. Returns
/// the new call_id + a was_new flag (idempotent on already-hydrated
/// rows). Same shape as the portal endpoint; the Tauri command just
/// proxies through `post_nop_typed`-shaped POST → JSON.
pub async fn hydrate_call(backend: &Backend, id: &str) -> std::result::Result<Value, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/calls/{}/hydrate",
        backend.url.trim_end_matches('/'),
        id,
    );
    let resp = c
        .post(&url)
        .header("authorization", auth)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return resp.json::<Value>().await.map_err(PortalError::from);
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

pub async fn permadelete_call(backend: &Backend, id: &str) -> std::result::Result<(), PortalError> {
    // Matches the same auth-header wiring delete_call uses.
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/calls/{}?permanent=true",
        backend.url.trim_end_matches('/'),
        id,
    );
    let resp = c
        .delete(&url)
        .header("authorization", auth)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

pub async fn get_call(backend: &Backend, id: &str) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, &format!("/v1/calls/{id}")).await
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
) -> std::result::Result<(), PortalError> {
    // #82: forward the optional FK. When `None` the field is still
    // emitted as `null` so the backend's `Option<Uuid>` deserializer
    // keeps the current (speaker-only) behaviour. An older backend
    // ignores the unknown field via serde's default-deny.
    patch_nop_typed(
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
    // #188: when present + non-empty, backend rewrites ONLY those idxs
    // (subset rename) and skips summary/participants/action-item
    // prose. `None` or empty slice → global rename (pre-existing).
    utterance_ids: Option<&[i32]>,
) -> std::result::Result<u64, PortalError> {
    // Only include `to_user_id` / `utterance_ids` in the payload when
    // we have them. Omitting `to_user_id` leaves existing FKs on
    // matching rows untouched (backend `Option<Uuid>` with
    // `#[serde(default)]`). Omitting `utterance_ids` selects the
    // backend's global-rename branch; an older backend that doesn't
    // recognise the field ignores it under `#[serde(default)]`.
    let mut payload = serde_json::json!({ "from": from, "to": to });
    if let Some(uid) = to_user_id {
        payload["to_user_id"] = serde_json::Value::String(uid.to_string());
    }
    if let Some(ids) = utterance_ids {
        if !ids.is_empty() {
            payload["utterance_ids"] = serde_json::Value::Array(
                ids.iter()
                    .map(|&i| serde_json::Value::Number(i.into()))
                    .collect(),
            );
        }
    }
    let body: Value = post_json_typed(
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

pub async fn get_org_vocab(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/org/vocab").await
}

pub async fn set_org_vocab(
    backend: &Backend,
    custom_spelling: &Value,
    word_boost: &[String],
) -> std::result::Result<(), PortalError> {
    put_nop_typed(
        backend,
        "/v1/org/vocab",
        serde_json::json!({
            "custom_spelling": custom_spelling,
            "word_boost": word_boost,
        }),
    )
    .await
}

pub async fn list_highlights(
    backend: &Backend,
    call_id: &str,
) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, &format!("/v1/calls/{call_id}/highlights")).await
}

pub async fn create_highlight(
    backend: &Backend,
    call_id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(backend, &format!("/v1/calls/{call_id}/highlights"), body.clone()).await
}

pub async fn update_highlight(
    backend: &Backend,
    id: &str,
    body: &Value,
) -> std::result::Result<(), PortalError> {
    patch_nop_typed(backend, &format!("/v1/highlights/{id}"), body.clone()).await
}

pub async fn delete_highlight(
    backend: &Backend,
    id: &str,
) -> std::result::Result<(), PortalError> {
    delete_nop_typed(backend, &format!("/v1/highlights/{id}")).await
}

pub async fn auto_highlight(
    backend: &Backend,
    call_id: &str,
) -> std::result::Result<Value, PortalError> {
    // LLM call can run a minute+ on a long transcript; bump the timeout.
    post_with_timeout_typed(
        backend,
        &format!("/v1/calls/{call_id}/auto-highlight"),
        serde_json::Value::Null,
        Duration::from_secs(240),
    )
    .await
}

pub async fn get_audio_urls(
    backend: &Backend,
    id: &str,
) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, &format!("/v1/calls/{id}/audio-urls")).await
}

pub async fn get_peaks(backend: &Backend, id: &str) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, &format!("/v1/calls/{id}/peaks.json")).await
}

pub async fn delete_call(backend: &Backend, id: &str) -> std::result::Result<(), PortalError> {
    delete_nop_typed(backend, &format!("/v1/calls/{id}")).await
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
) -> std::result::Result<(), PortalError> {
    patch_nop_typed(
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
) -> std::result::Result<(), PortalError> {
    patch_nop_typed(
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
/// with `{error: "cooldown", retry_after_seconds: N}`.
///
/// Returns `PortalError::Cooldown { retry_after_seconds }` on 429 so
/// the frontend can surface the retry window without sniffing a
/// stringified error (#124). Other backend statuses map through
/// `from_status` to their structured variants.
pub async fn resummarize_call(backend: &Backend, id: &str) -> std::result::Result<Value, PortalError> {
    // Same 600s ceiling as transcribe/summarize: the LLM call can
    // take a minute or two on a real call, and an agent user that
    // triggered the regenerate shouldn't see an IPC timeout before
    // the backend finishes.
    let auth = build_auth_header(backend).await?;
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(user_agent())
        .build()?;
    let url = format!(
        "{}/v1/calls/{id}/resummarize",
        backend.url.trim_end_matches('/'),
    );
    let resp = c.post(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.map_err(PortalError::from)?;
        return serde_json::from_str(&text).map_err(PortalError::from);
    }
    let retry_header = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry_header))
}

/// PATCH /v1/calls/{id} — partial update of summary_text / title /
/// matched_client. Tri-state server-side: each field is either
/// absent, null (clear), or a string (set). The Tauri front-end
/// passes pre-shaped JSON so we forward verbatim.
///
/// Returns the structured `PortalError` shape on failure (#124) so
/// the call-detail page can render kind-specific copy without regex.
pub async fn patch_call(
    backend: &Backend,
    id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    patch_json_typed(backend, &format!("/v1/calls/{id}"), body.clone()).await
}

/// POST /v1/calls/{id}/text-replace — highlight-to-correct (#11).
/// Forwards the verbatim JSON body so the TS side stays the
/// authoritative shape definition. Returns the `{ replaced, regions }`
/// envelope from the backend or a structured PortalError on failure.
pub async fn text_replace(
    backend: &Backend,
    id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(backend, &format!("/v1/calls/{id}/text-replace"), body.clone()).await
}

/// POST /v1/org/client-allowlist — auto-populate the persistent
/// client-allowlist when a user clicks "Leave as text" on a chip
/// (#195). Fire-and-forget from the front-end's point of view:
/// 201 / 200 / 4xx / 5xx all swallowed. Duplicates are collapsed
/// by the server's UNIQUE constraint and surface as 200 with the
/// existing row; past the 500-entry cap the server returns 409.
/// Either way the unlink already succeeded locally, so no user
/// action needed.
pub async fn add_client_allowlist_entry(
    backend: &Backend,
    name: &str,
    source: &str,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
        backend,
        "/v1/org/client-allowlist",
        serde_json::json!({
            "name": name,
            "source": source,
        }),
    )
    .await
}

/// PATCH /v1/calls/{id}/action-items/{item_id} — edit one action
/// item row. Cross-org assignee writes surface as 400 per #82, which
/// the UI renders as an inline error beneath the assignee picker
/// (now keyed on `PortalError::BadRequest.message` rather than a
/// regex on a stringified anyhow::Error, #124).
pub async fn patch_action_item(
    backend: &Backend,
    call_id: &str,
    item_id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    patch_json_typed(
        backend,
        &format!("/v1/calls/{call_id}/action-items/{item_id}"),
        body.clone(),
    )
    .await
}

/// POST /v1/calls/{id}/action-items/manual — manual-add (#104).
/// `body` carries `{description, assignee_user_id?}`. Backend returns
/// 201 with the fully-stitched row. Cross-org assignee FK surfaces
/// as 400; under the structured-error shape (#124) the frontend
/// matches `kind === "bad_request"` instead of regex-sniffing.
pub async fn add_action_item(
    backend: &Backend,
    call_id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
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
/// invoking. Returns a typed `PortalError` (#124).
pub async fn list_me_action_items(
    backend: &Backend,
    status: &str,
    cursor: Option<&str>,
    limit: i64,
    // #173 — Due filter token. `"all"` is the default and is omitted
    // from the URL (matches the portal helper's serialisation rule
    // so the wire shapes line up). Other values pass through.
    due: &str,
) -> std::result::Result<Value, PortalError> {
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
    if !due.is_empty() && due != "all" {
        path.push_str("&due=");
        path.push_str(&urlencoding_minimal(due));
    }
    get_json_typed(backend, &path).await
}

/// DELETE /v1/calls/{id}/action-items/{item_id} — hard delete (#104).
/// Backend returns 204 on success, 404 when the row is already gone;
/// we map 404 to Ok(()) so the frontend's flow matches the portal's
/// `calls.deleteActionItem` (ui-phase-3 §G "404 is silent success").
/// Every other non-2xx surfaces as a structured `PortalError` (#124).
pub async fn delete_action_item(
    backend: &Backend,
    call_id: &str,
    item_id: &str,
) -> std::result::Result<(), PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/calls/{call_id}/action-items/{item_id}",
        backend.url.trim_end_matches('/'),
    );
    let resp = c.delete(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

/// Prefix-match tag suggestions for the Add-tag popover autocomplete.
/// `kind` + `q` are optional but the UI always passes both.
pub async fn tag_suggestions(
    backend: &Backend,
    kind: Option<&str>,
    q: Option<&str>,
) -> std::result::Result<Value, PortalError> {
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
    get_json_typed(backend, &path).await
}

/// GET /v1/org/zoho/status — used on call-detail mount to gate the
/// "Send to CRM" button. Returns the same shape as the portal's
/// `api.zoho.status()`. (#186)
pub async fn zoho_status(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/org/zoho/status").await
}

/// GET /v1/zoho/record-types — record-type picker payload (#197).
/// Returns `{ standard, custom, custom_refreshed_at }`. The agent
/// modal calls this on mount to populate the Step-1 radio list.
pub async fn zoho_record_types(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/zoho/record-types").await
}

/// GET /v1/zoho/records?module=…&q=… — Step 2 of SendToZohoModal. (#186)
pub async fn zoho_search_records(
    backend: &Backend,
    module: &str,
    q: &str,
) -> std::result::Result<Value, PortalError> {
    let path = format!(
        "/v1/zoho/records?module={}&q={}",
        urlencoding_minimal(module),
        urlencoding_minimal(q),
    );
    get_json_typed(backend, &path).await
}

/// POST /v1/calls/{id}/zoho/push — Step 3+4 of SendToZohoModal.
/// `body` is forwarded verbatim; frontend pre-shapes
/// `{module, record_id, record_name, extra_tags?}`. (#186)
pub async fn zoho_push_call(
    backend: &Backend,
    call_id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
        backend,
        &format!("/v1/calls/{call_id}/zoho/push"),
        body.clone(),
    )
    .await
}

// ── Share call (#35 / #243) ─────────────────────────────────────────
//
// Three CRUD helpers wrapping the backend's owner-side share routes.
// All three return a typed `PortalError` (#124) so the agent UI can
// switch on `kind === "forbidden"` / `"network"` instead of regex-
// sniffing a stringified error. The Tauri commands in lib.rs forward
// these verbatim to the front-end.

/// POST /v1/calls/{id}/shares — mint a new share token. The backend
/// returns the raw token + assembled URL exactly once; subsequent
/// list calls only see the SHA256-hashed row. The `body` is shaped
/// by the front-end (`{expires_in_days?, included_sections?}`) and
/// forwarded verbatim so this stays in sync with the portal's
/// `api.calls.createShare` automatically.
pub async fn create_call_share(
    backend: &Backend,
    call_id: &str,
    body: &Value,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
        backend,
        &format!("/v1/calls/{call_id}/shares"),
        body.clone(),
    )
    .await
}

/// GET /v1/calls/{id}/shares — list active + historical shares for
/// the call. The list never includes the raw token / URL; the manage-
/// shares UI shows status + view count + per-link toggle chips. (#243)
pub async fn list_call_shares(
    backend: &Backend,
    call_id: &str,
) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, &format!("/v1/calls/{call_id}/shares")).await
}

/// DELETE /v1/calls/{id}/shares/{share_id} — flip `revoked_at` on
/// the share row. Idempotent — re-revoking a revoked row is a no-op
/// 204. Returns `()` on success; backend non-2xxs surface as a typed
/// `PortalError`. (#243)
pub async fn revoke_call_share(
    backend: &Backend,
    call_id: &str,
    share_id: &str,
) -> std::result::Result<(), PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/calls/{call_id}/shares/{share_id}",
        backend.url.trim_end_matches('/'),
    );
    let resp = c.delete(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
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

/// #179 — returns `(transcript JSON, x-aftercalls-api-version)` so
/// `pipeline::decode_error` telemetry can stamp the server build
/// alongside the malformed response shape. The header is `Option<String>`
/// because (a) older backends don't set it, (b) a 204-style empty
/// response carries no body but may still carry the header.
pub async fn transcribe(
    backend: &Backend,
    call_id: &str,
) -> Result<(Value, Option<String>)> {
    // AssemblyAI job + poll can take a couple minutes on a real call.
    let (value, headers) = post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/transcribe"),
        serde_json::Value::Null,
        Duration::from_secs(600),
    )
    .await?;
    Ok((value, extract_api_version(&headers)))
}

/// #179 — twin of `transcribe`; second tuple element is the backend's
/// `x-aftercalls-api-version` header for `pipeline::decode_error`
/// attribution. `Summary` is the prime decode-mismatch suspect (struct
/// has grown across releases) so the version stamp is most diagnostic
/// here.
pub async fn summarize(
    backend: &Backend,
    call_id: &str,
    transcript: &Value,
    candidate_clients: &[String],
) -> Result<(Value, Option<String>)> {
    let (value, headers) = post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/summarize"),
        serde_json::json!({
            "transcript": transcript,
            "candidate_clients": candidate_clients,
        }),
        Duration::from_secs(240),
    )
    .await?;
    Ok((value, extract_api_version(&headers)))
}

pub async fn generate_peaks(backend: &Backend, call_id: &str) -> Result<Value> {
    // Headers aren't surfaced — peaks generation isn't a decode-shape
    // contract the agent inspects. Drop the second tuple element.
    let (value, _headers) = post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/peaks"),
        serde_json::Value::Null,
        Duration::from_secs(180),
    )
    .await?;
    Ok(value)
}

/// #179 — pull `x-aftercalls-api-version` off a response. Returns None
/// when the header is missing (older backend) or non-UTF8 (shouldn't
/// happen — `SetResponseHeaderLayer` stamps a static ASCII semver).
fn extract_api_version(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-aftercalls-api-version")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// ── PIPEDA recording-ack + notice prefs (#44, #45, #48) ──────────────

/// Returns Some(Value) with `accepted_at` when the user has accepted,
/// None on 404 (not yet accepted). Any other status surfaces as a
/// structured `PortalError` so callers don't mistake a network blip
/// for an un-accepted user.
pub async fn get_recording_ack(
    backend: &Backend,
) -> std::result::Result<Option<Value>, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/me/recording-ack",
        backend.url.trim_end_matches('/')
    );
    let resp = c
        .get(&url)
        .header("authorization", auth)
        .send()
        .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let retry = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(from_status(status, body, retry));
    }
    let text = resp.text().await.map_err(PortalError::from)?;
    let v: Value = serde_json::from_str(&text).map_err(PortalError::from)?;
    Ok(Some(v))
}

pub async fn post_recording_ack(
    backend: &Backend,
    agent_version: &str,
    platform: &str,
) -> std::result::Result<(), PortalError> {
    post_nop_typed(
        backend,
        "/v1/me/recording-ack",
        serde_json::json!({
            "agent_version": agent_version,
            "platform": platform,
        }),
    )
    .await
}

pub async fn get_recording_prefs(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/org/recording-prefs").await
}

// ── Terms of Service / Privacy gate (#320) ───────────────────────────

/// Public endpoint — no auth required. Returns the latest published
/// `terms` + `privacy` document bodies so the accept-terms page can
/// render them. The auth header is built best-effort: when the user is
/// signed in we send it (the backend doesn't require it but doesn't
/// reject it either); when there's no auth.json on disk we send the
/// request unauthenticated rather than erroring out, since the route is
/// public-facing on the backend.
pub async fn tos_current(backend: &Backend) -> std::result::Result<Value, PortalError> {
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/tos/current",
        backend.url.trim_end_matches('/')
    );
    let mut req = c.get(&url);
    if let Ok(auth) = build_auth_header(backend).await {
        req = req.header("authorization", auth);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.map_err(PortalError::from)?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).map_err(PortalError::from);
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

/// Authed. Records acceptance(s) against the current user. Mirrors
/// the portal's `api.tos.accept(ids)` — the wire shape is
/// `{ tos_version_ids: [...] }`.
pub async fn tos_accept(
    backend: &Backend,
    ids: Vec<String>,
) -> std::result::Result<(), PortalError> {
    post_nop_typed(
        backend,
        "/v1/tos/accept",
        serde_json::json!({ "tos_version_ids": ids }),
    )
    .await
}

/// #320 — refetch `/v1/auth/me` and persist the fresh `pending_tos`
/// (and the rest of the profile bundle) into `auth.json` so the next
/// `current_user` read reflects the post-acceptance state without a
/// re-login. Used by the `tos_accept` Tauri command after a successful
/// POST so the layout's gate clears on the same tick.
pub async fn refresh_me(backend: &Backend) -> std::result::Result<AuthFile, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/auth/me",
        backend.url.trim_end_matches('/')
    );
    let resp = c.get(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let retry = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(from_status(status, body, retry));
    }
    let me: MeResponse = resp.json().await.map_err(PortalError::from)?;
    let existing = read_auth_file()
        .map_err(PortalError::from)?
        .ok_or(PortalError::Unauthorized)?;
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
        features: me.features,
        pending_tos: me.pending_tos,
    };
    write_auth_file(&merged).map_err(PortalError::from)?;
    Ok(merged)
}

// ── Org member roster (#65) ──────────────────────────────────────────

/// Slim `[{id, display_name, email}]` roster of active org members.
/// Used by the speaker-rename picker on the call-detail page. Backend
/// endpoint is readable by any authed user (not admin-gated), so the
/// normal `build_auth_header` flow suffices.
pub async fn list_org_members(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/org/members").await
}

/// #107 — backend reachability probe used by the offline indicator.
/// Hits the unauthenticated `/health` route with a short timeout and
/// reports success on any 2xx. Auth header is intentionally omitted —
/// the offline check should keep working when the access token has
/// expired (the user might be offline AND the JWT might be stale,
/// without that being a meaningful "offline" signal). Failures are
/// folded into a single boolean so the frontend doesn't have to
/// branch on every reqwest variant.
pub async fn health_check(backend: &Backend) -> bool {
    let c = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent(user_agent())
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let url = format!("{}/health", backend.url.trim_end_matches('/'));
    match c.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

// ── /settings/privacy parity (#592) ──────────────────────────────────
//
// Thin wrappers around `/v1/auth/me/privacy*` and `/v1/auth/me/export*`
// so the agent's `/settings/privacy` Svelte page can mirror the portal's
// surface byte-for-byte without re-implementing the auth-header refresh
// dance in the webview. Wire shapes match the portal's `myPrivacy` and
// `dataExports` clients exactly — see `portal/src/lib/api.ts` for the TS
// counterpart. All four endpoints route through `effective_user_id()` on
// the backend, so impersonation Just Works (the data-export endpoints
// also reject impersonation JWTs explicitly — surfaced as a 403).

/// `GET /v1/auth/me/privacy` — bundle endpoint backing the page paint.
pub async fn me_privacy_bundle(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/auth/me/privacy").await
}

/// `GET /v1/auth/me/privacy/access-log?cursor=…&limit=…` — cursor-paginated
/// access log used by the page's "Load more" CTA.
pub async fn me_privacy_access_log(
    backend: &Backend,
    cursor: Option<&str>,
    limit: i64,
) -> std::result::Result<Value, PortalError> {
    let mut path = String::from("/v1/auth/me/privacy/access-log?limit=");
    path.push_str(&limit.to_string());
    if let Some(c) = cursor {
        if !c.is_empty() {
            path.push_str("&cursor=");
            path.push_str(&urlencoding_minimal(c));
        }
    }
    get_json_typed(backend, &path).await
}

/// `POST /v1/auth/me/export` — request a fresh data export. 202 on
/// success; 400 with `retry_after_seconds=N` when the 24h cooldown is
/// live; 409 when a previous job is still pending or running. The
/// frontend reads the body string for the cooldown hint.
pub async fn data_exports_request(backend: &Backend) -> std::result::Result<Value, PortalError> {
    post_json_typed(backend, "/v1/auth/me/export", serde_json::json!({})).await
}

/// `GET /v1/auth/me/exports` — newest-first list. No URLs in the
/// response; call `data_exports_get_status` for the download URL of
/// a specific row.
pub async fn data_exports_list(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/auth/me/exports").await
}

/// `GET /v1/auth/me/exports/{id}` — single row plus a freshly-presigned
/// `download_url` when the row is `ready` and inside its retention
/// window.
pub async fn data_exports_get_status(
    backend: &Backend,
    id: &str,
) -> std::result::Result<Value, PortalError> {
    let path = format!("/v1/auth/me/exports/{}", urlencoding_minimal(id));
    get_json_typed(backend, &path).await
}

// ── #630 — per-user summary-style override ──────────────────────────
//
// Sibling of the privacy + data-export shims above. Backs the agent's
// new "AI summary style" Settings card. The wire shape matches the
// portal's `mySummaryStyle` TS client byte-for-byte:
//   - GET returns `{ style, effective_style, org_default }`.
//   - PATCH accepts `{ "style": "narrative"|"hybrid"|"bulleted"|null }`.
//     `null` reverts to inherit. Unknown values reject 400.

pub async fn me_summary_style_get(backend: &Backend) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, "/v1/me/summary-style").await
}

pub async fn me_summary_style_patch(
    backend: &Backend,
    style: Option<&str>,
) -> std::result::Result<Value, PortalError> {
    let body = serde_json::json!({ "style": style });
    patch_json_typed(backend, "/v1/me/summary-style", body).await
}

// ── #595 — per-user import-candidate flow ────────────────────────────
//
// Mirror of the portal's `importCandidates` TS client. The agent's
// `/calls` page renders candidates alongside real call rows when the
// filter pill is "All" or "Importable only". Import promotes a
// candidate (server downloads the upstream recording, runs the
// pipeline, stamps `imported_call_id` on the candidate row); Dismiss
// soft-deletes the candidate so it stops appearing. Both endpoints
// route through `effective_user_id()` on the backend — impersonation
// Just Works the same way the privacy + data-export shims do.

/// `GET /v1/import-candidates` — caller's own open candidates. `source`
/// narrows by `ingest_source` (`smartpbx` / `zoho_meeting`); omitting
/// it returns both. `include_dismissed` flips the default filter so
/// dismissed candidates surface alongside open ones (admin-y view; the
/// page leaves it false by default).
pub async fn import_candidates_list(
    backend: &Backend,
    source: Option<&str>,
    include_dismissed: bool,
) -> std::result::Result<Value, PortalError> {
    let mut path = String::from("/v1/import-candidates");
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(s) = source {
        if !s.is_empty() {
            params.push(("source".into(), s.into()));
        }
    }
    if include_dismissed {
        params.push(("include_dismissed".into(), "true".into()));
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
    get_json_typed(backend, &path).await
}

/// `POST /v1/import-candidates/{id}/import` — promote a candidate into
/// a real call. Returns `{ candidate_id, call_id, was_new }`; a second
/// click while the first is mid-flight returns `was_new=false`
/// referencing the same `call_id` (server-side idempotency).
pub async fn import_candidate_import(
    backend: &Backend,
    id: &str,
) -> std::result::Result<Value, PortalError> {
    let path = format!(
        "/v1/import-candidates/{}/import",
        urlencoding_minimal(id),
    );
    post_json_typed(backend, &path, serde_json::json!({})).await
}

/// `POST /v1/import-candidates/{id}/dismiss` — soft-delete the
/// candidate. Idempotent on already-dismissed rows; cross-org or
/// unknown `id` returns 404 → `PortalError::NotFound`.
pub async fn import_candidate_dismiss(
    backend: &Backend,
    id: &str,
) -> std::result::Result<(), PortalError> {
    let path = format!(
        "/v1/import-candidates/{}/dismiss",
        urlencoding_minimal(id),
    );
    post_nop_typed(backend, &path, serde_json::json!({})).await
}

// ── HTTP primitives ──────────────────────────────────────────────────

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(user_agent())
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

// Pipeline-internal POST helper. Stays on `anyhow::Result` because
// the only callers (`transcribe` / `summarize` / `generate_peaks`)
// thread their failures through pipeline.rs's `anyhow` flow rather
// than the Tauri-IPC path. Tauri commands now use `*_typed` siblings
// that surface a structured `PortalError` to the frontend.
//
// #179 — returns `(Value, HeaderMap)` so callers that care about
// response headers (transcribe + summarize stamp the
// `x-aftercalls-api-version` header onto `pipeline::decode_error`
// telemetry) can read them. Callers that don't care just discard `.1`.
async fn post_with_timeout(
    backend: &Backend,
    path: &str,
    body: Value,
    timeout: Duration,
) -> Result<(Value, HeaderMap)> {
    let auth = build_auth_header(backend).await?;
    let c = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent())
        .build()?;
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
    // Snapshot headers BEFORE consuming the body — `resp.text()` takes
    // `self`, after which the response (and its headers) are gone.
    let headers = resp.headers().clone();
    // Tolerate empty responses — a few endpoints return 204.
    let text = resp.text().await.unwrap_or_default();
    if text.is_empty() {
        return Ok((Value::Null, headers));
    }
    let value: Value = serde_json::from_str(&text).context("decode")?;
    Ok((value, headers))
}


// ── Typed primitives (#124) ───────────────────────────────────────────
//
// Mirrors the `*_json` / `*_nop` helpers above but returns
// `Result<_, PortalError>` so the Tauri-command layer ships the
// structured shape to the frontend. Existing untyped helpers stay in
// place for callers (auth / pipeline / recovery) that still want the
// `anyhow::Error` flow — only the six commands listed in #124
// migrate here.

async fn get_json_typed(backend: &Backend, path: &str) -> std::result::Result<Value, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c.get(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.map_err(PortalError::from)?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).map_err(PortalError::from);
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

async fn post_json_typed(
    backend: &Backend,
    path: &str,
    body: Value,
) -> std::result::Result<Value, PortalError> {
    post_with_timeout_typed(backend, path, body, Duration::from_secs(60)).await
}

async fn post_with_timeout_typed(
    backend: &Backend,
    path: &str,
    body: Value,
    timeout: Duration,
) -> std::result::Result<Value, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent())
        .build()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .post(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.map_err(PortalError::from)?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).map_err(PortalError::from);
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

async fn patch_json_typed(
    backend: &Backend,
    path: &str,
    body: Value,
) -> std::result::Result<Value, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .patch(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.map_err(PortalError::from)?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).map_err(PortalError::from);
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

// `*_nop_typed` siblings mirror the no-body-needed flavours. Same
// classification path as the `_json_typed` helpers — non-2xx funnels
// through `from_status` so the frontend receives the structured shape
// (#124 follow-up: extending the pattern beyond the original six).

async fn patch_nop_typed(
    backend: &Backend,
    path: &str,
    body: Value,
) -> std::result::Result<(), PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .patch(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

async fn put_nop_typed(
    backend: &Backend,
    path: &str,
    body: Value,
) -> std::result::Result<(), PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .put(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}

async fn post_nop_typed(
    backend: &Backend,
    path: &str,
    body: Value,
) -> std::result::Result<(), PortalError> {
    // Thin wrapper that discards the body — many of our POSTs return
    // 200/204 with payloads we don't read on the agent side.
    let _ = post_json_typed(backend, path, body).await?;
    Ok(())
}

async fn delete_nop_typed(
    backend: &Backend,
    path: &str,
) -> std::result::Result<(), PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = c
        .delete(&url)
        .header("authorization", auth)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let retry = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    Err(from_status(status, body, retry))
}
