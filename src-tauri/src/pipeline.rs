use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::config::Config;
use crate::{summary, transcription, upload, vault};

#[derive(Serialize, Clone)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PipelineEvent {
    Started { session_dir: String },
    Transcribing,
    Summarizing,
    WritingNote,
    Uploading,
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
        Ok(note_path) => {
            let note_str = note_path.to_string_lossy().into_owned();
            notify_done(&app, &note_path);
            emit(
                &app,
                PipelineEvent::Done {
                    session_dir: session_dir.to_string_lossy().into_owned(),
                    note_path: note_str,
                },
            );
        }
        Err(e) => {
            eprintln!("callscribe: pipeline failed: {e:#}");
            let _ = app
                .notification()
                .builder()
                .title("callscribe: transcription failed")
                .body(format!("{e:#}"))
                .show();
            emit(&app, PipelineEvent::Failed { error: format!("{e:#}") });
        }
    }
}

fn notify_done(app: &AppHandle, note_path: &std::path::Path) {
    let title = note_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "call saved".to_string());
    let _ = app
        .notification()
        .builder()
        .title("callscribe: note saved")
        .body(title)
        .show();
}

async fn run_inner(session_dir: &std::path::Path, app: &AppHandle) -> Result<PathBuf> {
    let config = Config::load()?;

    emit(app, PipelineEvent::Transcribing);
    let transcript = transcription::transcribe_session(session_dir, &config).await?;

    emit(app, PipelineEvent::Summarizing);
    let candidates = vault::list_clients(&config.vault)?;
    let summary = summary::generate(session_dir, &transcript, &config, &candidates).await?;

    emit(app, PipelineEvent::WritingNote);
    let note_path =
        vault::write_note(&config.vault, &summary, &transcript, session_dir, &candidates)?;

    if let Some(backend) = &config.backend {
        emit(app, PipelineEvent::Uploading);
        if let Err(e) = upload::post_call(backend, &transcript, &summary, session_dir, &note_path)
            .await
        {
            // Don't fail the whole pipeline if the backend is down — local
            // note already landed in the vault.
            eprintln!("callscribe: backend upload failed: {e:#}");
        }
    }

    Ok(note_path)
}

fn emit(app: &AppHandle, event: PipelineEvent) {
    if let Err(e) = app.emit("pipeline", event) {
        eprintln!("callscribe: emit failed: {e}");
    }
}
