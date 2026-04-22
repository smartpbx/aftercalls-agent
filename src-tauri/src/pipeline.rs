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
    /// Emitted the moment the transcribe step returns successfully,
    /// *before* summarize kicks in. This is when the call has a
    /// usable transcript but no title/summary/action-items yet —
    /// enough for the agent UI to surface "Open on web" / "Open in
    /// app" so the user can start reading while the rest of the
    /// pipeline fills in live (call-detail page polls until
    /// status='complete').
    Transcribed { session_dir: String, call_id: String },
    Summarizing,
    WritingNote,
    Done { session_dir: String, note_path: String, call_id: String },
    Failed { error: String },
}

/// Count of pipeline tasks currently in flight. Bumped at the top of
/// `run` and decremented at the end regardless of success/failure. The
/// tray "Quit" handler reads this (via is_pipeline_active) so it can
/// ask for confirmation before exiting while work is still in progress
/// (#62). AtomicUsize because multiple back-to-back recordings can
/// pipeline concurrently if the user stops/starts fast.
static PIPELINE_IN_FLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn is_pipeline_active() -> bool {
    PIPELINE_IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) > 0
}

pub async fn run(session_dir: PathBuf, app: AppHandle) {
    PIPELINE_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    // Scope guard in case any early-return is introduced later —
    // today the function can't panic mid-body because both branches
    // of the match already run the telemetry flush, but belt-and-
    // suspenders keeps the counter honest.
    struct Decrement;
    impl Drop for Decrement {
        fn drop(&mut self) {
            PIPELINE_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let _guard = Decrement;

    crate::tray_set_processing(&app);
    let session_str = session_dir.to_string_lossy().into_owned();
    crate::telemetry::log(
        "info",
        "pipeline::start",
        "pipeline started",
        None,
        Some(session_str.clone()),
    );
    emit(
        &app,
        PipelineEvent::Started {
            session_dir: session_str.clone(),
        },
    );
    match run_inner(&session_dir, &app).await {
        Ok((note_path, call_id)) => {
            let note_str = note_path.to_string_lossy().into_owned();
            notify_done(&app, &note_path);
            crate::telemetry::log(
                "info",
                "pipeline::done",
                "pipeline done",
                Some(serde_json::json!({ "call_id": call_id })),
                Some(session_str.clone()),
            );
            emit(
                &app,
                PipelineEvent::Done {
                    session_dir: session_str.clone(),
                    note_path: note_str,
                    call_id,
                },
            );
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            eprintln!("aftercalls: pipeline failed: {err_str}");
            crate::telemetry::log(
                "error",
                "pipeline::failed",
                err_str.clone(),
                None,
                Some(session_str.clone()),
            );
            let _ = app
                .notification()
                .builder()
                .title("aftercalls: transcription failed")
                .body(err_str.clone())
                .show();
            emit(&app, PipelineEvent::Failed { error: err_str });
        }
    }
    // Ship whatever's buffered so reported bugs don't wait on the
    // next 30s tick.
    let _ = crate::telemetry::flush_now().await;
    crate::tray_set_idle(&app);
}

async fn run_inner(session_dir: &Path, app: &AppHandle) -> Result<(PathBuf, String)> {
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
    // Previously mixed was uploaded as raw WAV — turned every call
    // into a multi-MB upload long after mic/system had already
    // landed. Opus-compress it the same way; upload.rs falls back
    // to the wav if ffmpeg is missing (Windows stock).
    compress_for_upload(&session_dir.join("mixed.wav")).await.ok();

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
    // Signal to the UI that the transcript is in and the call is
    // now openable — the call-detail route polls for summary /
    // action-items while the rest of the pipeline continues.
    emit(
        app,
        PipelineEvent::Transcribed {
            session_dir: session_dir.to_string_lossy().into_owned(),
            call_id: created.call_id.clone(),
        },
    );

    // Step 6: backend summarize (OpenAI with the org's key).
    // Vault is optional — if the user hasn't enabled Obsidian
    // integration, we skip client candidates (no matching) and skip
    // the note-writing step entirely. Transcription + summary + call
    // row still land on the backend exactly the same way.
    emit(app, PipelineEvent::Summarizing);
    let vault = config.vault.as_ref();
    let candidates: Vec<String> = match vault {
        Some(v) => vault::list_clients(v).unwrap_or_else(|e| {
            eprintln!("aftercalls: list_clients failed, summarizing without client candidates: {e:#}");
            Vec::new()
        }),
        None => Vec::new(),
    };
    let summary_json =
        portal::summarize(backend, &created.call_id, &serde_json::to_value(&transcript)?, &candidates)
            .await?;
    let summary: Summary =
        serde_json::from_value(summary_json).context("decode summary from backend")?;

    // Step 7: write the vault note. Skipped when vault isn't
    // configured — everything else already landed in the DB. The
    // return path here becomes the session_dir so the tray
    // "saved" notification still has something to surface.
    let note_path = if let Some(v) = vault {
        emit(app, PipelineEvent::WritingNote);
        match vault::write_note(v, &summary, &transcript, session_dir, &candidates) {
            Ok(p) => {
                // Step 8: attach the note path onto the call row for
                // portal deep-linking.
                if let Err(e) = upload::attach_note_path(backend, &created.call_id, &p).await {
                    eprintln!("aftercalls: attach note path failed: {e:#}");
                }
                p
            }
            Err(e) => {
                eprintln!("aftercalls: vault write failed: {e:#}");
                session_dir.to_path_buf()
            }
        }
    } else {
        session_dir.to_path_buf()
    };

    // Step 9: peaks, fire-and-forget — failure doesn't block the user
    // from seeing their note land.
    let backend_clone = backend.clone();
    let call_id = created.call_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = portal::generate_peaks(&backend_clone, &call_id).await {
            eprintln!("aftercalls: peaks generation failed: {e:#}");
        }
    });

    Ok((note_path, created.call_id))
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

/// Locate the ffmpeg binary. Prefers the sidecar bundled with the app
/// (next to the main executable — Tauri's externalBin mechanism puts
/// it there at bundle time on every platform), falls back to PATH so
/// `pnpm tauri:dev` and distros that install ffmpeg via Depends still
/// work. Returns a string suitable for Command::new.
///
/// Rationale: previously we called ffmpeg by bare name, which silently
/// no-op'd on Windows (where ffmpeg isn't pre-installed) — mix_tracks
/// failed, mixed.wav never landed, and Spaces stored only mic + system.
/// The backend now repairs that (see #51), but shipping a working
/// ffmpeg next to the binary skips the recovery round-trip entirely.
pub fn ffmpeg_binary() -> std::ffi::OsString {
    // The sidecar is named `ffmpeg-aftercalls` (not `ffmpeg`) so it
    // doesn't collide with a system ffmpeg in /usr/bin on Linux .deb
    // installs. Windows appends `.exe`. Tauri's externalBin drops the
    // target-triple suffix at bundle time, so on disk it's just the
    // bare name here.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sidecar = if cfg!(windows) {
                dir.join("ffmpeg-aftercalls.exe")
            } else {
                dir.join("ffmpeg-aftercalls")
            };
            if sidecar.exists() {
                return sidecar.into_os_string();
            }
        }
    }
    std::ffi::OsString::from("ffmpeg")
}

async fn mix_tracks(session_dir: &Path) -> Result<()> {
    let mic = session_dir.join("mic.wav");
    let system = session_dir.join("system.wav");
    let mixed = session_dir.join("mixed.wav");
    if !mic.exists() || !system.exists() {
        return Ok(());
    }
    let status = tokio::process::Command::new(ffmpeg_binary())
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
    let status = tokio::process::Command::new(ffmpeg_binary())
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
