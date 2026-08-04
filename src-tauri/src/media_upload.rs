//! Resumable client for the backend media-generation protocol.
//!
//! One immutable local artifact maps to one stable `client_operation_id`.
//! The backend is authoritative for generation state and confirmed parts; the
//! local manifest mirrors enough identity to resume after a process crash.
//! A successful `complete` request is never treated as durable ownership:
//! cleanup is authorized only by `state = ready`, `is_current = true`, and
//! matching actual byte/hash evidence.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use futures_util::StreamExt;
use rand::Rng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::config::{Backend, Config};
use crate::media_manifest::{self, ArtifactState, UploadCheckpoint};
use crate::portal::{
    build_auth_header, classify_reqwest_error, retry_http, user_agent, FailureClass, RetryGuard,
};

const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PART_COUNT: u32 = 10_000;
const MAX_BACKEND_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const FINALIZE_POLL_DELAYS_MS: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 15_000, 30_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Mic,
    System,
    Screen,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
            Self::Screen => "screen",
        }
    }

    pub fn from_audio_track(track: &str) -> Result<Self> {
        match track {
            "mic" => Ok(Self::Mic),
            "system" => Ok(Self::System),
            _ => anyhow::bail!("unsupported audio media kind {track:?}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaSource {
    pub kind: MediaKind,
    pub path: PathBuf,
    pub raw_path: Option<PathBuf>,
    pub content_type: Option<&'static str>,
    pub extension: Option<&'static str>,
    pub duration_ms: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub codec: Option<&'static str>,
    pub start_offset_ms: Option<i64>,
    resume_only: bool,
}

impl MediaSource {
    pub fn audio(kind: MediaKind, path: PathBuf, raw_path: PathBuf) -> Result<Self> {
        if !matches!(kind, MediaKind::Mic | MediaKind::System) {
            anyhow::bail!("audio source must be mic or system");
        }
        Ok(Self {
            kind,
            path,
            raw_path: Some(raw_path),
            content_type: Some("audio/ogg"),
            extension: Some("opus"),
            duration_ms: None,
            width: None,
            height: None,
            fps: None,
            codec: None,
            start_offset_ms: None,
            resume_only: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn screen(
        path: PathBuf,
        raw_path: PathBuf,
        duration_ms: i64,
        width: u32,
        height: u32,
        fps: f64,
        start_offset_ms: i64,
    ) -> Self {
        Self {
            kind: MediaKind::Screen,
            path,
            raw_path: Some(raw_path),
            content_type: Some("video/mp4"),
            extension: Some("mp4"),
            duration_ms: Some(duration_ms),
            width: Some(width),
            height: Some(height),
            fps: Some(fps),
            codec: Some("h264"),
            start_offset_ms: Some(start_offset_ms),
            resume_only: false,
        }
    }

    /// Resume an already-created screen generation without reconstructing its
    /// immutable init metadata. This source can GET/upload/poll that exact
    /// generation, but is forbidden from creating a replacement.
    pub fn resume_screen(path: PathBuf, raw_path: PathBuf) -> Self {
        Self {
            kind: MediaKind::Screen,
            path,
            raw_path: Some(raw_path),
            content_type: None,
            extension: None,
            duration_ms: None,
            width: None,
            height: None,
            fps: None,
            codec: None,
            start_offset_ms: None,
            resume_only: true,
        }
    }

    fn validate(&self) -> Result<()> {
        match self.kind {
            MediaKind::Mic | MediaKind::System => {
                if self.content_type != Some("audio/ogg")
                    || self.extension != Some("opus")
                    || self.duration_ms.is_some()
                {
                    anyhow::bail!("audio media request metadata is not canonical Opus");
                }
                if self.width.is_some()
                    || self.height.is_some()
                    || self.fps.is_some()
                    || self.codec.is_some()
                    || self.start_offset_ms.is_some()
                {
                    anyhow::bail!("audio media request contains screen-only metadata");
                }
            }
            MediaKind::Screen => {
                if self.resume_only {
                    if self.content_type.is_some()
                        || self.extension.is_some()
                        || self.duration_ms.is_some()
                        || self.width.is_some()
                        || self.height.is_some()
                        || self.fps.is_some()
                        || self.codec.is_some()
                        || self.start_offset_ms.is_some()
                    {
                        anyhow::bail!("resume-only screen source included mutable init metadata");
                    }
                    return Ok(());
                }
                if self.content_type != Some("video/mp4") || self.extension != Some("mp4") {
                    anyhow::bail!("screen media request metadata is not canonical MP4");
                }
                let duration_ms = self
                    .duration_ms
                    .context("screen upload requires duration")?;
                if duration_ms <= 0 {
                    anyhow::bail!("screen duration must be positive");
                }
                let width = self.width.context("screen upload requires width")?;
                let height = self.height.context("screen upload requires height")?;
                if !(1..=16_384).contains(&width) || !(1..=16_384).contains(&height) {
                    anyhow::bail!("screen dimensions must be within 1..=16384");
                }
                let fps = self.fps.context("screen upload requires fps")?;
                if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
                    anyhow::bail!("screen fps must be finite and within (0, 240]");
                }
                if self.codec != Some("h264") {
                    anyhow::bail!("screen upload codec must be h264");
                }
                let start_offset_ms = self
                    .start_offset_ms
                    .context("screen upload requires start offset")?;
                if start_offset_ms < 0 {
                    anyhow::bail!("screen start offset must not be negative");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ReadyGeneration {
    pub generation_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct CreateUploadBody {
    kind: &'static str,
    client_operation_id: String,
    declared_bytes: i64,
    declared_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extension: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codec: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_offset_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadStatus {
    generation_id: String,
    call_id: String,
    kind: String,
    object_key: String,
    state: String,
    is_current: bool,
    declared_bytes: u64,
    declared_sha256: String,
    actual_bytes: Option<u64>,
    actual_sha256: Option<String>,
    part_size_bytes: u64,
    part_count: u32,
    parts: Vec<UploadPartStatus>,
    #[allow(dead_code)]
    error_code: Option<String>,
    #[allow(dead_code)]
    created_at: String,
    #[allow(dead_code)]
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadPartStatus {
    part_number: u32,
    offset_bytes: u64,
    length_bytes: u64,
    sha256: Option<String>,
    #[allow(dead_code)]
    signed: bool,
    confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SignPartBody {
    offset_bytes: i64,
    length_bytes: i32,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SignPartResponse {
    generation_id: String,
    part_number: u32,
    url: String,
    expires_in_secs: u64,
    required_headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConfirmPartResponse {
    generation_id: String,
    part_number: u32,
    confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AbortBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateClass {
    Uploading,
    Finalizing,
    Ready,
    Terminal,
}

fn classify_state(state: &str) -> Result<StateClass> {
    match state {
        "uploading" => Ok(StateClass::Uploading),
        "assembling" | "validating" => Ok(StateClass::Finalizing),
        "ready" => Ok(StateClass::Ready),
        "failed" | "aborted" | "superseded" | "deleting" | "deleted" => Ok(StateClass::Terminal),
        _ => anyhow::bail!("backend returned unknown media generation state {state:?}"),
    }
}

/// Upload or resume one immutable source. The call returns success only after
/// the backend reports the same generation ready/current with matching actual
/// bytes and SHA-256.
pub async fn ensure_generation_ready(
    session_dir: &Path,
    call_id: &str,
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    source: &MediaSource,
) -> Result<ReadyGeneration> {
    let mut source = source.clone();
    source.path = validated_session_path(session_dir, &source.path)?;
    source.raw_path = source
        .raw_path
        .as_deref()
        .map(|path| validated_session_path(session_dir, path))
        .transpose()?;
    let source = &source;
    source.validate()?;
    validate_call_id(call_id)?;
    let kind = source.kind.as_str();

    let mut existing = media_manifest::artifact(session_dir, kind)?;
    let mut status = if let Some(upload) = existing.as_ref().and_then(|item| item.upload.as_ref()) {
        if let Some(generation_id) = upload.generation_id.as_deref() {
            let status = fetch_status(
                backend,
                guard,
                session_id_for_telemetry,
                call_id,
                generation_id,
            )
            .await?;
            validate_status(&status, call_id, kind, existing.as_ref(), Some(upload))?;
            persist_status(session_dir, kind, &status)?;
            Some(status)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(current) = status.as_ref() {
        match classify_state(&current.state)? {
            StateClass::Ready => {
                let ready = finish_ready(session_dir, source, current).await?;
                return Ok(ready);
            }
            StateClass::Finalizing => {
                let ready = drive_to_ready(
                    session_dir,
                    call_id,
                    backend,
                    guard,
                    session_id_for_telemetry,
                    source,
                    current.clone(),
                )
                .await?;
                return Ok(ready);
            }
            StateClass::Terminal => {
                // The backend generation is dead — aborted, failed, or
                // superseded. The local source is untouched and immutable, so
                // the recoverable move is to start a NEW generation, not to
                // give up on the call forever.
                //
                // Returning an error here meant any interrupted upload became
                // permanently unresumable: "Resume" re-fetched the same dead
                // generation and re-reported the same error every time, and
                // the only fix was hand-editing this manifest. Drop the dead
                // checkpoint and fall through to the fresh-upload path below,
                // which mints a new client_operation_id from the same bytes.
                eprintln!(
                    "aftercalls: {kind} generation is {} — discarding it and starting a fresh upload",
                    current.state
                );
                crate::telemetry::log(
                    "info",
                    "media::generation_restarted",
                    format!("{kind} generation {} — restarting upload", current.state),
                    Some(serde_json::json!({
                        "kind": kind,
                        "terminal_state": current.state,
                    })),
                    session_id_for_telemetry.map(|s| s.to_string()),
                );
                media_manifest::reset_aborted_upload(
                    session_dir,
                    kind,
                    format!(
                        "backend generation {} — starting a fresh upload",
                        current.state
                    ),
                )?;
                status = None;
                // `existing` still describes the discarded checkpoint; the
                // fresh path below compares it against the re-hashed source
                // and would reject the stale operation id as an immutability
                // violation. Re-read so it reflects the reset.
                existing = media_manifest::artifact(session_dir, kind)?;
            }
            StateClass::Uploading => {}
        }
    }

    let (declared_bytes, declared_sha256) = match hash_file(&source.path).await {
        Ok(digest) => digest,
        Err(error) => {
            if let Some(generation_id) = existing
                .as_ref()
                .and_then(|item| item.upload.as_ref())
                .and_then(|upload| upload.generation_id.as_deref())
            {
                abort_generation(
                    backend,
                    guard,
                    session_id_for_telemetry,
                    call_id,
                    kind,
                    generation_id,
                    Some("local_source_unavailable"),
                )
                .await?;
                media_manifest::reset_aborted_upload(
                    session_dir,
                    kind,
                    format!("backend generation aborted: local source unavailable: {error:#}"),
                )?;
            }
            return Err(error);
        }
    };
    if declared_bytes == 0 {
        anyhow::bail!("{} upload source is empty", kind);
    }
    let declared_bytes_wire =
        i64::try_from(declared_bytes).context("upload source exceeds backend byte range")?;

    if let Some(existing_upload) = existing.as_ref().and_then(|item| item.upload.as_ref()) {
        let expected_bytes = existing.as_ref().and_then(|item| item.byte_size);
        if expected_bytes != Some(declared_bytes)
            || !existing_upload
                .declared_sha256
                .eq_ignore_ascii_case(&declared_sha256)
        {
            if let Some(generation_id) = existing_upload.generation_id.as_deref() {
                abort_generation(
                    backend,
                    guard,
                    session_id_for_telemetry,
                    call_id,
                    kind,
                    generation_id,
                    Some("local_source_changed"),
                )
                .await?;
            }
            media_manifest::reset_aborted_upload(
                session_dir,
                kind,
                "backend generation reset because the local source changed".into(),
            )?;
            status = None;
        }
    }

    let checkpoint = media_manifest::prepare_upload(
        session_dir,
        call_id,
        kind,
        source.raw_path.as_deref(),
        &source.path,
        declared_bytes,
        &declared_sha256,
    )?;
    validate_operation_id(&checkpoint.client_operation_id)?;

    if status.is_none() {
        if source.resume_only {
            anyhow::bail!(
                "cannot create a replacement screen generation without immutable capture metadata"
            );
        }
        let create_path = format!("/v1/calls/{call_id}/media/uploads");
        let body = serde_json::to_value(CreateUploadBody {
            kind,
            client_operation_id: checkpoint.client_operation_id.clone(),
            declared_bytes: declared_bytes_wire,
            declared_sha256: declared_sha256.clone(),
            content_type: source.content_type,
            extension: source.extension,
            duration_ms: source.duration_ms,
            width: source.width,
            height: source.height,
            fps: source.fps,
            codec: source.codec,
            start_offset_ms: source.start_offset_ms,
        })
        .context("serialize media upload create body")?;
        let created: UploadStatus = retry_http(
            backend,
            guard,
            "media_upload_create",
            4,
            session_id_for_telemetry,
            |_attempt| {
                let path = create_path.clone();
                let body = body.clone();
                async move { post_create_upload(backend, &path, &body).await }
            },
        )
        .await?;
        let artifact = media_manifest::artifact(session_dir, kind)?;
        validate_status(
            &created,
            call_id,
            kind,
            artifact.as_ref(),
            Some(&checkpoint),
        )?;
        persist_status(session_dir, kind, &created)?;
        status = Some(created);
    }

    let mut status = status.context("media generation status missing after create")?;
    match classify_state(&status.state)? {
        StateClass::Ready => return finish_ready(session_dir, source, &status).await,
        StateClass::Finalizing => {
            return drive_to_ready(
                session_dir,
                call_id,
                backend,
                guard,
                session_id_for_telemetry,
                source,
                status,
            )
            .await;
        }
        StateClass::Terminal => {
            return Err(record_generation_error(
                session_dir,
                kind,
                terminal_error(&status),
            ));
        }
        StateClass::Uploading => {}
    }

    upload_unconfirmed_parts(
        session_dir,
        call_id,
        backend,
        guard,
        session_id_for_telemetry,
        source,
        &status,
    )
    .await?;
    status = fetch_status(
        backend,
        guard,
        session_id_for_telemetry,
        call_id,
        &status.generation_id,
    )
    .await?;
    let artifact = media_manifest::artifact(session_dir, kind)?;
    validate_status(
        &status,
        call_id,
        kind,
        artifact.as_ref(),
        artifact.as_ref().and_then(|item| item.upload.as_ref()),
    )?;
    persist_status(session_dir, kind, &status)?;
    if status.parts.iter().any(|part| !part.confirmed) {
        anyhow::bail!("backend did not confirm every uploaded {kind} part");
    }

    drive_to_ready(
        session_dir,
        call_id,
        backend,
        guard,
        session_id_for_telemetry,
        source,
        status,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn upload_unconfirmed_parts(
    session_dir: &Path,
    call_id: &str,
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    source: &MediaSource,
    status: &UploadStatus,
) -> Result<()> {
    let client = storage_client()?;
    let mut file = tokio::fs::File::open(&source.path)
        .await
        .with_context(|| format!("open {}", source.path.display()))?;
    let mut parts = status.parts.clone();
    parts.sort_by_key(|part| part.part_number);

    for part in parts.iter().filter(|part| !part.confirmed) {
        let length = usize::try_from(part.length_bytes)
            .context("media part length does not fit local address space")?;
        let mut bytes = vec![0u8; length];
        file.seek(std::io::SeekFrom::Start(part.offset_bytes))
            .await
            .context("seek media upload source")?;
        file.read_exact(&mut bytes)
            .await
            .context("read exact media part")?;
        let (sha256, checksum_b64) = digest_bytes(&bytes);
        if let Some(expected) = part.sha256.as_deref() {
            if !expected.eq_ignore_ascii_case(&sha256) {
                anyhow::bail!(
                    "backend part {} hash disagrees with local bytes",
                    part.part_number
                );
            }
        }

        let sign_path = format!(
            "/v1/calls/{call_id}/media/uploads/{}/parts/{}/sign",
            status.generation_id, part.part_number
        );
        let sign_body = serde_json::to_value(SignPartBody {
            offset_bytes: i64::try_from(part.offset_bytes)
                .context("media part offset exceeds backend range")?,
            length_bytes: i32::try_from(part.length_bytes)
                .context("media part length exceeds backend range")?,
            sha256: sha256.clone(),
        })
        .context("serialize media sign-part body")?;
        let signed: SignPartResponse = retry_http(
            backend,
            guard,
            "media_sign_part",
            4,
            session_id_for_telemetry,
            |_attempt| {
                let path = sign_path.clone();
                let body = sign_body.clone();
                async move { post_json::<SignPartResponse>(backend, &path, &body).await }
            },
        )
        .await?;
        if signed.generation_id != status.generation_id
            || signed.part_number != part.part_number
            || signed.expires_in_secs == 0
        {
            anyhow::bail!(
                "backend signed response identity or expiration did not match media part {}",
                part.part_number
            );
        }
        validate_signed_url(&signed.url)?;
        let headers =
            validated_required_headers(&signed.required_headers, part.length_bytes, &checksum_b64)?;
        put_exact_part_with_retry(
            &client,
            &signed.url,
            &headers,
            &bytes,
            session_id_for_telemetry,
        )
        .await?;

        let confirm_path = format!(
            "/v1/calls/{call_id}/media/uploads/{}/parts/{}/confirm",
            status.generation_id, part.part_number
        );
        let empty = serde_json::json!({});
        let confirmed: ConfirmPartResponse = retry_http(
            backend,
            guard,
            "media_confirm_part",
            4,
            session_id_for_telemetry,
            |_attempt| {
                let path = confirm_path.clone();
                let body = empty.clone();
                async move { post_json::<ConfirmPartResponse>(backend, &path, &body).await }
            },
        )
        .await?;
        if confirmed.generation_id != status.generation_id
            || confirmed.part_number != part.part_number
            || !confirmed.confirmed
        {
            anyhow::bail!(
                "backend did not authoritatively confirm media part {}",
                part.part_number
            );
        }

        // A 2xx confirm is the durable backend boundary. Persist immediately;
        // if the process dies before this write, the next GET repairs it.
        let mut confirmed: BTreeSet<u32> =
            media_manifest::artifact(session_dir, source.kind.as_str())?
                .and_then(|item| item.upload)
                .map(|upload| upload.confirmed_parts.into_iter().collect())
                .unwrap_or_default();
        confirmed.insert(part.part_number);
        media_manifest::sync_upload_status(
            session_dir,
            source.kind.as_str(),
            &status.generation_id,
            &status.state,
            &status.object_key,
            &confirmed.into_iter().collect::<Vec<_>>(),
            status.part_count,
            status.is_current,
            status.actual_bytes,
            status.actual_sha256.as_deref(),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_to_ready(
    session_dir: &Path,
    call_id: &str,
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    source: &MediaSource,
    mut status: UploadStatus,
) -> Result<ReadyGeneration> {
    let mut submissions = 0u8;
    let mut poll_index = 0usize;
    loop {
        match classify_state(&status.state)? {
            StateClass::Ready => return finish_ready(session_dir, source, &status).await,
            StateClass::Terminal => {
                return Err(record_generation_error(
                    session_dir,
                    source.kind.as_str(),
                    terminal_error(&status),
                ));
            }
            StateClass::Finalizing => {
                let Some(delay_ms) = FINALIZE_POLL_DELAYS_MS.get(poll_index).copied() else {
                    return Err(record_generation_error(
                        session_dir,
                        source.kind.as_str(),
                        format!(
                            "operation timed out while backend generation remained {}",
                            status.state
                        ),
                    ));
                };
                poll_index += 1;
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let next = fetch_status(
                    backend,
                    guard,
                    session_id_for_telemetry,
                    call_id,
                    &status.generation_id,
                )
                .await?;
                let artifact = media_manifest::artifact(session_dir, source.kind.as_str())?;
                validate_status(
                    &next,
                    call_id,
                    source.kind.as_str(),
                    artifact.as_ref(),
                    artifact.as_ref().and_then(|item| item.upload.as_ref()),
                )?;
                persist_status(session_dir, source.kind.as_str(), &next)?;
                status = next;
            }
            StateClass::Uploading => {
                if status.parts.iter().any(|part| !part.confirmed) {
                    anyhow::bail!("refusing complete while media parts remain unconfirmed");
                }
                if submissions >= 2 {
                    return Err(record_generation_error(
                        session_dir,
                        source.kind.as_str(),
                        "operation timed out while backend generation remained uploading".into(),
                    ));
                }
                submissions += 1;
                let complete_path = format!(
                    "/v1/calls/{call_id}/media/uploads/{}/complete",
                    status.generation_id
                );
                match post_complete(backend, &complete_path, &serde_json::json!({})).await {
                    Ok(next) => {
                        let artifact = media_manifest::artifact(session_dir, source.kind.as_str())?;
                        validate_status(
                            &next,
                            call_id,
                            source.kind.as_str(),
                            artifact.as_ref(),
                            artifact.as_ref().and_then(|item| item.upload.as_ref()),
                        )?;
                        persist_status(session_dir, source.kind.as_str(), &next)?;
                        status = next;
                    }
                    Err(error) if is_ambiguous_complete_error(&error) => {
                        // Ambiguous complete: ask the generation before doing
                        // anything else. A second complete is legal only when
                        // this authoritative GET returns `uploading`.
                        let next = fetch_status(
                            backend,
                            guard,
                            session_id_for_telemetry,
                            call_id,
                            &status.generation_id,
                        )
                        .await?;
                        let artifact = media_manifest::artifact(session_dir, source.kind.as_str())?;
                        validate_status(
                            &next,
                            call_id,
                            source.kind.as_str(),
                            artifact.as_ref(),
                            artifact.as_ref().and_then(|item| item.upload.as_ref()),
                        )?;
                        persist_status(session_dir, source.kind.as_str(), &next)?;
                        status = next;
                    }
                    Err(error) => return Err(error.context("complete media generation")),
                }
            }
        }
    }
}

fn is_ambiguous_complete_error(error: &anyhow::Error) -> bool {
    matches!(
        classify_reqwest_error(error),
        FailureClass::TransientNetwork | FailureClass::BackendFiveXx | FailureClass::AuthExpired
    ) || format!("{error:#}")
        .to_ascii_lowercase()
        .contains("backend 409")
}

async fn finish_ready(
    session_dir: &Path,
    source: &MediaSource,
    status: &UploadStatus,
) -> Result<ReadyGeneration> {
    validate_ready(status)?;
    // A ready checkpoint can be resumed before the local file is opened. If
    // bytes are still present, re-hash them before cleanup so a replaced or
    // corrupted path is never deleted on the strength of an older generation.
    // Missing bytes are valid after a prior ready cleanup and keep this path
    // idempotent across a crash between deletion and pipeline completion.
    match tokio::fs::metadata(&source.path).await {
        Ok(metadata) => {
            if !metadata.is_file() {
                anyhow::bail!("ready media source is no longer a regular file");
            }
            let (bytes, sha256) = hash_file(&source.path).await?;
            if bytes != status.declared_bytes
                || !sha256.eq_ignore_ascii_case(&status.declared_sha256)
            {
                return Err(record_generation_error(
                    session_dir,
                    source.kind.as_str(),
                    "local media source changed before ready cleanup".into(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect ready media source before cleanup {}",
                    source.path.display()
                )
            });
        }
    }
    persist_status(session_dir, source.kind.as_str(), status)?;
    media_manifest::acknowledge_ready(session_dir, source.kind.as_str(), &status.generation_id)?;
    Ok(ReadyGeneration {
        generation_id: status.generation_id.clone(),
    })
}

fn persist_status(session_dir: &Path, kind: &str, status: &UploadStatus) -> Result<()> {
    let confirmed = status
        .parts
        .iter()
        .filter(|part| part.confirmed)
        .map(|part| part.part_number)
        .collect::<Vec<_>>();
    media_manifest::sync_upload_status(
        session_dir,
        kind,
        &status.generation_id,
        &status.state,
        &status.object_key,
        &confirmed,
        status.part_count,
        status.is_current,
        status.actual_bytes,
        status.actual_sha256.as_deref(),
    )
}

fn validate_status(
    status: &UploadStatus,
    call_id: &str,
    kind: &str,
    artifact: Option<&media_manifest::ArtifactCheckpoint>,
    checkpoint: Option<&UploadCheckpoint>,
) -> Result<()> {
    uuid::Uuid::parse_str(&status.generation_id)
        .context("backend returned invalid media generation id")?;
    if status.call_id != call_id {
        anyhow::bail!("backend media generation call id mismatch");
    }
    if status.kind != kind {
        anyhow::bail!("backend media generation kind mismatch");
    }
    if status.object_key.trim().is_empty() {
        anyhow::bail!("backend returned an empty media object key");
    }
    validate_sha256_hex(&status.declared_sha256)?;
    if let Some(artifact) = artifact {
        if artifact.byte_size != Some(status.declared_bytes) {
            anyhow::bail!("backend media generation declared byte count mismatch");
        }
    }
    if let Some(checkpoint) = checkpoint {
        if !checkpoint
            .declared_sha256
            .eq_ignore_ascii_case(&status.declared_sha256)
        {
            anyhow::bail!("backend media generation declared hash mismatch");
        }
        if let Some(expected_generation) = checkpoint.generation_id.as_deref() {
            if expected_generation != status.generation_id {
                anyhow::bail!("backend media generation id changed during resume");
            }
        }
        if let Some(expected_object_key) = checkpoint.object_key.as_deref() {
            if expected_object_key != status.object_key {
                anyhow::bail!("backend media generation object key changed during resume");
            }
        }
        let server_confirmed = status
            .parts
            .iter()
            .filter(|part| part.confirmed)
            .map(|part| part.part_number)
            .collect::<BTreeSet<_>>();
        if checkpoint
            .confirmed_parts
            .iter()
            .any(|part| !server_confirmed.contains(part))
        {
            anyhow::bail!("backend regressed a durably confirmed media part");
        }
    }
    classify_state(&status.state)?;
    validate_part_plan(status)?;
    Ok(())
}

fn validate_part_plan(status: &UploadStatus) -> Result<()> {
    if status.declared_bytes == 0 {
        anyhow::bail!("backend media generation declared zero bytes");
    }
    if status.part_size_bytes == 0 || status.part_size_bytes > MAX_PART_BYTES {
        anyhow::bail!("backend media part size is outside local safety bounds");
    }
    if status.part_count == 0 || status.part_count > MAX_PART_COUNT {
        anyhow::bail!("backend media part count is outside local safety bounds");
    }
    if status.parts.len() != status.part_count as usize {
        anyhow::bail!("backend media part list/count mismatch");
    }
    let expected_part_count = status.declared_bytes.div_ceil(status.part_size_bytes);
    if u64::from(status.part_count) != expected_part_count {
        anyhow::bail!("backend media part count does not match exact part layout");
    }
    let mut parts = status.parts.clone();
    parts.sort_by_key(|part| part.part_number);
    let mut expected_offset = 0u64;
    for (index, part) in parts.iter().enumerate() {
        let expected_number = u32::try_from(index + 1).context("media part number overflow")?;
        if part.part_number != expected_number {
            anyhow::bail!("backend media parts are duplicated or non-contiguous");
        }
        if part.offset_bytes != expected_offset {
            anyhow::bail!("backend media part offsets contain a gap or overlap");
        }
        if part.length_bytes == 0 || part.length_bytes > MAX_PART_BYTES {
            anyhow::bail!("backend media part length is outside local safety bounds");
        }
        let remaining = status.declared_bytes.saturating_sub(expected_offset);
        let expected_length = remaining.min(status.part_size_bytes);
        if part.length_bytes != expected_length {
            anyhow::bail!("backend media part length does not match exact part layout");
        }
        expected_offset = expected_offset
            .checked_add(part.length_bytes)
            .context("backend media part range overflow")?;
        if let Some(sha256) = part.sha256.as_deref() {
            validate_sha256_hex(sha256)?;
        }
    }
    if expected_offset != status.declared_bytes {
        anyhow::bail!("backend media part coverage does not match declared bytes");
    }
    Ok(())
}

fn validate_ready(status: &UploadStatus) -> Result<()> {
    if status.state != "ready" || !status.is_current {
        anyhow::bail!("media generation is not authoritative ready/current");
    }
    if status.actual_bytes != Some(status.declared_bytes) {
        anyhow::bail!("ready media generation actual byte count mismatch");
    }
    let actual_sha256 = status
        .actual_sha256
        .as_deref()
        .context("ready media generation omitted actual sha256")?;
    validate_sha256_hex(actual_sha256)?;
    if !actual_sha256.eq_ignore_ascii_case(&status.declared_sha256) {
        anyhow::bail!("ready media generation actual sha256 mismatch");
    }
    if status.parts.iter().any(|part| !part.confirmed) {
        anyhow::bail!("ready media generation contains unconfirmed parts");
    }
    Ok(())
}

fn terminal_error(status: &UploadStatus) -> String {
    match status.error_code.as_deref() {
        Some(code) if !code.trim().is_empty() => {
            format!(
                "media generation became terminal: {} ({code})",
                status.state
            )
        }
        _ => format!("media generation became terminal: {}", status.state),
    }
}

fn record_generation_error(session_dir: &Path, kind: &str, error: String) -> anyhow::Error {
    match media_manifest::mark_upload_error(session_dir, kind, error.clone()) {
        Ok(()) => anyhow!(error),
        Err(checkpoint_error) => anyhow!(
            "{error}; additionally failed to persist media checkpoint: {checkpoint_error:#}"
        ),
    }
}

/// Delete local source bytes only after the caller has established its own
/// aggregate completion boundary. Audio upload waits until every recorded
/// track is ready; screen upload invokes this after its one generation is
/// ready. The per-generation ready helper deliberately does not clean up.
pub(crate) fn cleanup_ready_source(session_dir: &Path, source: &MediaSource) {
    let mut paths = BTreeSet::new();
    paths.insert(source.path.clone());
    if let Some(raw) = source.raw_path.as_ref() {
        paths.insert(raw.clone());
    }
    if source.kind == MediaKind::Screen {
        paths.insert(session_dir.join("screen").join("recording.json"));
        paths.insert(session_dir.join("screen").join("recording_fs.mp4"));
    } else {
        paths.insert(session_dir.join(format!("{}.wav", source.kind.as_str())));
        paths.insert(session_dir.join(format!("{}.opus", source.kind.as_str())));
    }
    for path in paths {
        let path = match validated_session_path(session_dir, &path) {
            Ok(path) => path,
            Err(error) => {
                eprintln!(
                    "aftercalls: refusing ready-media cleanup outside {}: {error:#}",
                    session_dir.display()
                );
                continue;
            }
        };
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "aftercalls: ready media local cleanup failed for {}: {error}",
                    path.display()
                );
            }
        }
    }
}

async fn fetch_status(
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    call_id: &str,
    generation_id: &str,
) -> Result<UploadStatus> {
    validate_call_id(call_id)?;
    uuid::Uuid::parse_str(generation_id).context("invalid checkpointed media generation id")?;
    let path = format!("/v1/calls/{call_id}/media/uploads/{generation_id}");
    retry_http(
        backend,
        guard,
        "media_upload_status",
        4,
        session_id_for_telemetry,
        |_attempt| {
            let path = path.clone();
            async move { get_json::<UploadStatus>(backend, &path).await }
        },
    )
    .await
}

pub async fn abort_generation(
    backend: &Backend,
    guard: &RetryGuard,
    session_id_for_telemetry: Option<&str>,
    call_id: &str,
    kind: &str,
    generation_id: &str,
    reason: Option<&str>,
) -> Result<()> {
    validate_call_id(call_id)?;
    if !matches!(kind, "mic" | "system" | "screen") {
        anyhow::bail!("unsupported media kind {kind:?}");
    }
    if reason
        .map(|reason| reason.len() > 128 || reason.chars().any(char::is_control))
        .unwrap_or(false)
    {
        anyhow::bail!("media abort reason must be at most 128 printable characters");
    }
    uuid::Uuid::parse_str(generation_id).context("invalid checkpointed media generation id")?;
    let path = format!("/v1/calls/{call_id}/media/uploads/{generation_id}/abort");
    let body = serde_json::to_value(AbortBody { reason }).context("serialize media abort body")?;
    let result = retry_http(
        backend,
        guard,
        "media_upload_abort",
        4,
        session_id_for_telemetry,
        |_attempt| {
            let path = path.clone();
            let body = body.clone();
            async move { post_no_content(backend, &path, &body).await }
        },
    )
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(error) if is_ambiguous_abort_error(&error) => {
            // If the abort committed but its response was lost, a replay can
            // legitimately return 409. GET the immutable generation and treat
            // any terminal state as proof that no active multipart remains.
            let status = fetch_status(
                backend,
                guard,
                session_id_for_telemetry,
                call_id,
                generation_id,
            )
            .await?;
            validate_status(&status, call_id, kind, None, None)?;
            if status.generation_id != generation_id {
                anyhow::bail!("backend abort status generation id mismatch");
            }
            match classify_state(&status.state)? {
                StateClass::Terminal => Ok(()),
                StateClass::Uploading | StateClass::Finalizing | StateClass::Ready => Err(error
                    .context(format!(
                        "abort media generation remained {} after authoritative GET",
                        status.state
                    ))),
            }
        }
        Err(error) => Err(error),
    }
}

fn is_ambiguous_abort_error(error: &anyhow::Error) -> bool {
    matches!(
        classify_reqwest_error(error),
        FailureClass::TransientNetwork | FailureClass::BackendFiveXx
    ) || format!("{error:#}")
        .to_ascii_lowercase()
        .contains("backend 409")
}

/// Explicit user discard must release any still-active backend multipart
/// generations before the local checkpoint containing their ids is removed.
pub async fn abort_checkpointed_generations(session_dir: &Path, reason: &str) -> Result<()> {
    let Some(manifest) = media_manifest::read(session_dir)? else {
        return Ok(());
    };
    let Some(call_id) = manifest.call_id.as_deref() else {
        return Ok(());
    };
    let mut generations = Vec::new();
    for (kind, artifact) in manifest
        .audio
        .iter()
        .map(|(kind, artifact)| (kind.as_str(), artifact))
        .chain(manifest.screen.iter().map(|artifact| ("screen", artifact)))
    {
        if artifact.state == ArtifactState::ReadyAcknowledged {
            continue;
        }
        if let Some(upload) = artifact.upload.as_ref() {
            // Only a known durable/terminal state proves there is no active
            // multipart to release. Treat missing, legacy, or unknown state
            // conservatively (including a crash-persisted `allocated` row).
            let needs_abort = !matches!(
                upload.backend_state.as_deref(),
                Some("ready" | "failed" | "aborted" | "superseded" | "deleting" | "deleted")
            );
            if needs_abort {
                if let Some(generation_id) = upload.generation_id.as_ref() {
                    generations.push((kind.to_string(), generation_id.clone()));
                }
            }
        }
    }
    if generations.is_empty() {
        return Ok(());
    }
    let config = Config::load()?;
    let backend = config
        .backend
        .as_ref()
        .context("no backend configured for media abort")?;
    let guard = RetryGuard::new();
    let session_label = session_dir.to_string_lossy().into_owned();
    for (kind, generation_id) in generations {
        abort_generation(
            backend,
            &guard,
            Some(session_label.as_str()),
            call_id,
            &kind,
            &generation_id,
            Some(reason),
        )
        .await?;
        // Persist each release before moving to the next generation. If a
        // later abort fails, a future discard will not replay an already
        // terminal generation and get stuck behind its 409.
        media_manifest::reset_aborted_upload(
            session_dir,
            &kind,
            format!("backend generation {generation_id} released for explicit discard"),
        )?;
    }
    Ok(())
}

async fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open upload source {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("hash upload source {}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .context("upload source size overflow")?;
        hasher.update(&buffer[..read]);
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

fn digest_bytes(bytes: &[u8]) -> (String, String) {
    let digest = Sha256::digest(bytes);
    (
        format!("{digest:x}"),
        base64::engine::general_purpose::STANDARD.encode(digest),
    )
}

fn validate_sha256_hex(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid sha256 hex digest");
    }
    Ok(())
}

fn validate_operation_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        anyhow::bail!("client operation id must be printable ASCII, 1..=160 bytes");
    }
    Ok(())
}

fn validate_call_id(call_id: &str) -> Result<()> {
    uuid::Uuid::parse_str(call_id).context("invalid call id")?;
    Ok(())
}

fn validated_session_path(session_dir: &Path, path: &Path) -> Result<PathBuf> {
    let session = std::fs::canonicalize(session_dir)
        .with_context(|| format!("resolve media session {}", session_dir.display()))?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.join(path)
    };
    let name = candidate
        .file_name()
        .context("media source path has no file name")?;
    let parent = candidate
        .parent()
        .context("media source path has no parent")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("resolve media source parent {}", parent.display()))?;
    if !parent.starts_with(&session) {
        anyhow::bail!("media source resolves outside its session directory");
    }
    let candidate = parent.join(name);
    if candidate.exists() {
        let resolved = std::fs::canonicalize(&candidate)
            .with_context(|| format!("resolve media source {}", candidate.display()))?;
        if !resolved.starts_with(&session) {
            anyhow::bail!("media source resolves outside its session directory");
        }
        Ok(resolved)
    } else {
        Ok(candidate)
    }
}

fn validated_required_headers(
    required: &HashMap<String, String>,
    length: u64,
    checksum_b64: &str,
) -> Result<reqwest::header::HeaderMap> {
    if required.len() != 2 {
        anyhow::bail!("signed media response returned an unexpected required-header set");
    }
    let mut headers = reqwest::header::HeaderMap::new();
    let mut normalized = HashMap::new();
    for (name, value) in required {
        let lower = name.to_ascii_lowercase();
        if normalized.insert(lower.clone(), value.as_str()).is_some() {
            anyhow::bail!("signed media response contains duplicate required header {lower}");
        }
        if matches!(
            lower.as_str(),
            "authorization" | "cookie" | "proxy-authorization"
        ) {
            anyhow::bail!("signed media response requested a forbidden credential header");
        }
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .context("invalid signed media header name")?;
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .context("invalid signed media header value")?;
        headers.insert(header_name, header_value);
    }
    let expected_length = length.to_string();
    if normalized.get("content-length").copied() != Some(expected_length.as_str()) {
        anyhow::bail!("signed media response omitted the exact content-length");
    }
    if normalized.get("x-amz-checksum-sha256").copied() != Some(checksum_b64) {
        anyhow::bail!("signed media response omitted the exact checksum header");
    }
    Ok(headers)
}

pub(crate) fn validate_signed_url(value: &str) -> Result<()> {
    if value.len() > 8_192 || value.chars().any(char::is_control) {
        anyhow::bail!("invalid signed media URL");
    }
    let url = reqwest::Url::parse(value).context("invalid signed media URL")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("signed media URL must include a host"))?;
    match url.scheme() {
        "https" => {}
        "http" => {
            // `Url::host_str` preserves brackets around IPv6 literals, while
            // `IpAddr::from_str` expects the bare address. Keep this in sync
            // with the native-download validator in `ipc_security`.
            let normalized_host = host
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(host);
            let non_loopback = normalized_host != "localhost"
                && normalized_host
                    .parse::<std::net::IpAddr>()
                    .map(|ip| !ip.is_loopback())
                    .unwrap_or(true);
            if non_loopback {
                anyhow::bail!("plain HTTP signed media URLs are limited to local development");
            }
        }
        _ => anyhow::bail!("signed media URL must use HTTPS"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("signed media URL must not contain userinfo credentials");
    }
    if url.fragment().is_some() {
        anyhow::bail!("signed media URL must not contain a fragment");
    }
    Ok(())
}

async fn put_exact_part(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    bytes: Vec<u8>,
) -> Result<()> {
    let response = client
        .put(url)
        .headers(headers)
        .body(bytes)
        .send()
        .await
        .context("PUT exact media part")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = read_limited_response(response, MAX_ERROR_BODY_BYTES).await?;
        anyhow::bail!("PUT returned {status}: {}", String::from_utf8_lossy(&body));
    }
    Ok(())
}

async fn put_exact_part_with_retry(
    client: &reqwest::Client,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    bytes: &[u8],
    session_id_for_telemetry: Option<&str>,
) -> Result<()> {
    const BACKOFF_MS: [u64; 3] = [2_000, 8_000, 30_000];
    for attempt in 1u8..=4 {
        match put_exact_part(client, url, headers.clone(), bytes.to_vec()).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let class = classify_reqwest_error(&error);
                if !matches!(
                    class,
                    FailureClass::TransientNetwork | FailureClass::BackendFiveXx
                ) || attempt == 4
                {
                    return Err(
                        error.context(format!("media_put_part failed after {attempt} attempt(s)"))
                    );
                }
                let base = BACKOFF_MS[usize::from(attempt - 1)];
                let wait_ms = (base as f64 * rand::thread_rng().gen_range(0.8..1.2)) as u64;
                crate::telemetry::log(
                    "debug",
                    "pipeline::retry",
                    format!("media_put_part attempt {attempt} failed; retrying"),
                    Some(serde_json::json!({
                        "step": "media_put_part",
                        "attempt": attempt,
                        "failure_class": class,
                        "wait_ms": wait_ms,
                    })),
                    session_id_for_telemetry.map(str::to_string),
                );
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }
    }
    unreachable!("bounded media PUT retry loop always returns")
}

async fn get_json<T: DeserializeOwned>(backend: &Backend, path: &str) -> Result<T> {
    let auth = build_auth_header(backend).await?;
    let client = backend_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("authorization", auth)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    decode_json_response(response, "GET", &url).await
}

async fn post_json<T: DeserializeOwned>(
    backend: &Backend,
    path: &str,
    body: &serde_json::Value,
) -> Result<T> {
    let auth = build_auth_header(backend).await?;
    let client = backend_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .header("authorization", auth)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    decode_json_response(response, "POST", &url).await
}

/// Creation is idempotent by `client_operation_id`. A replay of an operation
/// which became terminal while the original response was ambiguous is returned
/// by the backend as `409` with the authoritative status body. Preserve that
/// status so the caller can checkpoint the terminal state instead of retrying
/// forever or allocating a different operation.
async fn post_create_upload(
    backend: &Backend,
    path: &str,
    body: &serde_json::Value,
) -> Result<UploadStatus> {
    let auth = build_auth_header(backend).await?;
    let client = backend_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .header("authorization", auth)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    let accepted_success = matches!(
        status,
        reqwest::StatusCode::OK | reqwest::StatusCode::CREATED
    );
    let accepted_terminal_replay = status == reqwest::StatusCode::CONFLICT;
    if !accepted_success && !accepted_terminal_replay {
        let response_body = read_limited_response(response, MAX_ERROR_BODY_BYTES).await?;
        anyhow::bail!(
            "backend {status}: {}",
            String::from_utf8_lossy(&response_body)
        );
    }
    let response_body = read_limited_response(response, MAX_BACKEND_JSON_BYTES)
        .await
        .context("read media upload create response")?;
    match serde_json::from_slice::<UploadStatus>(&response_body) {
        Ok(status)
            if accepted_terminal_replay
                && classify_state(&status.state).ok() != Some(StateClass::Terminal) =>
        {
            anyhow::bail!("backend 409 media upload create response was not a terminal generation")
        }
        Ok(status) => Ok(status),
        Err(error) if accepted_terminal_replay => anyhow::bail!(
            "backend 409 media upload create response was not an authoritative status: {error}"
        ),
        Err(error) => Err(error).context("decode media upload create response"),
    }
}

async fn post_complete(
    backend: &Backend,
    path: &str,
    body: &serde_json::Value,
) -> Result<UploadStatus> {
    let auth = build_auth_header(backend).await?;
    let client = backend_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .header("authorization", auth)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if response.status() != reqwest::StatusCode::ACCEPTED {
        let status = response.status();
        let response_body = read_limited_response(response, MAX_ERROR_BODY_BYTES).await?;
        anyhow::bail!(
            "backend {status}: {}",
            String::from_utf8_lossy(&response_body)
        );
    }
    let response_body = read_limited_response(response, MAX_BACKEND_JSON_BYTES)
        .await
        .context("read media upload complete response")?;
    serde_json::from_slice(&response_body).context("decode media upload complete response")
}

async fn post_no_content(backend: &Backend, path: &str, body: &serde_json::Value) -> Result<()> {
    let auth = build_auth_header(backend).await?;
    let client = backend_client()?;
    let url = format!("{}{path}", backend.url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .header("authorization", auth)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if response.status() != reqwest::StatusCode::NO_CONTENT {
        let status = response.status();
        let body = read_limited_response(response, MAX_ERROR_BODY_BYTES).await?;
        anyhow::bail!("backend {status}: {}", String::from_utf8_lossy(&body));
    }
    Ok(())
}

async fn decode_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
    method: &str,
    url: &str,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let body = read_limited_response(response, MAX_ERROR_BODY_BYTES).await?;
        anyhow::bail!("backend {status}: {}", String::from_utf8_lossy(&body));
    }
    let body = read_limited_response(response, MAX_BACKEND_JSON_BYTES)
        .await
        .with_context(|| format!("read {method} {url} response"))?;
    serde_json::from_slice(&body).with_context(|| format!("decode {method} {url} response"))
}

async fn read_limited_response(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        anyhow::bail!("backend response exceeds {limit} byte limit");
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read backend response chunk")?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            anyhow::bail!("backend response exceeds {limit} byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn backend_client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let built = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(user_agent())
        .build()?;
    let _ = CLIENT.set(built.clone());
    Ok(CLIENT.get().cloned().unwrap_or(built))
}

fn storage_client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let built = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(user_agent())
        .build()?;
    let _ = CLIENT.set(built.clone());
    Ok(CLIENT.get().cloned().unwrap_or(built))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL_ID: &str = "4da6e5c4-7ac1-45bb-bab6-8e269a2664c2";
    const GENERATION_ID: &str = "9a0b77df-4391-407c-a1fb-b12bdaefa5dd";
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn part(number: u32, offset: u64, length: u64) -> UploadPartStatus {
        UploadPartStatus {
            part_number: number,
            offset_bytes: offset,
            length_bytes: length,
            sha256: None,
            signed: false,
            confirmed: false,
        }
    }

    fn status(parts: Vec<UploadPartStatus>) -> UploadStatus {
        UploadStatus {
            generation_id: GENERATION_ID.into(),
            call_id: CALL_ID.into(),
            kind: "mic".into(),
            object_key: "calls/call/mic.opus".into(),
            state: "uploading".into(),
            is_current: true,
            declared_bytes: 10,
            declared_sha256: HASH.into(),
            actual_bytes: None,
            actual_sha256: None,
            part_size_bytes: 6,
            part_count: parts.len() as u32,
            parts,
            error_code: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    #[test]
    fn part_plan_requires_exact_contiguous_coverage() {
        assert!(validate_part_plan(&status(vec![part(1, 0, 6), part(2, 6, 4)])).is_ok());
        assert!(validate_part_plan(&status(vec![part(1, 0, 6), part(2, 7, 3)])).is_err());
        assert!(validate_part_plan(&status(vec![part(1, 0, 6), part(3, 6, 4)])).is_err());
        assert!(validate_part_plan(&status(vec![part(1, 0, 6), part(2, 5, 5)])).is_err());
        assert!(validate_part_plan(&status(vec![part(1, 0, 5), part(2, 5, 5)])).is_err());
    }

    #[test]
    fn ready_requires_current_and_matching_actual_evidence() {
        let mut ready = status(vec![part(1, 0, 6), part(2, 6, 4)]);
        ready.state = "ready".into();
        ready
            .parts
            .iter_mut()
            .for_each(|part| part.confirmed = true);
        ready.actual_bytes = Some(10);
        ready.actual_sha256 = Some(HASH.into());
        assert!(validate_ready(&ready).is_ok());
        ready.is_current = false;
        assert!(validate_ready(&ready).is_err());
        ready.is_current = true;
        ready.actual_sha256 =
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
        assert!(validate_ready(&ready).is_err());
    }

    #[test]
    fn signed_headers_must_bind_exact_length_and_checksum() {
        let (_, checksum) = digest_bytes(b"hello");
        let required = HashMap::from([
            ("content-length".into(), "5".into()),
            ("x-amz-checksum-sha256".into(), checksum.clone()),
        ]);
        assert!(validated_required_headers(&required, 5, &checksum).is_ok());
        assert!(validated_required_headers(&required, 4, &checksum).is_err());
        let forbidden = HashMap::from([
            ("content-length".into(), "5".into()),
            ("x-amz-checksum-sha256".into(), checksum.clone()),
            ("authorization".into(), "secret".into()),
        ]);
        assert!(validated_required_headers(&forbidden, 5, &checksum).is_err());
        let unexpected = HashMap::from([
            ("content-length".into(), "5".into()),
            ("x-amz-checksum-sha256".into(), checksum.clone()),
            ("x-unexpected".into(), "value".into()),
        ]);
        assert!(validated_required_headers(&unexpected, 5, &checksum).is_err());
    }

    #[test]
    fn signed_url_is_http_without_embedded_credentials() {
        assert!(validate_signed_url("https://objects.example.test/part?signature=ok").is_ok());
        assert!(validate_signed_url("http://127.0.0.1:9000/bucket/key").is_ok());
        assert!(validate_signed_url("http://[::1]:9000/bucket/key").is_ok());
        assert!(validate_signed_url("http://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_signed_url("file:///tmp/leak").is_err());
        assert!(validate_signed_url("https://user:password@example.test/part").is_err());
    }

    #[test]
    fn media_source_path_cannot_escape_its_session() {
        let root = std::env::temp_dir().join(format!(
            "aftercalls-media-path-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let session = root.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let inside = session.join("mic.opus");
        let outside = root.join("outside.opus");
        std::fs::write(&inside, b"inside").unwrap();
        std::fs::write(&outside, b"outside").unwrap();

        assert_eq!(
            validated_session_path(&session, &inside).unwrap(),
            std::fs::canonicalize(&inside).unwrap()
        );
        assert!(validated_session_path(&session, &outside).is_err());
        assert!(validated_session_path(&session, Path::new("../outside.opus")).is_err());

        #[cfg(unix)]
        {
            let escaped_link = session.join("escaped.opus");
            std::os::unix::fs::symlink(&outside, &escaped_link).unwrap();
            assert!(validated_session_path(&session, &escaped_link).is_err());
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn operation_id_and_screen_metadata_validation_are_strict() {
        assert!(validate_operation_id("media-mic-123").is_ok());
        assert!(validate_operation_id("").is_err());
        assert!(validate_operation_id(&"x".repeat(161)).is_err());
        assert!(validate_operation_id("line\nbreak").is_err());

        let source = MediaSource::screen(
            PathBuf::from("screen.mp4"),
            PathBuf::from("raw.mp4"),
            1_000,
            1920,
            1080,
            15.0,
            0,
        );
        assert!(source.validate().is_ok());
        let mut invalid = source;
        invalid.fps = Some(f64::NAN);
        assert!(invalid.validate().is_err());

        let mut invalid_duration = MediaSource::screen(
            PathBuf::from("screen.mp4"),
            PathBuf::from("raw.mp4"),
            0,
            1920,
            1080,
            15.0,
            0,
        );
        assert!(invalid_duration.validate().is_err());
        invalid_duration.duration_ms = Some(1_000);
        invalid_duration.start_offset_ms = Some(-1);
        assert!(invalid_duration.validate().is_err());

        let resume =
            MediaSource::resume_screen(PathBuf::from("screen.mp4"), PathBuf::from("raw.mp4"));
        assert!(resume.validate().is_ok());
    }

    #[tokio::test]
    async fn cleanup_waits_for_caller_after_exact_ready_evidence_is_durable() {
        let session = std::env::temp_dir().join(format!(
            "aftercalls-ready-cleanup-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&session).unwrap();
        let opus = session.join("mic.opus");
        let wav = session.join("mic.wav");
        let bytes = b"0123456789";
        std::fs::write(&opus, bytes).unwrap();
        std::fs::write(&wav, b"raw").unwrap();
        let (declared_sha256, _) = digest_bytes(bytes);
        media_manifest::prepare_upload(
            &session,
            CALL_ID,
            "mic",
            Some(&wav),
            &opus,
            bytes.len() as u64,
            &declared_sha256,
        )
        .unwrap();
        let mut ready = status(vec![part(1, 0, 6), part(2, 6, 4)]);
        ready.declared_sha256 = declared_sha256.clone();
        ready.state = "ready".into();
        ready.is_current = true;
        ready.actual_bytes = Some(bytes.len() as u64);
        ready.actual_sha256 = Some(declared_sha256);
        ready
            .parts
            .iter_mut()
            .for_each(|part| part.confirmed = true);
        let source = MediaSource::audio(MediaKind::Mic, opus.clone(), wav.clone()).unwrap();

        finish_ready(&session, &source, &ready).await.unwrap();
        assert!(opus.exists());
        assert!(wav.exists());
        cleanup_ready_source(&session, &source);
        assert!(!opus.exists());
        assert!(!wav.exists());
        assert_eq!(
            media_manifest::artifact(&session, "mic")
                .unwrap()
                .unwrap()
                .state,
            ArtifactState::ReadyAcknowledged
        );
        let _ = std::fs::remove_dir_all(session);
    }

    #[test]
    fn cleanup_refuses_a_source_outside_the_bound_session() {
        let root = std::env::temp_dir().join(format!(
            "aftercalls-cleanup-fence-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let session = root.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let outside = root.join("outside.opus");
        let wav = session.join("mic.wav");
        std::fs::write(&outside, b"must survive").unwrap();
        std::fs::write(&wav, b"local raw").unwrap();
        let source = MediaSource::audio(MediaKind::Mic, outside.clone(), wav).unwrap();

        cleanup_ready_source(&session, &source);

        assert!(outside.exists());
        assert!(!session.join("mic.wav").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mismatched_ready_evidence_never_deletes_local_source() {
        let session = std::env::temp_dir().join(format!(
            "aftercalls-ready-retain-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&session).unwrap();
        let opus = session.join("mic.opus");
        let wav = session.join("mic.wav");
        std::fs::write(&opus, b"0123456789").unwrap();
        std::fs::write(&wav, b"raw").unwrap();
        let mut ready = status(vec![part(1, 0, 6), part(2, 6, 4)]);
        ready.state = "ready".into();
        ready.is_current = true;
        ready.actual_bytes = Some(10);
        ready.actual_sha256 =
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
        ready
            .parts
            .iter_mut()
            .for_each(|part| part.confirmed = true);
        let source = MediaSource::audio(MediaKind::Mic, opus.clone(), wav.clone()).unwrap();

        assert!(finish_ready(&session, &source, &ready).await.is_err());
        assert!(opus.exists());
        assert!(wav.exists());
        let _ = std::fs::remove_dir_all(session);
    }

    #[tokio::test]
    async fn changed_local_source_never_deletes_on_older_ready_generation() {
        let session = std::env::temp_dir().join(format!(
            "aftercalls-ready-local-mismatch-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&session).unwrap();
        let opus = session.join("mic.opus");
        let wav = session.join("mic.wav");
        let original = b"0123456789";
        let changed = b"abcdefghij";
        std::fs::write(&opus, original).unwrap();
        std::fs::write(&wav, b"raw").unwrap();
        let (declared_sha256, _) = digest_bytes(original);
        media_manifest::prepare_upload(
            &session,
            CALL_ID,
            "mic",
            Some(&wav),
            &opus,
            original.len() as u64,
            &declared_sha256,
        )
        .unwrap();
        std::fs::write(&opus, changed).unwrap();

        let mut ready = status(vec![part(1, 0, 6), part(2, 6, 4)]);
        ready.declared_sha256 = declared_sha256.clone();
        ready.state = "ready".into();
        ready.is_current = true;
        ready.actual_bytes = Some(original.len() as u64);
        ready.actual_sha256 = Some(declared_sha256);
        ready
            .parts
            .iter_mut()
            .for_each(|part| part.confirmed = true);
        let source = MediaSource::audio(MediaKind::Mic, opus.clone(), wav.clone()).unwrap();

        assert!(finish_ready(&session, &source, &ready).await.is_err());
        assert_eq!(std::fs::read(&opus).unwrap(), changed);
        assert!(wav.exists());
        assert_ne!(
            media_manifest::artifact(&session, "mic")
                .unwrap()
                .unwrap()
                .state,
            ArtifactState::ReadyAcknowledged
        );
        let _ = std::fs::remove_dir_all(session);
    }
}
