//! Local-only sqlite store of "apps that have used your microphone".
//!
//! Backs the auto-record per-app whitelist (#596). Privacy-sensitive —
//! reveals which apps a user runs — and stays per-machine: no backend
//! route accepts this data, no upload path includes it. The DB lives
//! next to `config.toml` / `auth.json` under
//! `dirs::config_dir()/aftercalls/agent.db` (or
//! `aftercalls-<profile>/agent.db` via the existing `AFTERCALLS_PROFILE`
//! machinery), is chmod'd 0600 on first create, and is never copied off
//! the machine.
//!
//! Schema is a single table plus a meta-version sentinel so future
//! schema bumps can branch on `schema_meta.version`. The opening API
//! takes a path (so tests can pass `:memory:` or a tempdir) and returns
//! a thin handle that's `Send + Sync` via `Mutex<Connection>` — agent
//! callers (the auto-recorder, the IPC commands) all live on tauri's
//! tokio runtime and never need parallel writes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Current logical schema version. Bump + add a migrator branch in
/// `migrate()` whenever the table shape changes.
const SCHEMA_VERSION: &str = "1";

/// One row of `observed_apps`. Mirrored 1:1 to the `AppRow` IPC type
/// in lib.rs; serializing here keeps the SQL access surface in one
/// place.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ObservedApp {
    pub bundle_id: String,
    pub friendly_name: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub enabled: bool,
}

/// Handle to the local sqlite store. Cloneable Arc-of-Mutex would let
/// tauri `manage()` the store directly, but we keep the public surface
/// small: callers wrap it in their own state struct.
pub struct AppObservations {
    conn: Mutex<Connection>,
}

impl AppObservations {
    /// Open (or create + migrate) the on-disk store at the given path.
    /// On Unix, fresh files land at chmod 0600 — same posture as the
    /// auth.json writer in `config::write_auth_file`. Existing files
    /// keep whatever permissions they already have so a user who's
    /// hardened the file further isn't reset.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir parent of {}", path.display()))?;
        }
        let fresh = !path.exists();
        let conn = Connection::open(&path)
            .with_context(|| format!("open sqlite at {}", path.display()))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.bootstrap()?;
        if fresh {
            // Privacy-sensitive — chmod the freshly-created DB so no
            // other user on the machine can read it.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(mut perms) = std::fs::metadata(&path).map(|m| m.permissions()) {
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(&path, perms);
                }
            }
        }
        Ok(store)
    }

    /// In-memory store for unit tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory sqlite")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.bootstrap()?;
        Ok(store)
    }

    fn bootstrap(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS observed_apps (
                bundle_id        TEXT PRIMARY KEY,
                friendly_name    TEXT NOT NULL,
                first_seen_at    TEXT NOT NULL,
                last_seen_at     TEXT NOT NULL,
                enabled          INTEGER NOT NULL DEFAULT 0
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS observed_apps_last_seen_idx
                ON observed_apps(last_seen_at DESC);

            CREATE TABLE IF NOT EXISTS schema_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .context("bootstrap schema")?;
        let current: Option<String> = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("read schema_meta.version")?;
        match current.as_deref() {
            None => {
                conn.execute(
                    "INSERT INTO schema_meta(key, value) VALUES ('version', ?1)",
                    params![SCHEMA_VERSION],
                )
                .context("seed schema_meta.version")?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(other) => {
                // Future schema bump lands here. For v1 we don't
                // pretend we can downgrade; surface the mismatch so
                // an older agent against a newer DB stays well-behaved.
                return Err(anyhow!(
                    "observed_apps schema version {} is newer than this build's {}",
                    other,
                    SCHEMA_VERSION
                ));
            }
        }
        Ok(())
    }

    /// Insert-or-touch for an observed app. Refreshes `last_seen_at`
    /// (and `friendly_name`, in case the app's display string improved
    /// across runs) without ever resetting `enabled` — that bit is the
    /// user's, owned only by `set_enabled`. Returns `true` when the row
    /// was newly inserted, so the caller can decide whether to fire an
    /// `observed-apps-updated` event.
    pub fn upsert(
        &self,
        bundle_id: &str,
        friendly_name: &str,
        seen_at: DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let now = seen_at.to_rfc3339();
        // Two-step (existence check + upsert) so we can return a
        // typed bool to the caller. INSERT OR REPLACE would lose the
        // user's `enabled` bit; ON CONFLICT preserves it.
        let existed: bool = conn
            .query_row(
                "SELECT 1 FROM observed_apps WHERE bundle_id = ?1",
                params![bundle_id],
                |_| Ok(true),
            )
            .optional()
            .context("probe existing row")?
            .unwrap_or(false);
        conn.execute(
            r#"INSERT INTO observed_apps
                   (bundle_id, friendly_name, first_seen_at, last_seen_at, enabled)
               VALUES (?1, ?2, ?3, ?3, 0)
               ON CONFLICT(bundle_id) DO UPDATE SET
                   last_seen_at  = excluded.last_seen_at,
                   friendly_name = excluded.friendly_name"#,
            params![bundle_id, friendly_name, now],
        )
        .context("upsert observed_apps row")?;
        Ok(!existed)
    }

    /// Newest-first list of every observed app. Used by the Settings
    /// page paint and by the IPC bundle command.
    pub fn list(&self) -> Result<Vec<ObservedApp>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT bundle_id, friendly_name, first_seen_at, last_seen_at, enabled
             FROM observed_apps
             ORDER BY last_seen_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let first_str: String = row.get(2)?;
                let last_str: String = row.get(3)?;
                let enabled: i64 = row.get(4)?;
                Ok(ObservedApp {
                    bundle_id: row.get(0)?,
                    friendly_name: row.get(1)?,
                    first_seen_at: DateTime::parse_from_rfc3339(&first_str)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    last_seen_at: DateTime::parse_from_rfc3339(&last_str)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    enabled: enabled != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Toggle one row's `enabled` flag. The IPC command guards against
    /// a UI race where the row was forgotten between paint and click;
    /// we mirror that here by returning an error when the row is gone.
    pub fn set_enabled(&self, bundle_id: &str, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let n = conn.execute(
            "UPDATE observed_apps SET enabled = ?1 WHERE bundle_id = ?2",
            params![if enabled { 1 } else { 0 }, bundle_id],
        )?;
        if n == 0 {
            return Err(anyhow!("no observed_apps row for {}", bundle_id));
        }
        Ok(())
    }

    /// True when the row exists AND has `enabled = 1`. Hot path during
    /// the auto-record gate; no allocation beyond the single query.
    pub fn is_enabled(&self, bundle_id: &str) -> Result<bool> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let row: Option<i64> = conn
            .query_row(
                "SELECT enabled FROM observed_apps WHERE bundle_id = ?1",
                params![bundle_id],
                |row| row.get(0),
            )
            .optional()
            .context("probe enabled flag")?;
        Ok(row.map(|v| v != 0).unwrap_or(false))
    }

    /// Drop a row. The user can clean up false positives ("oh, that
    /// was Slack's notification ping"). The row will reappear next time
    /// the app actually grabs the mic — that's intentional.
    pub fn forget(&self, bundle_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        conn.execute(
            "DELETE FROM observed_apps WHERE bundle_id = ?1",
            params![bundle_id],
        )?;
        Ok(())
    }

    /// Sweep stale rows whose stable `bundle_id` is now blacklisted.
    /// Called once at agent startup so users upgrading
    /// from v0.14.0–v0.14.2 (when the source-side blacklist wasn't
    /// applied — see #604) don't have to manually click Forget on
    /// every leaked `aftercalls` / `parec` / `Chromium input` row
    /// (#605). Returns the number of rows deleted so the caller can
    /// log the cleanup.
    pub fn purge_blacklisted_rows(&self) -> Result<usize> {
        // Pull every (bundle, friendly) pair out under the lock, then
        // drop the lock before running the blacklist predicate +
        // delete loop. Avoids the borrow-checker friction of holding
        // both `stmt` (borrows `conn`) and the predicate iterator
        // alive in one expression, and keeps the mutex held for as
        // little time as possible.
        let rows: Vec<(String, String)> = {
            let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
            let mut stmt = conn.prepare("SELECT bundle_id, friendly_name FROM observed_apps")?;
            let collected: Vec<(String, String)> = stmt
                .query_map([], |row| {
                    let b: String = row.get(0)?;
                    let f: String = row.get(1)?;
                    Ok((b, f))
                })?
                .collect::<rusqlite::Result<_>>()?;
            collected
        };

        let to_delete: Vec<String> = rows
            .into_iter()
            .filter(|(b, _)| crate::mic_consumers::is_blacklisted(b))
            .map(|(b, _)| b)
            .collect();

        if to_delete.is_empty() {
            return Ok(0);
        }

        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let mut deleted = 0;
        for bundle_id in &to_delete {
            deleted += conn.execute(
                "DELETE FROM observed_apps WHERE bundle_id = ?1",
                params![bundle_id],
            )?;
        }
        Ok(deleted)
    }
}

/// Resolve the canonical on-disk path for the agent's local sqlite
/// store. Mirrors `config::config_path()`'s use of
/// `AFTERCALLS_PROFILE` so a dev profile and the installed prod
/// profile have separate stores.
pub fn agent_db_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no user config dir"))?;
    let profile = match std::env::var("AFTERCALLS_PROFILE") {
        Ok(p) if !p.is_empty() => format!("aftercalls-{p}"),
        _ => "aftercalls".to_string(),
    };
    Ok(dir.join(profile).join("agent.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn bootstrap_then_upsert_reads_back() {
        let store = AppObservations::open_in_memory().unwrap();
        let inserted = store.upsert("zoom", "Zoom", now()).unwrap();
        assert!(inserted);
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bundle_id, "zoom");
        assert_eq!(rows[0].friendly_name, "Zoom");
        assert!(!rows[0].enabled);
    }

    #[test]
    fn second_upsert_returns_false_and_preserves_enabled() {
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("zoom", "Zoom", now()).unwrap();
        store.set_enabled("zoom", true).unwrap();

        let inserted = store.upsert("zoom", "Zoom: Meeting", now()).unwrap();
        assert!(!inserted);

        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        // friendly_name refreshed
        assert_eq!(rows[0].friendly_name, "Zoom: Meeting");
        // enabled bit preserved
        assert!(rows[0].enabled);
    }

    #[test]
    fn list_sorted_by_last_seen_desc() {
        let store = AppObservations::open_in_memory().unwrap();
        let t0 = now();
        store.upsert("firefox", "Firefox", t0 - Duration::minutes(10)).unwrap();
        store.upsert("zoom", "Zoom", t0 - Duration::minutes(1)).unwrap();
        store.upsert("slack", "Slack", t0 - Duration::minutes(5)).unwrap();
        let rows = store.list().unwrap();
        let order: Vec<_> = rows.iter().map(|r| r.bundle_id.as_str()).collect();
        assert_eq!(order, vec!["zoom", "slack", "firefox"]);
    }

    #[test]
    fn set_enabled_missing_row_errors() {
        let store = AppObservations::open_in_memory().unwrap();
        let res = store.set_enabled("ghost", true);
        assert!(res.is_err());
    }

    #[test]
    fn forget_removes_row_and_is_idempotent() {
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("slack", "Slack", now()).unwrap();
        store.forget("slack").unwrap();
        assert_eq!(store.list().unwrap().len(), 0);
        // idempotent — second forget is a no-op
        store.forget("slack").unwrap();
    }

    #[test]
    fn purge_blacklisted_rows_drops_stale_pre_v0_14_3_rows() {
        // Users upgrading from v0.14.0–v0.14.2 carried these in their
        // store from before the source-side blacklist landed (#604).
        // Auto-purge at startup means they don't have to hunt for the
        // Forget button (which was itself broken on wlroots Wayland —
        // see #605, this same release).
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("aftercalls", "PipeWire ALSA [aftercalls]", now()).unwrap();
        store.upsert("parec", "parec", now()).unwrap();
        store.upsert("Chromium input", "Chromium input", now()).unwrap();
        store.upsert("cliq", "Zoho Cliq", now()).unwrap();
        store.upsert("zoom", "Zoom", now()).unwrap();

        let deleted = store.purge_blacklisted_rows().unwrap();
        assert_eq!(deleted, 3);

        let remaining: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.bundle_id)
            .collect();
        assert!(remaining.contains(&"cliq".to_string()));
        assert!(remaining.contains(&"zoom".to_string()));
        assert_eq!(remaining.len(), 2);

        // Idempotent — calling again is a no-op.
        let deleted = store.purge_blacklisted_rows().unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn purge_preserves_real_bundle_with_generic_friendly_name() {
        // #610: Electron/Chromium apps can report a generic friendly
        // label while still carrying a real process binary. Preserve
        // that row, including the user's enabled bit.
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("cliq", "Chromium input", now()).unwrap();
        store.set_enabled("cliq", true).unwrap();
        let deleted = store.purge_blacklisted_rows().unwrap();
        assert_eq!(deleted, 0);
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bundle_id, "cliq");
        assert!(rows[0].enabled);
    }

    #[test]
    fn is_enabled_reflects_row_state() {
        let store = AppObservations::open_in_memory().unwrap();
        assert!(!store.is_enabled("zoom").unwrap());
        store.upsert("zoom", "Zoom", now()).unwrap();
        assert!(!store.is_enabled("zoom").unwrap());
        store.set_enabled("zoom", true).unwrap();
        assert!(store.is_enabled("zoom").unwrap());
        store.set_enabled("zoom", false).unwrap();
        assert!(!store.is_enabled("zoom").unwrap());
    }

    #[test]
    fn upsert_advances_last_seen_but_keeps_first_seen() {
        let store = AppObservations::open_in_memory().unwrap();
        let t0 = now() - Duration::minutes(30);
        store.upsert("zoom", "Zoom", t0).unwrap();
        let t1 = now();
        store.upsert("zoom", "Zoom", t1).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        // first_seen frozen at t0; last_seen advanced to t1
        assert!((rows[0].first_seen_at - t0).num_seconds().abs() <= 1);
        assert!((rows[0].last_seen_at - t1).num_seconds().abs() <= 1);
    }
}
