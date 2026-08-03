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
//!
//! #531 — optional Note-title also lives here. The title file is
//! `title.txt` in the same session_dir; it's read alongside notes at
//! create_call time and sent as `title: Some(...)` so the call-list
//! shows the user-chosen name instead of "(untitled)".

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

const NOTES_FILE: &str = "notes.md";
/// #531 — note title sidecar file.
const TITLE_FILE: &str = "title.txt";

/// Hard cap on notes length — matches the backend's 100k limit so we
/// reject large paste-bombs before they hit the wire.
const MAX_NOTES_BYTES: usize = 100_000;

fn recordings_root(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|path| path.join("recordings"))
}

fn existing_session_dir(app: &AppHandle, session_id: &str) -> Option<PathBuf> {
    crate::session_fs::resolve_existing_dir(&recordings_root(app)?, session_id)
}

/// Read notes from a session_dir. Missing file → empty string. Used
/// by the pipeline right before create_call.
pub fn read_from_dir(session_dir: &Path) -> String {
    std::fs::read_to_string(session_dir.join(NOTES_FILE)).unwrap_or_default()
}

/// #531 — Read an optional note title from the session_dir.
/// Missing file → None. Sent as `title` in create_call so the user-
/// provided label replaces "(untitled)" on the call list.
pub fn read_title_from_dir(session_dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(session_dir.join(TITLE_FILE)).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

fn write_to_dir(session_dir: &Path, notes: &str) -> Result<()> {
    if notes.len() > MAX_NOTES_BYTES {
        anyhow::bail!("notes exceed {MAX_NOTES_BYTES} bytes");
    }
    crate::session_fs::ensure_private_dir(session_dir)
        .with_context(|| format!("create {}", session_dir.display()))?;
    crate::session_fs::write_private_file(&session_dir.join(NOTES_FILE), notes.as_bytes())
        .with_context(|| format!("write {NOTES_FILE}"))?;
    Ok(())
}

#[tauri::command]
pub fn save_notes(app: AppHandle, session_id: String, notes: String) -> Result<(), String> {
    let dir = existing_session_dir(&app, &session_id)
        .ok_or_else(|| "recording session was not found".to_string())?;
    write_to_dir(&dir, &notes).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_notes(app: AppHandle, session_id: String) -> Result<String, String> {
    if !crate::session_fs::valid_session_id(&session_id) {
        return Err("invalid recording session".into());
    }
    let Some(root) = recordings_root(&app) else {
        return Err("no app_local_data_dir".into());
    };
    let Some(dir) = crate::session_fs::resolve_existing_dir(&root, &session_id) else {
        return Ok(String::new());
    };
    Ok(read_from_dir(&dir))
}

/// #531 — Persist a user-provided title for a note-to-self session.
/// Written to `title.txt` in the session_dir; the pipeline reads it at
/// create_call time and sends it as the call's `title` field so the
/// call-list shows the chosen label rather than "(untitled)".
/// Hard cap: 200 bytes (prevents storing a runaway paste).
#[tauri::command]
pub fn save_title(app: AppHandle, session_id: String, title: String) -> Result<(), String> {
    const MAX_TITLE_BYTES: usize = 200;
    if title.len() > MAX_TITLE_BYTES {
        return Err(format!("title exceeds {MAX_TITLE_BYTES} bytes"));
    }
    let dir = existing_session_dir(&app, &session_id)
        .ok_or_else(|| "recording session was not found".to_string())?;
    crate::session_fs::write_private_file(&dir.join(TITLE_FILE), title.trim().as_bytes())
        .map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use crate::session_fs::valid_session_id;

    #[test]
    fn session_id_is_exactly_one_safe_path_component() {
        assert!(valid_session_id("20260731T101112Z_0123456789abcdef"));
        assert!(valid_session_id("imp_20260731T101112Z_0123456789abcdef"));
        for unsafe_id in ["", ".", "..", "../other", "sub/other", "sub\\other", "/tmp/x"] {
            assert!(!valid_session_id(unsafe_id), "accepted {unsafe_id:?}");
        }
    }
}
