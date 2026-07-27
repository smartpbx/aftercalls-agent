//! Backend interaction for creating the call row, uploading audio, and
//! attaching the local vault-note path after processing lands. The
//! transcription + summarization work is now a separate backend
//! pipeline (see portal::transcribe / portal::summarize).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{read_auth_file, AuthFile, Backend};
use crate::portal::{build_auth_header, retry_http, user_agent, FailureClass, RetryGuard};
use crate::screen_recorder::{ScreenRecordingMeta, SCREEN_SUBDIR};

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
    let mut body_value = serde_json::to_value(&body).context("serialize create-call body")?;
    // #live — attach the record-start session_uuid the agent persisted into
    // the session_dir (`live_session.json`) when it opened a live relay, so
    // the backend can reconcile the disposable live session to this new call
    // row. Absent for recordings that never opened a live session.
    // Forward-compatible: the backend's CreateCall is a plain serde struct
    // (no deny_unknown_fields), so a build that doesn't yet read the field
    // ignores it.
    if let Some(uuid) = read_live_session_uuid(session_dir) {
        if let serde_json::Value::Object(ref mut map) = body_value {
            map.insert("session_uuid".to_string(), serde_json::Value::String(uuid));
        }
    }
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

/// #live — read the record-start `session_uuid` the agent persisted into the
/// session_dir (`live_session.json`) when a live relay opened. `None` when the
/// file is absent (no live session) or unparseable.
fn read_live_session_uuid(session_dir: &Path) -> Option<String> {
    let path = session_dir.join("live_session.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("session_uuid")?.as_str().map(|s| s.to_string())
}

// AuthFile needs to be reachable for pipeline.rs's peeks at the current
// user (e.g. to surface org_display_name in UI bits). Re-export here
// so callers don't need to reach into config directly.
#[allow(dead_code)]
pub fn current_auth() -> Option<AuthFile> {
    read_auth_file().ok().flatten()
}

// ── Screen recording — resumable multipart upload (#302 Slice B) ──────
//
// After a Call that captured the screen, `screen/recording.json` +
// `screen/recording.mp4` sit in the session dir (written by
// `screen_recorder::ScreenRecorder::stop_and_persist`). This step
// remuxes the mp4 for `<video>` seeking (moov-at-front) and uploads it to
// object storage via the backend's S3-multipart contract:
//
//   init  → POST /v1/calls/{id}/screen/upload/init      { start_offset_ms, dims, fps, codec }
//   part  → POST /v1/calls/{id}/screen/upload/part      { upload_id, part_number } → presigned PUT url
//           PUT the 8 MiB part bytes to the url, read the ETag response header
//   done  → POST /v1/calls/{id}/screen/upload/complete  { upload_id, parts, byte_size, duration_ms, ... }
//   fail  → POST /v1/calls/{id}/screen/upload/abort     { upload_id }   (best-effort, no leaked parts)
//
// EVERYTHING here is best-effort: a missing capture, a remux failure, a
// consent 400, a flag-off 404, or an exhausted retry ladder leaves the
// call fully functional (audio + transcript + summary intact) — the
// backend row simply goes `failed`/absent and the call-detail player
// shows its empty state. The screen video is NEVER fed to the AI.

/// Retention of the S3 ETag verbatim as the UploadPart response returned
/// it (S3 quotes it); the backend hands it straight to
/// CompleteMultipartUpload, which expects the exact returned value.
#[derive(Debug, Clone, Serialize)]
struct ScreenCompletePart {
    part_number: i32,
    etag: String,
}

#[derive(Serialize)]
struct ScreenInitBody {
    start_offset_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fps: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codec: Option<String>,
}

#[derive(Deserialize)]
struct ScreenInitResponse {
    upload_id: String,
    /// Part size the agent uses for every part except the last (8 MiB).
    /// `recording_id` / `key` / `min_part_size` are also on the wire but
    /// unused agent-side (serde ignores them).
    part_size: usize,
}

#[derive(Serialize)]
struct ScreenSignPartBody {
    upload_id: String,
    part_number: i32,
}

#[derive(Deserialize)]
struct ScreenSignPartResponse {
    url: String,
}

#[derive(Serialize)]
struct ScreenCompleteBody {
    upload_id: String,
    parts: Vec<ScreenCompletePart>,
    byte_size: Option<i64>,
    duration_ms: Option<i64>,
    start_offset_ms: Option<i64>,
    width: Option<i32>,
    height: Option<i32>,
    fps: Option<i32>,
    codec: Option<String>,
}

/// Upload the captured screen video for `call_id`, if this session
/// produced one. Best-effort throughout; returns `Ok(())` on every path
/// that leaves the call intact (which is all of them).
pub async fn upload_screen_recording(
    session_dir: &Path,
    call_id: &str,
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
) -> Result<()> {
    // No capture this session → nothing to do.
    let Some(meta) = ScreenRecordingMeta::read(session_dir) else {
        return Ok(());
    };
    let screen_dir = session_dir.join(SCREEN_SUBDIR);
    let raw_path = screen_dir.join(&meta.file);
    if !raw_path.exists() {
        eprintln!(
            "aftercalls: screen recording meta present but file missing at {} — skipping",
            raw_path.display()
        );
        return Ok(());
    }

    // Faststart remux so the portal/agent `<video>` can seek (the raw mp4
    // has moov-at-end). Best-effort: on failure upload the raw file.
    let upload_path = match remux_faststart(&raw_path).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("aftercalls: screen faststart remux failed, uploading raw: {e:#}");
            raw_path.clone()
        }
    };

    let byte_size = match std::fs::metadata(&upload_path) {
        Ok(m) => m.len() as i64,
        Err(e) => {
            eprintln!("aftercalls: stat screen video failed: {e}");
            return Ok(());
        }
    };
    if byte_size == 0 {
        eprintln!("aftercalls: screen video is empty — skipping upload");
        return Ok(());
    }

    // init — open the multipart upload. A consent 400
    // (`screen_capture_consent_required`) or a flag-off 404 both bubble as
    // non-retryable and we skip gracefully; init failing means there's no
    // upload to abort.
    let init_body = ScreenInitBody {
        start_offset_ms: meta.start_offset_ms,
        width: meta.width,
        height: meta.height,
        fps: Some(meta.fps),
        codec: Some(meta.codec.clone()),
    };
    let init_body_value = serde_json::to_value(&init_body).context("serialize screen init body")?;
    let init_path = format!("/v1/calls/{call_id}/screen/upload/init");
    let init: ScreenInitResponse = match retry_http(
        backend,
        guard,
        "screen_init",
        4,
        session_id_for_telemetry,
        |_attempt| {
            let body = init_body_value.clone();
            let path = init_path.clone();
            async move { screen_post_json::<ScreenInitResponse>(backend, &path, body).await }
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("{e:#}");
            // Vendor-opaque, calm breadcrumb — a rejected init is expected
            // when consent hasn't been posted or the flag is off.
            let reason = if msg.contains("screen_capture_consent_required") {
                "consent not recorded"
            } else if msg.contains("backend 404") {
                "feature not enabled"
            } else {
                "upload init rejected"
            };
            eprintln!("aftercalls: screen upload skipped ({reason}): {msg}");
            crate::telemetry::log(
                "warn",
                "pipeline::screen_upload_skipped",
                format!("screen upload init failed: {reason}"),
                Some(serde_json::json!({ "final_error": msg })),
                session_id_for_telemetry.map(|s| s.to_string()),
            );
            return Ok(());
        }
    };

    // Body of the upload: sign + PUT each part, then complete. On any
    // failure, abort so parts don't leak on the bucket.
    match screen_upload_parts_and_complete(
        backend,
        guard,
        session_id_for_telemetry,
        call_id,
        &meta,
        &upload_path,
        &init,
        byte_size,
    )
    .await
    {
        Ok(()) => {
            eprintln!("aftercalls: screen recording uploaded for call {call_id}");
            Ok(())
        }
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("aftercalls: screen upload failed, aborting: {msg}");
            crate::telemetry::log(
                "warn",
                "pipeline::screen_upload_failed",
                "screen recording upload failed (object storage)".to_string(),
                Some(serde_json::json!({ "final_error": msg })),
                session_id_for_telemetry.map(|s| s.to_string()),
            );
            // Best-effort abort — releases any parts already uploaded.
            let abort_path = format!("/v1/calls/{call_id}/screen/upload/abort");
            let abort_body = serde_json::json!({ "upload_id": init.upload_id });
            if let Err(ae) = screen_post_nop(backend, &abort_path, abort_body).await {
                eprintln!("aftercalls: screen upload abort failed: {ae:#}");
            }
            Ok(())
        }
    }
}

/// Sign + PUT every 8 MiB part in order, collecting `(part_number, etag)`,
/// then finalize with `complete`. Bubbles on the first exhausted retry so
/// the caller can abort. Resume-friendly at the backend level: the row +
/// `upload_id` persist, so a future `init` (re-run) aborts the stale
/// upload and starts fresh.
#[allow(clippy::too_many_arguments)]
async fn screen_upload_parts_and_complete(
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    call_id: &str,
    meta: &ScreenRecordingMeta,
    upload_path: &Path,
    init: &ScreenInitResponse,
    byte_size: i64,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let part_size = init.part_size.max(1);
    let client = screen_http_client()?;

    let mut file = tokio::fs::File::open(upload_path)
        .await
        .with_context(|| format!("open {}", upload_path.display()))?;

    let mut parts: Vec<ScreenCompletePart> = Vec::new();
    let mut part_number: i32 = 1;
    loop {
        // Read up to one full part (handles short reads on large files).
        let mut buf = vec![0u8; part_size];
        let mut filled = 0usize;
        while filled < part_size {
            let n = file
                .read(&mut buf[filled..])
                .await
                .context("read screen video part")?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break; // clean EOF on a part boundary
        }
        buf.truncate(filled);

        // Sign this part's PUT URL.
        let sign_path = format!("/v1/calls/{call_id}/screen/upload/part");
        let sign_body = serde_json::to_value(ScreenSignPartBody {
            upload_id: init.upload_id.clone(),
            part_number,
        })
        .context("serialize sign-part body")?;
        let signed: ScreenSignPartResponse = retry_http(
            backend,
            guard,
            "screen_sign_part",
            4,
            session_id_for_telemetry,
            |_attempt| {
                let body = sign_body.clone();
                let path = sign_path.clone();
                async move { screen_post_json::<ScreenSignPartResponse>(backend, &path, body).await }
            },
        )
        .await?;

        // PUT the bytes to object storage; read the ETag back. The
        // presigned UploadPart signs only host — no content-type header
        // (sending an extra signed header would 403), matching the audio
        // presign discipline in `put_file`.
        let put_url = signed.url.clone();
        let etag = retry_http(
            backend,
            guard,
            "screen_put_part",
            4,
            session_id_for_telemetry,
            |_attempt| {
                let client = client.clone();
                let url = put_url.clone();
                let bytes = buf.clone();
                async move { put_screen_part(&client, &url, bytes).await }
            },
        )
        .await?;

        parts.push(ScreenCompletePart { part_number, etag });

        // A short read means we just handled the final part.
        if filled < part_size {
            break;
        }
        part_number += 1;
    }

    if parts.is_empty() {
        anyhow::bail!("screen video produced no parts");
    }

    // complete — finalize + stamp metadata/retention.
    let complete_path = format!("/v1/calls/{call_id}/screen/upload/complete");
    let complete_body = serde_json::to_value(ScreenCompleteBody {
        upload_id: init.upload_id.clone(),
        parts,
        byte_size: Some(byte_size),
        duration_ms: Some(meta.duration_ms),
        start_offset_ms: Some(meta.start_offset_ms),
        width: meta.width,
        height: meta.height,
        fps: Some(meta.fps),
        codec: Some(meta.codec.clone()),
    })
    .context("serialize screen complete body")?;
    retry_http(
        backend,
        guard,
        "screen_complete",
        4,
        session_id_for_telemetry,
        |_attempt| {
            let body = complete_body.clone();
            let path = complete_path.clone();
            async move { screen_post_nop(backend, &path, body).await }
        },
    )
    .await?;

    Ok(())
}

/// Faststart remux (`-c copy -movflags +faststart`) so `<video>` can seek
/// without downloading the whole file. Stream copy → fast, no re-encode.
/// Output is `recording_fs.mp4` next to the raw capture.
async fn remux_faststart(raw: &Path) -> Result<PathBuf> {
    let out = raw.with_file_name("recording_fs.mp4");
    let mut cmd = tokio::process::Command::new(crate::pipeline::ffmpeg_binary());
    cmd.arg("-y")
        .arg("-i")
        .arg(raw)
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::pipeline::no_console(&mut cmd);
    let status = cmd.status().await.context("run ffmpeg faststart remux")?;
    if !status.success() {
        anyhow::bail!("ffmpeg faststart remux exited with {status}");
    }
    Ok(out)
}

/// PUT one part's bytes to its presigned URL; return the S3 `ETag`
/// (verbatim, quotes included) the backend needs for `complete`. No
/// content-type header — the UploadPart presign signs only host.
async fn put_screen_part(client: &reqwest::Client, url: &str, bytes: Vec<u8>) -> Result<String> {
    let resp = client
        .put(url)
        .body(bytes)
        .send()
        .await
        .context("PUT screen part")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("PUT returned {s}: {t}");
    }
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("screen part upload response missing ETag"))
}

/// Authed POST → typed JSON against the backend. Non-2xx bails with the
/// same `backend {status}: {body}` shape `classify_reqwest_error`
/// recognizes, so `retry_http` retries 5xx/network + bubbles 4xx.
async fn screen_post_json<T: serde::de::DeserializeOwned>(
    backend: &Backend,
    path: &str,
    body: serde_json::Value,
) -> Result<T> {
    let auth = build_auth_header(backend).await?;
    let client = screen_http_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {status}: {t}");
    }
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).context("decode screen upload response")
}

/// Authed POST that ignores the body (complete/abort return small or
/// no-content payloads the agent doesn't read).
async fn screen_post_nop(
    backend: &Backend,
    path: &str,
    body: serde_json::Value,
) -> Result<()> {
    let auth = build_auth_header(backend).await?;
    let client = screen_http_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("authorization", auth)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("backend {status}: {t}");
    }
    Ok(())
}

/// reqwest client for the screen-upload flow. Long timeout because a
/// single 8 MiB part PUT over a slow uplink can take a while; the UA is
/// stamped for backend attribution (matches `http_client`).
fn screen_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(user_agent())
        .build()?)
}
