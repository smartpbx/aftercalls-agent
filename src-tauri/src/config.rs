use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

/// The on-disk config used to carry per-user API keys (AssemblyAI, OpenAI)
/// along with vault paths and a backend bearer token. Transcription +
/// summarization moved server-side, so user machines no longer need the
/// external API keys — the backend holds them per-org.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Config {
    // Optional so fresh installs can hit the login screen without the
    // user having to hand-write a config.toml first. Pipeline code
    // skips note-writing gracefully when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<Vault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// When true (default), clicking the X button hides the window to
    /// the tray and keeps the process + detector alive. When false, X
    /// exits the app entirely. Kept as a per-user-machine preference
    /// rather than an org setting because it reflects personal OS-
    /// close-button habits.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// When true (default), the mic-consumer detector watches for
    /// known call apps (Zoom, Teams, browser call tabs, etc.) and
    /// prompts to record. Off disables the prompt AND the poll loop
    /// — no CPU wasted if the user never wants auto-detection.
    #[serde(default = "default_true")]
    pub auto_detect: bool,
    /// #180 (v0.6.x) — when true (default), an auto-detected call
    /// also raises and focuses the main window so the in-app prompt
    /// is unmissable. Off keeps the in-app prompt firing (the
    /// detector still emits its event so the slide-out renders) but
    /// suppresses the window-show / focus path. Tiling-WM users
    /// (Hyprland, Sway, etc.) flip this off so a detected call
    /// doesn't yank focus mid-task. Serde default keeps existing
    /// config.toml files loading cleanly with the historical
    /// always-focus behavior.
    #[serde(default = "default_true")]
    pub auto_detect_popup: bool,
    /// When true (default during dev), the agent ships a ring buffer
    /// of log entries + pipeline events + panics to
    /// `/v1/agent/logs/batch` every 30s. Off disables the flush task
    /// AND the per-event buffer push, so zero trace data leaves the
    /// machine. See #<telemetry issue>.
    #[serde(default = "default_true")]
    pub telemetry_enabled: bool,
    /// Short synthesized tones on recording start/stop, pipeline
    /// complete/failed, auto-detect prompt. Default on. Off silences
    /// every notification the agent produces (system notifications
    /// from tauri-plugin-notification are unaffected).
    #[serde(default = "default_true")]
    pub sounds_enabled: bool,
    /// Hard upper bound on a single recording's length, in minutes.
    /// When elapsed time crosses this threshold the recorder auto-
    /// stops via the same path as manual Stop. Protects against a
    /// softphone that holds the mic open long after a call ended and
    /// prevents a runaway multi-gigabyte WAV. Default 120min (2h);
    /// validated in the UI to [5, 1440].
    #[serde(default = "default_max_recording_minutes")]
    pub max_recording_minutes: u32,
    /// #142 · v0.4.5 — Hard upper bound on a single note-to-self
    /// recording. Half the regular cap by default because a
    /// dictation is typically 10–120s; the 5-minute ceiling is a
    /// safety rail against a user walking away from the mic. UI
    /// clamps to [1, 60]; serde default covers existing config.toml
    /// files.
    #[serde(default = "default_max_self_note_minutes")]
    pub max_self_note_minutes: u32,
    /// When true, the record screen exposes a manual notes panel during
    /// an active recording. Notes persist to the session_dir and ride
    /// into create_call; the backend optionally feeds them into the
    /// summarizer. Off by default — opt-in so the record screen stays
    /// minimal for users who don't take notes.
    #[serde(default)]
    pub manual_notes_enabled: bool,
    /// Tracks whether the Linux in-app note under the Super+Shift+R
    /// kbd-row has been dismissed. The note explains that the global
    /// hotkey only reaches an unfocused app on most desktop
    /// environments when the shortcut is bound in the user's own
    /// keyboard-shortcut config (via the `aftercalls --toggle-recording`
    /// CLI). Defaults to false so fresh installs see the note once;
    /// serde-default covers existing config.toml files without the
    /// key.
    #[serde(default)]
    pub wayland_hotkey_notice_dismissed: bool,
    /// Preferred microphone identified by its cpal-reported `name()`.
    /// `None` (key absent from config.toml) means "use system default"
    /// — this is both the fresh-install state and the explicit-reset
    /// state (the Settings UI clears the key by selecting "System
    /// default"). cpal has no stable cross-platform device ID, so we
    /// store the name and resolve it at `recorder::begin()` time;
    /// miss → silent fall-through to `host.default_input_device()`
    /// with a one-time `mic-fallback` toast on the Record page. See
    /// #3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_device: Option<String>,
    /// #149 (v0.4.7) — user-configurable global shortcut for
    /// "note to self" capture. `Some("Super+Shift+N")` is the default
    /// so fresh installs behave exactly like v0.4.6 did; the user can
    /// rebind from Settings (`reapply_self_note_hotkey` re-binds the
    /// OS hotkey on save). Serialized as a single string so the TOML
    /// stays human-readable. `None` disables the hotkey entirely;
    /// tray + button + CLI keep working.
    #[serde(default = "default_self_note_shortcut")]
    pub self_note_shortcut: Option<String>,
    /// #161 (v0.5.2) — user-configurable global shortcut for
    /// "start/stop recording". `Some("Super+Shift+R")` is the default
    /// so fresh installs behave exactly like v0.5.1 did; the user can
    /// rebind from Settings (`reapply_record_toggle_hotkey` re-binds
    /// the OS hotkey on save). Mirror of `self_note_shortcut` above.
    /// `None` disables the hotkey entirely; tray + UI Record button +
    /// CLI keep working.
    #[serde(default = "default_record_toggle_shortcut")]
    pub record_toggle_shortcut: Option<String>,
    /// #56 — when true, the spoken recording-start announcement
    /// ("Please note — this call is being recorded") plays after the
    /// start chime. Defaults OFF: the chime alone is sufficient for
    /// most users, and an unexpected voice on the very first
    /// recording would surprise people. The org-level
    /// `recording_notification_mode = enforced` path bypasses this
    /// pref entirely — compliance posture set by an admin can't be
    /// silenced from local Settings. Serde default keeps existing
    /// config.toml files loading cleanly.
    #[serde(default)]
    pub consent_announcement_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_max_recording_minutes() -> u32 {
    120
}

fn default_max_self_note_minutes() -> u32 {
    5
}

fn default_self_note_shortcut() -> Option<String> {
    Some("Super+Shift+N".to_string())
}

fn default_record_toggle_shortcut() -> Option<String> {
    Some("Super+Shift+R".to_string())
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Backend {
    pub url: String,
    /// Legacy opaque bearer token. Left here so existing dev setups keep
    /// working through the auth transition; new installs use auth.json
    /// (email/password → access + refresh token) via [`auth_file`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Vault {
    pub path: String,
    pub clients_subpath: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let mut cfg: Config = if path.exists() {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("read config at {}", path.display()))?;
            toml::from_str(&content).context("parse config toml")?
        } else {
            // Fresh install (no config.toml). Write a default pointing at
            // prod so the login screen works out of the box; user can
            // fill in vault + overrides later from Settings.
            let cfg = Config {
                vault: None,
                backend: Some(Backend {
                    url: "https://api.aftercalls.io".to_string(),
                    token: None,
                }),
                close_to_tray: true,
                auto_detect: true,
                auto_detect_popup: true,
                telemetry_enabled: true,
                sounds_enabled: true,
                max_recording_minutes: default_max_recording_minutes(),
                max_self_note_minutes: default_max_self_note_minutes(),
                manual_notes_enabled: false,
                wayland_hotkey_notice_dismissed: false,
                input_device: None,
                self_note_shortcut: default_self_note_shortcut(),
                record_toggle_shortcut: default_record_toggle_shortcut(),
                consent_announcement_enabled: false,
            };
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).context("mkdir config dir")?;
            }
            let default_toml = "[backend]\nurl = \"https://api.aftercalls.io\"\n";
            fs::write(&path, default_toml)
                .with_context(|| format!("write default config at {}", path.display()))?;
            cfg
        };

        // Dev override: point the agent at a different backend without
        // editing config.toml.
        if let Ok(url) = env::var("AFTERCALLS_BACKEND_URL") {
            let token = env::var("AFTERCALLS_BACKEND_TOKEN").ok();
            cfg.backend = Some(Backend { url, token });
        }
        Ok(cfg)
    }

    /// Serialize back to config.toml. Used by the Settings UI to flip
    /// the vault section on/off and persist the path. Round-trips the
    /// whole struct, which means user-written comments in the file
    /// get stripped — that's acceptable here because the config file
    /// is tiny and machine-managed.
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("mkdir config dir")?;
        }
        let text = toml::to_string_pretty(self).context("serialize config")?;
        fs::write(&path, text)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

/// Name of the config subdirectory. `AFTERCALLS_PROFILE=dev` switches to
/// `aftercalls-dev` so a locally-running dev build and the installed prod
/// build don't stomp each other's config + credentials.
fn profile_dir_name() -> String {
    match env::var("AFTERCALLS_PROFILE") {
        Ok(p) if !p.is_empty() => format!("aftercalls-{p}"),
        _ => "aftercalls".to_string(),
    }
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no user config dir"))?;
    Ok(dir.join(profile_dir_name()).join("config.toml"))
}

/// Path to auth.json — email/password login stashes access + refresh
/// tokens here with chmod 600. Separate file from config.toml so the
/// user-editable config and the machine-managed credentials don't
/// interleave.
pub fn auth_file() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no user config dir"))?;
    Ok(dir.join(profile_dir_name()).join("auth.json"))
}

/// Shape of auth.json. Serialized by the login flow, read by the
/// authenticated HTTP client that talks to the backend.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthFile {
    pub access_token: String,
    pub access_expires_at: chrono::DateTime<chrono::Utc>,
    pub refresh_token: String,
    pub refresh_expires_at: chrono::DateTime<chrono::Utc>,
    pub user_id: String,
    pub email: String,
    /// Structured names (#96). Serde defaults so old auth.json files
    /// written before the split still parse — backend backfill means
    /// the next login populates them. Meanwhile display_name remains
    /// authoritative for rendering.
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    pub display_name: String,
    pub role: String,
    /// #86 — aftercalls-staff capability flag, orthogonal to role.
    /// Serde default keeps old auth.json files (written before this
    /// field existed) readable; next login repopulates from backend.
    #[serde(default)]
    pub is_platform_staff: bool,
    pub org_id: String,
    pub org_slug: String,
    pub org_display_name: String,
    /// Cached from `/v1/auth/me`'s `recording_acknowledged` at login.
    /// The Record page uses this as the source of truth to decide
    /// whether to show the PIPEDA ack modal before Start Recording;
    /// flipped true locally after a successful POST so future clicks
    /// short-circuit without a roundtrip. Serde default keeps old
    /// auth.json files (written before this field existed) readable.
    #[serde(default)]
    pub recording_acknowledged: bool,
    /// #215 — per-org feature flags. Same backward-compat posture as
    /// the other additive fields: serde default of an all-false snapshot
    /// keeps older auth.json files parseable, and the next login
    /// repopulates from `/v1/auth/me`. The Send-to-CRM affordances + the
    /// `/admin/zoho` admin link gate on `features.zoho` so a stale
    /// auth.json from before this field landed simply hides the Zoho UI
    /// (matching the per-org-disabled state). The backend is still the
    /// security boundary; this is purely UX.
    #[serde(default)]
    pub features: FeatureFlags,
}

/// Mirror of the backend's `FeaturesSnapshot` shape (see
/// `backend/src/routes/features.rs`). Serde default = all-false so
/// older auth.json + missing-key cases land as "feature OFF" — same
/// semantics as the backend's absent-row default.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct FeatureFlags {
    #[serde(default)]
    pub zoho: bool,
}

pub fn read_auth_file() -> Result<Option<AuthFile>> {
    let p = auth_file()?;
    if !p.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&p)
        .with_context(|| format!("read auth at {}", p.display()))?;
    let parsed: AuthFile = serde_json::from_str(&text).context("parse auth.json")?;
    Ok(Some(parsed))
}

pub fn write_auth_file(auth: &AuthFile) -> Result<()> {
    let p = auth_file()?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).context("mkdir config dir")?;
    }
    let json = serde_json::to_string_pretty(auth).context("serialize auth.json")?;
    fs::write(&p, json).with_context(|| format!("write {}", p.display()))?;
    // chmod 600 — keep tokens readable only by the user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&p)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&p, perms)?;
    }
    Ok(())
}

pub fn delete_auth_file() -> Result<()> {
    let p = auth_file()?;
    if p.exists() {
        fs::remove_file(&p)?;
    }
    Ok(())
}
