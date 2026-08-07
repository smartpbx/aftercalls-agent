//! Durable, versioned local media checkpoint.
//!
//! The checkpoint carries the backend media-generation identity as well as the
//! local artifact state. That lets a restarted agent resume confirmed parts,
//! distinguish "complete requested" from authoritative `ready`, and retain the
//! only local source until the backend acknowledges the exact generation/hash.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const MANIFEST_FILENAME: &str = "media-state.json";
const MANIFEST_VERSION: u32 = 2;
const OLDEST_SUPPORTED_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    Recording,
    RawReady,
    EncodingFailed,
    Published,
    UploadPending,
    UploadedAwaitingBackendReady,
    ReadyAcknowledged,
    NotPresent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadCheckpoint {
    /// Stable across retries/restarts for one immutable local source.
    pub client_operation_id: String,
    /// Present after the create/replay response is durably checkpointed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    pub declared_sha256: String,
    #[serde(default)]
    pub confirmed_parts: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_current: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCheckpoint {
    pub state: ArtifactState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<UploadCheckpoint>,
}

impl ArtifactCheckpoint {
    fn recording(raw_path: &str) -> Self {
        Self {
            state: ArtifactState::Recording,
            raw_path: Some(raw_path.into()),
            published_path: None,
            byte_size: None,
            last_error: None,
            upload: None,
        }
    }
}

fn has_backend_ownership(checkpoint: &ArtifactCheckpoint) -> bool {
    matches!(
        checkpoint.state,
        ArtifactState::UploadedAwaitingBackendReady | ArtifactState::ReadyAcknowledged
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalMediaManifest {
    pub version: u32,
    pub session_generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default)]
    pub pipeline_complete: bool,
    pub audio: BTreeMap<String, ArtifactCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<ArtifactCheckpoint>,
    pub updated_at: String,
}

impl LocalMediaManifest {
    fn new() -> Self {
        let mut audio = BTreeMap::new();
        audio.insert("mic".into(), ArtifactCheckpoint::recording("mic.wav"));
        audio.insert("system".into(), ArtifactCheckpoint::recording("system.wav"));
        Self {
            version: MANIFEST_VERSION,
            session_generation: uuid::Uuid::new_v4().to_string(),
            call_id: None,
            pipeline_complete: false,
            audio,
            screen: None,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    /// A completed backend call can still have a failed screen/audio upload.
    /// `UploadedAwaitingBackendReady` is retryable too: a recovery run polls
    /// its immutable generation and uploads only parts the backend still marks
    /// unconfirmed. It never blindly starts a second generation.
    pub fn has_retryable_media(&self) -> bool {
        self.audio.values().any(|a| {
            matches!(
                a.state,
                ArtifactState::Recording
                    | ArtifactState::RawReady
                    | ArtifactState::EncodingFailed
                    | ArtifactState::Published
                    | ArtifactState::UploadPending
                    | ArtifactState::UploadedAwaitingBackendReady
            )
        }) || self
            .screen
            .as_ref()
            .map(|s| {
                matches!(
                    s.state,
                    ArtifactState::Recording
                        | ArtifactState::RawReady
                        | ArtifactState::EncodingFailed
                        | ArtifactState::Published
                        | ArtifactState::UploadPending
                        | ArtifactState::UploadedAwaitingBackendReady
                )
            })
            .unwrap_or(false)
    }

    pub fn has_unacknowledged_media(&self) -> bool {
        self.audio.values().any(|a| {
            !matches!(
                a.state,
                ArtifactState::ReadyAcknowledged | ArtifactState::NotPresent
            )
        }) || self
            .screen
            .as_ref()
            .map(|s| {
                !matches!(
                    s.state,
                    ArtifactState::ReadyAcknowledged | ArtifactState::NotPresent
                )
            })
            .unwrap_or(false)
    }

    /// Media the *client* still owes bytes for. Distinct from
    /// [`Self::has_unacknowledged_media`]: `UploadedAwaitingBackendReady` means
    /// every byte is stored and the backend is validating, so nothing is left
    /// for this run to do even though the artifact is not acknowledged yet.
    ///
    /// Only this weaker predicate gates pipeline completion. The strict one
    /// still governs recovery terminality and the local-cleanup boundary, so a
    /// finalizing generation is revisited and driven to ready later.
    pub fn has_client_pending_media(&self) -> bool {
        fn pending(state: ArtifactState) -> bool {
            matches!(
                state,
                ArtifactState::Recording
                    | ArtifactState::RawReady
                    | ArtifactState::EncodingFailed
                    | ArtifactState::Published
                    | ArtifactState::UploadPending
            )
        }
        self.audio.values().any(|a| pending(a.state))
            || self.screen.as_ref().map(|s| pending(s.state)).unwrap_or(false)
    }
}

fn manifest_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn initialize(session_dir: &Path) -> Result<LocalMediaManifest> {
    update(session_dir, |_| {})
}

pub fn read(session_dir: &Path) -> Result<Option<LocalMediaManifest>> {
    let path = session_dir.join(MANIFEST_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("read media manifest {}", path.display()))?;
    let manifest: LocalMediaManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode media manifest {}", path.display()))?;
    if !(OLDEST_SUPPORTED_MANIFEST_VERSION..=MANIFEST_VERSION).contains(&manifest.version) {
        anyhow::bail!(
            "unsupported media manifest version {} in {}",
            manifest.version,
            path.display()
        );
    }
    Ok(Some(manifest))
}

pub fn update(
    session_dir: &Path,
    mutate: impl FnOnce(&mut LocalMediaManifest),
) -> Result<LocalMediaManifest> {
    let _guard = manifest_lock().lock().unwrap();
    crate::session_fs::ensure_private_dir(session_dir)
        .with_context(|| format!("protect media session {}", session_dir.display()))?;
    let mut manifest = read(session_dir)?.unwrap_or_else(LocalMediaManifest::new);
    mutate(&mut manifest);
    // v1 is structurally readable because every generation field is optional.
    // Publish the upgraded version on the first mutation so future changes can
    // distinguish legacy "uploaded" markers from resumable generations.
    manifest.version = MANIFEST_VERSION;
    manifest.updated_at = Utc::now().to_rfc3339();
    write_atomic(session_dir, &manifest)?;
    Ok(manifest)
}

pub fn mark_audio_raw(
    session_dir: &Path,
    track: &str,
    ready: bool,
    error: Option<String>,
) -> Result<()> {
    update(session_dir, |manifest| {
        let item = manifest
            .audio
            .entry(track.to_string())
            .or_insert_with(|| ArtifactCheckpoint::recording(&format!("{track}.wav")));
        if has_backend_ownership(item) {
            return;
        }
        item.state = if ready {
            ArtifactState::RawReady
        } else {
            ArtifactState::EncodingFailed
        };
        item.last_error = error;
        item.byte_size = std::fs::metadata(session_dir.join(format!("{track}.wav")))
            .ok()
            .map(|m| m.len());
    })
    .map(|_| ())
}

pub fn mark_audio_published(session_dir: &Path, track: &str, opus_path: &Path) -> Result<()> {
    update(session_dir, |manifest| {
        let item = manifest
            .audio
            .entry(track.to_string())
            .or_insert_with(|| ArtifactCheckpoint::recording(&format!("{track}.wav")));
        if has_backend_ownership(item) {
            return;
        }
        item.state = ArtifactState::Published;
        item.published_path = relative_path(session_dir, opus_path);
        item.byte_size = std::fs::metadata(opus_path).ok().map(|m| m.len());
        item.last_error = None;
    })
    .map(|_| ())
}

pub fn mark_audio_fallback(session_dir: &Path, track: &str, error: String) -> Result<()> {
    update(session_dir, |manifest| {
        let item = manifest
            .audio
            .entry(track.to_string())
            .or_insert_with(|| ArtifactCheckpoint::recording(&format!("{track}.wav")));
        if has_backend_ownership(item) {
            return;
        }
        item.state = ArtifactState::EncodingFailed;
        item.last_error = Some(error);
        item.published_path = None;
    })
    .map(|_| ())
}

pub fn mark_audio_upload(
    session_dir: &Path,
    track: &str,
    uploaded: bool,
    error: Option<String>,
) -> Result<()> {
    update(session_dir, |manifest| {
        let item = manifest
            .audio
            .entry(track.to_string())
            .or_insert_with(|| ArtifactCheckpoint::recording(&format!("{track}.wav")));
        if item.state == ArtifactState::ReadyAcknowledged
            || (!uploaded && has_backend_ownership(item))
        {
            return;
        }
        item.state = if uploaded {
            ArtifactState::UploadedAwaitingBackendReady
        } else {
            ArtifactState::UploadPending
        };
        item.last_error = error;
    })
    .map(|_| ())
}

pub fn mark_audio_not_present(session_dir: &Path, track: &str) -> Result<()> {
    update(session_dir, |manifest| {
        let item = manifest
            .audio
            .entry(track.to_string())
            .or_insert_with(|| ArtifactCheckpoint::recording(&format!("{track}.wav")));
        if has_backend_ownership(item) {
            return;
        }
        item.state = ArtifactState::NotPresent;
        item.last_error = None;
    })
    .map(|_| ())
}

pub fn mark_screen_published(session_dir: &Path, path: &Path) -> Result<()> {
    update(session_dir, |manifest| {
        if manifest
            .screen
            .as_ref()
            .map(has_backend_ownership)
            .unwrap_or(false)
        {
            return;
        }
        manifest.screen = Some(ArtifactCheckpoint {
            state: ArtifactState::Published,
            raw_path: relative_path(session_dir, path),
            published_path: relative_path(session_dir, path),
            byte_size: std::fs::metadata(path).ok().map(|m| m.len()),
            last_error: None,
            upload: None,
        });
    })
    .map(|_| ())
}

pub fn mark_screen_upload_pending(
    session_dir: &Path,
    call_id: Option<&str>,
    path: &Path,
    error: String,
) -> Result<()> {
    if let Some(call_id) = call_id {
        bind_call(session_dir, call_id)?;
    }
    update(session_dir, |manifest| {
        if manifest
            .screen
            .as_ref()
            .map(has_backend_ownership)
            .unwrap_or(false)
        {
            return;
        }
        let item = manifest.screen.get_or_insert(ArtifactCheckpoint {
            state: ArtifactState::UploadPending,
            raw_path: None,
            published_path: None,
            byte_size: None,
            last_error: None,
            upload: None,
        });
        item.state = ArtifactState::UploadPending;
        item.raw_path = relative_path(session_dir, path);
        if item.published_path.is_none() {
            item.published_path = relative_path(session_dir, path);
        }
        if item.byte_size.is_none() {
            item.byte_size = std::fs::metadata(path).ok().map(|m| m.len());
        }
        item.last_error = Some(error);
    })
    .map(|_| ())
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn mark_screen_uploaded(session_dir: &Path, call_id: &str, path: &Path) -> Result<()> {
    bind_call(session_dir, call_id)?;
    update(session_dir, |manifest| {
        if matches!(
            manifest.screen.as_ref().map(|screen| screen.state),
            Some(ArtifactState::ReadyAcknowledged)
        ) {
            return;
        }
        manifest.screen = Some(ArtifactCheckpoint {
            state: ArtifactState::UploadedAwaitingBackendReady,
            raw_path: relative_path(session_dir, path),
            published_path: relative_path(session_dir, path),
            byte_size: std::fs::metadata(path).ok().map(|m| m.len()),
            last_error: None,
            upload: None,
        });
    })
    .map(|_| ())
}

pub fn mark_screen_not_present(session_dir: &Path) -> Result<()> {
    update(session_dir, |manifest| {
        if manifest
            .screen
            .as_ref()
            .map(has_backend_ownership)
            .unwrap_or(false)
        {
            return;
        }
        manifest.screen = Some(ArtifactCheckpoint {
            state: ArtifactState::NotPresent,
            raw_path: None,
            published_path: None,
            byte_size: None,
            last_error: None,
            upload: None,
        });
    })
    .map(|_| ())
}

/// Checkpoint the pipeline as finished. Gated on
/// [`LocalMediaManifest::has_client_pending_media`], not the strict
/// acknowledgement predicate: a generation the backend is still assembling or
/// validating has all of its bytes stored, so failing the run here reported a
/// complete, usable call as a hard failure and stranded intact recordings
/// behind a red banner. `pipeline_complete` alone is not terminal — recovery
/// still requires every artifact acknowledged or absent — so a finalizing
/// generation is polled to ready and its local source cleaned up on a later
/// sweep.
pub fn mark_pipeline_complete(session_dir: &Path, call_id: &str) -> Result<LocalMediaManifest> {
    bind_call(session_dir, call_id)?;
    let mut pending = false;
    let manifest = update(session_dir, |manifest| {
        if manifest.has_client_pending_media() {
            pending = true;
        } else {
            manifest.pipeline_complete = true;
        }
    })?;
    if pending {
        anyhow::bail!("refusing to complete pipeline while recorded media is still unuploaded");
    }
    Ok(manifest)
}

/// Bind a local session to exactly one backend call. A retry under a changed
/// account/backend must never overwrite this identity and reattach private
/// media to a different call.
pub fn bind_call(session_dir: &Path, call_id: &str) -> Result<LocalMediaManifest> {
    uuid::Uuid::parse_str(call_id).context("invalid call id for local media binding")?;
    let mut mismatch = None;
    let manifest = update(session_dir, |manifest| match manifest.call_id.as_deref() {
        Some(existing) if existing != call_id => {
            mismatch = Some(existing.to_string());
        }
        Some(_) => {}
        None => manifest.call_id = Some(call_id.to_string()),
    })?;
    if let Some(existing) = mismatch {
        anyhow::bail!(
            "local media checkpoint is bound to call {existing}, refusing call {call_id}"
        );
    }
    Ok(manifest)
}

fn artifact_mut<'a>(
    manifest: &'a mut LocalMediaManifest,
    kind: &str,
) -> Result<&'a mut ArtifactCheckpoint> {
    match kind {
        "mic" | "system" => Ok(manifest
            .audio
            .entry(kind.to_string())
            .or_insert_with(|| ArtifactCheckpoint::recording(&format!("{kind}.wav")))),
        "screen" => Ok(manifest.screen.get_or_insert(ArtifactCheckpoint {
            state: ArtifactState::UploadPending,
            raw_path: None,
            published_path: None,
            byte_size: None,
            last_error: None,
            upload: None,
        })),
        _ => anyhow::bail!("unsupported local media kind {kind:?}"),
    }
}

fn validate_kind(kind: &str) -> Result<()> {
    if matches!(kind, "mic" | "system" | "screen") {
        Ok(())
    } else {
        anyhow::bail!("unsupported local media kind {kind:?}")
    }
}

pub fn artifact(session_dir: &Path, kind: &str) -> Result<Option<ArtifactCheckpoint>> {
    let Some(manifest) = read(session_dir)? else {
        return Ok(None);
    };
    Ok(match kind {
        "mic" | "system" => manifest.audio.get(kind).cloned(),
        "screen" => manifest.screen,
        _ => anyhow::bail!("unsupported local media kind {kind:?}"),
    })
}

/// Persist immutable upload intent before the first network request. Reusing
/// an operation id with changed bytes/hash would be a protocol violation, so a
/// mismatched existing checkpoint is rejected instead of silently rotated.
pub fn prepare_upload(
    session_dir: &Path,
    call_id: &str,
    kind: &str,
    raw_path: Option<&Path>,
    published_path: &Path,
    declared_bytes: u64,
    declared_sha256: &str,
) -> Result<UploadCheckpoint> {
    validate_kind(kind)?;
    let mut prepared = None;
    let mut invariant_error = None;
    update(session_dir, |manifest| {
        if manifest
            .call_id
            .as_deref()
            .map(|existing| existing != call_id)
            .unwrap_or(false)
        {
            invariant_error =
                Some("local media checkpoint belongs to a different call".to_string());
            return;
        }
        manifest.call_id = Some(call_id.to_string());
        let item = artifact_mut(manifest, kind).expect("validated media kind");
        if let Some(existing) = item.upload.as_ref() {
            if item.byte_size == Some(declared_bytes)
                && existing
                    .declared_sha256
                    .eq_ignore_ascii_case(declared_sha256)
            {
                prepared = Some(existing.clone());
                return;
            }
            invariant_error = Some(format!(
                "local {kind} bytes changed after upload checkpoint; refusing idempotency-key replay"
            ));
            return;
        }
        let upload = UploadCheckpoint {
            client_operation_id: format!("media-{kind}-{}", uuid::Uuid::new_v4()),
            generation_id: None,
            declared_sha256: declared_sha256.to_string(),
            confirmed_parts: Vec::new(),
            part_count: None,
            backend_state: None,
            object_key: None,
            is_current: None,
            actual_bytes: None,
            actual_sha256: None,
        };
        item.state = ArtifactState::UploadPending;
        if let Some(raw_path) = raw_path {
            item.raw_path = relative_path(session_dir, raw_path);
        }
        item.published_path = relative_path(session_dir, published_path);
        item.byte_size = Some(declared_bytes);
        item.last_error = None;
        item.upload = Some(upload.clone());
        prepared = Some(upload);
    })?;
    if let Some(error) = invariant_error {
        anyhow::bail!(error);
    }
    prepared.ok_or_else(|| anyhow::anyhow!("failed to persist local {kind} upload checkpoint"))
}

#[allow(clippy::too_many_arguments)]
pub fn sync_upload_status(
    session_dir: &Path,
    kind: &str,
    generation_id: &str,
    backend_state: &str,
    object_key: &str,
    confirmed_parts: &[u32],
    part_count: u32,
    is_current: bool,
    actual_bytes: Option<u64>,
    actual_sha256: Option<&str>,
) -> Result<()> {
    validate_kind(kind)?;
    let mut invariant_error = None;
    update(session_dir, |manifest| {
        let item = artifact_mut(manifest, kind).expect("validated media kind");
        if item.state == ArtifactState::ReadyAcknowledged {
            return;
        }
        let Some(upload) = item.upload.as_mut() else {
            invariant_error = Some(format!(
                "cannot mirror backend status without a local {kind} upload checkpoint"
            ));
            return;
        };
        if upload
            .generation_id
            .as_deref()
            .map(|existing| existing != generation_id)
            .unwrap_or(false)
        {
            invariant_error = Some(format!(
                "backend generation id changed for checkpointed {kind} upload"
            ));
            return;
        }
        upload.generation_id = Some(generation_id.to_string());
        upload.backend_state = Some(backend_state.to_string());
        upload.object_key = Some(object_key.to_string());
        upload.confirmed_parts = confirmed_parts.to_vec();
        upload.confirmed_parts.sort_unstable();
        upload.confirmed_parts.dedup();
        upload.part_count = Some(part_count);
        upload.is_current = Some(is_current);
        upload.actual_bytes = actual_bytes;
        upload.actual_sha256 = actual_sha256.map(str::to_string);
        // Mirroring a claimed state is not the deletion boundary. Only
        // `acknowledge_ready` can promote the artifact after independently
        // checking actual bytes/hash and complete part confirmation.
        item.state = ArtifactState::UploadedAwaitingBackendReady;
        item.last_error = None;
    })?;
    if let Some(error) = invariant_error {
        anyhow::bail!(error);
    }
    Ok(())
}

/// Cross the durable ownership boundary only after the caller has validated a
/// backend `ready/current` response. Recheck all persisted evidence here so a
/// future call site cannot accidentally authorize cleanup from a partial
/// status mirror.
pub fn acknowledge_ready(session_dir: &Path, kind: &str, generation_id: &str) -> Result<()> {
    validate_kind(kind)?;
    let mut invariant_error = None;
    update(session_dir, |manifest| {
        let item = artifact_mut(manifest, kind).expect("validated media kind");
        let Some(upload) = item.upload.as_ref() else {
            invariant_error = Some(format!(
                "cannot acknowledge {kind} readiness without an upload checkpoint"
            ));
            return;
        };
        if upload.generation_id.as_deref() != Some(generation_id) {
            invariant_error = Some(format!(
                "cannot acknowledge a different backend generation for {kind}"
            ));
            return;
        }
        let exact_actual = upload.actual_bytes == item.byte_size
            && upload
                .actual_sha256
                .as_deref()
                .map(|actual| actual.eq_ignore_ascii_case(&upload.declared_sha256))
                .unwrap_or(false);
        let all_parts_confirmed = upload
            .part_count
            .filter(|count| *count > 0)
            .map(|count| {
                upload.confirmed_parts.len() == count as usize
                    && upload.confirmed_parts.iter().copied().eq(1..=count)
            })
            .unwrap_or(false);
        if upload.backend_state.as_deref() != Some("ready")
            || upload.is_current != Some(true)
            || !exact_actual
            || !all_parts_confirmed
        {
            invariant_error = Some(format!(
                "refusing to acknowledge {kind}: ready/current byte, hash, or part evidence is incomplete"
            ));
            return;
        }
        item.state = ArtifactState::ReadyAcknowledged;
        item.last_error = None;
    })?;
    if let Some(error) = invariant_error {
        anyhow::bail!(error);
    }
    Ok(())
}

pub fn mark_upload_error(session_dir: &Path, kind: &str, error: String) -> Result<()> {
    validate_kind(kind)?;
    update(session_dir, |manifest| {
        let item = artifact_mut(manifest, kind).expect("validated media kind");
        if item.state != ArtifactState::ReadyAcknowledged {
            item.state = if item
                .upload
                .as_ref()
                .and_then(|upload| upload.generation_id.as_ref())
                .is_some()
            {
                ArtifactState::UploadedAwaitingBackendReady
            } else {
                ArtifactState::UploadPending
            };
            item.last_error = Some(error);
        }
    })
    .map(|_| ())
}

/// Called only after the backend has acknowledged an abort. Clearing the
/// generation identity after that 204 lets a later retry mint a fresh
/// operation for newly produced bytes without colliding with the old active
/// generation.
pub fn reset_aborted_upload(session_dir: &Path, kind: &str, error: String) -> Result<()> {
    validate_kind(kind)?;
    update(session_dir, |manifest| {
        let item = artifact_mut(manifest, kind).expect("validated media kind");
        item.state = ArtifactState::UploadPending;
        item.upload = None;
        item.last_error = Some(error);
    })
    .map(|_| ())
}

fn relative_path(session_dir: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(session_dir)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn write_atomic(session_dir: &Path, manifest: &LocalMediaManifest) -> Result<()> {
    let final_path = session_dir.join(MANIFEST_FILENAME);
    let staged = session_dir.join(format!("{MANIFEST_FILENAME}.part.{}", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(manifest).context("encode media manifest")?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staged)
        .with_context(|| format!("create staged manifest {}", staged.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write staged manifest {}", staged.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("finish staged manifest {}", staged.display()))?;
    file.sync_all()
        .with_context(|| format!("sync staged manifest {}", staged.display()))?;
    drop(file);
    if let Err(e) = atomic_replace_file(&staged, &final_path) {
        // Keep the staged checkpoint for crash/debug recovery.
        return Err(e).with_context(|| {
            format!(
                "publish media manifest {} -> {}",
                staged.display(),
                final_path.display()
            )
        });
    }
    Ok(())
}

/// Owns a disposable staged artifact. Successful atomic publication moves the
/// path away; every failure/early return removes the leftover automatically.
#[must_use]
pub(crate) struct PrivateStage {
    path: PathBuf,
}

impl Drop for PrivateStage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Reserve an unpredictable producer path without following/replacing an
/// attacker-controlled entry. On Unix the stage is private from its first
/// inode; ffmpeg opens the existing file with `-y`/truncate.
pub(crate) fn reserve_private_stage(path: &Path) -> Result<PrivateStage> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("reserve private stage {}", path.display()))?;
    Ok(PrivateStage {
        path: path.to_path_buf(),
    })
}

pub(crate) fn enforce_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set private permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Flush a completed staged file before its atomic publication. Windows'
/// `FlushFileBuffers` requires a write-capable handle; `File::open` creates a
/// read-only handle and therefore fails with `ERROR_ACCESS_DENIED` even when
/// the caller owns the file.
pub(crate) fn sync_staged_file(path: &Path) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("open staged file for sync {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync staged file {}", path.display()))
}

/// Same-filesystem atomic publication. On Windows `std::fs::rename` does not
/// replace an existing destination, so use the native replace + write-through
/// flags instead of a remove/rename gap.
pub(crate) fn atomic_replace_file(staged: &Path, final_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let from: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = final_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .context("MoveFileExW")?;
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(staged, final_path).context("rename staged file")?;
        if let Some(parent) = final_path.parent() {
            sync_parent(parent)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    File::open(parent)
        .with_context(|| format!("open parent {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync parent {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    // MoveFileExW uses MOVEFILE_WRITE_THROUGH.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const CALL_ID: &str = "4da6e5c4-7ac1-45bb-bab6-8e269a2664c2";

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "aftercalls-media-manifest-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_manifest_preserves_pending_screen_across_read() {
        let scratch = Scratch::new();
        let video = scratch.0.join("screen").join("recording.mp4");
        std::fs::create_dir_all(video.parent().unwrap()).unwrap();
        std::fs::write(&video, b"local-only-video").unwrap();

        initialize(&scratch.0).unwrap();
        mark_screen_upload_pending(&scratch.0, Some(CALL_ID), &video, "network timeout".into())
            .unwrap();

        let loaded = read(&scratch.0).unwrap().unwrap();
        let screen = loaded.screen.unwrap();
        assert_eq!(screen.state, ArtifactState::UploadPending);
        assert_eq!(screen.byte_size, Some(16));
        assert!(video.exists(), "checkpointing must not remove local media");
    }

    #[test]
    fn legacy_v1_manifest_is_read_and_upgraded_on_first_mutation() {
        let scratch = Scratch::new();
        let mut manifest = LocalMediaManifest::new();
        manifest.version = 1;
        write_atomic(&scratch.0, &manifest).unwrap();

        assert_eq!(read(&scratch.0).unwrap().unwrap().version, 1);
        mark_audio_not_present(&scratch.0, "mic").unwrap();
        assert_eq!(read(&scratch.0).unwrap().unwrap().version, MANIFEST_VERSION);
    }

    #[test]
    fn call_binding_is_idempotent_and_never_reassigned() {
        let scratch = Scratch::new();
        let other = "83a52a90-cdf3-42d3-b80d-68012de6ef61";
        bind_call(&scratch.0, CALL_ID).unwrap();
        bind_call(&scratch.0, CALL_ID).unwrap();
        assert!(bind_call(&scratch.0, other).is_err());
        assert_eq!(
            read(&scratch.0).unwrap().unwrap().call_id.as_deref(),
            Some(CALL_ID)
        );
    }

    #[test]
    fn pipeline_completion_requires_all_media_uploaded_or_absent() {
        let scratch = Scratch::new();
        initialize(&scratch.0).unwrap();
        assert!(mark_pipeline_complete(&scratch.0, CALL_ID).is_err());
        assert!(!read(&scratch.0).unwrap().unwrap().pipeline_complete);

        mark_audio_not_present(&scratch.0, "mic").unwrap();
        mark_audio_not_present(&scratch.0, "system").unwrap();
        mark_screen_not_present(&scratch.0).unwrap();
        assert!(mark_pipeline_complete(&scratch.0, CALL_ID)
            .unwrap()
            .pipeline_complete);
    }

    /// A generation the backend is still assembling has nothing left for the
    /// client to do, so it must not fail the run — but it stays retryable so a
    /// later sweep drives it to acknowledged before local bytes are released.
    #[test]
    fn pipeline_completes_while_the_backend_is_still_validating() {
        let scratch = Scratch::new();
        let video = scratch.0.join("screen").join("recording.mp4");
        std::fs::create_dir_all(video.parent().unwrap()).unwrap();
        std::fs::write(&video, b"video").unwrap();
        mark_audio_upload(&scratch.0, "mic", true, None).unwrap();
        mark_audio_not_present(&scratch.0, "system").unwrap();
        mark_screen_uploaded(&scratch.0, CALL_ID, &video).unwrap();

        assert!(mark_pipeline_complete(&scratch.0, CALL_ID)
            .unwrap()
            .pipeline_complete);
        let loaded = read(&scratch.0).unwrap().unwrap();
        assert!(!loaded.has_client_pending_media());
        assert!(
            loaded.has_unacknowledged_media(),
            "completion must not fake acknowledgement"
        );
        assert!(
            loaded.has_retryable_media(),
            "a finalizing generation is still swept to ready"
        );
        assert!(video.exists(), "local source is retained until acknowledged");
    }

    /// The inverse: bytes the client never handed over still fail the run.
    #[test]
    fn pipeline_completion_still_blocks_on_a_failed_upload() {
        let scratch = Scratch::new();
        mark_audio_upload(&scratch.0, "mic", false, Some("network".into())).unwrap();
        mark_audio_not_present(&scratch.0, "system").unwrap();
        assert!(mark_pipeline_complete(&scratch.0, CALL_ID).is_err());
        assert!(!read(&scratch.0).unwrap().unwrap().pipeline_complete);
    }

    #[test]
    fn uploaded_without_ready_ack_remains_unacknowledged() {
        let scratch = Scratch::new();
        let video = scratch.0.join("screen").join("recording.mp4");
        std::fs::create_dir_all(video.parent().unwrap()).unwrap();
        std::fs::write(&video, b"video").unwrap();
        mark_audio_not_present(&scratch.0, "mic").unwrap();
        mark_audio_not_present(&scratch.0, "system").unwrap();
        mark_screen_uploaded(&scratch.0, CALL_ID, &video).unwrap();
        let loaded = read(&scratch.0).unwrap().unwrap();
        assert!(loaded.has_unacknowledged_media());
        assert!(
            loaded.has_retryable_media(),
            "legacy awaiting-ready checkpoints must be revisited"
        );
    }

    #[test]
    fn immutable_upload_intent_reuses_operation_and_rejects_changed_bytes() {
        const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let scratch = Scratch::new();
        let opus = scratch.0.join("mic.opus");
        let wav = scratch.0.join("mic.wav");
        std::fs::write(&opus, b"hello").unwrap();
        std::fs::write(&wav, b"raw").unwrap();

        let first = prepare_upload(&scratch.0, CALL_ID, "mic", Some(&wav), &opus, 5, HASH).unwrap();
        let second =
            prepare_upload(&scratch.0, CALL_ID, "mic", Some(&wav), &opus, 5, HASH).unwrap();
        assert_eq!(first.client_operation_id, second.client_operation_id);
        assert!(prepare_upload(
            &scratch.0,
            CALL_ID,
            "mic",
            Some(&wav),
            &opus,
            6,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .is_err());
    }

    #[test]
    fn ready_mirror_does_not_acknowledge_until_exact_evidence_is_rechecked() {
        const CALL_ID: &str = "4da6e5c4-7ac1-45bb-bab6-8e269a2664c2";
        const GENERATION_ID: &str = "9a0b77df-4391-407c-a1fb-b12bdaefa5dd";
        const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let scratch = Scratch::new();
        let opus = scratch.0.join("mic.opus");
        std::fs::write(&opus, b"hello").unwrap();
        prepare_upload(&scratch.0, CALL_ID, "mic", None, &opus, 5, HASH).unwrap();

        sync_upload_status(
            &scratch.0,
            "mic",
            GENERATION_ID,
            "ready",
            "calls/call/mic.opus",
            &[1, 2],
            2,
            true,
            Some(5),
            Some(HASH),
        )
        .unwrap();
        assert_eq!(
            artifact(&scratch.0, "mic").unwrap().unwrap().state,
            ArtifactState::UploadedAwaitingBackendReady
        );
        acknowledge_ready(&scratch.0, "mic", GENERATION_ID).unwrap();
        assert_eq!(
            artifact(&scratch.0, "mic").unwrap().unwrap().state,
            ArtifactState::ReadyAcknowledged
        );
    }

    #[test]
    fn readiness_ack_rejects_mismatched_actual_hash() {
        const CALL_ID: &str = "4da6e5c4-7ac1-45bb-bab6-8e269a2664c2";
        const GENERATION_ID: &str = "9a0b77df-4391-407c-a1fb-b12bdaefa5dd";
        const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let scratch = Scratch::new();
        let opus = scratch.0.join("mic.opus");
        std::fs::write(&opus, b"hello").unwrap();
        prepare_upload(&scratch.0, CALL_ID, "mic", None, &opus, 5, HASH).unwrap();
        sync_upload_status(
            &scratch.0,
            "mic",
            GENERATION_ID,
            "ready",
            "calls/call/mic.opus",
            &[1],
            1,
            true,
            Some(5),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .unwrap();
        assert!(acknowledge_ready(&scratch.0, "mic", GENERATION_ID).is_err());
        assert_ne!(
            artifact(&scratch.0, "mic").unwrap().unwrap().state,
            ArtifactState::ReadyAcknowledged
        );
    }

    #[test]
    fn producer_updates_do_not_regress_uploaded_audio() {
        let scratch = Scratch::new();
        let opus = scratch.0.join("mic.opus");
        std::fs::write(&opus, b"validated opus").unwrap();
        mark_audio_published(&scratch.0, "mic", &opus).unwrap();
        mark_audio_upload(&scratch.0, "mic", true, None).unwrap();

        mark_audio_raw(&scratch.0, "mic", true, None).unwrap();
        mark_audio_fallback(&scratch.0, "mic", "later retry failed".into()).unwrap();
        mark_audio_not_present(&scratch.0, "mic").unwrap();
        mark_audio_upload(
            &scratch.0,
            "mic",
            false,
            Some("redundant upload failed".into()),
        )
        .unwrap();

        let loaded = read(&scratch.0).unwrap().unwrap();
        assert_eq!(
            loaded.audio["mic"].state,
            ArtifactState::UploadedAwaitingBackendReady
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_and_reserved_stage_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new();
        initialize(&scratch.0).unwrap();
        let manifest_mode = std::fs::metadata(scratch.0.join(MANIFEST_FILENAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(manifest_mode & 0o777, 0o600);

        let stage = scratch.0.join("mic.opus.part.test");
        {
            let _guard = reserve_private_stage(&stage).unwrap();
            let mode = std::fs::metadata(&stage).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!stage.exists());
    }
}
