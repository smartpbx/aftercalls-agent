//! Diagnostic telemetry.
//!
//! A small ring buffer of log entries we periodically ship to the
//! backend's `/v1/agent/logs/batch`. Kept deliberately simple so the
//! implementation doesn't itself become a source of bugs that need
//! diagnosing:
//!
//! - `log(level, module, message, meta)` pushes one entry onto the
//!   buffer. Drops the oldest entry if the buffer is full (2000).
//! - A tokio task flushes the buffer every 30s.
//! - `flush()` is also callable manually — pipeline calls it on
//!   `Done` / `Failed` so post-call failures ship within seconds.
//! - `install_panic_hook()` wires `std::panic` so any panic lands in
//!   the buffer with a backtrace before the process exits.
//! - Opt-out via config.telemetry_enabled — we check on every push.
//!
//! We do NOT capture call audio, transcripts, or summaries. The
//! meta blob is intended for stage names, error strings, HTTP
//! status codes — stuff you'd paste into a bug report.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Emitter};

const RING_CAPACITY: usize = 2000;
const FLUSH_INTERVAL_SECS: u64 = 30;
const BATCH_MAX: usize = 200;

#[derive(Clone, Serialize)]
pub struct Entry {
    pub session_id: Option<String>,
    pub ts: DateTime<Utc>,
    pub level: String, // trace/debug/info/warn/error
    pub module: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

fn buffer() -> &'static Mutex<VecDeque<Entry>> {
    static BUF: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAPACITY)))
}

/// Push a single entry. Returns fast; never blocks on I/O. If the
/// user has opted out, the call is a no-op — we still let call sites
/// use `log!` freely so telemetry-off doesn't change control flow.
pub fn log(
    level: &str,
    module: &str,
    message: impl Into<String>,
    meta: Option<Value>,
    session_id: Option<String>,
) {
    let enabled = crate::config::Config::load()
        .map(|c| c.telemetry_enabled)
        .unwrap_or(true);
    if !enabled {
        return;
    }
    let entry = Entry {
        session_id,
        ts: Utc::now(),
        level: level.to_string(),
        module: module.to_string(),
        message: message.into(),
        meta,
    };
    if let Ok(mut buf) = buffer().lock() {
        if buf.len() >= RING_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(entry);
    }
}

pub fn install_panic_hook() {
    // Keep the default hook wired so stderr still gets the backtrace
    // alongside our capture — useful when running from a terminal.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_string)
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        log(
            "error",
            "panic",
            format!("panic at {loc}: {payload}"),
            Some(serde_json::json!({ "location": loc })),
            None,
        );
        // Best-effort sync flush — we're about to crash. Spawn a
        // detached task; if it doesn't make it, the next launch's
        // flush still ships earlier entries.
        if let Some(app) = APP_HANDLE.get() {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = flush_once(&app).await;
            });
        }
        previous(info);
    }));
}

/// Start the periodic flush loop. Call once from `setup` after the
/// app handle is available.
pub fn start(app: AppHandle) {
    let _ = APP_HANDLE.set(app.clone());
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));
        // First tick fires immediately; skip it so we don't race the
        // first user action on a slow backend.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = flush_once(&app).await {
                eprintln!("aftercalls telemetry flush failed: {e:#}");
            }
        }
    });
}

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// Drain up to `BATCH_MAX` entries and POST them. Exposed for callers
/// that want an immediate flush (pipeline done/failed).
pub async fn flush_now() -> anyhow::Result<()> {
    let app = APP_HANDLE
        .get()
        .ok_or_else(|| anyhow::anyhow!("telemetry not started"))?;
    flush_once(app).await
}

/// #646 Layer D — read-only query over the in-process ring buffer.
/// Returns the most recent `pipeline::failed` entry for the given
/// session and the failure_class stamped on its `meta` blob (Phase 1
/// shape: `{ "failure_class": "transient_network" | ... }`). Returns
/// None when no matching failure is buffered (e.g. agent restart
/// dropped the entry, session never failed in-process, or telemetry
/// is opt-out and the buffer is empty).
///
/// Read in the auto-resume sweeper's eligibility classifier
/// (`recovery::auto_resume_eligible`). Reads only — never mutates the
/// ring; safe to call from any context.
pub fn recent_failure_for_session(
    session_id: &str,
) -> Option<(crate::portal::FailureClass, DateTime<Utc>)> {
    let buf = buffer().lock().ok()?;
    for entry in buf.iter().rev() {
        if entry.module != "pipeline::failed" {
            continue;
        }
        let Some(sid) = entry.session_id.as_ref() else {
            continue;
        };
        // The pipeline stamps the absolute session_dir path on
        // `session_id`, while orphan-resume callers typically know
        // the basename (`20260522T140158Z`). Match both:
        // exact-equal AND trailing-component-equal.
        let matches = sid == session_id
            || std::path::Path::new(sid)
                .file_name()
                .and_then(|s| s.to_str())
                .map(|n| n == session_id)
                .unwrap_or(false);
        if !matches {
            continue;
        }
        let meta = entry.meta.as_ref()?;
        let class_token = meta.get("failure_class")?.as_str()?;
        let class = match class_token {
            "transient_network" => crate::portal::FailureClass::TransientNetwork,
            "backend_5xx" => crate::portal::FailureClass::BackendFiveXx,
            "auth_expired" => crate::portal::FailureClass::AuthExpired,
            "decode_error" => crate::portal::FailureClass::DecodeError,
            "signature_mismatch" => crate::portal::FailureClass::SignatureMismatch,
            "other" => crate::portal::FailureClass::Other,
            _ => return None,
        };
        return Some((class, entry.ts));
    }
    None
}

/// #646 Layer A — emit a Tauri event on the registered AppHandle.
/// Used by `retry_http` to broadcast a `PipelineEvent::Retrying`
/// while it waits for the next attempt slot. Returns false if no
/// AppHandle has been registered yet (e.g. in unit tests) so callers
/// can degrade silently. The payload is serialized verbatim — caller
/// is responsible for matching the existing `PipelineEvent` JSON
/// shape (`{ stage: "retrying", step, attempt, wait_ms }`).
pub fn emit_app_event(name: &str, payload: &Value) -> bool {
    let Some(app) = APP_HANDLE.get() else {
        return false;
    };
    if let Err(e) = app.emit(name, payload) {
        eprintln!("aftercalls: emit_app_event {name} failed: {e}");
        return false;
    }
    true
}

/// Return the most recent unique `session_id` values currently held
/// in the ring buffer. Used by the support-report submit (#183) to
/// stamp the report's `metadata` blob with pivot points the staff
/// page can use to fetch the matching `agent_logs` rows. Bounded by
/// `cap` and dedupes while preserving recency.
pub fn recent_session_ids(cap: usize) -> Vec<String> {
    let Ok(buf) = buffer().lock() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::with_capacity(cap.min(buf.len()));
    // Walk the buffer back-to-front so newer entries land first.
    for entry in buf.iter().rev() {
        let Some(sid) = entry.session_id.as_ref() else {
            continue;
        };
        if !out.iter().any(|x| x == sid) {
            out.push(sid.clone());
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

async fn flush_once(_app: &AppHandle) -> anyhow::Result<()> {
    // Drain what's in the buffer; if the POST fails, re-insert at the
    // front so we retry on the next tick.
    let drained: Vec<Entry> = {
        let mut buf = buffer()
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;
        let take = buf.len().min(BATCH_MAX);
        buf.drain(..take).collect()
    };
    if drained.is_empty() {
        return Ok(());
    }

    let cfg = crate::config::Config::load()?;
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no backend configured"))?;
    let header = match crate::portal::build_auth_header(backend).await {
        Ok(h) => h,
        Err(e) => {
            // Not authed yet — drop silently. We don't want telemetry
            // gating the user's login flow.
            eprintln!("aftercalls telemetry: no auth yet ({e:#}); dropping batch");
            return Ok(());
        }
    };

    let agent_version = env!("CARGO_PKG_VERSION").to_string();
    let platform = std::env::consts::OS.to_string();

    let url = format!("{}/v1/agent/logs/batch", backend.url.trim_end_matches('/'));
    let body = serde_json::json!({
        "agent_version": agent_version,
        "platform": platform,
        "entries": drained,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client
        .post(&url)
        .header("Authorization", header)
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            // Non-2xx — put them back, stderr the status.
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            eprintln!("aftercalls telemetry ingest returned {status}: {text}");
            requeue(drained);
            Ok(())
        }
        Err(e) => {
            // Network error — put them back for the next tick.
            eprintln!("aftercalls telemetry ingest error: {e:#}");
            requeue(drained);
            Ok(())
        }
    }
}

fn requeue(entries: Vec<Entry>) {
    if let Ok(mut buf) = buffer().lock() {
        for e in entries.into_iter().rev() {
            if buf.len() >= RING_CAPACITY {
                buf.pop_back(); // newer entries fall off first on an already-full ring
            }
            buf.push_front(e);
        }
    }
    let _ = app_handle_suppress_unused();
}

// Silences the "unused" warning when `APP_HANDLE` ends up only read
// from async paths the dead-code analyzer can't trace.
fn app_handle_suppress_unused() -> bool {
    APP_HANDLE.get().is_some()
}

// ── Tauri command for JS-side events ────────────────────────────────
//
// Frontend dispatches window.onerror / unhandledrejection into this
// command so frontend stack traces end up in the same batch flow as
// Rust events.

#[derive(serde::Deserialize)]
pub struct LogEventInput {
    pub level: String,
    pub module: String,
    pub message: String,
    pub session_id: Option<String>,
    pub meta: Option<Value>,
}

#[tauri::command]
pub fn log_event(input: LogEventInput) -> Result<(), String> {
    log(
        &input.level,
        &input.module,
        input.message,
        input.meta,
        input.session_id,
    );
    Ok(())
}
