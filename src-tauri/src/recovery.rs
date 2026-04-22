//! Orphan session recovery (#63).
//!
//! When the agent starts up, a session_dir containing audio but no
//! matching `status = 'complete'` call row on the backend means a
//! previous process crashed, was force-quit, or lost its connection
//! partway through the pipeline. We scan for these on launch, offer
//! the user a chance to resume them (re-run the pipeline from the
//! top — AssemblyAI + OpenAI are idempotent for our purposes), and
//! auto-clean anything older than 7 days so the recordings directory
//! doesn't grow without bound.
//!
//! A session_dir counts as an orphan when ALL of:
//!   1. The dir contains mic.wav OR mic.opus AND is older than 5
//!      minutes (so we don't snapshot a recording still in the act of
//!      uploading). Age is measured against mic.wav's mtime (most
//!      robust on filesystems where ctime can drift) with a fallback
//!      to the dir's ctime.
//!   2. The backend HAS a row for this session_id. A missing row
//!      means the pipeline never reached create_call (step 3 of 9) —
//!      those are abandoned-before-upload folders, not "crashed
//!      mid-pipeline." Typical source: dev Start→Stop cycles that
//!      never got to upload, old recordings from before the user
//!      signed in, etc. Surfacing those as "unfinished calls" is a
//!      false-positive that on dev machines accumulates to dozens.
//!   3. That row's status is not 'complete'. Anything in
//!      uploading/transcribed/summarizing/failed means the user had
//!      real intent to process this recording and didn't get to the
//!      finish line.

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};

use crate::config::Config;

/// Minimum age of a session_dir before we consider it for recovery.
/// Shorter than this and it might still be in the act of uploading —
/// the pipeline is kicked off async from stop_recording, so a folder
/// created 30s ago is almost certainly mid-pipeline not orphaned.
const MIN_AGE_MINUTES: i64 = 5;

/// Anything older than this gets silently deleted regardless of
/// backend state. Keeps the recordings folder from accumulating
/// dismissed orphans forever.
const AUTO_CLEAN_DAYS: i64 = 7;

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
}

/// Resolve the base recordings directory. Matches what start_recording
/// uses (app_local_data_dir/recordings).
fn recordings_root(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|p| p.join("recordings"))
}

/// Iterate the recordings directory and return every dir that meets
/// the orphan criteria. Also silently deletes anything older than
/// AUTO_CLEAN_DAYS while we're iterating — keeps the cleanup in one
/// pass without a separate scheduled task.
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
    let mut candidates: Vec<(PathBuf, String, SystemTime)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let session_id = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Need audio to be considered. mic.opus (compressed) or
        // mic.wav (raw — compression may have never run).
        let mic_wav = path.join("mic.wav");
        let mic_opus = path.join("mic.opus");
        if !mic_wav.exists() && !mic_opus.exists() {
            continue;
        }

        // Age: prefer mic.wav mtime, fall back to the dir's mtime.
        // Both come from std::fs::Metadata.modified() — ctime on unix
        // is actually inode-change-time and flips on chmod/rename,
        // which is worse noise than mtime for "when did we stop
        // writing to this dir".
        let age_anchor = mtime_of(&mic_wav)
            .or_else(|| mtime_of(&mic_opus))
            .or_else(|| mtime_of(&path))
            .unwrap_or(now);

        let age_secs = now
            .duration_since(age_anchor)
            .unwrap_or(Duration::from_secs(0))
            .as_secs() as i64;
        let age_minutes = age_secs / 60;

        // Auto-clean path: anything older than AUTO_CLEAN_DAYS gets
        // nuked silently and never surfaces in the orphan list.
        if age_minutes >= AUTO_CLEAN_DAYS * 24 * 60 {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                eprintln!(
                    "aftercalls: auto-clean remove {} failed: {e}",
                    path.display()
                );
            } else {
                eprintln!(
                    "aftercalls: auto-cleaned stale session {} ({}d old)",
                    session_id,
                    age_minutes / (24 * 60)
                );
            }
            continue;
        }

        // Too-new gate: skip anything that could still be mid-pipeline.
        if age_minutes < MIN_AGE_MINUTES {
            continue;
        }

        candidates.push((path, session_id, age_anchor));
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

    for (session_dir, session_id, age_anchor) in candidates {
        let needs_recovery =
            match crate::portal::get_call_by_session(backend, &session_id).await {
                Ok(Some(v)) => {
                    // Backend knows this recording. Orphan iff the row
                    // didn't reach complete. Missing status field is
                    // treated as non-complete for safety.
                    v.get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s != "complete")
                        .unwrap_or(true)
                }
                Ok(None) => {
                    // No backend row: this folder was abandoned before
                    // create_call fired. Not a crash to recover —
                    // probably a Start→Stop dev cycle, a pre-sign-in
                    // recording, or a session the user explicitly
                    // chose not to upload. Skip.
                    false
                }
                Err(e) => {
                    // Network / auth failure. Don't surface — we'd
                    // rather under-report orphans than prompt on every
                    // folder when the backend is briefly unreachable.
                    eprintln!(
                        "aftercalls: scan_orphans backend check failed for {session_id}: {e}",
                    );
                    false
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

        out.push(OrphanSession {
            session_dir,
            session_id,
            recorded_at,
            age_minutes,
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

/// Delete the session folder on disk. Called both from the UI
/// (user clicked Discard) and from the auto-clean branch in scan_orphans.
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
    let p = root.join(session_id);
    // Light sanity check — the session_id must name a direct child of
    // root and the folder must actually exist. Blocks any attempt to
    // smuggle "../" through the IPC boundary, even though the Tauri
    // command signature already constrains it to a string.
    if p.parent() != Some(&root) {
        return None;
    }
    if !p.exists() {
        return None;
    }
    Some(p)
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
/// Returns None for imports ("imp_...") or anything else — the caller
/// falls back to the directory's mtime in that case.
fn parse_session_timestamp(session_id: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(session_id, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|ndt| ndt.and_utc())
}
