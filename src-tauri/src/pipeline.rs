use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};

use crate::config::Config;
use crate::{summary, transcription, vault};

#[derive(Serialize, Clone)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PipelineEvent {
    Started { session_dir: String },
    Transcribing,
    Summarizing,
    WritingNote,
    Done { session_dir: String, note_path: String },
    Failed { error: String },
}

pub async fn run(session_dir: PathBuf, app: AppHandle) {
    emit(
        &app,
        PipelineEvent::Started {
            session_dir: session_dir.to_string_lossy().into_owned(),
        },
    );
    match run_inner(&session_dir, &app).await {
        Ok(note_path) => emit(
            &app,
            PipelineEvent::Done {
                session_dir: session_dir.to_string_lossy().into_owned(),
                note_path: note_path.to_string_lossy().into_owned(),
            },
        ),
        Err(e) => {
            eprintln!("callscribe: pipeline failed: {e:#}");
            emit(&app, PipelineEvent::Failed { error: format!("{e:#}") });
        }
    }
}

async fn run_inner(session_dir: &std::path::Path, app: &AppHandle) -> Result<PathBuf> {
    let config = Config::load()?;

    emit(app, PipelineEvent::Transcribing);
    let transcript = transcription::transcribe_session(session_dir, &config).await?;

    emit(app, PipelineEvent::Summarizing);
    let candidates = vault::list_clients(&config.vault)?;
    let summary = summary::generate(session_dir, &transcript, &config, &candidates).await?;

    emit(app, PipelineEvent::WritingNote);
    let note_path = vault::write_note(&config.vault, &summary, &transcript, session_dir, &candidates)?;
    Ok(note_path)
}

fn emit(app: &AppHandle, event: PipelineEvent) {
    if let Err(e) = app.emit("pipeline", event) {
        eprintln!("callscribe: emit failed: {e}");
    }
}
