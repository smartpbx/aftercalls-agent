//! Post-recording orchestration. Steps:
//!
//! 1. Mix mic + system into mixed.wav (best-effort)
//! 2. Compress mic + system to .opus (AssemblyAI consumes Opus directly)
//! 3. Create a pending call row on the backend, collect upload URLs
//! 4. PUT the audio tracks to Spaces via the upload URLs
//! 5. Ask backend to transcribe — AssemblyAI runs server-side, backend
//!    persists utterances, returns the merged transcript
//! 6. Ask backend to summarize — OpenAI runs server-side, backend
//!    persists title/summary/action_items, returns the summary
//! 7. Write the local Obsidian vault note with the transcript + summary
//! 8. Attach note path back to the call row
//! 9. Fire-and-forget peaks generation
//!
//! Transcription and summarization used to run locally against per-user
//! API keys; now only the backend has the keys.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::config::Config;
use crate::summary::Summary;
use crate::transcription::MergedTranscript;
use crate::{portal, upload, vault};

#[derive(Serialize, Clone)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PipelineEvent {
    Started { session_dir: String },
    Uploading,
    Transcribing,
    Summarizing,
    WritingNote,
    Done { session_dir: String, note_path: String },
    Failed { error: String },
}

pub async fn run(session_dir: PathBuf, app: AppHandle) {
    crate::tray_set_processing(&app);
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
            eprintln!("aftercalls: pipeline failed: {e:#}");
            let _ = app
                .notification()
                .builder()
                .title("aftercalls: transcription failed")
                .body(format!("{e:#}"))
                .show();
            emit(&app, PipelineEvent::Failed { error: format!("{e:#}") });
        }
    }
    crate::tray_set_idle(&app);
}

async fn run_inner(session_dir: &Path, app: &AppHandle) -> Result<PathBuf> {
    let config = Config::load()?;
    let backend = config
        .backend
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no backend configured in config.toml"))?;

    // Steps 1–2: mix + compress. Both are best-effort; the pipeline can
    // still upload + transcribe with whichever tracks landed.
    if let Err(e) = mix_tracks(session_dir).await {
        eprintln!("aftercalls: mix failed: {e:#}");
    }
    compress_for_upload(&session_dir.join("mic.wav")).await.ok();
    compress_for_upload(&session_dir.join("system.wav")).await.ok();

    // Step 3: create the call row so we get upload URLs.
    emit(app, PipelineEvent::Uploading);
    let created = upload::create_call(backend, session_dir, 0).await?;

    // Step 4: PUT audio to Spaces.
    upload::upload_audio(session_dir, &created.upload_urls).await?;

    // Step 5: backend transcribe (AssemblyAI with the org's key).
    emit(app, PipelineEvent::Transcribing);
    let transcript_json = portal::transcribe(backend, &created.call_id).await?;
    let transcript: MergedTranscript = serde_json::from_value(transcript_json)
        .context("decode transcript from backend")?;

    // Step 6: backend summarize (OpenAI with the org's key).
    emit(app, PipelineEvent::Summarizing);
    let candidates = vault::list_clients(&config.vault)?;
    let summary_json =
        portal::summarize(backend, &created.call_id, &serde_json::to_value(&transcript)?, &candidates)
            .await?;
    let summary: Summary =
        serde_json::from_value(summary_json).context("decode summary from backend")?;

    // Step 7: write the vault note. Everything else is now in the DB;
    // this is the one bit of work that has to stay local because the
    // user's Obsidian vault is local-first by design.
    emit(app, PipelineEvent::WritingNote);
    let note_path =
        vault::write_note(&config.vault, &summary, &transcript, session_dir, &candidates)?;

    // Step 8: attach the note path back onto the row so the portal can
    // link out (or we can later use it for Obsidian URI deep-links).
    if let Err(e) = upload::attach_note_path(backend, session_dir, &note_path).await {
        eprintln!("aftercalls: attach note path failed: {e:#}");
    }

    // Step 9: peaks, fire-and-forget — failure doesn't block the user
    // from seeing their note land.
    let backend_clone = backend.clone();
    let call_id = created.call_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = portal::generate_peaks(&backend_clone, &call_id).await {
            eprintln!("aftercalls: peaks generation failed: {e:#}");
        }
    });

    Ok(note_path)
}

fn notify_done(app: &AppHandle, note_path: &Path) {
    let title = note_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "call saved".to_string());
    let _ = app
        .notification()
        .builder()
        .title("aftercalls: note saved")
        .body(title)
        .show();
}

fn emit(app: &AppHandle, event: PipelineEvent) {
    if let Err(e) = app.emit("pipeline", event) {
        eprintln!("aftercalls: emit failed: {e}");
    }
}

async fn mix_tracks(session_dir: &Path) -> Result<()> {
    let mic = session_dir.join("mic.wav");
    let system = session_dir.join("system.wav");
    let mixed = session_dir.join("mixed.wav");
    if !mic.exists() || !system.exists() {
        return Ok(());
    }
    let status = tokio::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&mic)
        .arg("-i")
        .arg(&system)
        .arg("-filter_complex")
        .arg("[0:a][1:a]amix=inputs=2:duration=longest:normalize=0")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg(&mixed)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("ffmpeg amix failed: {status}");
    }
    Ok(())
}

/// 16 kHz mono Opus @ 32kbps — ~15× smaller than the raw WAV with no
/// quality loss the transcription model would care about. Output sits
/// next to the input with a `.opus` extension; callers use it by path.
async fn compress_for_upload(input: &Path) -> Result<PathBuf> {
    if !input.exists() {
        anyhow::bail!("{} not present", input.display());
    }
    let output = input.with_extension("opus");
    let status = tokio::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("libopus")
        .arg("-b:a")
        .arg("32k")
        .arg(&output)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("run ffmpeg")?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with {status}");
    }
    Ok(output)
}
