use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};

use crate::config::Config;
use crate::transcription;

#[derive(Serialize, Clone)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PipelineEvent {
    Started { session_dir: String },
    Transcribing,
    Done { session_dir: String },
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
        Ok(()) => emit(
            &app,
            PipelineEvent::Done {
                session_dir: session_dir.to_string_lossy().into_owned(),
            },
        ),
        Err(e) => {
            eprintln!("callscribe: pipeline failed: {e:#}");
            emit(&app, PipelineEvent::Failed { error: format!("{e:#}") });
        }
    }
}

async fn run_inner(session_dir: &std::path::Path, app: &AppHandle) -> Result<()> {
    let config = Config::load()?;
    emit(app, PipelineEvent::Transcribing);
    transcription::transcribe_session(session_dir, &config).await?;
    Ok(())
}

fn emit(app: &AppHandle, event: PipelineEvent) {
    if let Err(e) = app.emit("pipeline", event) {
        eprintln!("callscribe: emit failed: {e}");
    }
}
