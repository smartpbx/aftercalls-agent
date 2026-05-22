//! Backend interaction for creating the call row, uploading audio, and
//! attaching the local vault-note path after processing lands. The
//! transcription + summarization work is now a separate backend
//! pipeline (see portal::transcribe / portal::summarize).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{read_auth_file, AuthFile, Backend};
use crate::portal::{build_auth_header, retry_http, user_agent, FailureClass, RetryGuard};

#[derive(Deserialize, Debug, Default)]
pub struct UploadUrls {
    pub mic: Option<String>,
    pub system: Option<String>,
    pub mixed: Option<String>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct CreateCallResponse {
    pub call_id: String,
    pub upload_urls: UploadUrls,
}

/// Create (or upsert) the call row on the backend with the minimal
/// metadata we know pre-pipeline: session_id, recorded_at, duration
/// estimate (zero is fine — transcribe backfills the real duration),
/// the source descriptor, and an empty utterances array. The response
/// carries the presigned PUT URLs the agent needs for the audio.
///
/// #646 Layer A — wrapped in `retry_http` so a transient network blip
/// between agent and backend doesn't lose the row. create_call is
/// idempotent server-side (UPSERT on session_id with status-preserving
/// conflict handling, see backend `pipeline.rs::create_call`), so
/// retrying is safe.
pub async fn create_call(
    backend: &Backend,
    session_dir: &Path,
    duration_ms_hint: u64,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
) -> Result<CreateCallResponse> {
    let session_id = session_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let recorded_at = parse_session_timestamp(&session_id);
    let source = read_source_json(session_dir);

    let body = CreateCall {
        session_id: session_id.clone(),
        recorded_at,
        duration_ms: duration_ms_hint as i64,
        // #531 — use user-provided title from title.txt when present.
        title: crate::notes::read_title_from_dir(session_dir),
        matched_client: None,
        summary_text: None,
        action_items: Vec::new(),
        participants: Vec::new(),
        note_markdown_path: None,
        source_kind: source.kind,
        source_app: source.app,
        utterances: Vec::new(),
        notes: crate::notes::read_from_dir(session_dir),
    };

    let url = format!("{}/v1/calls", backend.url.trim_end_matches('/'));
    let body_value = serde_json::to_value(&body).context("serialize create-call body")?;
    retry_http(
        backend,
        guard,
        "create_call",
        4,
        session_id_for_telemetry,
        |_attempt| {
            let url = url.clone();
            let body_value = body_value.clone();
            async move {
                let auth = build_auth_header(backend).await?;
                let client = http_client()?;
                let resp = client
                    .post(&url)
                    .header("authorization", auth)
                    .json(&body_value)
                    .send()
                    .await
                    .with_context(|| format!("POST {url}"))?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("backend {status}: {text}");
                }
                resp.json::<CreateCallResponse>()
                    .await
                    .context("decode create-call response")
            }
        },
    )
    .await
}

/// After transcribe + summarize land server-side the row already has
/// the transcript + title + summary persisted. This call just attaches
/// the local vault note path so the portal can link out to it.
///
/// History: previously POSTed back to /v1/calls with a sparse body,
/// trusting a comment that claimed ON CONFLICT DO UPDATE preserved
/// absent fields. It doesn't — the backend overwrites every column
/// from EXCLUDED and re-DELETEs utterances. The narrow /note-path
/// endpoint on the backend only touches the column named.
///
/// #646 Layer A — wrapped in `retry_http`. The note-path endpoint is
/// a narrow PATCH-shape POST that's naturally idempotent (replaces a
/// single column with the supplied value), so retry is safe.
pub async fn attach_note_path(
    backend: &Backend,
    call_id: &str,
    note_path: &Path,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
) -> Result<()> {
    let body = serde_json::json!({
        "note_markdown_path": note_path.to_string_lossy(),
    });

    let url = format!(
        "{}/v1/calls/{}/note-path",
        backend.url.trim_end_matches('/'),
        call_id
    );
    retry_http(
        backend,
        guard,
        "attach_note_path",
        4,
        session_id_for_telemetry,
        |_attempt| {
            let url = url.clone();
            let body = body.clone();
            async move {
                let auth = build_auth_header(backend).await?;
                let client = http_client()?;
                let resp = client
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
                Ok(())
            }
        },
    )
    .await
}

/// #646 Layer B — per-track outcome of `upload_audio`. One entry per
/// track the pipeline attempted. `uploaded=true` means Spaces returned
/// 2xx; `failure_class` carries the last classified error for the
/// tail-failed tracks so the pending_uploads sentinel can record it.
/// Skipped tracks (file missing on disk, presigned URL absent in the
/// create_call response) yield `uploaded=false, failure_class=None,
/// final_error=None`.
#[derive(Debug, Clone, Serialize)]
pub struct TrackOutcome {
    /// One of `"mic" | "system" | "mixed"`.
    pub track: &'static str,
    pub uploaded: bool,
    /// `None` on success or skipped; populated only when retry_http
    /// returned an error.
    pub failure_class: Option<FailureClass>,
    /// Human-readable last error string, only present on failure. Kept
    /// vendor-opaque: "object storage" rather than "DigitalOcean Spaces".
    pub final_error: Option<String>,
}

/// #646 Layer B — sentinel file written to `<session_dir>/pending_uploads.json`
/// when at least one track exhausted its retry ladder. Survives
/// agent restarts so the orphan-resume path can re-upload only the
/// missing tracks instead of blindly re-uploading everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUploads {
    /// Subset of `["mic", "system", "mixed"]` that still needs to land.
    pub tracks: Vec<String>,
    /// `failure_class` of the last failed attempt, snake_case wire
    /// format (`transient_network`, `backend_5xx`, etc.). The Layer C
    /// sweeper reads this to decide whether the session is auto-
    /// resumable.
    pub last_failure_class: String,
}

const PENDING_UPLOADS_FILENAME: &str = "pending_uploads.json";

fn pending_uploads_path(session_dir: &Path) -> PathBuf {
    session_dir.join(PENDING_UPLOADS_FILENAME)
}

/// Read `pending_uploads.json` if present. A corrupt / unreadable
/// sentinel returns `None` and the resume path falls back to
/// "re-upload all tracks" — the failure mode is conservative on
/// purpose.
pub fn read_pending_uploads(session_dir: &Path) -> Option<PendingUploads> {
    let path = pending_uploads_path(session_dir);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Delete the sentinel — called after a successful re-upload run when
/// every previously-failed track has landed. Best-effort; a leftover
/// file is cleaned by the 7-day orphan sweep anyway.
pub fn clear_pending_uploads(session_dir: &Path) {
    let path = pending_uploads_path(session_dir);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!(
                "aftercalls: clear pending_uploads {} failed: {e}",
                path.display()
            );
        }
    }
}

fn write_pending_uploads(session_dir: &Path, pending: &PendingUploads) {
    let path = pending_uploads_path(session_dir);
    match serde_json::to_string_pretty(pending) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                eprintln!(
                    "aftercalls: write pending_uploads {} failed: {e}",
                    path.display()
                );
            }
        }
        Err(e) => {
            eprintln!("aftercalls: serialize pending_uploads failed: {e}");
        }
    }
}

/// PUTs the three track files to their presigned URLs. Prefers the
/// `.opus` (compressed by pipeline.rs via ffmpeg) and falls back to
/// the raw `.wav` when ffmpeg isn't available — notably on stock
/// Windows. The consuming services (AssemblyAI, ffmpeg on the
/// backend, the browser's `<audio>`) sniff the container on
/// download, so the bytes drive decoding regardless of the
/// advertised content type.
///
/// Critically, the `content-type` we send on the PUT must match the
/// one the backend baked into the presigned URL's signature — S3
/// SigV4 signs that header. If we send `audio/wav` for a WAV fallback
/// to a URL signed as `audio/ogg`, Spaces returns 403 and the upload
/// silently drops. We use the signed-for content-type for the PUT
/// header and let the consumer sniff the actual bytes.
///
/// #646 Layer B — return value is now `Vec<TrackOutcome>` instead of
/// `Result<()>`. Each PUT routes through `retry_http`; tracks that
/// exhaust the retry ladder yield a `TrackOutcome { uploaded: false,
/// ... }` entry and emit `pipeline::track_upload_failed` telemetry.
/// On any partial failure we write a `pending_uploads.json` sentinel
/// next to the audio so a later orphan-resume can attempt only the
/// still-missing tracks. On full success in a re-run we delete the
/// existing sentinel.
pub async fn upload_audio(
    session_dir: &Path,
    urls: &UploadUrls,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
) -> Result<Vec<TrackOutcome>> {
    let client = http_client()?;

    // If a sentinel from a previous failed run is present, narrow this
    // attempt to ONLY the tracks it lists — tracks that already landed
    // at Spaces don't need re-PUTting. A missing/corrupt sentinel
    // falls through to "attempt all three" (conservative re-upload).
    let pending = read_pending_uploads(session_dir);
    let allowed: Option<Vec<String>> = pending.as_ref().map(|p| p.tracks.clone());

    // Each entry: (track-name, presigned URL, the content-type the
    // backend signed the URL with, preferred-then-fallback source
    // paths). The first existing source file gets uploaded with the
    // signed content-type.
    let candidates: [(&'static str, Option<&str>, &str, [PathBuf; 2]); 3] = [
        (
            "mic",
            urls.mic.as_deref(),
            "audio/ogg",
            [session_dir.join("mic.opus"), session_dir.join("mic.wav")],
        ),
        (
            "system",
            urls.system.as_deref(),
            "audio/ogg",
            [session_dir.join("system.opus"), session_dir.join("system.wav")],
        ),
        (
            "mixed",
            urls.mixed.as_deref(),
            "audio/ogg",
            [session_dir.join("mixed.opus"), session_dir.join("mixed.wav")],
        ),
    ];

    let mut outcomes: Vec<TrackOutcome> = Vec::with_capacity(3);

    for (track, url, content_type, sources) in &candidates {
        // Skip if a previous run's sentinel restricts the set and
        // this track is NOT in the resume list — it already landed
        // and re-PUTting would just rewrite the same object.
        if let Some(allowed_tracks) = &allowed {
            if !allowed_tracks.iter().any(|s| s == track) {
                continue;
            }
        }
        let Some(url) = url else { continue };
        let Some(path) = sources.iter().find(|p| p.exists()) else {
            continue;
        };

        let attempt_path = path.clone();
        let attempt_url = url.to_string();
        let attempt_ct = content_type.to_string();
        let client_ref = &client;
        let result = retry_http(
            // The backend handle is needed for `force_refresh_auth`
            // on a 401. Spaces PUTs don't carry our JWT (they're
            // pre-signed), so a 401 here is structurally impossible —
            // but the helper still wants a backend reference for type
            // signature consistency, and the no-op refresh path is
            // safe.
            &dummy_backend_for_spaces_retry(),
            guard,
            "put_file",
            4,
            session_id_for_telemetry,
            |_attempt| {
                let path = attempt_path.clone();
                let url = attempt_url.clone();
                let content_type = attempt_ct.clone();
                async move { put_file(client_ref, &url, &path, &content_type).await }
            },
        )
        .await;

        match result {
            Ok(()) => {
                outcomes.push(TrackOutcome {
                    track,
                    uploaded: true,
                    failure_class: None,
                    final_error: None,
                });
            }
            Err(e) => {
                let class = crate::portal::classify_reqwest_error(&e);
                let err_str = format!("{e:#}");
                eprintln!(
                    "aftercalls: audio upload failed for {} ({track}): {err_str}",
                    path.display()
                );
                // pipeline::track_upload_failed (warn) per failed
                // track. Vendor-opaque copy: no "DigitalOcean Spaces"
                // in the message. Final_error is the raw chain — that
                // ships in telemetry meta to staff, not to users.
                crate::telemetry::log(
                    "warn",
                    "pipeline::track_upload_failed",
                    format!("track {track} upload failed (object storage)"),
                    Some(serde_json::json!({
                        "track": track,
                        "final_error": err_str,
                        "failure_class": class,
                    })),
                    session_id_for_telemetry.map(|s| s.to_string()),
                );
                outcomes.push(TrackOutcome {
                    track,
                    uploaded: false,
                    failure_class: Some(class),
                    final_error: Some(err_str),
                });
            }
        }
    }

    // Sentinel bookkeeping. If any track failed, persist the list +
    // last failure class so the sweeper can re-target this session.
    // If everything that was attempted landed, clear any stale
    // sentinel from a previous run.
    let failed_tracks: Vec<String> = outcomes
        .iter()
        .filter(|o| !o.uploaded)
        .map(|o| o.track.to_string())
        .collect();
    if failed_tracks.is_empty() {
        clear_pending_uploads(session_dir);
    } else {
        // The last classified failure wins — useful enough for the
        // sweeper's eligibility check; the per-track meta is in
        // pipeline::track_upload_failed for staff triage.
        let last_class = outcomes
            .iter()
            .rev()
            .find_map(|o| o.failure_class)
            .unwrap_or(FailureClass::Other);
        write_pending_uploads(
            session_dir,
            &PendingUploads {
                tracks: failed_tracks,
                last_failure_class: failure_class_wire_token(last_class),
            },
        );
    }

    Ok(outcomes)
}

/// Stand-in `Backend` used only as a refresh anchor for `retry_http`
/// from inside the Spaces PUT loop. The PUTs themselves never hit our
/// backend URL (they go to Spaces via the presigned URL the
/// create_call response delivered), so a 401 is structurally
/// impossible — if it ever appeared it would be a SigV4 issue, which
/// our classifier surfaces as `SignatureMismatch` and bubbles
/// non-retryable. The helper still needs a `&Backend` reference for
/// the type signature, hence this dummy.
fn dummy_backend_for_spaces_retry() -> Backend {
    Backend {
        url: String::new(),
        token: None,
    }
}

/// Serialize a `FailureClass` to the same snake_case wire token the
/// staff dashboard uses. Kept here to avoid pulling `serde_json` into
/// every callsite that needs the string. Mirrors the
/// `#[serde(rename_all = "snake_case")]` on the enum + the explicit
/// `backend_5xx` rename.
fn failure_class_wire_token(class: FailureClass) -> String {
    match class {
        FailureClass::TransientNetwork => "transient_network".into(),
        FailureClass::BackendFiveXx => "backend_5xx".into(),
        FailureClass::AuthExpired => "auth_expired".into(),
        FailureClass::DecodeError => "decode_error".into(),
        FailureClass::SignatureMismatch => "signature_mismatch".into(),
        FailureClass::Other => "other".into(),
    }
}

async fn put_file(
    client: &reqwest::Client,
    url: &str,
    path: &PathBuf,
    content_type: &str,
) -> Result<()> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let resp = client
        .put(url)
        .header("content-type", content_type)
        .body(bytes)
        .send()
        .await
        .with_context(|| format!("PUT {}", path.display()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("PUT returned {status}: {text}");
    }
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
    // #293 — stamp the same `aftercalls/<ver> (<os>)` UA portal::client()
    // uses, so backend tracing on `POST /v1/calls` can attribute the
    // request to a specific agent build instead of logging
    // `agent_ver = "unknown"`. `attach_note_path` reuses this client too
    // and benefits from the same attribution; the S3 PUTs in
    // `upload_audio` ignore custom UAs (Spaces signs `host` + `content-
    // type`, not user-agent), so the change is a no-op there.
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(user_agent())
        .build()?)
}

fn parse_session_timestamp(session_id: &str) -> DateTime<Utc> {
    chrono::NaiveDateTime::parse_from_str(session_id, "%Y%m%dT%H%M%SZ")
        .map(|ndt| ndt.and_utc())
        .unwrap_or_else(|_| Utc::now())
}

#[derive(Serialize)]
struct CreateCall {
    session_id: String,
    recorded_at: DateTime<Utc>,
    duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_text: Option<String>,
    action_items: Vec<String>,
    participants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_markdown_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_app: Option<String>,
    utterances: Vec<serde_json::Value>,
    notes: String,
}

#[derive(Deserialize, Default)]
struct SourceDescriptor {
    kind: Option<String>,
    app: Option<String>,
}

fn read_source_json(session_dir: &Path) -> SourceDescriptor {
    let path = session_dir.join("source.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => SourceDescriptor::default(),
    }
}

// AuthFile needs to be reachable for pipeline.rs's peeks at the current
// user (e.g. to surface org_display_name in UI bits). Re-export here
// so callers don't need to reach into config directly.
#[allow(dead_code)]
pub fn current_auth() -> Option<AuthFile> {
    read_auth_file().ok().flatten()
}
