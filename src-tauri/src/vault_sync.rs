//! Obsidian notes for calls imported server-side.
//!
//! Vault notes are normally written by the local post-recording pipeline
//! (`pipeline.rs` step 7). Calls the backend imports on its own (SmartPBX,
//! Zoho Meeting, Zoho Cliq, uploaded files) never pass through that
//! pipeline, so they never got a note. This background task fills that gap
//! while the agent is running, **only after the user opts in** ("Also write
//! notes for imported calls" is off by default: a vault is often synced to
//! other devices, so transcripts must not appear there after an update):
//!
//!   1. ask `GET /v1/calls/vault-pending` for the signed-in user's own
//!      imported calls that are complete, summarized and transcribed and
//!      have no note path, recorded since the user turned this on for this
//!      account (minus [`SINCE_GRACE`]);
//!   2. read each call through the normal call-detail route, with
//!      `mark_read=false` so the sync never clears the user's unread marker;
//!   3. write it with [`vault::write_imported_note`] (same folders and
//!      client matching as local notes, inert rendering, no-clobber,
//!      idempotent on the call-id suffix);
//!   4. record the path with `only_if_unset`, like pipeline step 8 but
//!      conditional, so a call gets one note across all machines.
//!
//! Multiple machines: `calls.note_markdown_path` is per call, not per
//! machine. The first machine to record a path owns the note; every other
//! machine sees the call drop out of its pending list. A machine that loses
//! a race (`409`) **never deletes** its file: recorded paths are absolute
//! per machine (a Mac and a Windows mount of one synced vault differ), so
//! nothing can prove the local file isn't the registered note or its synced
//! copy. The leftover is harmless: it has the same call-id suffix, and
//! idempotency matches on that suffix, so no further copies are written.
//!
//! Account binding: the start of the window is stamped for the signed-in
//! account. Signing in as someone else restarts the window and the failure
//! state, so user B's history never backfills into a vault set up under A.
//!
//! Bounds: at most [`MAX_PER_TICK`] notes per tick, [`MAX_PAGES`] pages
//! scanned; a call that fails [`MAX_FAILURES`] times is skipped (persisted
//! across restarts); listing errors back off exponentially up to
//! [`MAX_BACKOFF`]; nothing is written while a recording is active (checked
//! before every note). It never touches the recording pipeline.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::config::{self, Backend, Vault};
use crate::portal;
use crate::recorder::Recorder;
use crate::summary::{ActionItem, Summary};
use crate::transcription::{MergedTranscript, Utterance};
use crate::vault::{self, ImportedNote};

/// Wait after launch before the first tick, so startup work settles.
const INITIAL_DELAY: Duration = Duration::from_secs(60);
/// Normal spacing between ticks.
const TICK_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Ceiling for the exponential backoff after a failed listing.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Notes written per tick, at most.
const MAX_PER_TICK: usize = 10;
/// Calls requested per page.
const PAGE_LIMIT: u32 = 10;
/// Pages scanned per tick, at most (bounds the walk past skipped calls).
const MAX_PAGES: usize = 5;
/// After this many failures a call is skipped for good (on this machine,
/// for this account).
const MAX_FAILURES: u32 = 3;
/// Failure entries kept in the state file; oldest dropped first.
const MAX_TRACKED_FAILURES: usize = 1000;
/// How far before the setting took effect to look. Imports arrive after
/// the call (polling plus transcription), so a call recorded shortly
/// before the user turned this on still gets a note. Nothing older does:
/// enabling the feature never backfills the user's history.
pub const SINCE_GRACE: ChronoDuration = ChronoDuration::hours(24);

fn wake() -> &'static tokio::sync::Notify {
    static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
    WAKE.get_or_init(tokio::sync::Notify::new)
}

/// Run a tick soon instead of waiting out the interval. Called when the
/// user saves vault settings so turning the feature on takes effect.
pub fn nudge() {
    wake().notify_one();
}

/// Start the background loop. Called once from `setup()`.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(INITIAL_DELAY) => {}
            _ = wake().notified() => {}
        }
        let mut delay = TICK_INTERVAL;
        loop {
            match tick(&app).await {
                Ok(()) => delay = TICK_INTERVAL,
                Err(error) => {
                    delay = (delay * 2).min(MAX_BACKOFF);
                    eprintln!("aftercalls: vault sync failed, retrying in {delay:?}: {error:#}");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = wake().notified() => {}
            }
        }
    });
}

/// The imported-notes window a Settings save should persist:
/// `(imported_notes_since, imported_notes_account)`. Off → cleared. On and
/// already on for the same signed-in account → kept. Otherwise (turned on
/// now, or a different account) → restarted at `now` for `account`.
pub fn window_after_settings_save(
    previous: Option<&Vault>,
    imported_notes: bool,
    account: Option<String>,
    now: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<String>) {
    if !imported_notes {
        return (None, None);
    }
    match previous {
        Some(prev)
            if prev.imported_notes
                && prev.imported_notes_since.is_some()
                && prev.imported_notes_account == account =>
        {
            (prev.imported_notes_since, account)
        }
        _ => (Some(now), account),
    }
}

/// The window the sync should use for `account`, or `None` when the stored
/// one belongs to someone else (or was never stamped) and must restart.
fn window_for_account(vault: &Vault, account: &str) -> Option<DateTime<Utc>> {
    match (&vault.imported_notes_account, vault.imported_notes_since) {
        (Some(stored), Some(since)) if stored == account => Some(since),
        _ => None,
    }
}

fn recording_active(app: &AppHandle) -> bool {
    app.state::<Recorder>().is_active()
}

async fn tick(app: &AppHandle) -> Result<()> {
    let cfg = config::Config::load().context("load config")?;
    let Some(vault) = cfg.vault.clone() else {
        return Ok(());
    };
    if !vault.imported_notes {
        return Ok(());
    }
    let Some(backend) = cfg.backend.clone() else {
        return Ok(());
    };
    // Only a signed-in account (auth.json) can own the window; a legacy
    // static token has no account id, so nothing is synced for it.
    let Some(account) = config::read_auth_file().ok().flatten().map(|auth| auth.user_id) else {
        return Ok(());
    };
    // Signed out / expired: nothing to do, not an error worth backing off for.
    if portal::build_auth_header(&backend).await.is_err() {
        return Ok(());
    }
    if recording_active(app) {
        return Ok(());
    }
    let enabled_at = match window_for_account(&vault, &account) {
        Some(at) => at,
        None => restart_window(&account, Utc::now())?,
    };
    let since = pending_since(enabled_at);
    let candidates = vault::list_clients(&vault).unwrap_or_else(|error| {
        eprintln!("aftercalls: vault sync list_clients failed, using _Unsorted: {error:#}");
        Vec::new()
    });
    let mut state = SyncState::load(&account);

    let mut written = 0usize;
    let mut cursor: Option<(String, String)> = None;
    for _ in 0..MAX_PAGES {
        let page = portal::list_vault_pending(
            &backend,
            since,
            cursor.as_ref().map(|(at, id)| (at.as_str(), id.as_str())),
            PAGE_LIMIT,
        )
        .await
        .map_err(|error| anyhow!("list vault-pending calls: {error:?}"))?;
        let items = parse_pending(&page)?;
        for item in &items {
            cursor = Some((item.recorded_at_raw.clone(), item.id.clone()));
            if state.failures(&item.id) >= MAX_FAILURES {
                continue;
            }
            if written >= MAX_PER_TICK {
                return Ok(());
            }
            // A recording may have started since the tick began.
            if recording_active(app) {
                return Ok(());
            }
            written += 1;
            match write_one(&backend, &vault, &candidates, item).await {
                Ok(()) => state.clear(&item.id),
                Err(error) => {
                    let count = state.record(&item.id);
                    eprintln!(
                        "aftercalls: vault note for imported call {} failed ({count}/{MAX_FAILURES}): {error:#}",
                        item.id
                    );
                }
            }
        }
        if items.len() < PAGE_LIMIT as usize {
            break;
        }
    }
    Ok(())
}

async fn write_one(
    backend: &Backend,
    vault: &Vault,
    candidates: &[String],
    item: &PendingCall,
) -> Result<()> {
    let detail = portal::get_call_unread_preserving(backend, &item.id)
        .await
        .map_err(|error| anyhow!("read call: {error:?}"))?;
    let inputs = note_inputs_from_detail(&detail)?;
    let written = vault::write_imported_note(
        vault,
        &inputs.summary,
        &inputs.transcript,
        candidates,
        &item.id,
        &item.ingest_source,
        inputs.recorded_at,
    )?;
    match written {
        ImportedNote::Created(path) => {
            match portal::attach_note_path_if_unset(backend, &item.id, &path).await {
                Ok(true) => Ok(()),
                // Another machine recorded a note first. Never delete here:
                // recorded paths are absolute per machine (a Mac and a
                // Windows mount of the same synced vault differ), so no
                // comparison can prove our file isn't the registered note.
                // Leaving it is harmless: it carries the same call-id
                // suffix, and idempotency matches on that suffix.
                Ok(false) => Ok(()),
                // Keep the file: the next tick finds it by its call-id
                // suffix and retries the attach.
                Err(error) => Err(anyhow!("record note path: {error:?}")),
            }
        }
        // Ours from an earlier attempt, or written by a machine sharing this
        // vault. Never deleted here; just make sure the path is recorded.
        ImportedNote::AlreadyExists(path) => {
            portal::attach_note_path_if_unset(backend, &item.id, &path)
                .await
                .map(|_| ())
                .map_err(|error| anyhow!("record note path: {error:?}"))
        }
    }
}

/// Stamp a fresh window for `account` (first run after opting in, or a
/// different signed-in account). Done under the shared config write lock,
/// re-reading the file so a concurrent Settings save isn't lost.
fn restart_window(account: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let _config_write = config::config_write_lock();
    let mut cfg = config::Config::load().context("reload config")?;
    let Some(vault) = cfg.vault.as_mut() else {
        return Ok(now);
    };
    if let Some(existing) = window_for_account(vault, account) {
        return Ok(existing);
    }
    vault.imported_notes_since = Some(now);
    vault.imported_notes_account = Some(account.to_string());
    cfg.save().context("save imported-notes window")?;
    Ok(now)
}

fn pending_since(enabled_at: DateTime<Utc>) -> DateTime<Utc> {
    enabled_at - SINCE_GRACE
}

/// Per-call failure counts, persisted next to config.toml so the
/// [`MAX_FAILURES`] skip survives restarts. Scoped to one account: a
/// different signed-in account starts empty.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct SyncState {
    #[serde(default)]
    account: String,
    /// Oldest first.
    #[serde(default)]
    failures: Vec<(String, u32)>,
    #[serde(skip)]
    path: Option<std::path::PathBuf>,
}

impl SyncState {
    fn load(account: &str) -> Self {
        let path = config::vault_sync_state_file().ok();
        Self::load_from(path, account)
    }

    fn load_from(path: Option<std::path::PathBuf>, account: &str) -> Self {
        let mut state = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str::<SyncState>(&text).ok())
            .filter(|state| state.account == account)
            .unwrap_or_default();
        state.account = account.to_string();
        state.path = path;
        state
    }

    fn failures(&self, id: &str) -> u32 {
        self.failures
            .iter()
            .find(|(known, _)| known == id)
            .map_or(0, |(_, count)| *count)
    }

    fn record(&mut self, id: &str) -> u32 {
        let count = self.failures(id) + 1;
        self.failures.retain(|(known, _)| known != id);
        self.failures.push((id.to_string(), count));
        if self.failures.len() > MAX_TRACKED_FAILURES {
            let excess = self.failures.len() - MAX_TRACKED_FAILURES;
            self.failures.drain(..excess);
        }
        self.save();
        count
    }

    fn clear(&mut self, id: &str) {
        let before = self.failures.len();
        self.failures.retain(|(known, _)| known != id);
        if self.failures.len() != before {
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let write = || -> Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(self)?)?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        };
        if let Err(error) = write() {
            eprintln!("aftercalls: vault sync could not persist failure state: {error:#}");
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PendingCall {
    /// Canonical (hyphenated, lowercase) UUID: validated before it is ever
    /// put into a URL path.
    id: String,
    /// Echoed back verbatim as the keyset cursor.
    recorded_at_raw: String,
    ingest_source: String,
}

fn parse_pending(page: &Value) -> Result<Vec<PendingCall>> {
    let calls = page
        .get("calls")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("vault-pending response has no calls array"))?;
    calls
        .iter()
        .map(|call| {
            let field = |name: &str| {
                call.get(name)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("vault-pending row missing {name}"))
            };
            let id = uuid::Uuid::parse_str(&field("id")?)
                .map_err(|_| anyhow!("vault-pending row has a non-UUID id"))?
                .hyphenated()
                .to_string();
            let recorded_at_raw = field("recorded_at")?;
            DateTime::parse_from_rfc3339(&recorded_at_raw)
                .map_err(|_| anyhow!("vault-pending row has a bad recorded_at"))?;
            Ok(PendingCall {
                id,
                recorded_at_raw,
                ingest_source: field("ingest_source")?,
            })
        })
        .collect()
}

struct NoteInputs {
    summary: Summary,
    transcript: MergedTranscript,
    recorded_at: DateTime<Utc>,
}

/// Shape a `GET /v1/calls/{id}` body into what the vault writer takes.
/// Refuses calls that aren't finished, so a half-processed import never
/// produces a note without its summary.
fn note_inputs_from_detail(detail: &Value) -> Result<NoteInputs> {
    let status = detail.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "complete" {
        anyhow::bail!("call is not complete yet (status `{status}`)");
    }
    let summary_text = detail
        .get("summary_text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("call has no summary yet"))?;
    let recorded_at = detail
        .get("recorded_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .ok_or_else(|| anyhow!("call has no recorded_at"))?;
    let strings = |name: &str| -> Vec<String> {
        detail
            .get(name)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let action_items = detail
        .get("action_items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("description").and_then(Value::as_str))
                .map(|description| ActionItem {
                    description: description.to_string(),
                    // The detail route carries an assignee user id, not a
                    // name; the description already reads naturally.
                    assignee_name: None,
                })
                .collect()
        })
        .unwrap_or_default();
    let timeline: Vec<Utterance> = detail
        .get("utterances")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Utterance {
                        speaker: row.get("speaker")?.as_str()?.to_string(),
                        start_ms: row.get("start_ms")?.as_i64()?.max(0) as u64,
                        end_ms: row.get("end_ms")?.as_i64()?.max(0) as u64,
                        text: row.get("text")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let duration_ms = detail
        .get("duration_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    Ok(NoteInputs {
        summary: Summary {
            title: detail
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Call")
                .to_string(),
            matched_client: detail
                .get("matched_client")
                .and_then(Value::as_str)
                .map(str::to_string),
            summary: summary_text.to_string(),
            action_items,
            participants: strings("participants"),
        },
        transcript: MergedTranscript {
            duration_ms,
            timeline,
        },
        recorded_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail(status: &str, summary: Option<&str>) -> Value {
        json!({
            "id": "0b5f6a2c-1111-2222-3333-444455556666",
            "recorded_at": "2026-10-08T14:30:00Z",
            "duration_ms": 65000,
            "title": "Quote follow-up",
            "matched_client": "Acme",
            "summary_text": summary,
            "status": status,
            "participants": ["Nick", "Caller"],
            "action_items": [
                { "id": "a", "description": "Send revised quote", "assignee_user_id": null, "status": "open" },
                { "id": "b", "description": "Book site visit", "assignee_user_id": "u", "status": "done" }
            ],
            "utterances": [
                { "idx": 0, "speaker": "Nick", "original_speaker": "A", "start_ms": 1200, "end_ms": 3400, "text": "Hi, it's Nick.", "speaker_user_id": null },
                { "idx": 1, "speaker": "Caller", "original_speaker": "B", "start_ms": 3500, "end_ms": 5000, "text": "Hello.", "speaker_user_id": null }
            ]
        })
    }

    #[test]
    fn detail_maps_to_note_inputs() {
        let inputs = note_inputs_from_detail(&detail("complete", Some("They want a quote."))).unwrap();
        assert_eq!(inputs.summary.title, "Quote follow-up");
        assert_eq!(inputs.summary.matched_client.as_deref(), Some("Acme"));
        assert_eq!(inputs.summary.summary, "They want a quote.");
        assert_eq!(inputs.summary.participants, vec!["Nick", "Caller"]);
        let actions: Vec<_> = inputs
            .summary
            .action_items
            .iter()
            .map(|a| a.description.as_str())
            .collect();
        assert_eq!(actions, vec!["Send revised quote", "Book site visit"]);
        assert_eq!(inputs.transcript.timeline.len(), 2);
        assert_eq!(inputs.transcript.timeline[0].start_ms, 1200);
        assert_eq!(inputs.transcript.duration_ms, 65000);
        assert_eq!(inputs.recorded_at.to_rfc3339(), "2026-10-08T14:30:00+00:00");
    }

    #[test]
    fn unfinished_or_unsummarized_calls_are_refused() {
        assert!(note_inputs_from_detail(&detail("transcribing", Some("x"))).is_err());
        assert!(note_inputs_from_detail(&detail("complete", None)).is_err());
        assert!(note_inputs_from_detail(&detail("complete", Some("   "))).is_err());
    }

    #[test]
    fn blank_title_falls_back() {
        let mut body = detail("complete", Some("s"));
        body["title"] = Value::Null;
        assert_eq!(note_inputs_from_detail(&body).unwrap().summary.title, "Call");
    }

    #[test]
    fn pending_page_requires_uuid_ids_and_timestamps() {
        let id = "0B5F6A2C-1111-2222-3333-444455556666";
        let page = json!({ "calls": [
            { "id": id, "recorded_at": "2026-10-08T14:30:00Z", "ingest_source": "smartpbx" }
        ]});
        assert_eq!(
            parse_pending(&page).unwrap(),
            vec![PendingCall {
                id: id.to_ascii_lowercase(),
                recorded_at_raw: "2026-10-08T14:30:00Z".into(),
                ingest_source: "smartpbx".into(),
            }]
        );
        for bad_id in ["c1", "../../v1/admin", "0b5f6a2c-1111-2222-3333-444455556666/x"] {
            let page = json!({ "calls": [
                { "id": bad_id, "recorded_at": "2026-10-08T14:30:00Z", "ingest_source": "smartpbx" }
            ]});
            assert!(parse_pending(&page).is_err(), "{bad_id}");
        }
        let bad_time = json!({ "calls": [
            { "id": id, "recorded_at": "yesterday", "ingest_source": "smartpbx" }
        ]});
        assert!(parse_pending(&bad_time).is_err());
        assert!(parse_pending(&json!({ "calls": [{ "id": id }] })).is_err());
        assert!(parse_pending(&json!({})).is_err());
    }

    #[test]
    fn lookback_is_bounded_by_the_grace_window() {
        let enabled_at = DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            pending_since(enabled_at).to_rfc3339(),
            "2026-10-08T12:00:00+00:00"
        );
    }

    // ── Fix 5: the window belongs to one account ─────────────────────

    fn vault_with(on: bool, since: Option<&str>, account: Option<&str>) -> Vault {
        Vault {
            path: "/v".into(),
            clients_subpath: String::new(),
            imported_notes: on,
            imported_notes_since: since.map(|s| {
                DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
            }),
            imported_notes_account: account.map(str::to_string),
        }
    }

    #[test]
    fn window_restarts_for_a_different_account() {
        let vault = vault_with(true, Some("2026-10-01T00:00:00Z"), Some("user-a"));
        assert!(window_for_account(&vault, "user-a").is_some());
        assert!(window_for_account(&vault, "user-b").is_none(), "B must not inherit A's window");
        assert!(window_for_account(&vault_with(true, None, Some("user-a")), "user-a").is_none());
        assert!(window_for_account(&vault_with(true, Some("2026-10-01T00:00:00Z"), None), "user-a").is_none());
    }

    #[test]
    fn settings_save_keeps_or_restarts_the_window() {
        let now = DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z").unwrap().with_timezone(&Utc);
        let on_a = vault_with(true, Some("2026-10-01T00:00:00Z"), Some("user-a"));
        // Staying on for the same account keeps the original start.
        let (since, account) = window_after_settings_save(Some(&on_a), true, Some("user-a".into()), now);
        assert_eq!(since, on_a.imported_notes_since);
        assert_eq!(account.as_deref(), Some("user-a"));
        // Same vault, different signed-in account: restart now.
        let (since, account) = window_after_settings_save(Some(&on_a), true, Some("user-b".into()), now);
        assert_eq!(since, Some(now));
        assert_eq!(account.as_deref(), Some("user-b"));
        // Turning it on (was off): start now, never backfill the off period.
        let off = vault_with(false, Some("2026-01-01T00:00:00Z"), Some("user-a"));
        assert_eq!(window_after_settings_save(Some(&off), true, Some("user-a".into()), now).0, Some(now));
        // Turning it off clears the window.
        assert_eq!(window_after_settings_save(Some(&on_a), false, Some("user-a".into()), now), (None, None));
    }

    // ── Fix 4: the 3-strike skip survives restarts ───────────────────

    #[test]
    fn failure_state_persists_per_account() {
        let dir = std::env::temp_dir().join(format!("aftercalls-vault-sync-{}", uuid::Uuid::new_v4().simple()));
        let file = dir.join("vault_sync_state.json");
        let mut state = SyncState::load_from(Some(file.clone()), "user-a");
        assert_eq!(state.record("call-1"), 1);
        assert_eq!(state.record("call-1"), 2);
        assert_eq!(state.record("call-1"), 3);
        assert_eq!(state.record("call-2"), 1);

        // "Restart": a fresh load sees the counts, so call-1 stays skipped.
        let mut reloaded = SyncState::load_from(Some(file.clone()), "user-a");
        assert_eq!(reloaded.failures("call-1"), MAX_FAILURES);
        reloaded.clear("call-2");
        assert_eq!(SyncState::load_from(Some(file.clone()), "user-a").failures("call-2"), 0);

        // Another account starts clean.
        assert_eq!(SyncState::load_from(Some(file.clone()), "user-b").failures("call-1"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_state_is_bounded() {
        let mut state = SyncState::load_from(None, "user-a");
        for i in 0..(MAX_TRACKED_FAILURES + 5) {
            state.record(&format!("call-{i}"));
        }
        assert_eq!(state.failures.len(), MAX_TRACKED_FAILURES);
        assert_eq!(state.failures("call-0"), 0, "oldest dropped");
        assert_eq!(state.failures(&format!("call-{}", MAX_TRACKED_FAILURES + 4)), 1);
    }
}
