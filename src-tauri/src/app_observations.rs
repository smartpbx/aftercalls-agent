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
/// `bootstrap()` whenever the table shape changes.
///
/// v1 → v2 (this release): the per-row `enabled INTEGER` flag is
/// joined by a string `mode` column with values `auto` / `ask` /
/// `never`. `mode` is authoritative; `enabled` is mirrored on every
/// write so a one-release downgrade still reads sensibly (mode='auto'
/// → enabled=1, anything else → enabled=0).
const SCHEMA_VERSION: &str = "2";

/// The three states a row can occupy. Stored as a CHECK-constrained
/// TEXT column so adding a future state (`auto_with_alarm`, etc.) is
/// a constraint widen, not a column re-type.
///
/// - `Auto`: auto-record on mic capture (master toggle permitting).
/// - `Ask`: prompt the user via the existing detector flow. Default.
/// - `Never`: silence the detector entirely for this app — no toast,
///   no in-app slide-out, no PIPEDA modal. Reversible from Settings.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppMode {
    Auto,
    Ask,
    Never,
}

impl AppMode {
    /// The string form persisted in `observed_apps.mode` and surfaced
    /// over IPC. Mirrors `serde(rename_all = "lowercase")` above so the
    /// SQL value and the IPC value are guaranteed identical.
    pub fn as_str(&self) -> &'static str {
        match self {
            AppMode::Auto => "auto",
            AppMode::Ask => "ask",
            AppMode::Never => "never",
        }
    }

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(AppMode::Auto),
            "ask" => Ok(AppMode::Ask),
            "never" => Ok(AppMode::Never),
            other => Err(anyhow!("unknown app mode {other:?}")),
        }
    }
}

/// One row of `observed_apps`. Mirrored 1:1 to the `AppRow` IPC type
/// in lib.rs; serializing here keeps the SQL access surface in one
/// place.
///
/// `enabled` is retained for one release as a downgrade-safety mirror
/// of `mode == Auto`. Reads derive it; writes set both columns in
/// lockstep so a v0.17.x agent reading a v2 DB still gets the right
/// behavior on the row.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ObservedApp {
    pub bundle_id: String,
    pub friendly_name: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub enabled: bool,
    pub mode: AppMode,
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
        // Base shape — created at v1, kept here for fresh installs.
        // Existing v1 DBs already have this; CREATE IF NOT EXISTS makes
        // both paths idempotent.
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
                // Fresh install. Create the v2 shape (mode column +
                // CHECK) before stamping the version, so the very
                // first INSERT can never violate the constraint.
                Self::migrate_v1_to_v2(&conn)?;
                conn.execute(
                    "INSERT INTO schema_meta(key, value) VALUES ('version', ?1)",
                    params![SCHEMA_VERSION],
                )
                .context("seed schema_meta.version")?;
            }
            Some("1") => {
                // v0.17.x → vNext upgrade path. Add the `mode` column
                // with its CHECK constraint and seed it from the
                // existing `enabled` bit. `enabled` stays around for
                // one release as a downgrade-safety mirror.
                Self::migrate_v1_to_v2(&conn)?;
                conn.execute(
                    "UPDATE schema_meta SET value = ?1 WHERE key = 'version'",
                    params![SCHEMA_VERSION],
                )
                .context("bump schema_meta.version to 2")?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(other) => {
                // Future schema bump lands here. For v2 we don't
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

    /// Add the v2 `mode` column (with its CHECK constraint) and seed
    /// it from the existing `enabled` bit. Safe to call on a fresh DB
    /// (the column won't exist yet) and on a v1 upgrade. The CHECK is
    /// written in the SAME statement as the ADD COLUMN so the very
    /// first write carrying a `mode` value can't violate the
    /// constraint — see `feedback_check_constraint_widening.md`.
    fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
        // sqlite has no IF NOT EXISTS for ADD COLUMN; pre-probe.
        let already_has_mode: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('observed_apps') WHERE name = 'mode'",
                [],
                |_| Ok(true),
            )
            .optional()
            .context("probe observed_apps.mode existence")?
            .unwrap_or(false);
        if !already_has_mode {
            conn.execute(
                "ALTER TABLE observed_apps
                   ADD COLUMN mode TEXT NOT NULL DEFAULT 'ask'
                   CHECK (mode IN ('auto','ask','never'))",
                [],
            )
            .context("add observed_apps.mode column")?;
            // Backfill: rows previously ticked map to Auto; everything
            // else stays at the column default 'ask'. The Never mode
            // is post-v2-only, so no existing row gets it on upgrade.
            conn.execute(
                "UPDATE observed_apps SET mode = 'auto' WHERE enabled = 1",
                [],
            )
            .context("backfill observed_apps.mode from enabled")?;
        }
        Ok(())
    }

    /// Insert-or-touch for an observed app. Refreshes `last_seen_at`
    /// (and `friendly_name`, in case the app's display string improved
    /// across runs) without ever resetting `mode` — that's the user's,
    /// owned only by `set_mode` (and its legacy `set_enabled` shim).
    /// Returns `true` when the row was newly inserted, so the caller
    /// can decide whether to fire an `observed-apps-updated` event.
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
        // user's `mode` choice; ON CONFLICT preserves it.
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
                   (bundle_id, friendly_name, first_seen_at, last_seen_at, enabled, mode)
               VALUES (?1, ?2, ?3, ?3, 0, 'ask')
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
            "SELECT bundle_id, friendly_name, first_seen_at, last_seen_at, enabled, mode
             FROM observed_apps
             ORDER BY last_seen_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let first_str: String = row.get(2)?;
                let last_str: String = row.get(3)?;
                let enabled: i64 = row.get(4)?;
                let mode_str: String = row.get(5)?;
                // The CHECK constraint guarantees `mode_str` is one of
                // the known values, but be defensive: fall back to Ask
                // on any unexpected literal so we don't poison the
                // list paint over one bad row.
                let mode = AppMode::from_str(&mode_str).unwrap_or(AppMode::Ask);
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
                    mode,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Update one row's `mode`. Writes both `mode` and the legacy
    /// `enabled` mirror in one statement so a one-release downgrade
    /// (v0.17.x reading a v2 DB) still gets the right behaviour on
    /// the row. The IPC command guards against a UI race where the
    /// row was forgotten between paint and click; we mirror that here
    /// by returning an error when the row is gone.
    pub fn set_mode(&self, bundle_id: &str, mode: AppMode) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let enabled = if matches!(mode, AppMode::Auto) { 1 } else { 0 };
        let n = conn.execute(
            "UPDATE observed_apps SET mode = ?1, enabled = ?2 WHERE bundle_id = ?3",
            params![mode.as_str(), enabled, bundle_id],
        )?;
        if n == 0 {
            return Err(anyhow!("no observed_apps row for {}", bundle_id));
        }
        Ok(())
    }

    /// Read one row's `mode`. Returns `None` when the row doesn't
    /// exist (the auto-record / detector hot paths treat a missing
    /// row as `Ask` — the safest default — so callers usually
    /// `.unwrap_or(AppMode::Ask)`).
    pub fn mode_of(&self, bundle_id: &str) -> Result<Option<AppMode>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db mutex poisoned"))?;
        let row: Option<String> = conn
            .query_row(
                "SELECT mode FROM observed_apps WHERE bundle_id = ?1",
                params![bundle_id],
                |row| row.get(0),
            )
            .optional()
            .context("probe mode column")?;
        match row {
            None => Ok(None),
            Some(s) => Ok(Some(AppMode::from_str(&s).unwrap_or(AppMode::Ask))),
        }
    }

    /// Legacy shim: maps `enabled=true → mode=Auto`, `enabled=false →
    /// mode=Ask`. Kept so an unexpected partial rollback (stale
    /// frontend after an in-place update) doesn't 500 on the old IPC
    /// command — and so callers in the hot path that only care about
    /// "auto-record yes/no" don't have to learn the new enum yet.
    pub fn set_enabled(&self, bundle_id: &str, enabled: bool) -> Result<()> {
        self.set_mode(
            bundle_id,
            if enabled { AppMode::Auto } else { AppMode::Ask },
        )
    }

    /// True when the row exists AND has `mode == Auto`. Hot path
    /// during the auto-record gate; equivalent to the previous
    /// `enabled` read but routed through `mode_of` so the two stay in
    /// lockstep without duplicating the column read.
    ///
    /// Currently has no in-tree caller after auto_recorder migrated
    /// to `mode_of`; kept as a stable shim in case an out-of-tree
    /// consumer (or a future surface) needs the boolean read without
    /// learning the new enum.
    #[allow(dead_code)]
    pub fn is_enabled(&self, bundle_id: &str) -> Result<bool> {
        Ok(matches!(self.mode_of(bundle_id)?, Some(AppMode::Auto)))
    }

    /// True when the row exists AND has `mode == Never`. The detector
    /// uses this to drop a bundle from `interesting_mic_consumers`
    /// before any phase transition runs.
    pub fn is_silenced(&self, bundle_id: &str) -> Result<bool> {
        Ok(matches!(self.mode_of(bundle_id)?, Some(AppMode::Never)))
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
        // Fresh rows default to Ask, matching the previous "unticked"
        // behaviour: observed, but no auto-record.
        assert_eq!(rows[0].mode, AppMode::Ask);
    }

    #[test]
    fn second_upsert_returns_false_and_preserves_mode() {
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("zoom", "Zoom", now()).unwrap();
        store.set_enabled("zoom", true).unwrap();

        let inserted = store.upsert("zoom", "Zoom: Meeting", now()).unwrap();
        assert!(!inserted);

        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        // friendly_name refreshed
        assert_eq!(rows[0].friendly_name, "Zoom: Meeting");
        // mode preserved (and `enabled` mirror tracks it)
        assert_eq!(rows[0].mode, AppMode::Auto);
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
        // that row, including the user's mode choice.
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("cliq", "Chromium input", now()).unwrap();
        store.set_enabled("cliq", true).unwrap();
        let deleted = store.purge_blacklisted_rows().unwrap();
        assert_eq!(deleted, 0);
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bundle_id, "cliq");
        assert_eq!(rows[0].mode, AppMode::Auto);
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

    // ── v2 schema additions ─────────────────────────────────────────

    #[test]
    fn mode_defaults_to_ask_on_fresh_upsert() {
        // The visible-but-not-recording state from the v1 era now
        // maps to `mode = Ask`. New rows must land there, not on
        // Auto (would auto-record without consent) or Never (would
        // silently hide a brand-new app from the detector).
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("discord", "Discord", now()).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, AppMode::Ask);
        assert_eq!(store.mode_of("discord").unwrap(), Some(AppMode::Ask));
    }

    #[test]
    fn mode_round_trip_through_set_mode() {
        let store = AppObservations::open_in_memory().unwrap();
        store.upsert("discord", "Discord", now()).unwrap();

        store.set_mode("discord", AppMode::Auto).unwrap();
        assert_eq!(store.mode_of("discord").unwrap(), Some(AppMode::Auto));
        // `enabled` mirror tracks `mode == Auto` for downgrade safety.
        assert!(store.is_enabled("discord").unwrap());
        assert!(!store.is_silenced("discord").unwrap());

        store.set_mode("discord", AppMode::Never).unwrap();
        assert_eq!(store.mode_of("discord").unwrap(), Some(AppMode::Never));
        assert!(!store.is_enabled("discord").unwrap());
        assert!(store.is_silenced("discord").unwrap());

        store.set_mode("discord", AppMode::Ask).unwrap();
        assert_eq!(store.mode_of("discord").unwrap(), Some(AppMode::Ask));
        assert!(!store.is_enabled("discord").unwrap());
        assert!(!store.is_silenced("discord").unwrap());
    }

    #[test]
    fn set_mode_errors_on_missing_row() {
        let store = AppObservations::open_in_memory().unwrap();
        let res = store.set_mode("ghost", AppMode::Never);
        assert!(res.is_err());
    }

    #[test]
    fn is_silenced_is_false_for_missing_and_non_never_rows() {
        // The detector treats a missing row as "ask each time" — same
        // as today's behaviour for an unobserved app. Make sure
        // `is_silenced` doesn't bail on those paths.
        let store = AppObservations::open_in_memory().unwrap();
        assert!(!store.is_silenced("ghost").unwrap());
        store.upsert("zoom", "Zoom", now()).unwrap();
        assert!(!store.is_silenced("zoom").unwrap());
        store.set_mode("zoom", AppMode::Auto).unwrap();
        assert!(!store.is_silenced("zoom").unwrap());
        store.set_mode("zoom", AppMode::Never).unwrap();
        assert!(store.is_silenced("zoom").unwrap());
    }

    #[test]
    fn v1_database_migrates_to_v2_with_mode_seeded_from_enabled() {
        // Simulate a v0.17.x-shaped database: v1 schema, no `mode`
        // column, schema_meta.version = '1', some rows ticked
        // (enabled = 1) and some not. Re-opening with the new build
        // must add the `mode` column, seed it from `enabled`, and
        // stamp the version forward.
        use rusqlite::Connection;

        // tempfile isn't a workspace dep yet and pulling it in for one
        // test isn't worth a fresh dependency review. Use a per-test
        // path under the OS temp dir; the cleanup at the end of the
        // test is best-effort.
        let tmp_dir = std::env::temp_dir().join(format!(
            "aftercalls-app_observations-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("agent.db");
        // Wipe any leftover from a previous test run on this PID.
        let _ = std::fs::remove_file(&path);

        // Hand-roll the v1 shape so we don't accidentally test against
        // the v2 migrator's idempotence.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE observed_apps (
                    bundle_id        TEXT PRIMARY KEY,
                    friendly_name    TEXT NOT NULL,
                    first_seen_at    TEXT NOT NULL,
                    last_seen_at     TEXT NOT NULL,
                    enabled          INTEGER NOT NULL DEFAULT 0
                ) WITHOUT ROWID;
                CREATE INDEX observed_apps_last_seen_idx
                    ON observed_apps(last_seen_at DESC);
                CREATE TABLE schema_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO schema_meta(key, value) VALUES ('version', '1');
                "#,
            )
            .unwrap();
            let now_str = now().to_rfc3339();
            conn.execute(
                "INSERT INTO observed_apps(bundle_id, friendly_name, first_seen_at, last_seen_at, enabled)
                 VALUES ('zoom', 'Zoom', ?1, ?1, 1)",
                params![now_str],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO observed_apps(bundle_id, friendly_name, first_seen_at, last_seen_at, enabled)
                 VALUES ('discord', 'Discord', ?1, ?1, 0)",
                params![now_str],
            )
            .unwrap();
        }

        // Re-open through the production code path. Must succeed AND
        // backfill mode correctly.
        let store = AppObservations::open(&path).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 2);
        let zoom = rows.iter().find(|r| r.bundle_id == "zoom").unwrap();
        let discord = rows.iter().find(|r| r.bundle_id == "discord").unwrap();
        assert_eq!(zoom.mode, AppMode::Auto);
        assert!(zoom.enabled);
        assert_eq!(discord.mode, AppMode::Ask);
        assert!(!discord.enabled);

        // Version was stamped forward and the second open is idempotent.
        let store2 = AppObservations::open(&path).unwrap();
        let rows2 = store2.list().unwrap();
        assert_eq!(rows2.len(), 2);

        // Best-effort cleanup.
        drop(store);
        drop(store2);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }
}
