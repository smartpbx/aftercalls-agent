//! Manual notes taken during a call (#73).
//!
//! Persistence: two files in the session_dir.
//!   - `notes.md` — the raw markdown text (empty string = no notes).
//!   - `notes_meta.json` — `{"include_in_summary": bool}`.
//!
//! Why two files: the notes are what the user types; the meta flag is
//! independent of the text and would otherwise force us to parse
//! frontmatter or reserve a marker line. Keeping them split means a
//! crash mid-typing can't corrupt the flag.
//!
//! The pipeline reads these at create_call time and ships them to the
//! backend. Post-pipeline edits happen on the portal via PATCH and do
//! not round-trip through the agent.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

const NOTES_FILE: &str = "notes.md";
const META_FILE: &str = "notes_meta.json";

/// Hard cap on notes length — matches the backend's 100k limit so we
/// reject large paste-bombs before they hit the wire.
const MAX_NOTES_BYTES: usize = 100_000;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct NotesMeta {
    include_in_summary: bool,
}

impl Default for NotesMeta {
    fn default() -> Self {
        Self {
            include_in_summary: true,
        }
    }
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct LoadedNotes {
    pub notes: String,
    pub include_in_summary: bool,
}

/// Resolve the session_dir for a given session_id (matches recorder +
/// recovery's convention: app_local_data_dir/recordings/<session_id>).
fn session_dir(app: &AppHandle, session_id: &str) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|p| p.join("recordings").join(session_id))
}

/// Read notes + meta from a session_dir. Missing files → defaults
/// (empty text, include=true). Used by the pipeline right before
/// create_call.
pub fn read_from_dir(session_dir: &Path) -> LoadedNotes {
    let notes = std::fs::read_to_string(session_dir.join(NOTES_FILE))
        .unwrap_or_default();
    let meta: NotesMeta = std::fs::read_to_string(session_dir.join(META_FILE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    LoadedNotes {
        notes,
        include_in_summary: meta.include_in_summary,
    }
}

fn write_to_dir(session_dir: &Path, notes: &str, include_in_summary: bool) -> Result<()> {
    if notes.len() > MAX_NOTES_BYTES {
        anyhow::bail!("notes exceed {MAX_NOTES_BYTES} bytes");
    }
    if !session_dir.exists() {
        std::fs::create_dir_all(session_dir)
            .with_context(|| format!("create {}", session_dir.display()))?;
    }
    std::fs::write(session_dir.join(NOTES_FILE), notes)
        .with_context(|| format!("write {NOTES_FILE}"))?;
    let meta = NotesMeta { include_in_summary };
    let json = serde_json::to_string(&meta).context("serialize notes meta")?;
    std::fs::write(session_dir.join(META_FILE), json)
        .with_context(|| format!("write {META_FILE}"))?;
    Ok(())
}

#[tauri::command]
pub fn save_notes(
    app: AppHandle,
    session_id: String,
    notes: String,
    include_in_summary: bool,
) -> Result<(), String> {
    let dir = session_dir(&app, &session_id).ok_or_else(|| "no app_local_data_dir".to_string())?;
    write_to_dir(&dir, &notes, include_in_summary).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_notes(app: AppHandle, session_id: String) -> Result<LoadedNotes, String> {
    let dir = session_dir(&app, &session_id).ok_or_else(|| "no app_local_data_dir".to_string())?;
    if !dir.exists() {
        return Ok(LoadedNotes::default());
    }
    Ok(read_from_dir(&dir))
}
