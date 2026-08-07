//! Orphan session recovery (#63).
//!
//! When the agent starts up, a session_dir containing audio but no
//! matching `status = 'complete'` call row on the backend means a
//! previous process crashed, was force-quit, or lost its connection
//! partway through the pipeline. We scan for these on launch, offer
//! the user a chance to resume them (re-run the pipeline from the
//! top — AssemblyAI + OpenAI are idempotent for our purposes). Local media is
//! never age-deleted: a user may explicitly discard it, or a future immutable
//! backend-ready generation/hash acknowledgement may authorize cleanup.
//!
//! A session_dir counts as an orphan when ALL of:
//!   1. The dir contains a mic/system artifact or durable media manifest AND
//!      is older than 5 minutes (so we don't snapshot a recording still in
//!      the act of uploading). Age prefers artifact/manifest mtime with a
//!      fallback to the directory mtime.
//!   2. The backend has no 'complete' row for this session_id, OR the durable
//!      manifest still has a locally retryable media job. The former covers
//!      both mid-pipeline crashes and crashes before create_call; the latter
//!      preserves a failed screen/audio upload even when transcript/summary
//!      already completed. Discard is a user decision, not ours.

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};

use crate::config::Config;
use crate::portal::FailureClass;

/// Minimum age of a session_dir before we consider it for recovery.
/// Shorter than this and it might still be in the act of uploading —
/// the pipeline is kicked off async from stop_recording, so a folder
/// created 30s ago is almost certainly mid-pipeline not orphaned.
const MIN_AGE_MINUTES: i64 = 5;

/// #646 Layer C — auto-resume is only attempted on sessions younger
/// than this. Older orphans fall through to the prompted-UI list
/// regardless of failure class. 30 min matches the architect plan
/// (auto-resume cap × sweeper interval = 3 × 5 min = 15 min, well
/// inside this window).
const AUTO_RESUME_MAX_AGE_MINUTES: i64 = 30;

/// #646 Layer C — max number of silent auto-resume attempts per
/// session before falling through to the prompted UI. Persisted in
/// `auto_resume_state.json` next to the session_dir so the count
/// survives agent restarts.
const AUTO_RESUME_CAP: u8 = 3;

/// #646 Layer D — discriminator on `OrphanSession`. `NeverCreated`
/// = no backend row exists (pipeline crashed before create_call).
/// `StuckPipeline` = backend row exists but `status != 'complete'`
/// (pipeline crashed mid-transcribe / summarize / etc.). The agent
/// review UI renders different badge copy per kind so the user
/// understands what they're resuming.
#[derive(Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum OrphanKind {
    NeverCreated,
    StuckPipeline,
}

#[derive(Serialize, Clone, Debug)]
pub struct OrphanSession {
    /// Absolute path to the session_dir on disk. Not surfaced to the
    /// UI (security + it's just noise); the Tauri commands take a
    /// session_id string and re-resolve via `resolve_session_dir`
    /// to keep the IPC surface simple. Kept on the struct so future
    /// Rust-side callers (e.g. tests) can reach for it.
    #[serde(skip)]
    #[allow(dead_code)]
    pub session_dir: PathBuf,
    /// The directory's name — timestamped like "20260422T173128Z"
    /// (or "imp_<stamp>" for imports).
    pub session_id: String,
    /// When the recording was captured. Parsed from the session_id if
    /// it matches our YYYYMMDDTHHMMSSZ format, else falls back to the
    /// mic.wav mtime.
    pub recorded_at: DateTime<Utc>,
    /// How old the folder is right now, in minutes. The UI uses this
    /// for the "12 min ago / 4 hr ago / 2 days ago" label.
    pub age_minutes: i64,
    /// #646 Layer D — which class of orphan this is. Drives the
    /// `.orphan-kind-badge` copy in the review panel.
    pub kind: OrphanKind,
    /// #646 Layer C — how many silent auto-resume attempts have fired
    /// against this session. Read from `auto_resume_state.json`;
    /// defaults to 0 when no state file exists. When this reaches
    /// `AUTO_RESUME_CAP` the orphan-review panel shows the
    /// "Couldn't resume automatically." line.
    pub auto_resume_count: u8,
}

/// #646 Layer C — sentinel persisted next to the session_dir so the
/// auto-resume attempt counter survives agent restarts. Read at the
/// top of `auto_resume_eligible` to enforce the 3-attempt cap, bumped
/// when a sweeper-driven pipeline run kicks off, cleared by `discard`
/// when the user explicitly drops the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoResumeState {
    pub attempts: u8,
    pub last_failure_class: Option<String>,
    pub last_attempt_ts: DateTime<Utc>,
}

const AUTO_RESUME_STATE_FILENAME: &str = "auto_resume_state.json";

fn auto_resume_state_path(session_dir: &Path) -> PathBuf {
    session_dir.join(AUTO_RESUME_STATE_FILENAME)
}

/// Read `auto_resume_state.json` if it exists. A corrupt / missing
/// file returns `None`; the sweeper treats that as "no prior auto-
/// resume attempts" which is safe — we'd rather try once too many
/// than skip a session the user can't recover manually.
pub fn read_auto_resume_state(session_dir: &Path) -> Option<AutoResumeState> {
    let path = auto_resume_state_path(session_dir);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `auto_resume_state.json`. Best-effort; failure is logged but
/// not surfaced. A missing state file falls back to "treat as fresh"
/// on the next sweeper tick.
pub fn write_auto_resume_state(session_dir: &Path, state: &AutoResumeState) {
    let path = auto_resume_state_path(session_dir);
    match serde_json::to_string_pretty(state) {
        Ok(text) => {
            if let Err(e) = crate::session_fs::write_private_file(&path, text.as_bytes()) {
                eprintln!(
                    "aftercalls: write auto_resume_state {} failed: {e}",
                    path.display()
                );
            }
        }
        Err(e) => {
            eprintln!("aftercalls: serialize auto_resume_state failed: {e}");
        }
    }
}

/// #646 Layer D — module-level lock of session_ids currently being
/// auto-resumed by the sweeper. `scan_orphans` filters its result
/// through this set so a session mid-resume never appears in the
/// prompted-UI list (would let the user race the sweeper). Inserted
/// before the auto-resume pipeline spawn, removed when it finishes.
fn in_flight_recovery() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn mark_in_flight(session_id: &str) {
    if let Ok(mut set) = in_flight_recovery().lock() {
        set.insert(session_id.to_string());
    }
}

pub fn clear_in_flight(session_id: &str) {
    if let Ok(mut set) = in_flight_recovery().lock() {
        set.remove(session_id);
    }
}

fn is_in_flight(session_id: &str) -> bool {
    in_flight_recovery()
        .lock()
        .map(|set| set.contains(session_id))
        .unwrap_or(false)
}

/// Resolve the base recordings directory. Matches what start_recording
/// uses (app_local_data_dir/recordings).
fn recordings_root(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|p| p.join("recordings"))
}

/// Iterate the recordings directory and return every dir that meets the
/// orphan criteria. Age never authorizes deletion: unknown or unacknowledged
/// local media remains recoverable until the user explicitly discards it.
///
/// #646 Layer C/D — as of Phase 2 this function runs both on launch
/// AND on the 5-min sweeper tick driven by the Svelte layout. Both
/// callers consume the same filtered Vec. Sessions currently mid-
/// auto-resume (tracked by the `in_flight_recovery` set) are excluded
/// from the returned list so the prompted-UI never races the sweeper.
pub async fn scan_orphans(app: &AppHandle) -> Vec<OrphanSession> {
    let Some(root) = recordings_root(app) else {
        return Vec::new();
    };
    if !root.exists() {
        return Vec::new();
    }

    let now = SystemTime::now();
    let mut out = Vec::new();

    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("aftercalls: scan_orphans read_dir failed: {e}");
            return Vec::new();
        }
    };

    // Collect candidate (session_dir, age) pairs up front so we can do
    // the per-folder backend check asynchronously afterward without
    // holding the ReadDir iterator open.
    let mut candidates: Vec<(PathBuf, String, SystemTime, bool, bool)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let session_id = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Need local media intent to be considered. Imports are commonly
        // system-only, and a durable manifest can represent a pending screen
        // retry even when an audio artifact has moved or failed validation.
        let mic_wav = path.join("mic.wav");
        let mic_opus = path.join("mic.opus");
        let system_wav = path.join("system.wav");
        let system_opus = path.join("system.opus");
        let manifest_path = path.join(crate::media_manifest::MANIFEST_FILENAME);
        let mut manifest_terminal = false;
        let mut manifest_client_pending = false;
        let mut manifest_authoritative = false;
        let manifest_retryable = match crate::media_manifest::read(&path) {
            Ok(Some(manifest)) => {
                // A session whose pipeline finished and whose every artifact is
                // acknowledged or absent holds nothing recoverable. The media
                // bytes are already cleaned up, but `media-state.json` itself
                // survives, so without this the dir stays a candidate forever
                // and any call deleted outside this agent (portal, another
                // machine, an admin) resurfaces as a permanent "N unfinished
                // calls" chip that the user can never clear.
                manifest_terminal =
                    manifest.pipeline_complete && !manifest.has_unacknowledged_media();
                manifest_client_pending = manifest.has_client_pending_media();
                manifest_authoritative = true;
                manifest.has_retryable_media()
            }
            Ok(None) => false,
            Err(e) => {
                // A corrupt checkpoint is unknown state, never evidence that
                // cleanup/recovery can be skipped.
                eprintln!(
                    "aftercalls: media manifest unreadable for {}: {e:#}",
                    path.display()
                );
                manifest_client_pending = true;
                true
            }
        };
        if manifest_terminal {
            continue;
        }
        // `pending_uploads.json` predates the durable manifest and is cleared in
        // exactly one place — `upload_audio`'s all-tracks-landed path. A session
        // that now bails BEFORE that call (nothing left to upload, because its
        // sources were legitimately cleaned up) can never clear it, so the
        // sentinel becomes immortal and re-offers the session forever. Observed
        // on 0.33.0: a call the backend had already completed was re-prompted on
        // every launch and failed with "no validated audio tracks are available
        // for upload", because the sentinel alone forced it back into the
        // candidate set.
        //
        // The v2 manifest is authoritative wherever it exists; the sentinel only
        // speaks for sessions that predate it. When the manifest is present and
        // says the client owes nothing, the sentinel is stale — drop it so the
        // session stops resurfacing.
        let legacy_sentinel = crate::upload::read_pending_uploads(&path).is_some();
        if legacy_sentinel && manifest_authoritative && !manifest_client_pending {
            crate::upload::clear_pending_uploads(&path);
        }
        let legacy_pending_audio = legacy_sentinel && !manifest_authoritative;
        if !mic_wav.exists()
            && !mic_opus.exists()
            && !system_wav.exists()
            && !system_opus.exists()
            && !manifest_path.exists()
            && !legacy_pending_audio
        {
            continue;
        }

        // Age: prefer mic.wav mtime, fall back to the dir's mtime.
        // Both come from std::fs::Metadata.modified() — ctime on unix
        // is actually inode-change-time and flips on chmod/rename,
        // which is worse noise than mtime for "when did we stop
        // writing to this dir".
        let age_anchor = mtime_of(&mic_wav)
            .or_else(|| mtime_of(&mic_opus))
            .or_else(|| mtime_of(&system_wav))
            .or_else(|| mtime_of(&system_opus))
            .or_else(|| mtime_of(&manifest_path))
            .or_else(|| mtime_of(&path))
            .unwrap_or(now);

        let age_secs = now
            .duration_since(age_anchor)
            .unwrap_or(Duration::from_secs(0))
            .as_secs() as i64;
        let age_minutes = age_secs / 60;

        // Too-new gate: skip anything that could still be mid-pipeline.
        if age_minutes < MIN_AGE_MINUTES {
            continue;
        }

        candidates.push((
            path,
            session_id,
            age_anchor,
            manifest_retryable || legacy_pending_audio,
            manifest_client_pending || legacy_pending_audio,
        ));
    }

    // Now check the backend for each candidate. Any session whose
    // backend row is 'complete' is NOT an orphan. Anything else
    // (missing row OR non-complete status) IS.
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("aftercalls: scan_orphans config load failed: {e}");
            return Vec::new();
        }
    };
    let backend = match cfg.backend.as_ref() {
        Some(b) => b,
        // No backend configured → we can't tell if these are orphans,
        // so surface nothing rather than prompting against a null
        // reference point.
        None => return Vec::new(),
    };

    for (session_dir, session_id, age_anchor, local_media_retryable, client_pending) in candidates {
        // Filter out anything Layer C is mid-resuming so the prompted
        // UI never races the sweeper.
        if is_in_flight(&session_id) {
            continue;
        }

        let (needs_recovery, kind) =
            match crate::portal::get_call_by_session(backend, &session_id).await {
                Ok(Some(v)) => {
                    // Backend knows this recording. Orphan iff the row
                    // didn't reach complete. Missing status field is
                    // treated as non-complete for safety.
                    let stuck = v
                        .get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s != "complete")
                        .unwrap_or(true);
                    // A completed call qualifies only on media the CLIENT still
                    // owes bytes for — not on anything merely `has_retryable_media`.
                    // An artifact the backend already holds but has not finished
                    // validating is retryable, and gating on that re-ran the whole
                    // pipeline against a call that was already transcribed and
                    // summarized. Worse, its local sources are legitimately gone by
                    // then, so the re-run failed on a missing source and reported
                    // "all track uploads failed" for a call that was perfectly fine.
                    (stuck || client_pending, OrphanKind::StuckPipeline)
                }
                Ok(None) => {
                    // No backend row: pipeline crashed before
                    // create_call fired. Still an orphan — the audio
                    // on disk is real user intent we haven't given up
                    // on. Resume will push it through the full
                    // pipeline (create_call is idempotent on
                    // session_id).
                    (true, OrphanKind::NeverCreated)
                }
                Err(e) => {
                    // Network/auth failure cannot erase explicit local retry
                    // intent. Surface only checkpointed pending media; avoid
                    // prompting for every historical folder.
                    eprintln!(
                        "aftercalls: scan_orphans backend check failed for {session_id}: {e}",
                    );
                    (local_media_retryable, OrphanKind::StuckPipeline)
                }
            };
        if !needs_recovery {
            continue;
        }

        let age_secs = SystemTime::now()
            .duration_since(age_anchor)
            .unwrap_or(Duration::from_secs(0))
            .as_secs() as i64;
        let age_minutes = age_secs / 60;
        let recorded_at = parse_session_timestamp(&session_id)
            .unwrap_or_else(|| system_time_to_utc(age_anchor));

        let auto_resume_count = read_auto_resume_state(&session_dir)
            .map(|s| s.attempts)
            .unwrap_or(0);

        out.push(OrphanSession {
            session_dir,
            session_id,
            recorded_at,
            age_minutes,
            kind,
            auto_resume_count,
        });
    }

    // Oldest first so "Resume all" processes the earliest recording
    // first — matches what a user intuitively expects (chronological
    // catch-up), and avoids leaving the oldest orphan for last where
    // it's most likely to already be stale.
    out.sort_by_key(|o| o.recorded_at);
    out
}

/// Kick off the normal pipeline for this session_dir. Same code path
/// stop_recording uses, so AssemblyAI + OpenAI and upload get a fresh
/// run — the backend create_call upsert is keyed on (org, user,
/// session_id) so re-running just updates the existing row if there
/// already is one.
pub async fn resume(app: AppHandle, session_dir: PathBuf) {
    crate::pipeline::run(session_dir, app).await;
}

/// Delete the session folder on disk only after an explicit user discard or
/// call-deletion action.
pub async fn discard(session_dir: &Path) -> Result<(), String> {
    tokio::fs::remove_dir_all(session_dir)
        .await
        .map_err(|e| format!("remove {}: {e}", session_dir.display()))
}

/// Resolve a session_id coming in from the frontend back to its
/// absolute path under the recordings root. Used by the Tauri
/// commands so the frontend never needs to handle paths directly.
pub fn resolve_session_dir(app: &AppHandle, session_id: &str) -> Option<PathBuf> {
    let root = recordings_root(app)?;
    crate::session_fs::resolve_existing_dir(&root, session_id)
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn system_time_to_utc(t: SystemTime) -> DateTime<Utc> {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => Utc
            .timestamp_opt(d.as_secs() as i64, d.subsec_nanos())
            .single()
            .unwrap_or_else(Utc::now),
        Err(_) => Utc::now(),
    }
}

/// Parse a session_id that matches our standard %Y%m%dT%H%M%SZ format.
/// Accepts legacy, collision-suffixed, and import session ids. Anything else
/// falls back to the directory's mtime in the caller.
fn parse_session_timestamp(session_id: &str) -> Option<DateTime<Utc>> {
    crate::session_fs::parse_timestamp(session_id)
}

// ── #646 Layer C — auto-resume sweeper plumbing ──────────────────────

/// Eligibility check for the silent-auto-resume path. Returns
/// `Some(FailureClass)` when the session can be safely auto-resumed
/// (transient failure class, within the 30 min age window, fewer than
/// AUTO_RESUME_CAP prior attempts); returns `None` to fall through to
/// the prompted-UI orphan list.
///
/// The age + attempt-cap checks are filesystem-only (the `session_dir`
/// mtime + `auto_resume_state.json`); the failure-class check reads
/// the in-process telemetry ring buffer. A session whose
/// `pipeline::failed` event has already aged out of the ring (agent
/// restart, very old session) returns `None` here too — the user
/// makes the call on whether to retry from the prompted UI.
pub fn auto_resume_eligible(
    session_id: &str,
    session_dir: &Path,
) -> Option<FailureClass> {
    // Age gate first — cheap. If the dir is older than the cutoff,
    // skip even reading the state file.
    let age_minutes = mtime_of(session_dir)
        .or_else(|| mtime_of(&session_dir.join("mic.wav")))
        .or_else(|| mtime_of(&session_dir.join("mic.opus")))
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs() as i64 / 60)
        .unwrap_or(i64::MAX);
    if age_minutes > AUTO_RESUME_MAX_AGE_MINUTES {
        return None;
    }

    // Cap check — 3 strikes and the session falls through.
    let state = read_auto_resume_state(session_dir);
    let prior_attempts = state.as_ref().map(|s| s.attempts).unwrap_or(0);
    if prior_attempts >= AUTO_RESUME_CAP {
        return None;
    }

    // Failure class — pull from the in-process ring buffer. Match on
    // either the full session_str (absolute path) or the bare
    // session_id basename; pipeline.rs stamps the absolute path while
    // tests / older entries may use the bare basename.
    let (class, _ts) = crate::telemetry::recent_failure_for_session(session_id)?;
    match class {
        FailureClass::TransientNetwork
        | FailureClass::BackendFiveXx
        | FailureClass::AuthExpired => Some(class),
        FailureClass::DecodeError
        | FailureClass::SignatureMismatch
        | FailureClass::Other => None,
    }
}

/// One entry in the diagnostic result of `auto_resume_orphans`. Not
/// rendered in the UI today — tests assert on it and the staff
/// dashboard can correlate against `pipeline::auto_resume`
/// telemetry, but the live user surface is the silent pipeline run
/// itself.
#[derive(Serialize, Clone, Debug)]
pub struct AutoResumeResult {
    pub session_id: String,
    pub resumed: bool,
    /// Why this session was not auto-resumed (cap reached, age cutoff,
    /// non-transient class, no telemetry). `None` when `resumed` is
    /// true.
    pub reason: Option<String>,
}

/// Spawn an auto-resume pipeline for a single session. Helper that
/// `auto_resume_orphans` uses; broken out so the in-flight lock
/// bookkeeping is symmetric across the success / failure paths.
async fn spawn_auto_resume(
    app: AppHandle,
    session_dir: PathBuf,
    session_id: String,
    attempt_count: u8,
    prior_failure_class: Option<FailureClass>,
) {
    mark_in_flight(&session_id);
    // Bump the persisted counter BEFORE the pipeline kicks off so a
    // crash mid-run still counts against the cap (avoids retry loops
    // on a session that panics the agent).
    write_auto_resume_state(
        &session_dir,
        &AutoResumeState {
            attempts: attempt_count,
            last_failure_class: prior_failure_class.map(failure_class_token),
            last_attempt_ts: Utc::now(),
        },
    );

    crate::telemetry::log(
        "info",
        "pipeline::auto_resume",
        format!("auto-resume attempt {attempt_count} for session {session_id}"),
        Some(serde_json::json!({
            "session_id": session_id,
            "attempt_count": attempt_count,
            "prior_failure_class": prior_failure_class.map(failure_class_token),
        })),
        Some(session_dir.to_string_lossy().into_owned()),
    );

    // Spawn in a fresh task so the sweeper command can return after
    // dispatching all eligible sessions; each pipeline cleans up its
    // own in-flight slot when it completes.
    let session_id_for_cleanup = session_id.clone();
    tauri::async_runtime::spawn(async move {
        crate::pipeline::run_with_trigger(
            session_dir,
            app,
            crate::pipeline::PipelineTrigger::Auto {
                attempt_count,
            },
        )
        .await;
        clear_in_flight(&session_id_for_cleanup);
    });
}

fn failure_class_token(class: FailureClass) -> String {
    match class {
        FailureClass::TransientNetwork => "transient_network".into(),
        FailureClass::BackendFiveXx => "backend_5xx".into(),
        FailureClass::AuthExpired => "auth_expired".into(),
        FailureClass::DecodeError => "decode_error".into(),
        FailureClass::SignatureMismatch => "signature_mismatch".into(),
        FailureClass::Other => "other".into(),
    }
}

/// #646 Layer C — Tauri command driven by the Svelte sweeper's 5-min
/// interval. Re-runs `scan_orphans`, filters to auto-resumable
/// sessions, and dispatches a silent pipeline run for each one. The
/// command returns a `Vec<AutoResumeResult>` for diagnostic /
/// test purposes; the user-visible surface is the pipeline itself
/// (topstrip indicator picks up the next stage) plus the eventual
/// orphan-pill failure ceiling if all 3 attempts fail.
#[tauri::command]
pub async fn auto_resume_orphans(app: AppHandle) -> Result<Vec<AutoResumeResult>, String> {
    let orphans = scan_orphans(&app).await;
    let mut results: Vec<AutoResumeResult> = Vec::with_capacity(orphans.len());

    for orphan in orphans {
        let session_id = orphan.session_id.clone();
        let session_dir = orphan.session_dir.clone();

        // Double-check the in-flight set: scan_orphans already filters
        // through it, but this sweep may take time and another agent
        // path could have started a resume between scan and dispatch.
        if is_in_flight(&session_id) {
            results.push(AutoResumeResult {
                session_id,
                resumed: false,
                reason: Some("already in flight".into()),
            });
            continue;
        }

        match auto_resume_eligible(&session_id, &session_dir) {
            Some(class) => {
                let attempt_count = read_auto_resume_state(&session_dir)
                    .map(|s| s.attempts)
                    .unwrap_or(0)
                    + 1;
                spawn_auto_resume(
                    app.clone(),
                    session_dir,
                    session_id.clone(),
                    attempt_count,
                    Some(class),
                )
                .await;
                results.push(AutoResumeResult {
                    session_id,
                    resumed: true,
                    reason: None,
                });
            }
            None => {
                // Articulate the reason for the diagnostic blob. The
                // exact why-not isn't critical; the prompted UI will
                // surface the session shortly anyway.
                let reason = if read_auto_resume_state(&session_dir)
                    .map(|s| s.attempts)
                    .unwrap_or(0)
                    >= AUTO_RESUME_CAP
                {
                    "auto-resume cap reached"
                } else {
                    "ineligible (age, failure class, or no telemetry)"
                };
                results.push(AutoResumeResult {
                    session_id,
                    resumed: false,
                    reason: Some(reason.into()),
                });
            }
        }
    }

    Ok(results)
}

#[cfg(test)]
mod auto_resume_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Create a per-test temp dir under the system tmp root. We avoid
    /// pulling in `tempfile` (not currently a dep) and roll a tiny
    /// counter-suffixed helper instead. Dirs are best-effort cleaned
    /// when the test drops them; an orphaned tmp dir is harmless.
    struct ScratchDir {
        path: PathBuf,
    }
    impl ScratchDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("aftercalls-test-{pid}-{tag}-{n}"));
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Self { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn auto_resume_eligible_transient_returns_some() {
        // Hand-seed a `pipeline::failed` entry with a transient
        // failure class for an arbitrary session_id; verify
        // auto_resume_eligible returns Some(TransientNetwork). Uses a
        // scratch dir as the session_dir so the age check passes.
        let tmp = ScratchDir::new("transient");
        let session_id = "20260522T140158Z";
        // Create a mic.wav touch so mtime_of finds something. The
        // file's mtime is now → age 0 min → inside the 30 min window.
        std::fs::write(tmp.path().join("mic.wav"), &[0u8; 8]).expect("write mic.wav");

        crate::telemetry::log(
            "error",
            "pipeline::failed",
            "synthetic",
            Some(serde_json::json!({
                "failure_class": "transient_network",
            })),
            Some(session_id.to_string()),
        );

        let class = auto_resume_eligible(session_id, tmp.path());
        assert!(
            matches!(class, Some(FailureClass::TransientNetwork)),
            "expected Some(TransientNetwork), got {class:?}",
        );
    }

    #[test]
    fn auto_resume_eligible_decode_returns_none() {
        let tmp = ScratchDir::new("decode");
        let session_id = "20260522T140159Z";
        std::fs::write(tmp.path().join("mic.wav"), &[0u8; 8]).expect("write mic.wav");

        crate::telemetry::log(
            "error",
            "pipeline::failed",
            "synthetic decode",
            Some(serde_json::json!({
                "failure_class": "decode_error",
            })),
            Some(session_id.to_string()),
        );

        let class = auto_resume_eligible(session_id, tmp.path());
        assert!(class.is_none(), "decode_error must not auto-resume");
    }

    #[test]
    fn auto_resume_eligible_cap_reached_returns_none() {
        let tmp = ScratchDir::new("cap");
        let session_id = "20260522T140200Z";
        std::fs::write(tmp.path().join("mic.wav"), &[0u8; 8]).expect("write mic.wav");

        write_auto_resume_state(
            tmp.path(),
            &AutoResumeState {
                attempts: AUTO_RESUME_CAP,
                last_failure_class: Some("transient_network".into()),
                last_attempt_ts: Utc::now(),
            },
        );

        crate::telemetry::log(
            "error",
            "pipeline::failed",
            "synthetic capped",
            Some(serde_json::json!({
                "failure_class": "transient_network",
            })),
            Some(session_id.to_string()),
        );

        let class = auto_resume_eligible(session_id, tmp.path());
        assert!(
            class.is_none(),
            "cap reached must fall through to prompted UI, got {class:?}",
        );
    }
}
