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
use crate::screen_recorder::{
    ScreenRecordingMeta, META_FILENAME, RECORDING_FILENAME, SCREEN_SUBDIR,
};

#[derive(Deserialize, Debug)]
pub struct CreateCallResponse {
    pub call_id: String,
}

/// Create (or upsert) the call row on the backend with the minimal
/// metadata we know pre-pipeline: session_id, recorded_at, duration
/// estimate (zero is fine — transcribe backfills the real duration),
/// the source descriptor, and an empty utterances array. The response
/// returns the call id used by the resumable media-generation API.
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
/// validated candidate. `uploaded=true` means this run obtained authoritative
/// ready/current evidence for the immutable generation.
/// `failure_class` carries the last classified error for tail-failed tracks so
/// the pending_uploads sentinel can record it.
#[derive(Debug, Clone, Serialize)]
pub struct TrackOutcome {
    /// One of `"mic" | "system"`.
    pub track: &'static str,
    pub uploaded: bool,
    /// `None` on success or skipped; populated only when retry_http
    /// returned an error.
    pub failure_class: Option<FailureClass>,
    /// Human-readable last error string, only present on failure. Kept
    /// vendor-opaque: "object storage" rather than "DigitalOcean Spaces".
    pub final_error: Option<String>,
}

/// A path the local media pipeline positively published after clean encode and
/// decode/duration validation, or the same path recovered from an immutable
/// generation checkpoint. Recovery may carry a now-missing path solely so the
/// generation client can GET authority before deciding whether an abort is
/// required; file presence alone never manufactures readiness.
#[derive(Debug, Clone)]
pub struct PreparedAudioTrack {
    pub track: &'static str,
    pub opus_path: PathBuf,
}

/// #646 Layer B — sentinel file written to `<session_dir>/pending_uploads.json`
/// when at least one track exhausted its retry ladder. Survives
/// agent restarts so the orphan-resume path can revisit only tracks whose
/// generations are not yet ready/current.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUploads {
    /// Subset of `["mic", "system"]` that still needs authoritative readiness.
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

/// Delete the sentinel after a successful re-upload run when every
/// previously-failed track has landed. Best-effort; local media itself is
/// retained until a matching backend-ready acknowledgement exists.
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
            if let Err(e) = crate::session_fs::write_private_file(&path, text.as_bytes()) {
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

/// Uploads only explicitly prepared, fully validated Opus tracks. Raw WAV is
/// never passed to the upload layer and a file's presence/length can never
/// manufacture a readiness proof.
///
/// Each immutable file is hashed before create/resume. The backend supplies an
/// exact part plan and checksum-bound required headers; local bytes are removed
/// only after that generation is authoritative `ready/current`.
///
/// #646 Layer B — return value is now `Vec<TrackOutcome>` instead of
/// `Result<()>`. Backend control-plane calls use guarded retries; signed
/// object-storage PUTs use their own credential-isolated transient retry
/// ladder. Tracks that exhaust it yield a
/// `TrackOutcome { uploaded: false, ... }` entry and emit
/// `pipeline::track_upload_failed` telemetry.
/// On any partial failure we write a `pending_uploads.json` sentinel
/// next to the audio so a later orphan-resume can attempt only the
/// still-missing tracks. On full success in a re-run we delete the
/// existing sentinel.
pub async fn upload_audio(
    session_dir: &Path,
    prepared: &[PreparedAudioTrack],
    backend: &Backend,
    call_id: &str,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
) -> Result<Vec<TrackOutcome>> {
    let mut outcomes: Vec<TrackOutcome> = Vec::with_capacity(2);

    for candidate in prepared {
        let track = candidate.track;
        // The explicit `PreparedAudioTrack` is the validity proof. A stat
        // below may size the request, but cannot create that proof.
        let opus_path = &candidate.opus_path;
        let kind = crate::media_upload::MediaKind::from_audio_track(track)?;
        let source = crate::media_upload::MediaSource::audio(
            kind,
            opus_path.clone(),
            session_dir.join(format!("{track}.wav")),
        )?;
        match crate::media_upload::ensure_generation_ready(
            session_dir,
            call_id,
            backend,
            guard,
            session_id_for_telemetry,
            &source,
        )
        .await
        {
            // Audio treats both outcomes as uploaded. `Ready` is the backend's
            // confirmation; `Finalizing` means every byte is stored and the
            // backend is still validating. Audio validation measures ~12s
            // against a ~60s budget so the second arm is rare, but when it does
            // happen the bytes are just as safe as in the first — re-uploading
            // them would be pure waste.
            Ok(outcome) => {
                match &outcome {
                    crate::media_upload::GenerationOutcome::Ready(ready) => eprintln!(
                        "aftercalls: audio {track} generation {} is ready/current",
                        ready.generation_id
                    ),
                    crate::media_upload::GenerationOutcome::Finalizing {
                        generation_id,
                        state,
                    } => eprintln!(
                        "aftercalls: audio {track} generation {generation_id} uploaded; backend still {state}"
                    ),
                }
                outcomes.push(TrackOutcome {
                    track,
                    uploaded: true,
                    failure_class: None,
                    final_error: None,
                });
                continue;
            }
            Err(e) => {
                eprintln!("aftercalls: audio generation for {track} remains pending: {e:#}");
                let class = crate::portal::classify_reqwest_error(&e);
                let err_str = format!("{e:#}");
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
        // Aggregate cleanup boundary: do not delete the first ready track if
        // another recorded track failed later in the same run.
        //
        // `uploaded` is NOT the deletion boundary — a `Finalizing` track counts
        // as uploaded so the pipeline can move on, but the backend has not yet
        // confirmed the generation. Deleting on that signal destroys the only
        // verified copy while validation could still go terminal, so every
        // cleanup below is gated on the artifact reaching `ReadyAcknowledged`.
        let manifest = crate::media_manifest::read(session_dir)?;
        let acknowledged = |track: &str| {
            manifest
                .as_ref()
                .and_then(|manifest| manifest.audio.get(track))
                .is_some_and(|item| {
                    item.state == crate::media_manifest::ArtifactState::ReadyAcknowledged
                })
        };
        for candidate in prepared {
            if !acknowledged(candidate.track) {
                continue;
            }
            let kind = crate::media_upload::MediaKind::from_audio_track(candidate.track)?;
            let source = crate::media_upload::MediaSource::audio(
                kind,
                candidate.opus_path.clone(),
                session_dir.join(format!("{}.wav", candidate.track)),
            )?;
            crate::media_upload::cleanup_ready_source(session_dir, &source);
        }
        // A retry can enter with one or both generations already acknowledged
        // ready while their local bytes survived a crash before aggregate
        // cleanup. Once this run proves there are no pending tracks, remove
        // the canonical local sources for every acknowledged audio kind too.
        for track in ["mic", "system"] {
            if acknowledged(track) {
                let kind = crate::media_upload::MediaKind::from_audio_track(track)?;
                let source = crate::media_upload::MediaSource::audio(
                    kind,
                    session_dir.join(format!("{track}.opus")),
                    session_dir.join(format!("{track}.wav")),
                )?;
                crate::media_upload::cleanup_ready_source(session_dir, &source);
            }
        }
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

fn http_client() -> Result<reqwest::Client> {
    // #293 — stamp the same `aftercalls/<ver> (<os>)` UA portal::client()
    // uses, so backend tracing on `POST /v1/calls` can attribute the
    // request to a specific agent build instead of logging
    // `agent_ver = "unknown"`. `attach_note_path` reuses this client too
    // and benefits from the same attribution.
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(user_agent())
        .build()?)
}

fn parse_session_timestamp(session_id: &str) -> DateTime<Utc> {
    crate::session_fs::parse_timestamp(session_id).unwrap_or_else(Utc::now)
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

// ── Screen recording — resumable media generation ─────────────────────
//
// Screen metadata and the faststart representation remain local until the
// shared generation client validates authoritative ready/current evidence.

#[derive(Debug, Clone)]
pub enum ScreenUploadOutcome {
    NotPresent,
    ReadyAcknowledged { generation_id: String },
    /// Fully uploaded; the backend is still assembling/validating. The pipeline
    /// completes on this — the call and its audio are usable immediately and
    /// the video attaches itself once validation lands.
    Finalizing { generation_id: String },
}

/// Upload the captured screen video for `call_id`, if this session
/// produced one. Upload failure is not success: it returns `Err` after
/// atomically checkpointing a retryable local copy, and the aggregate pipeline
/// must not report completion until a later retry reaches ready/current.
pub async fn upload_screen_recording(
    session_dir: &Path,
    call_id: &str,
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
) -> Result<ScreenUploadOutcome> {
    let screen_dir = session_dir.join(SCREEN_SUBDIR);
    let raw_default = screen_dir.join(RECORDING_FILENAME);
    let checkpoint = crate::media_manifest::read(session_dir)?.and_then(|manifest| manifest.screen);
    if let Some(checkpoint) = &checkpoint {
        if checkpoint.state == crate::media_manifest::ArtifactState::ReadyAcknowledged {
            let generation_id = checkpoint
                .upload
                .as_ref()
                .and_then(|upload| upload.generation_id.clone())
                .context("ready screen checkpoint omitted its generation id")?;
            // A crash can land after durable ready evidence but before local
            // cleanup. This path trusts only the fixed in-session filenames;
            // no checkpoint-controlled path is passed to deletion.
            let source = crate::media_upload::MediaSource::resume_screen(
                raw_default.clone(),
                raw_default.clone(),
            );
            crate::media_upload::cleanup_ready_source(session_dir, &source);
            return Ok(ScreenUploadOutcome::ReadyAcknowledged { generation_id });
        }
        if checkpoint
            .upload
            .as_ref()
            .and_then(|upload| upload.generation_id.as_ref())
            .is_some()
        {
            let upload_path = checkpoint
                .published_path
                .as_deref()
                .map(|path| session_dir.join(path))
                .unwrap_or_else(|| screen_dir.join("recording_fs.mp4"));
            let raw_path = checkpoint
                .raw_path
                .as_deref()
                .map(|path| session_dir.join(path))
                .unwrap_or_else(|| raw_default.clone());
            let source =
                crate::media_upload::MediaSource::resume_screen(upload_path, raw_path.clone());
            return upload_screen_source(
                session_dir,
                call_id,
                backend,
                guard,
                session_id_for_telemetry,
                source,
                &raw_path,
            )
            .await;
        }
    }

    // No capture this session → nothing to do. If durable state or local
    // bytes say a capture existed, missing/corrupt metadata is a retryable
    // failure, never "not present".
    let Some(meta) = ScreenRecordingMeta::read(session_dir) else {
        let meta_path = screen_dir.join(META_FILENAME);
        let checkpoint_needs_media = checkpoint
            .as_ref()
            .map(|checkpoint| {
                matches!(
                    checkpoint.state,
                    crate::media_manifest::ArtifactState::Recording
                        | crate::media_manifest::ArtifactState::RawReady
                        | crate::media_manifest::ArtifactState::EncodingFailed
                        | crate::media_manifest::ArtifactState::Published
                        | crate::media_manifest::ArtifactState::UploadPending
                        | crate::media_manifest::ArtifactState::UploadedAwaitingBackendReady
                )
            })
            .unwrap_or(false);
        if checkpoint_needs_media || meta_path.exists() || raw_default.exists() {
            return Err(retain_screen_upload_failure(
                session_dir,
                call_id,
                &raw_default,
                "screen recording metadata is missing or unreadable".into(),
            ));
        }
        crate::media_manifest::mark_screen_not_present(session_dir)?;
        return Ok(ScreenUploadOutcome::NotPresent);
    };
    if meta.file != RECORDING_FILENAME {
        return Err(retain_screen_upload_failure(
            session_dir,
            call_id,
            &raw_default,
            format!(
                "screen recording metadata referenced unsupported filename {:?}",
                meta.file
            ),
        ));
    }
    let raw_path = screen_dir.join(&meta.file);
    if !raw_path.exists() {
        return Err(retain_screen_upload_failure(
            session_dir,
            call_id,
            &raw_path,
            format!(
                "screen recording metadata exists but {} is missing",
                raw_path.display()
            ),
        ));
    }

    // Persist intent before the first remux/network side effect. A crash from
    // here onward leaves an explicit pending job for restart recovery.
    crate::media_manifest::mark_screen_upload_pending(
        session_dir,
        Some(call_id),
        &raw_path,
        "screen upload started".into(),
    )?;

    // Faststart remux so the portal/agent `<video>` can seek (the raw mp4
    // has moov-at-end). Remux failure remains pending; do not silently upload
    // a different, unvalidated representation.
    let checkpointed_upload = checkpoint.as_ref().and_then(|item| {
        item.upload.as_ref()?;
        item.published_path
            .as_deref()
            .map(|path| session_dir.join(path))
            .filter(|path| path.exists())
    });
    let upload_path = match checkpointed_upload {
        Some(path) => path,
        None => match remux_faststart(&raw_path).await {
            Ok(path) => path,
            Err(error) => {
                return Err(retain_screen_upload_failure(
                    session_dir,
                    call_id,
                    &raw_path,
                    format!("screen faststart remux failed: {error:#}"),
                ));
            }
        },
    };
    let (width, height) = match resolve_screen_dimensions(&meta, &upload_path).await {
        Ok(dimensions) => dimensions,
        Err(error) => {
            return Err(retain_screen_upload_failure(
                session_dir,
                call_id,
                &raw_path,
                format!("screen dimensions unavailable: {error:#}"),
            ));
        }
    };
    if !meta.codec.eq_ignore_ascii_case("h264") {
        return Err(retain_screen_upload_failure(
            session_dir,
            call_id,
            &raw_path,
            format!("unsupported screen codec {:?}", meta.codec),
        ));
    }
    let source = crate::media_upload::MediaSource::screen(
        upload_path,
        raw_path.clone(),
        meta.duration_ms,
        width,
        height,
        f64::from(meta.fps),
        meta.start_offset_ms,
    );
    upload_screen_source(
        session_dir,
        call_id,
        backend,
        guard,
        session_id_for_telemetry,
        source,
        &raw_path,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn upload_screen_source(
    session_dir: &Path,
    call_id: &str,
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    source: crate::media_upload::MediaSource,
    raw_path: &Path,
) -> Result<ScreenUploadOutcome> {
    match crate::media_upload::ensure_generation_ready(
        session_dir,
        call_id,
        backend,
        guard,
        session_id_for_telemetry,
        &source,
    )
    .await
    {
        Ok(crate::media_upload::GenerationOutcome::Ready(ready)) => {
            eprintln!(
                "aftercalls: screen generation {} is ready/current for call {call_id}",
                ready.generation_id
            );
            crate::media_upload::cleanup_ready_source(session_dir, &source);
            Ok(ScreenUploadOutcome::ReadyAcknowledged {
                generation_id: ready.generation_id,
            })
        }
        // Uploaded in full; the backend is still validating it. This is the
        // common shape for a long call — a 1.6 GB capture took the backend
        // 2m43s to re-hash and probe, well past the ~60s poll budget — so it
        // must NOT read as a failure. The call is complete and usable now; the
        // video attaches itself when validation finishes.
        //
        // The local source is deliberately RETAINED here (unlike the ready
        // arm, which cleans it up): until the backend confirms the generation,
        // the local file is still the only verified copy.
        Ok(crate::media_upload::GenerationOutcome::Finalizing {
            generation_id,
            state,
        }) => {
            eprintln!(
                "aftercalls: screen generation {generation_id} uploaded for call {call_id}; backend still {state}"
            );
            crate::telemetry::log(
                "info",
                "pipeline::screen_upload_finalizing",
                "screen recording uploaded; backend still processing".to_string(),
                Some(serde_json::json!({ "generation_id": generation_id, "state": state })),
                session_id_for_telemetry.map(str::to_string),
            );
            Ok(ScreenUploadOutcome::Finalizing { generation_id })
        }
        Err(error) => {
            let message = format!("{error:#}");
            crate::telemetry::log(
                "warn",
                "pipeline::screen_upload_failed",
                "screen recording upload remains pending".to_string(),
                Some(serde_json::json!({ "final_error": message })),
                session_id_for_telemetry.map(str::to_string),
            );
            Err(retain_screen_upload_failure(
                session_dir,
                call_id,
                raw_path,
                format!("screen generation upload pending: {message}"),
            ))
        }
    }
}

fn retain_screen_upload_failure(
    session_dir: &Path,
    call_id: &str,
    local_path: &Path,
    error: String,
) -> anyhow::Error {
    let checkpoint_error = crate::media_manifest::mark_screen_upload_pending(
        session_dir,
        Some(call_id),
        local_path,
        error.clone(),
    )
    .err();
    match checkpoint_error {
        Some(checkpoint_error) => anyhow!(
            "{error}; additionally failed to persist pending-media checkpoint: {checkpoint_error:#}"
        ),
        None => anyhow!(error),
    }
}

async fn resolve_screen_dimensions(
    meta: &ScreenRecordingMeta,
    media_path: &Path,
) -> Result<(u32, u32)> {
    match (meta.width, meta.height) {
        (Some(width), Some(height))
            if (1..=16_384).contains(&width) && (1..=16_384).contains(&height) =>
        {
            return Ok((width as u32, height as u32));
        }
        (Some(_), Some(_)) => anyhow::bail!("recorded dimensions are outside 1..=16384"),
        _ => {}
    }
    let path = media_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(crate::pipeline::ffmpeg_binary());
        command
            .arg("-hide_banner")
            .arg("-i")
            .arg(&path)
            .arg("-map")
            .arg("0:v:0")
            .arg("-frames:v")
            .arg("1")
            .arg("-f")
            .arg("null")
            .arg("-");
        let output = crate::media_process::run_bounded(
            command,
            Duration::from_secs(2 * 60),
            crate::media_process::STDERR_LIMIT_BYTES,
        )
        .context("probe screen recording dimensions")?;
        if !output.success() {
            anyhow::bail!("screen dimension probe failed: {}", output.diagnostic());
        }
        parse_ffmpeg_video_dimensions(&String::from_utf8_lossy(&output.stderr))
            .context("screen dimension probe did not report a video size")
    })
    .await
    .context("join screen dimension probe")?
}

fn parse_ffmpeg_video_dimensions(stderr: &str) -> Option<(u32, u32)> {
    for line in stderr.lines().filter(|line| line.contains("Video:")) {
        for token in line.split_whitespace() {
            let candidate = token
                .trim_matches(|character: char| !character.is_ascii_digit() && character != 'x');
            let Some((width, height)) = candidate.split_once('x') else {
                continue;
            };
            let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>()) else {
                continue;
            };
            if (1..=16_384).contains(&width) && (1..=16_384).contains(&height) {
                return Some((width, height));
            }
        }
    }
    None
}

/// Faststart remux (`-c copy -movflags +faststart`) so `<video>` can seek
/// without downloading the whole file. Stream copy → fast, no re-encode.
/// Output is `recording_fs.mp4` next to the raw capture.
async fn remux_faststart(raw: &Path) -> Result<PathBuf> {
    let raw = raw.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let out = raw.with_file_name("recording_fs.mp4");
        let staged = out.with_file_name(format!("recording_fs.mp4.part.{}", uuid::Uuid::new_v4()));
        let _stage_guard = crate::media_manifest::reserve_private_stage(&staged)?;
        let mut command = std::process::Command::new(crate::pipeline::ffmpeg_binary());
        command
            .arg("-y")
            .arg("-i")
            .arg(&raw)
            .arg("-c")
            .arg("copy")
            .arg("-movflags")
            .arg("+faststart")
            .arg("-f")
            .arg("mp4")
            .arg(&staged);
        let process = crate::media_process::run_bounded(
            command,
            Duration::from_secs(10 * 60),
            crate::media_process::STDERR_LIMIT_BYTES,
        )
        .context("run bounded ffmpeg faststart remux")?;
        if !process.success() {
            let _ = std::fs::remove_file(&staged);
            anyhow::bail!("ffmpeg faststart remux failed: {}", process.diagnostic());
        }
        let byte_size = std::fs::metadata(&staged)
            .with_context(|| format!("stat staged screen remux {}", staged.display()))?
            .len();
        if byte_size == 0 {
            let _ = std::fs::remove_file(&staged);
            anyhow::bail!("ffmpeg faststart remux produced an empty file");
        }
        crate::media_manifest::enforce_private_file(&staged)?;
        crate::media_manifest::sync_staged_file(&staged)
            .with_context(|| format!("sync staged screen remux {}", staged.display()))?;
        crate::media_manifest::atomic_replace_file(&staged, &out)?;
        Ok(out)
    })
    .await
    .context("join screen faststart remux")?
}

#[cfg(test)]
mod media_retention_tests {
    use super::*;
    use crate::media_manifest::{self, ArtifactState};
    use std::sync::atomic::{AtomicU64, Ordering};

    const CALL_ID: &str = "4da6e5c4-7ac1-45bb-bab6-8e269a2664c2";

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "aftercalls-screen-retention-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(path.join("screen")).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn screen_upload_failure_is_pending_and_retains_only_local_video() {
        let scratch = Scratch::new();
        let video = scratch.0.join("screen").join("recording.mp4");
        std::fs::write(&video, b"only local video").unwrap();
        media_manifest::initialize(&scratch.0).unwrap();

        let error = retain_screen_upload_failure(
            &scratch.0,
            CALL_ID,
            &video,
            "injected complete failure".into(),
        );
        assert!(error.to_string().contains("injected complete failure"));
        assert!(video.exists(), "failure must not delete the only video");
        let manifest = media_manifest::read(&scratch.0).unwrap().unwrap();
        assert_eq!(manifest.screen.unwrap().state, ArtifactState::UploadPending);
    }

    #[tokio::test]
    async fn missing_metadata_preserves_pending_screen_for_restart() {
        let scratch = Scratch::new();
        let video = scratch.0.join("screen").join(RECORDING_FILENAME);
        std::fs::write(&video, b"recoverable video").unwrap();
        media_manifest::mark_screen_upload_pending(
            &scratch.0,
            Some(CALL_ID),
            &video,
            "injected crash before retry".into(),
        )
        .unwrap();
        let backend = Backend {
            url: "http://127.0.0.1:1".into(),
            token: None,
        };

        let result =
            upload_screen_recording(&scratch.0, CALL_ID, &backend, &RetryGuard::new(), None).await;
        assert!(result.is_err());
        assert!(video.exists());
        let manifest = media_manifest::read(&scratch.0).unwrap().unwrap();
        assert_eq!(manifest.screen.unwrap().state, ArtifactState::UploadPending);
    }

    #[tokio::test]
    async fn legacy_awaiting_screen_without_generation_remains_pending() {
        let scratch = Scratch::new();
        let video = scratch.0.join("screen").join(RECORDING_FILENAME);
        std::fs::write(&video, b"uploaded video").unwrap();
        media_manifest::mark_screen_uploaded(&scratch.0, CALL_ID, &video).unwrap();
        let backend = Backend {
            url: "http://127.0.0.1:1".into(),
            token: None,
        };

        let result =
            upload_screen_recording(&scratch.0, CALL_ID, &backend, &RetryGuard::new(), None).await;
        assert!(result.is_err());
        assert!(video.exists());
        let manifest = media_manifest::read(&scratch.0).unwrap().unwrap();
        assert_eq!(
            manifest.screen.unwrap().state,
            ArtifactState::UploadedAwaitingBackendReady
        );
    }

    #[test]
    fn parses_only_bounded_video_dimensions() {
        let stderr = "Stream #0:0: Video: h264, yuv420p, 1920x1080, 15 fps";
        assert_eq!(parse_ffmpeg_video_dimensions(stderr), Some((1920, 1080)));
        assert_eq!(
            parse_ffmpeg_video_dimensions(
                "Stream #0:0: Audio: opus\nmetadata 1920x1080 but no video marker"
            ),
            None
        );
        assert_eq!(
            parse_ffmpeg_video_dimensions("Video: h264, 99999x1080"),
            None
        );
    }
}
