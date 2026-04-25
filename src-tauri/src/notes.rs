//! Manual notes taken during a call (#73).
//!
//! Notes live as `notes.md` inside the session_dir. The pipeline reads
//! that file at create_call time and ships the text to the backend;
//! post-pipeline edits happen in the portal via PATCH.
//!
//! Notes stay private to the user — they do NOT feed into the AI
//! summarizer. That decision lives in the code, not in a per-call
//! flag: users expressed that private scratch space is the useful
//! mental model, and a toggle would suggest otherwise.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

const NOTES_FILE: &str = "notes.md";

/// Hard cap on notes length — matches the backend's 100k limit so we
/// reject large paste-bombs before they hit the wire.
const MAX_NOTES_BYTES: usize = 100_000;

fn session_dir(app: &AppHandle, session_id: &str) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|p| p.join("recordings").join(session_id))
}

/// Read notes from a session_dir. Missing file → empty string. Used
/// by the pipeline right before create_call.
pub fn read_from_dir(session_dir: &Path) -> String {
    std::fs::read_to_string(session_dir.join(NOTES_FILE)).unwrap_or_default()
}

fn write_to_dir(session_dir: &Path, notes: &str) -> Result<()> {
    if notes.len() > MAX_NOTES_BYTES {
        anyhow::bail!("notes exceed {MAX_NOTES_BYTES} bytes");
    }
    if !session_dir.exists() {
        std::fs::create_dir_all(session_dir)
            .with_context(|| format!("create {}", session_dir.display()))?;
    }
    std::fs::write(session_dir.join(NOTES_FILE), notes)
        .with_context(|| format!("write {NOTES_FILE}"))?;
    Ok(())
}

#[tauri::command]
pub fn save_notes(app: AppHandle, session_id: String, notes: String) -> Result<(), String> {
    let dir = session_dir(&app, &session_id).ok_or_else(|| "no app_local_data_dir".to_string())?;
    write_to_dir(&dir, &notes).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_notes(app: AppHandle, session_id: String) -> Result<String, String> {
    let dir = session_dir(&app, &session_id).ok_or_else(|| "no app_local_data_dir".to_string())?;
    if !dir.exists() {
        return Ok(String::new());
    }
    Ok(read_from_dir(&dir))
}

/// PATCH the notes field on an existing call row. Called from the
/// call-detail page after the pipeline has completed, so the
/// session_dir on disk is no longer the source of truth — the
/// backend row is.
#[tauri::command]
pub async fn update_call_notes(
    call_id: String,
    notes: String,
) -> Result<(), crate::error::PortalError> {
    let cfg = crate::config::Config::load().map_err(crate::error::PortalError::from)?;
    let backend = cfg.backend.as_ref().ok_or_else(|| crate::error::PortalError::Other {
        message: "no backend configured".into(),
    })?;
    crate::portal::update_call_notes(backend, &call_id, &notes).await
}
