//! Post-recording orchestration. Steps:
//!
//! 1. Consume the rolling encoder's explicit finalization report. A track is
//!    uploadable only after clean encode exit plus full decode/duration
//!    validation; any other state is safely re-encoded from the finalized WAV.
//!    `mixed` is NOT produced — the backend regenerates it server-side.
//! 2. Create or resume the pending call row on the backend.
//! 3. Create or resume immutable mic/system media generations, retaining
//!    local bytes until the backend proves each generation ready/current.
//! 4. Ask backend to transcribe — the vendor runs server-side, backend
//!    persists utterances, returns the merged transcript
//! 5. Ask backend to summarize — runs server-side, backend persists
//!    title/summary/action_items, returns the summary
//! 6. Write the local Obsidian vault note with the transcript + summary
//! 7. Attach note path back to the call row
//! 8. Fire-and-forget peaks generation (also regenerates mixed server-side)
//!
//! Transcription and summarization used to run locally against per-user
//! API keys; now only the backend has the keys.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::config::Config;
use crate::portal::RetryGuard;
use crate::rolling_encode::{
    fallback_encode_track, RollingFinalizationReport, TrackPublicationState,
};
use crate::summary::Summary;
use crate::transcription::MergedTranscript;
use crate::{portal, upload, vault};

#[derive(Serialize, Clone)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PipelineEvent {
    /// #646 Layer C — `triggered_by` distinguishes a user-initiated
    /// stop_recording from a Layer C auto-resume sweep. The Svelte
    /// sweeper treats `"auto"` silently (no toast / no tray flash)
    /// while `"user"` keeps today's UX. Field defaulted to `"user"`
    /// on the (de)serializer side via `#[serde(default = ...)]` is
    /// not possible on a tuple-variant; instead we emit the field on
    /// every Started and let the frontend default it if absent.
    Started {
        session_dir: String,
        triggered_by: &'static str,
    },
    /// #646 Layer C — emitted when the auto-resume sweeper kicks off a
    /// pipeline for a previously-failed session. Distinct from
    /// `Started { triggered_by: "auto" }` so the Svelte sweeper can
    /// react with a quieter UI path while the original `Started` event
    /// shape stays backwards-compatible with anything reading older
    /// telemetry.
    Resuming {
        session_dir: String,
        attempt_count: u8,
    },
    Uploading,
    Transcribing,
    /// #646 Layer A — emitted while an HTTP step is between retry
    /// attempts. The Svelte topstrip shows a "Retrying…" indicator
    /// instead of the stage label. `step` is the same `step` token
    /// `retry_http` uses (e.g. "transcribe", "put_file"). Optional
    /// `wait_ms` lets the UI show a "next attempt in Xs" hint —
    /// today's spec uses it as advisory only.
    Retrying {
        step: &'static str,
        attempt: u8,
        wait_ms: u64,
    },
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

/// What kicked off this pipeline run. Drives the `Started` event's
/// `triggered_by` field, and (for `Auto`) the additional `Resuming`
/// breadcrumb the Svelte sweeper subscribes to. Defaults to `User`
/// for backwards-compat with `pub fn run(session_dir, app)`.
#[derive(Clone, Copy, Debug)]
pub enum PipelineTrigger {
    User,
    Auto { attempt_count: u8 },
}

impl PipelineTrigger {
    fn token(&self) -> &'static str {
        match self {
            PipelineTrigger::User => "user",
            PipelineTrigger::Auto { .. } => "auto",
        }
    }
}

pub async fn run(session_dir: PathBuf, app: AppHandle) {
    let rolling = RollingFinalizationReport::conservative(&session_dir);
    run_with_trigger_and_report(session_dir, app, PipelineTrigger::User, rolling).await
}

/// Entry point used only by the live Stop path. Unlike recovery/import runs,
/// it carries current-process proof for each atomically published rolling
/// track.
pub async fn run_after_stop(
    session_dir: PathBuf,
    app: AppHandle,
    rolling: RollingFinalizationReport,
) {
    run_with_trigger_and_report(session_dir, app, PipelineTrigger::User, rolling).await
}

pub async fn run_with_trigger(
    session_dir: PathBuf,
    app: AppHandle,
    trigger: PipelineTrigger,
) {
    let rolling = RollingFinalizationReport::conservative(&session_dir);
    run_with_trigger_and_report(session_dir, app, trigger, rolling).await
}

async fn run_with_trigger_and_report(
    session_dir: PathBuf,
    app: AppHandle,
    trigger: PipelineTrigger,
    rolling: RollingFinalizationReport,
) {
    PIPELINE_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    // Scope guard in case any early-return is introduced later —
    // today the function can't panic mid-body because both branches
    // of the match already run the telemetry flush, but belt-and-
    // suspenders keeps the counter honest.
    struct InFlightGuard {
        active: bool,
    }
    impl InFlightGuard {
        fn finish(&mut self) -> usize {
            if self.active {
                self.active = false;
                PIPELINE_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) - 1
            } else {
                PIPELINE_IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire)
            }
        }
    }
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            if self.active {
                PIPELINE_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
    }
    let mut guard = InFlightGuard { active: true };

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
            triggered_by: trigger.token(),
        },
    );
    // #646 Layer C — Resuming fires *in addition to* Started when the
    // sweeper drove this run. Frontend can subscribe to either; the
    // Resuming event carries the attempt counter.
    if let PipelineTrigger::Auto { attempt_count } = trigger {
        emit(
            &app,
            PipelineEvent::Resuming {
                session_dir: session_str.clone(),
                attempt_count,
            },
        );
    }
    match run_inner(&session_dir, &app, &rolling).await {
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
                    call_id: call_id.clone(),
                },
            );
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            eprintln!("aftercalls: pipeline failed: {err_str}");
            // #645 Phase 1 — stamp a `failure_class` onto the
            // telemetry meta blob so the staff agent-logs dashboard
            // can filter pipeline failures by class
            // (transient_network / backend_5xx / auth_expired /
            // decode_error / signature_mismatch / other). The `error`
            // free-form string stays in the top-level message field
            // for backwards compatibility — the dashboard reads it
            // today and will gain `meta.failure_class`
            // opportunistically. No retry behavior change ships here;
            // Phase 2 (#646) layers `retry_http` on top.
            let failure_class = portal::classify_reqwest_error(&e);
            crate::telemetry::log(
                "error",
                "pipeline::failed",
                err_str.clone(),
                Some(serde_json::json!({
                    "failure_class": failure_class,
                })),
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
    let pipeline_still_active = guard.finish() > 0;
    crate::tray_refresh_after_pipeline(&app, pipeline_still_active);
}

async fn run_inner(
    session_dir: &Path,
    app: &AppHandle,
    rolling: &RollingFinalizationReport,
) -> Result<(PathBuf, String)> {
    let config = Config::load()?;
    let backend = config
        .backend
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no backend configured in config.toml"))?;
    // Held for telemetry session_id stamping on the new
    // `pipeline::decode_error` event (#179). Same string `run` derives
    // for `pipeline::start` / `pipeline::done` / `pipeline::failed` so
    // the four events tie back to one session_dir in agent_logs.
    let session_str = session_dir.to_string_lossy().into_owned();
    let session_telemetry = Some(session_str.as_str());

    // #646 Phase 2 Layer A — single retry guard per pipeline run.
    // Threaded into every retry_http call so a 401 can refresh tokens
    // ONCE and subsequent steps coast on the freshened bundle.
    let guard = RetryGuard::new();

    // Steps 1–2 (chunked-upload): consume only current-process publication
    // proof from rolling_encode. Recovery/import runs deliberately carry a
    // conservative report and re-encode the finalized WAV; a pre-existing,
    // nonempty `.opus` is never promoted to ready based on file length.
    let media_checkpoint = crate::media_manifest::read(session_dir)?;
    let already_ready = |track: &str| {
        media_checkpoint
            .as_ref()
            .and_then(|manifest| manifest.audio.get(track))
            .map(|checkpoint| {
                checkpoint.state == crate::media_manifest::ArtifactState::ReadyAcknowledged
            })
            .unwrap_or(false)
    };
    let mut prepared = Vec::with_capacity(2);
    for track in ["mic", "system"] {
        if already_ready(track) {
            continue;
        }
        // A prior run may have already created/partially confirmed an
        // immutable generation. Reconstruct the resume job only when the
        // private checkpoint carries that identity and its published path's
        // parent resolves inside this exact session. A missing file is passed
        // through intentionally: the generation client GETs authority first,
        // then either recognizes ready/current or aborts an incomplete upload.
        if let Some(checkpoint) = media_checkpoint
            .as_ref()
            .and_then(|manifest| manifest.audio.get(track))
        {
            if checkpoint.upload.is_some() {
                if let Some(opus_path) =
                    checkpointed_media_path(session_dir, checkpoint.published_path.as_deref())
                {
                    prepared.push(upload::PreparedAudioTrack { track, opus_path });
                    continue;
                }
            }
        }
        if let Some(candidate) = ensure_track_opus(session_dir, track, rolling).await? {
            prepared.push(candidate);
        }
    }
    let had_ready_audio = ["mic", "system"].iter().any(|track| already_ready(track));
    if prepared.is_empty() && !had_ready_audio {
        anyhow::bail!("no validated audio tracks are available for upload");
    }

    // Step 3: create the call row and obtain its stable call id. Retries the
    // create_call POST through retry_http (idempotent server-side).
    emit(app, PipelineEvent::Uploading);
    let created = upload::create_call(backend, session_dir, 0, &guard, session_telemetry).await?;
    crate::media_manifest::bind_call(session_dir, &created.call_id)
        .context("bind local media session to backend call")?;

    // Step 4: create/resume one immutable media generation per validated
    // `.opus`. The backend's confirmed-part status is authoritative across
    // restarts, and local media survives until exact ready/current evidence.
    // Every recorded track must become authoritative ready/current before the
    // pipeline advances. The retry sentinel + telemetry are managed by
    // upload_audio, and any partial failure retains local media and fails this
    // run for recovery.
    let track_outcomes = upload::upload_audio(
        session_dir,
        &prepared,
        backend,
        &created.call_id,
        &guard,
        session_telemetry,
    )
    .await?;
    for outcome in &track_outcomes {
        crate::media_manifest::mark_audio_upload(
            session_dir,
            outcome.track,
            outcome.uploaded,
            outcome.final_error.clone(),
        )?;
    }
    let any_uploaded = had_ready_audio || track_outcomes.iter().any(|o| o.uploaded);
    let any_failed = track_outcomes.iter().any(|o| !o.uploaded);
    if !any_uploaded && !track_outcomes.is_empty() {
        // Every track failed — there's nothing for AssemblyAI to chew
        // on, bubble out instead of marching into a transcribe call
        // that would fail loudly with "no audio". This still respects
        // Layer B's per-track surfacing because upload_audio has
        // already emitted track_upload_failed + written the sentinel.
        anyhow::bail!("all track uploads failed");
    }
    if any_failed {
        // Preserve every still-local source and fail the run. Reporting Done
        // here would launch destructive completion/cleanup paths while a
        // recorded channel is still pending, making the call appear complete
        // with silently missing audio.
        crate::telemetry::log(
            "warn",
            "pipeline::partial_upload",
            "one or more recorded tracks failed to upload; pipeline retained for retry",
            Some(serde_json::json!({
                "outcomes": track_outcomes,
            })),
            session_telemetry.map(|s| s.to_string()),
        );
        anyhow::bail!("one or more recorded audio track uploads remain pending");
    }

    // Step 5: backend transcribe (AssemblyAI with the org's key).
    // #646 Layer A — wrapped in retry_http now that Phase 1's backend
    // short-circuit makes a retry against a status='complete' row
    // safe (no AssemblyAI re-bill).
    emit(app, PipelineEvent::Transcribing);
    let call_id_for_transcribe = created.call_id.clone();
    let (transcript_json, transcribe_api_version) = portal::retry_http(
        backend,
        &guard,
        "transcribe",
        4,
        session_telemetry,
        |_attempt| {
            let call_id = call_id_for_transcribe.clone();
            async move { portal::transcribe(backend, &call_id).await }
        },
    )
    .await?;
    let transcript: MergedTranscript = match serde_json::from_value(transcript_json.clone()) {
        Ok(t) => t,
        Err(e) => {
            // #179 — split decode failures out as a structured event so
            // a "client is stuck" report can distinguish "backend
            // returned 5xx" (existing pipeline::failed shape) from
            // "backend returned a 200 the agent can't deserialize" (a
            // schema-skew between agent + backend builds — much rarer
            // but invisible without this discriminator). Fire BEFORE
            // bubbling so the existing pipeline::failed parent still
            // emits via the outer `match` in `run`.
            emit_decode_error(
                &session_str,
                &format!("/v1/calls/{}/transcribe", created.call_id),
                &transcript_json,
                transcribe_api_version.as_deref(),
            );
            return Err(anyhow::Error::new(e).context("decode transcript from backend"));
        }
    };
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
    // #646 Layer A — summarize through retry_http. Backend short-
    // circuit on status='complete' makes a retry idempotent (no
    // OpenAI re-bill, no overwrite of persisted summary).
    let transcript_value = serde_json::to_value(&transcript)?;
    let call_id_for_summarize = created.call_id.clone();
    let (summary_json, summarize_api_version) = portal::retry_http(
        backend,
        &guard,
        "summarize",
        4,
        session_telemetry,
        |_attempt| {
            let transcript_value = transcript_value.clone();
            let candidates = candidates.clone();
            let call_id = call_id_for_summarize.clone();
            async move {
                portal::summarize(backend, &call_id, &transcript_value, &candidates).await
            }
        },
    )
    .await?;
    let summary: Summary = match serde_json::from_value(summary_json.clone()) {
        Ok(s) => s,
        Err(e) => {
            // #179 — twin of the transcript decode_error path. Summary
            // is the most likely place a future schema bump on the
            // backend trips an out-of-date agent.
            emit_decode_error(
                &session_str,
                &format!("/v1/calls/{}/summarize", created.call_id),
                &summary_json,
                summarize_api_version.as_deref(),
            );
            return Err(anyhow::Error::new(e).context("decode summary from backend"));
        }
    };

    // Step 7: write the vault note. Skipped when vault isn't
    // configured — everything else already landed in the DB. The
    // return path here becomes the session_dir so the tray
    // "saved" notification still has something to surface.
    let note_path = if let Some(v) = vault {
        emit(app, PipelineEvent::WritingNote);
        match vault::write_note(v, &summary, &transcript, session_dir, &candidates) {
            Ok(p) => {
                // Step 8: attach the note path onto the call row for
                // portal deep-linking. Wrapped in retry_http; the
                // endpoint is narrow PATCH-shape and idempotent.
                if let Err(e) =
                    upload::attach_note_path(backend, &created.call_id, &p, &guard, session_telemetry)
                        .await
                {
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

    // Step 8.5 (#302 Slice B): upload the screen recording, if present.
    // A failure is durably retryable and fails this run so it cannot emit Done
    // or mark the local pipeline complete while recorded video is still local.
    // Cleanup occurs only after the screen generation is ready/current with
    // exact byte/hash evidence.
    match upload::upload_screen_recording(
        session_dir,
        &created.call_id,
        backend,
        &guard,
        session_telemetry,
    )
    .await
    {
        Ok(upload::ScreenUploadOutcome::NotPresent) => {}
        Ok(upload::ScreenUploadOutcome::ReadyAcknowledged { generation_id }) => {
            eprintln!(
                "aftercalls: screen recording generation {generation_id} is authoritative ready/current"
            );
        }
        Err(e) => return Err(e.context("screen recording upload remains pending")),
    }

    // Step 9: peaks, fire-and-forget — failure doesn't block the user
    // from seeing their note land. Retries via its own RetryGuard
    // (the per-pipeline guard is scoped to run_inner; this spawned
    // task lives past it).
    let backend_clone = backend.clone();
    let call_id = created.call_id.clone();
    let peaks_session = session_str.clone();
    tauri::async_runtime::spawn(async move {
        let peaks_guard = RetryGuard::new();
        let result = portal::retry_http(
            &backend_clone,
            &peaks_guard,
            "generate_peaks",
            4,
            Some(peaks_session.as_str()),
            |_attempt| {
                let backend_inner = backend_clone.clone();
                let call_id_inner = call_id.clone();
                async move { portal::generate_peaks(&backend_inner, &call_id_inner).await }
            },
        )
        .await;
        if let Err(e) = result {
            eprintln!("aftercalls: peaks generation failed: {e:#}");
        }
    });

    crate::media_manifest::mark_pipeline_complete(session_dir, &created.call_id)
        .context("durably checkpoint completed media pipeline")?;
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

/// #179 — structured decode-failure event that fires alongside the
/// existing `pipeline::failed` catch-all. The parent event stays as the
/// generic outer signal staff dashboards already query; this event adds
/// the discriminator (endpoint, expected shape version, the actual
/// response body) so a "client is stuck" report can be triaged from
/// agent_logs alone instead of needing the user to reproduce.
///
/// `received_type` + `received_keys` capture the STRUCTURAL shape of
/// the response — variant name plus the top-level object keys when
/// applicable. Keys-only, never values: the security review of #179
/// flagged that `MergedTranscript.timeline[].text` and
/// `Summary.{title, matched_client, summary}` are the leading fields
/// in real responses, so logging the body bytes would leak transcript
/// / summary / client-name content on a schema-skew incident — the
/// exact case this telemetry exists to catch. Keys + variant name
/// preserve diagnostic value (object vs scalar, renamed field, etc.)
/// without that risk. `server_version_header` is the backend's
/// `x-aftercalls-api-version` — `None` when the agent is talking to a
/// pre-#179 backend.
fn emit_decode_error(
    session_str: &str,
    endpoint: &str,
    received: &serde_json::Value,
    server_version_header: Option<&str>,
) {
    let received_type = match received {
        serde_json::Value::Object(_) => "object",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Null => "null",
    };
    let received_keys: Vec<&String> = match received {
        serde_json::Value::Object(map) => map.keys().collect(),
        _ => Vec::new(),
    };
    crate::telemetry::log(
        "error",
        "pipeline::decode_error",
        format!("decode failure on {endpoint}"),
        Some(serde_json::json!({
            "endpoint": endpoint,
            "expected_shape_version": "v2",
            "received_type": received_type,
            "received_keys": received_keys,
            "server_version_header": server_version_header,
        })),
        Some(session_str.to_string()),
    );
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

/// Suppress the transient console window Windows allocates for console-
/// subsystem children (#91). Must be applied to every ffmpeg spawn on
/// Windows — `stdio::null()` doesn't suppress the window, only the
/// CREATE_NO_WINDOW creation flag does. No-op on non-Windows.
#[cfg(windows)]
pub fn no_console(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}
#[cfg(not(windows))]
pub fn no_console(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd
}

fn checkpointed_media_path(session_dir: &Path, relative: Option<&str>) -> Option<PathBuf> {
    let relative = Path::new(relative?);
    if relative.is_absolute()
        || relative.components().any(|component| {
            !matches!(component, std::path::Component::Normal(_))
        })
    {
        return None;
    }
    let session = std::fs::canonicalize(session_dir).ok()?;
    let candidate = session_dir.join(relative);
    let parent = std::fs::canonicalize(candidate.parent()?).ok()?;
    if !parent.starts_with(&session) {
        return None;
    }
    let candidate = parent.join(candidate.file_name()?);
    if candidate.exists() {
        let resolved = std::fs::canonicalize(candidate).ok()?;
        resolved.starts_with(&session).then_some(resolved)
    } else {
        // A checkpointed generation still needs an authoritative GET when the
        // local source vanished. The generation client will either recognize
        // ready/current or abort an upload that can no longer be completed.
        Some(candidate)
    }
}

/// Resolve one track into an explicit upload capability. The rolling report is
/// the only fast path; all recovery/import/unknown states re-encode from WAV
/// through the same staged, validated, atomic publication boundary.
async fn ensure_track_opus(
    session_dir: &Path,
    track: &'static str,
    rolling: &RollingFinalizationReport,
) -> Result<Option<upload::PreparedAudioTrack>> {
    if rolling.session_dir == session_dir {
        if let Some(result) = rolling.track(track) {
            if result.state == TrackPublicationState::Published {
                if let Some(opus_path) = result.opus_path.clone() {
                    crate::media_manifest::mark_audio_published(
                        session_dir,
                        track,
                        &opus_path,
                    )?;
                    return Ok(Some(upload::PreparedAudioTrack { track, opus_path }));
                }
            }
        }
    }

    let owned_session = session_dir.to_path_buf();
    let encoded =
        tokio::task::spawn_blocking(move || fallback_encode_track(&owned_session, track))
            .await
            .context("join fallback audio encoder")?;
    match encoded.state {
        TrackPublicationState::Published => {
            let opus_path = encoded
                .opus_path
                .context("published fallback missing Opus path")?;
            crate::media_manifest::mark_audio_published(session_dir, track, &opus_path)?;
            Ok(Some(upload::PreparedAudioTrack { track, opus_path }))
        }
        TrackPublicationState::FallbackRequired => {
            let error = encoded
                .error
                .unwrap_or_else(|| "audio fallback did not publish".into());
            crate::media_manifest::mark_audio_fallback(session_dir, track, error.clone())?;
            anyhow::bail!("fallback compress for recorded {track} track failed: {error}")
        }
        TrackPublicationState::NotRecorded => {
            let explicitly_absent = crate::media_manifest::artifact(session_dir, track)?
                .is_some_and(|item| {
                    item.state == crate::media_manifest::ArtifactState::NotPresent
                });
            // Sessions recorded before this release have no `media-state.json`
            // at all, so a track that legitimately never existed (system audio
            // on a mic-only call) cannot be "explicitly absent" — the manifest
            // is created mid-resume and seeds both tracks as Pending. Treat a
            // track with no raw WAV and no Opus as genuinely not recorded
            // rather than failing the resume permanently. The bail below stays
            // for the real fault: raw audio present but unencodable.
            let raw_missing = !session_dir.join(format!("{track}.wav")).exists()
                && !session_dir.join(format!("{track}.opus")).exists();
            if explicitly_absent || raw_missing {
                if !explicitly_absent {
                    crate::media_manifest::mark_audio_not_present(session_dir, track)?;
                }
                Ok(None)
            } else {
                let error = encoded
                    .error
                    .unwrap_or_else(|| "expected raw audio is missing".into());
                crate::media_manifest::mark_audio_fallback(
                    session_dir,
                    track,
                    error.clone(),
                )?;
                anyhow::bail!("recorded {track} track is unavailable: {error}")
            }
        }
    }
}
