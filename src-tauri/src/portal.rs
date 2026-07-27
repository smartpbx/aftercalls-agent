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
use rand::Rng;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::{
    read_auth_file, write_auth_file, AuthFile, Backend, FeatureFlags, PendingTos, Subscription,
};
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
    /// #659 P5a — the org's default co-pilot persona (`"sales"` /
    /// `"support"`). Serde default = `"sales"` covers a backend that
    /// predates the field. Cached into `AuthFile` so the Record page seeds
    /// the CoPilotPanel mode toggle at mount.
    #[serde(default = "crate::config::default_copilot_mode")]
    copilot_default_mode: String,
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
        copilot_default_mode: p.user.copilot_default_mode,
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
        copilot_default_mode: me.copilot_default_mode,
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

// #661 (speaker-identity Phase A) — unresolved naming suggestions for a
// call. Returns the raw JSON array; the front-end owns the shape. Confirm
// has no endpoint of its own (it reuses `rename_speaker`), so only the GET
// + the dismiss below are net-new on the agent bridge.
pub async fn speaker_suggestions(
    backend: &Backend,
    id: &str,
) -> std::result::Result<Value, PortalError> {
    get_json_typed(backend, &format!("/v1/calls/{id}/speaker-suggestions")).await
}

// #661 — dismiss a pending suggestion (204). Marks it resolved server-side
// so it stops re-nagging; no rename. Body is unused by the handler (Path
// extractor only) but `post_nop_typed` always sends one — harmless.
pub async fn dismiss_speaker_suggestion(
    backend: &Backend,
    id: &str,
    suggestion_id: &str,
) -> std::result::Result<(), PortalError> {
    post_nop_typed(
        backend,
        &format!("/v1/calls/{id}/speaker-suggestions/{suggestion_id}/dismiss"),
        serde_json::json!({}),
    )
    .await
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

/// GET /v1/live/crm-context — #653 co-pilot CRM pull. `contact_id` is the
/// Zoho contact id the user picked (MVP primary key); `session_uuid` is
/// the optional live-session anchor (write-through target / future
/// auto-match fallback). At least one must be present — the backend
/// returns 400 otherwise. Returns the contact-card + open-Deals envelope;
/// the copilot flag gate 404s when off, and a Zoho hiccup degrades
/// in-band (`zoho:"not_connected"` / `deals.status:"unavailable"`).
/// Mirrors `zoho_search_records`'s `get_json_typed` transport.
pub async fn live_crm_context(
    backend: &Backend,
    contact_id: Option<&str>,
    session_uuid: Option<&str>,
    mode: Option<&str>,
) -> std::result::Result<Value, PortalError> {
    let mut params: Vec<String> = Vec::new();
    if let Some(cid) = contact_id.map(str::trim).filter(|s| !s.is_empty()) {
        params.push(format!("contact_id={}", urlencoding_minimal(cid)));
    }
    if let Some(su) = session_uuid.map(str::trim).filter(|s| !s.is_empty()) {
        params.push(format!("session_uuid={}", urlencoding_minimal(su)));
    }
    // #659 P5a — carry the active persona so the backend best-effort
    // persists it to `state.copilot.mode` (the post-call record then knows
    // which persona ran). Backend normalises anything but "support" to
    // "sales"; does NOT change what's fetched (both Deals + Cases always
    // return).
    if let Some(m) = mode.map(str::trim).filter(|s| !s.is_empty()) {
        params.push(format!("mode={}", urlencoding_minimal(m)));
    }
    let path = if params.is_empty() {
        "/v1/live/crm-context".to_string()
    } else {
        format!("/v1/live/crm-context?{}", params.join("&"))
    };
    get_json_typed(backend, &path).await
}

/// POST /v1/live/ask — #660 co-pilot ask-chip. `chip` is one of
/// `catch_me_up | summarize | what_did_they_ask | action_items`; the
/// backend generates a plain-text answer over the live-transcript window
/// (org's own summarization key) and returns `{ answer, based_on_turns }`.
/// The endpoint degrades **calm-200** — empty transcript / no key /
/// generation failure all resolve to a plain-text answer line, never an
/// error status — so a successful call always carries a renderable
/// `answer`. Copilot-gated (404 when off) + impersonation-write-gated;
/// those + a genuine transport hiccup surface as a structured
/// `PortalError` the lane renders as its "not available right now" calm
/// degrade. Mirrors `live_crm_context`'s `post_json_typed` transport.
pub async fn live_ask(
    backend: &Backend,
    session_uuid: &str,
    chip: &str,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
        backend,
        "/v1/live/ask",
        serde_json::json!({
            "session_uuid": session_uuid,
            "chip": chip,
        }),
    )
    .await
}

/// POST /v1/live/knowledge — #659 P5b Support-mode cited knowledge answer.
/// `query` is the optional manual question; when omitted the backend derives it
/// from the counterpart's most recent transcript turn. Returns
/// `{ answer, sources }` — grounding-first, so no snippet match yields a calm
/// no-match line with empty sources rather than a hallucination. The endpoint
/// degrades calm-200 (no-match / no-key / generation failure all resolve to a
/// renderable answer line); copilot-gated (404 when off) +
/// impersonation-write-gated, which + a transport hiccup surface as a
/// structured `PortalError` the lane renders as its calm degrade. Mirrors
/// `live_ask`'s `post_json_typed` transport.
pub async fn live_knowledge(
    backend: &Backend,
    session_uuid: &str,
    query: Option<&str>,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
        backend,
        "/v1/live/knowledge",
        serde_json::json!({
            "session_uuid": session_uuid,
            "query": query,
        }),
    )
    .await
}

/// POST /v1/live/highlight — #660 one-click star toggle. Marks (or
/// un-marks, `starred:false`) a live transcript turn keyed by its natural
/// wire key `channel + start_ms`; the backend stores it on
/// `state.copilot.highlights` and the post-call summary weights the
/// flagged text. Returns `{ starred, count }`. Copilot-gated +
/// impersonation-write-gated. `speaker` is optional (the turn's resolved
/// label when known). Mirrors `live_crm_context`'s `post_json_typed`
/// transport.
#[allow(clippy::too_many_arguments)]
pub async fn live_highlight(
    backend: &Backend,
    session_uuid: &str,
    channel: &str,
    start_ms: i64,
    end_ms: i64,
    speaker: Option<&str>,
    text: &str,
    starred: bool,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(
        backend,
        "/v1/live/highlight",
        serde_json::json!({
            "session_uuid": session_uuid,
            "channel": channel,
            "start_ms": start_ms,
            "end_ms": end_ms,
            "speaker": speaker,
            "text": text,
            "starred": starred,
        }),
    )
    .await
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
    // v0.17.1 — bumped 240 → 600 to match the surrounding transcribe /
    // resummarize ceilings and the backend's `length_scaled_timeout`,
    // which can run to ~600 s per LLM leg (plus a retry slot, plus a
    // parallel `extract_action_items` leg of the same shape). The
    // previous 240 s asymmetry was introduced when #614 widened the
    // backend cap without updating the agent ceiling, so long calls
    // (~15 min+ transcripts) reliably timed out client-side while the
    // backend was still legitimately producing a summary. The durable
    // fire-and-poll refactor is tracked separately; this is the
    // hotfix.
    let (value, headers) = post_with_timeout(
        backend,
        &format!("/v1/calls/{call_id}/summarize"),
        serde_json::json!({
            "transcript": transcript,
            "candidate_clients": candidate_clients,
        }),
        Duration::from_secs(600),
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

// ── Screen-capture consent ack (#302 Slice B) ────────────────────────
//
// Screen capture is a DISTINCT, heavier consent than the audio recording
// ack (it records the whole screen as continuous video). The backend
// keeps a separate `screen_capture_acknowledgments` row; the Settings
// toggle (Slice C) posts the ack before it lets the user enable capture,
// and the upload path rejects with `screen_capture_consent_required`
// (400) until a row exists. Shapes mirror the audio recording-ack pair
// above verbatim.

/// `GET /v1/me/screen-capture-ack` → Some(accepted_at) when the user has
/// acknowledged, None on 404. Any other status is a structured error so a
/// network blip isn't mistaken for an un-acked user.
pub async fn get_screen_capture_ack(
    backend: &Backend,
) -> std::result::Result<Option<Value>, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/me/screen-capture-ack",
        backend.url.trim_end_matches('/')
    );
    let resp = c.get(&url).header("authorization", auth).send().await?;
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

/// `POST /v1/me/screen-capture-ack` — record the screen-capture consent
/// for the current user. Body carries the running agent version +
/// platform (the agent knows both cheaply). 204 on success.
pub async fn post_screen_capture_ack(
    backend: &Backend,
    agent_version: &str,
    platform: &str,
) -> std::result::Result<(), PortalError> {
    post_nop_typed(
        backend,
        "/v1/me/screen-capture-ack",
        serde_json::json!({
            "agent_version": agent_version,
            "platform": platform,
        }),
    )
    .await
}

/// `GET /v1/calls/{id}/screen` → screen-recording metadata (Slice A).
/// `Some(json)` when a row exists, `None` on 404 (org flag off OR no
/// recording — the call-detail player renders nothing). The JSON passes
/// straight through to the frontend, which types it as
/// `@aftercalls/shared/types → ScreenRecording`. When `status='ready'`
/// the payload carries a short-lived presigned `url` the `<video>` binds
/// directly (Spaces serves Range natively — no proxy needed). Mirrors the
/// `get_screen_capture_ack` GET shape verbatim.
pub async fn get_screen_recording(
    backend: &Backend,
    call_id: &str,
) -> std::result::Result<Option<Value>, PortalError> {
    // #302 review (security low #1): defense-in-depth — reject a non-UUID
    // `call_id` before it reaches the URL. `reqwest`/`url` normalize
    // dot-segments at parse time, so a crafted `../`-bearing id could
    // otherwise redirect this Bearer-authenticated request to a different
    // path on the same backend host. Only first-party webview JS invokes
    // this command, but the guard is free.
    if uuid::Uuid::parse_str(call_id).is_err() {
        return Err(PortalError::Other {
            message: "invalid call id".into(),
        });
    }
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!(
        "{}/v1/calls/{}/screen",
        backend.url.trim_end_matches('/'),
        call_id
    );
    let resp = c.get(&url).header("authorization", auth).send().await?;
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
        copilot_default_mode: me.copilot_default_mode,
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

// ── #634 — per-user unread-call state ────────────────────────────────
//
// Three thin wrappers that ride the `_typed` HTTP helpers so failures
// surface to the webview as structured PortalError. The portal mirror
// in `portal/src/lib/api.ts` hits the same endpoints with the same
// payload shapes; keeping them consistent end-to-end means the agent
// and portal /calls pages converge on identical optimistic-update
// behaviour without one surface drifting on cross-org IDOR semantics.

/// `POST /v1/calls/{id}/read` — mark one call read for the caller.
/// Idempotent (server-side ON CONFLICT DO NOTHING); cross-org or
/// missing id surfaces as `PortalError::NotFound` (per learning #82,
/// 404 not 403, so unauthorised callers can't enumerate ids).
pub async fn mark_call_read(
    backend: &Backend,
    id: &str,
) -> std::result::Result<(), PortalError> {
    let path = format!("/v1/calls/{}/read", urlencoding_minimal(id));
    post_nop_typed(backend, &path, serde_json::json!({})).await
}

/// `POST /v1/calls/read-bulk` — bulk mark-as-read with the discriminated
/// body the backend expects: `{ "all": true }` to flip every unread
/// complete call in the caller's org, or `{ "call_ids": [<uuid>, ...] }`
/// to flip a specific set. Returns `{ marked: <count> }`. Mixed bodies
/// (both keys set) reject 400 server-side.
pub async fn mark_calls_read_bulk(
    backend: &Backend,
    body: Value,
) -> std::result::Result<Value, PortalError> {
    post_json_typed(backend, "/v1/calls/read-bulk", body).await
}

/// Fetch the caller's live unread-call count by hitting `/v1/auth/me`
/// and pulling out the new `unread_calls` field. We deliberately do
/// NOT merge the response into `auth.json` — the cached profile is
/// long-lived (refreshes only on TOS-accept / login / explicit
/// `refresh_me`); piping a 60s poll through `read_auth_file` /
/// `write_auth_file` would churn the file on every tick. Returning
/// the bare i64 keeps the surface tight and matches the portal's
/// `me.unread_calls` direct-field read.
pub async fn me_unread_count(backend: &Backend) -> std::result::Result<i64, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}/v1/auth/me", backend.url.trim_end_matches('/'));
    let resp = c.get(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let retry = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(from_status(status, body, retry));
    }
    let body: Value = resp.json().await.map_err(PortalError::from)?;
    let count = body
        .get("unread_calls")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Ok(count)
}

/// #602/WS-G — fetch the caller's live subscription snapshot by hitting
/// `/v1/auth/me` and pulling out the `subscription` block. Same
/// reach-around discipline as `me_unread_count`: we deliberately do NOT
/// merge into `auth.json` — the Settings card wants a fresh trial
/// countdown / seat readout on each open, and the cached profile stays
/// long-lived. A backend that predates the field (or omits it) decodes
/// to the serde `Default` snapshot (empty status), which the frontend
/// treats as "unknown" and renders nothing rather than a misleading
/// state.
pub async fn me_subscription(backend: &Backend) -> std::result::Result<Subscription, PortalError> {
    let auth = build_auth_header(backend).await?;
    let c = client().map_err(PortalError::from)?;
    let url = format!("{}/v1/auth/me", backend.url.trim_end_matches('/'));
    let resp = c.get(&url).header("authorization", auth).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let retry = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(from_status(status, body, retry));
    }
    let body: Value = resp.json().await.map_err(PortalError::from)?;
    let sub = body
        .get("subscription")
        .cloned()
        .and_then(|v| serde_json::from_value::<Subscription>(v).ok())
        .unwrap_or_default();
    Ok(sub)
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

// ── Failure classification (#645 Phase 1) ─────────────────────────────
//
// Substrate for the auto-recovery work tracked in #645 / #646. Phase 1
// (this commit) only stamps a `failure_class` onto `pipeline::failed`
// telemetry so the staff agent-logs dashboard can filter by class and
// we can measure how often transient blips are killing pipelines.
// Phase 2 (#646) layers a `retry_http<T>` wrapper on top of these same
// variants; that wrapper is intentionally NOT added here.
//
// The classifier inspects an `anyhow::Error` two ways: (1) downcast to
// `reqwest::Error` / `serde_json::Error` to read structured fields
// (`is_connect`, `status`, etc.), then (2) fall back to a substring
// scan of the formatted error chain for the cases where the pipeline
// HTTP helpers (`post_with_timeout` in this file, `put_file` in
// `upload.rs`) have already converted a non-2xx response into an
// `anyhow::bail!("backend {s}: {t}")` / `anyhow::bail!("PUT returned
// {status}: {text}")` string. Best-effort match is acceptable for a
// telemetry tag — this isn't a security boundary.

/// Coarse classification of a pipeline HTTP failure, suitable for
/// stamping into `pipeline::failed` telemetry meta and (Phase 2) for
/// driving retry-vs-bubble decisions in `retry_http`. Serialized
/// snake_case to keep the wire format stable for dashboard filters
/// (`transient_network`, `backend_5xx`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Connect timeouts, DNS failures, broken pipes, EOFs, "connection
    /// reset/refused", "network is unreachable", `os error 110`.
    /// Retryable in Phase 2.
    TransientNetwork,
    /// HTTP 500-599 from the backend. Retryable in Phase 2.
    ///
    /// Wire format is the plan-mandated `backend_5xx` token (the staff
    /// dashboard filter the architect specced uses that exact string),
    /// not whatever serde's snake_case derives from `BackendFiveXx`.
    #[serde(rename = "backend_5xx")]
    BackendFiveXx,
    /// HTTP 401 from the backend — access token expired or rotated.
    /// Phase 2 retries this once after a `do_refresh` call.
    AuthExpired,
    /// `serde_json::Error` decoding a backend response body. Non-
    /// retryable — schema skew won't fix itself on the next attempt.
    DecodeError,
    /// S3 `SignatureDoesNotMatch` (Spaces PUT path) — credential or
    /// clock-skew issue. Non-retryable.
    SignatureMismatch,
    /// Anything we couldn't pin down. Conservatively non-retryable so
    /// unknown failure modes don't accidentally cause loops.
    Other,
}

/// Classify a pipeline error into a `FailureClass`. Best-effort: the
/// classifier looks at the structured `reqwest::Error` / `serde_json::
/// Error` first, then falls back to substring matching on the
/// formatted error chain for cases where the HTTP helpers have already
/// stringified a non-2xx response. Always returns a class — `Other` on
/// unknown.
pub fn classify_reqwest_error(err: &anyhow::Error) -> FailureClass {
    // Structured `reqwest::Error` — the helpers that propagate the raw
    // reqwest error (e.g. `c.post(...).send().await.with_context(...)`)
    // give us `is_connect`/`is_timeout`/`status()` access.
    if let Some(re) = err.downcast_ref::<reqwest::Error>() {
        if let Some(class) = classify_reqwest_status(re) {
            return class;
        }
        if re.is_connect() || re.is_timeout() || re.is_request() {
            return FailureClass::TransientNetwork;
        }
    }

    // Structured serde_json decode failure — `post_with_timeout` /
    // `transcribe` / `summarize` propagate these via `.context("decode")`,
    // which downcasts back to the inner error.
    if err.downcast_ref::<serde_json::Error>().is_some() {
        return FailureClass::DecodeError;
    }
    // Also check the cause chain — `.context("decode")` puts the
    // serde_json::Error one level down.
    for cause in err.chain() {
        if cause.downcast_ref::<serde_json::Error>().is_some() {
            return FailureClass::DecodeError;
        }
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(class) = classify_reqwest_status(re) {
                return class;
            }
            if re.is_connect() || re.is_timeout() || re.is_request() {
                return FailureClass::TransientNetwork;
            }
        }
    }

    // Fallback: substring scan of the full error chain. The pipeline
    // HTTP helpers stringify non-2xx into `anyhow::bail!("backend
    // {status}: {body}")` (portal.rs `post_with_timeout`) and
    // `anyhow::bail!("PUT returned {status}: {text}")` (upload.rs
    // `put_file`), so the structured `reqwest::Error::status()` path
    // above misses those cases — they're plain `anyhow::Error` strings
    // by the time the pipeline sees them. Match in priority order
    // (most-specific first).
    let msg = format!("{err:#}").to_ascii_lowercase();

    // S3 SignatureDoesNotMatch — Spaces PUT path. The XML body S3
    // returns is preserved by `put_file`'s `bail!`. Treat 403 with
    // SignatureDoesNotMatch as a distinct class; bare 403 without
    // that substring stays Other (could be a Spaces ACL drift, not
    // necessarily a creds issue).
    if msg.contains("signaturedoesnotmatch")
        || (msg.contains("put returned 403") && msg.contains("signature"))
    {
        return FailureClass::SignatureMismatch;
    }

    // HTTP-status substring fallback. Both `post_with_timeout`'s
    // `backend {status}` and `put_file`'s `PUT returned {status}` land
    // here. `StatusCode`'s Display is "401 Unauthorized" / "503
    // Service Unavailable"; the leading 3-digit number is what we
    // anchor on.
    if msg.contains("backend 401")
        || msg.contains("put returned 401")
        || msg.contains(" 401 unauthorized")
    {
        return FailureClass::AuthExpired;
    }
    if msg.contains("backend 5") || msg.contains("put returned 5") {
        // Catches "backend 500", "backend 502", "backend 503",
        // "backend 504", "PUT returned 500", etc. The leading "backend
        // 5" prefix is specific enough not to collide with arbitrary
        // body text — the helpers always print status before body.
        for code in &["500", "501", "502", "503", "504", "505", "507", "508", "511"] {
            if msg.contains(&format!("backend {code}"))
                || msg.contains(&format!("put returned {code}"))
            {
                return FailureClass::BackendFiveXx;
            }
        }
    }

    // Network-shape substrings. `os error 110` is Linux ETIMEDOUT,
    // `os error 111` is ECONNREFUSED, `os error 113` is EHOSTUNREACH.
    // Cover the common Display formats reqwest/hyper/std::io render.
    let transient_markers = [
        "os error 110",
        "os error 111",
        "os error 113",
        "connection reset",
        "connection refused",
        "connection closed",
        "network is unreachable",
        "broken pipe",
        "unexpected end of file",
        "unexpected eof",
        "dns error",
        "failed to lookup",
        "name or service not known",
        "operation timed out",
        "timed out",
    ];
    if transient_markers.iter().any(|m| msg.contains(m)) {
        return FailureClass::TransientNetwork;
    }

    FailureClass::Other
}

/// Helper: map a `reqwest::Error` carrying an HTTP status into a class.
/// Returns `None` when the error has no associated status (i.e. the
/// failure happened before a response — connect/dns/timeout etc.; the
/// caller falls through to the transient-network check).
fn classify_reqwest_status(re: &reqwest::Error) -> Option<FailureClass> {
    let status = re.status()?;
    let code = status.as_u16();
    if code == 401 {
        return Some(FailureClass::AuthExpired);
    }
    if (500..600).contains(&code) {
        return Some(FailureClass::BackendFiveXx);
    }
    // 403 without a body to inspect — we don't have access to the
    // response here (the error already consumed it). Surface as
    // `Other`; the substring-fallback path catches SignatureDoesNotMatch
    // for the put_file flow.
    None
}

// ── Retry helper (#646 Phase 2 Layer A) ──────────────────────────────
//
// Generic in-process retry wrapper used by every HTTP step in the
// pipeline (`create_call`, the three Spaces PUTs, `transcribe`,
// `summarize`, `attach_note_path`, `generate_peaks`). 4 attempts total,
// 2 s / 8 s / 30 s backoff with ±20% jitter, retry only on
// `TransientNetwork | BackendFiveXx | AuthExpired` (and the latter only
// after a single in-process refresh per pipeline run). The reqwest
// per-request timeout (600 s on `transcribe` / `summarize`, 30 s on the
// rest) stays unchanged — every retry attempt gets its own fresh
// timeout budget.
//
// The "refresh once per pipeline run" guard lives on a per-call
// `RetryGuard` struct that the pipeline constructs and threads through.
// We deliberately do not modify the existing `do_refresh` flow; the
// guard wraps it in a `force_refresh_auth` helper that reads auth.json,
// calls `do_refresh`, and persists the new tokens.

/// Per-pipeline-run state for `retry_http`. Today carries the auth-
/// refresh latch — flipped once a 401 has triggered a refresh inside
/// this pipeline run so subsequent steps don't loop on a broken token.
/// Pass the same `&RetryGuard` to every `retry_http` call within one
/// pipeline run; create a fresh one for the next.
#[derive(Default)]
pub struct RetryGuard {
    auth_refreshed: Mutex<bool>,
}

impl RetryGuard {
    pub fn new() -> Self {
        Self {
            auth_refreshed: Mutex::new(false),
        }
    }

    /// Returns `true` exactly once per `RetryGuard`. The caller treats
    /// this as "yes, do the refresh now"; subsequent 401s in the same
    /// pipeline run bubble immediately so we don't loop on stale creds.
    fn claim_refresh_slot(&self) -> bool {
        let mut guard = match self.auth_refreshed.lock() {
            Ok(g) => g,
            // Poisoned lock means a previous task panicked while
            // holding it — recover the inner value rather than
            // propagating poison through the pipeline.
            Err(p) => p.into_inner(),
        };
        if *guard {
            false
        } else {
            *guard = true;
            true
        }
    }
}

/// Force a token refresh outside of the lazy `build_auth_header` flow.
/// Used by `retry_http` when a step returned 401 — the cached
/// `auth.json` might be syntactically not-yet-expired (so
/// `build_auth_header` won't refresh on its own) but semantically
/// revoked server-side (rotated by a parallel sign-in / explicit
/// revoke). Reads the current `auth.json`, posts to `/v1/auth/refresh`
/// via the existing `do_refresh` helper, persists the new bundle, and
/// returns `Ok(())`. Errors bubble — the caller drops the retry.
pub(crate) async fn force_refresh_auth(backend: &Backend) -> Result<()> {
    let auth = read_auth_file()?
        .ok_or_else(|| anyhow!("not logged in — cannot refresh auth"))?;
    if auth.refresh_expires_at <= Utc::now() {
        return Err(anyhow!("refresh token expired — please log in again"));
    }
    let refreshed = do_refresh(backend, &auth.refresh_token).await?;
    let merged = merge_auth(refreshed);
    write_auth_file(&merged)?;
    Ok(())
}

/// Backoff slot in milliseconds for attempts 2/3/4 with ±20% jitter
/// applied per the spec. Attempt 1 fires immediately; this returns the
/// wait BEFORE attempts 2, 3, and 4. `attempt_just_failed` is 1-indexed
/// — pass 1 after the first failure, etc. Returns None when no further
/// retry is permitted (attempt 4 has already failed).
fn backoff_wait_ms(attempt_just_failed: u8) -> Option<u64> {
    // Plan §Layer A: 2 s / 8 s / 30 s.
    let base = match attempt_just_failed {
        1 => 2_000u64,
        2 => 8_000u64,
        3 => 30_000u64,
        _ => return None,
    };
    let jitter: f64 = rand::thread_rng().gen_range(0.8..1.2);
    Some((base as f64 * jitter) as u64)
}

/// Run `attempt_fn` up to `max_attempts` times. The closure receives
/// the 1-based attempt index. Classify each `Err` via
/// `classify_reqwest_error`:
/// - `DecodeError | SignatureMismatch | Other` → bubble immediately.
/// - `AuthExpired` → refresh once per `RetryGuard` then retry the next
///   slot; if the refresh already fired or fails, bubble.
/// - `TransientNetwork | BackendFiveXx` → wait the next backoff slot
///   and retry.
///
/// Emits `pipeline::retry` at debug level on every retry with
/// `{ step, attempt, failure_class, wait_ms }`. Stamps the step name
/// onto the final bubbled error so the pipeline failure log records
/// which HTTP call ran out of attempts.
///
/// `session_id` is threaded through purely for telemetry attribution —
/// callers pass the same string they use on `pipeline::start` /
/// `pipeline::failed` so the retry events sit in the same agent_logs
/// row group.
pub(crate) async fn retry_http<T, F, Fut>(
    backend: &Backend,
    guard: &RetryGuard,
    step: &'static str,
    max_attempts: u8,
    session_id: Option<&str>,
    mut attempt_fn: F,
) -> Result<T>
where
    F: FnMut(u8) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let max_attempts = max_attempts.max(1);
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        match attempt_fn(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let class = classify_reqwest_error(&e);
                // Non-retryable classes bubble immediately, no wait.
                match class {
                    FailureClass::DecodeError
                    | FailureClass::SignatureMismatch
                    | FailureClass::Other => {
                        return Err(e.context(format!("{step} failed")));
                    }
                    FailureClass::AuthExpired => {
                        if !guard.claim_refresh_slot() {
                            // Already refreshed once in this pipeline
                            // run — a second 401 means the refresh
                            // didn't help. Bubble.
                            return Err(e.context(format!(
                                "{step} failed after auth refresh"
                            )));
                        }
                        // Attempt the refresh inline; on failure bubble
                        // the ORIGINAL 401 (more useful for staff
                        // triage than a refresh-side bubble).
                        if let Err(re) = force_refresh_auth(backend).await {
                            eprintln!(
                                "aftercalls: retry_http {step} refresh failed: {re:#}"
                            );
                            return Err(e.context(format!(
                                "{step} failed and auth refresh failed"
                            )));
                        }
                        // Fall through into the backoff-wait path so
                        // the next attempt has a small breathing room
                        // (mirrors transient retries).
                    }
                    FailureClass::TransientNetwork | FailureClass::BackendFiveXx => {
                        // Fall through into the backoff-wait path.
                    }
                }

                // We've decided to retry. Compute the wait and emit a
                // telemetry breadcrumb. If we've already exhausted
                // attempts, bubble.
                let wait_ms = backoff_wait_ms(attempt);
                let Some(wait_ms) = wait_ms else {
                    return Err(e.context(format!("{step} failed after {attempt} attempts")));
                };
                if attempt >= max_attempts {
                    return Err(e.context(format!("{step} failed after {attempt} attempts")));
                }
                crate::telemetry::log(
                    "debug",
                    "pipeline::retry",
                    format!("{step} attempt {attempt} failed; retrying"),
                    Some(serde_json::json!({
                        "step": step,
                        "attempt": attempt,
                        "failure_class": class,
                        "wait_ms": wait_ms,
                    })),
                    session_id.map(|s| s.to_string()),
                );
                // Mirror the breadcrumb onto the Tauri event bus so
                // the topstrip can flip its label to "Retrying…" for
                // the duration of the wait. `pipeline` is the same
                // event name `PipelineEvent::*` rides; the `Retrying`
                // variant's serialized shape sits next to the other
                // tagged variants ({ stage: "retrying", step,
                // attempt, wait_ms }).
                crate::telemetry::emit_app_event(
                    "pipeline",
                    &serde_json::json!({
                        "stage": "retrying",
                        "step": step,
                        "attempt": attempt,
                        "wait_ms": wait_ms,
                    }),
                );
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }
    }
    // Shouldn't be reachable — the inner branches return on success or
    // bubble after the final attempt — but keep a defensive fallback.
    Err(last_err.unwrap_or_else(|| anyhow!("{step} failed after {max_attempts} attempts")))
}

#[cfg(test)]
mod failure_class_tests {
    use super::*;

    // Constructing genuine `reqwest::Error` instances outside the
    // crate is awkward (no public constructor), but the inline-doc
    // pattern `client.get(<invalid>).send().await.unwrap_err()` works:
    // we exercise the real `is_connect` / `is_timeout` paths the
    // pipeline will hit in production. `tokio::runtime::Runtime` keeps
    // the tests synchronous from cargo's POV.

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    #[test]
    fn is_connect_error_classifies_as_transient_network() {
        // Port 1 on localhost is essentially guaranteed-refused —
        // generates a real `reqwest::Error` with `is_connect()` true.
        let err = rt().block_on(async {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap()
                .get("http://127.0.0.1:1/never")
                .send()
                .await
                .expect_err("port 1 connect must fail")
        });
        // Sanity: the structured reqwest::Error reports connect/request.
        assert!(
            err.is_connect() || err.is_request() || err.is_timeout(),
            "expected connect/request/timeout-shaped reqwest error, got {err:?}"
        );
        let anyerr: anyhow::Error = anyhow::Error::new(err);
        assert_eq!(
            classify_reqwest_error(&anyerr),
            FailureClass::TransientNetwork
        );
    }

    #[test]
    fn dns_failure_classifies_as_transient_network_via_substring() {
        // DNS lookup against a non-routable .invalid TLD — surfaces as a
        // wrapped `with_context` anyhow chain in production
        // (`post_with_timeout` does `.with_context(|| format!("POST
        // {url}"))?;`), so this also covers the chain-traversal path.
        let err = rt().block_on(async {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap()
                .get("http://this-host-does-not-exist-aftercalls-test.invalid/")
                .send()
                .await
                .expect_err("DNS lookup must fail")
        });
        let anyerr = anyhow::Error::new(err).context("POST /v1/calls/x/transcribe");
        // Either the structured `is_connect`/`is_request` path or the
        // substring fallback should land us on transient_network.
        assert_eq!(
            classify_reqwest_error(&anyerr),
            FailureClass::TransientNetwork
        );
    }

    #[test]
    fn backend_5xx_substring_classifies_as_backend_fivexx() {
        // Mirror what `post_with_timeout` produces on a 502/503/504.
        for code in &[500u16, 502, 503, 504] {
            let err = anyhow::anyhow!("backend {code} Service Unavailable: upstream down");
            assert_eq!(
                classify_reqwest_error(&err),
                FailureClass::BackendFiveXx,
                "code {code} should be BackendFiveXx",
            );
        }
        // Same shape for the upload.rs PUT helper.
        let put_err = anyhow::anyhow!("PUT returned 503 Service Unavailable: x");
        assert_eq!(
            classify_reqwest_error(&put_err),
            FailureClass::BackendFiveXx,
        );
    }

    #[test]
    fn http_401_substring_classifies_as_auth_expired() {
        let err = anyhow::anyhow!("backend 401 Unauthorized: token expired");
        assert_eq!(classify_reqwest_error(&err), FailureClass::AuthExpired);
    }

    #[test]
    fn serde_decode_error_classifies_as_decode_error() {
        let serde_err = serde_json::from_str::<Value>("not json at all").unwrap_err();
        let anyerr: anyhow::Error = anyhow::Error::new(serde_err);
        assert_eq!(classify_reqwest_error(&anyerr), FailureClass::DecodeError);

        // Also verify the wrapped-in-context path that
        // `post_with_timeout` uses (`.context("decode")`).
        let serde_err2 = serde_json::from_str::<Value>("{nope}").unwrap_err();
        let wrapped: anyhow::Error =
            anyhow::Error::new(serde_err2).context("decode");
        assert_eq!(classify_reqwest_error(&wrapped), FailureClass::DecodeError);
    }

    #[test]
    fn s3_signature_mismatch_classifies_as_signature_mismatch() {
        let err = anyhow::anyhow!(
            "PUT returned 403 Forbidden: <Error><Code>SignatureDoesNotMatch</Code></Error>"
        );
        assert_eq!(
            classify_reqwest_error(&err),
            FailureClass::SignatureMismatch
        );
    }

    #[test]
    fn unknown_error_classifies_as_other() {
        let err = anyhow::anyhow!("something weird but http 200 in body somehow");
        assert_eq!(classify_reqwest_error(&err), FailureClass::Other);
    }

    #[test]
    fn enum_serializes_as_snake_case() {
        // Locked-in wire format for the staff dashboard filter.
        assert_eq!(
            serde_json::to_value(FailureClass::TransientNetwork).unwrap(),
            serde_json::json!("transient_network")
        );
        assert_eq!(
            serde_json::to_value(FailureClass::BackendFiveXx).unwrap(),
            serde_json::json!("backend_5xx")
        );
        assert_eq!(
            serde_json::to_value(FailureClass::AuthExpired).unwrap(),
            serde_json::json!("auth_expired")
        );
        assert_eq!(
            serde_json::to_value(FailureClass::DecodeError).unwrap(),
            serde_json::json!("decode_error")
        );
        assert_eq!(
            serde_json::to_value(FailureClass::SignatureMismatch).unwrap(),
            serde_json::json!("signature_mismatch")
        );
        assert_eq!(
            serde_json::to_value(FailureClass::Other).unwrap(),
            serde_json::json!("other")
        );
    }
}

#[cfg(test)]
mod retry_http_tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    /// Build a `Backend` that any HTTP call will bail on instantly (we
    /// never invoke a real refresh in the retry-only tests below).
    fn dummy_backend() -> Backend {
        Backend {
            url: "http://127.0.0.1:1".to_string(),
            token: None,
        }
    }

    #[test]
    fn retry_http_succeeds_on_attempt_3_after_two_transient_errors() {
        // Plan acceptance check: 2 injected `TransientNetwork` errors
        // followed by a success on attempt 3. The closure tracks how
        // many times it ran via an AtomicU8.
        let calls = Arc::new(AtomicU8::new(0));
        let calls_inner = calls.clone();
        let started = std::time::Instant::now();
        let result: Result<&'static str> = rt().block_on(async move {
            let guard = RetryGuard::new();
            let backend = dummy_backend();
            retry_http(&backend, &guard, "test_step", 4, None, |attempt| {
                let calls = calls_inner.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if attempt < 3 {
                        // Synthesize a TransientNetwork via the
                        // `os error 110` substring path the classifier
                        // recognises (no real reqwest::Error needed).
                        Err(anyhow!("connect: os error 110 (connection timed out)"))
                    } else {
                        Ok("got-it")
                    }
                }
            })
            .await
        });
        let elapsed = started.elapsed();
        assert!(matches!(result, Ok("got-it")));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // 2 backoff slots fired: ~2s + ~8s with ±20% jitter. Floor
        // bounds at 0.8x each = 1.6 + 6.4 = 8 s. Ceiling at 1.2x each
        // = 2.4 + 9.6 = 12 s. Allow slop on both ends.
        assert!(
            elapsed >= Duration::from_millis(7_500),
            "expected >=7.5s of backoff, got {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_secs(14),
            "expected <=14s of backoff, got {elapsed:?}"
        );
    }

    #[test]
    fn retry_http_bubbles_immediately_on_decode_error() {
        // Plan acceptance check: a DecodeError must NOT retry — the
        // attempt-fn closure runs exactly once. We exercise the
        // structured `serde_json::Error` path via `serde_json::from_str`.
        let calls = Arc::new(AtomicU8::new(0));
        let calls_inner = calls.clone();
        let result: Result<()> = rt().block_on(async move {
            let guard = RetryGuard::new();
            let backend = dummy_backend();
            retry_http(&backend, &guard, "test_decode", 4, None, |_attempt| {
                let calls = calls_inner.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let parse_err =
                        serde_json::from_str::<Value>("not json at all").unwrap_err();
                    Err(anyhow::Error::new(parse_err).context("decode"))
                }
            })
            .await
        });
        assert!(result.is_err(), "expected decode error to bubble");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "decode errors must not retry",
        );
    }
}
