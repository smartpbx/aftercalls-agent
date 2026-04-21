use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

/// The on-disk config used to carry per-user API keys (AssemblyAI, OpenAI)
/// along with vault paths and a backend bearer token. Transcription +
/// summarization moved server-side, so user machines no longer need the
/// external API keys — the backend holds them per-org.
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub vault: Vault,
    #[serde(default)]
    pub backend: Option<Backend>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Backend {
    pub url: String,
    /// Legacy opaque bearer token. Left here so existing dev setups keep
    /// working through the auth transition; new installs use auth.json
    /// (email/password → access + refresh token) via [`auth_file`].
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Vault {
    pub path: String,
    pub clients_subpath: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read config at {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&content).context("parse config toml")?;

        // Dev override: point the agent at a different backend without
        // editing config.toml.
        if let Ok(url) = env::var("AFTERCALLS_BACKEND_URL") {
            let token = env::var("AFTERCALLS_BACKEND_TOKEN").ok();
            cfg.backend = Some(Backend { url, token });
        }
        Ok(cfg)
    }
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no user config dir"))?;
    Ok(dir.join("aftercalls").join("config.toml"))
}

/// Path to auth.json — email/password login stashes access + refresh
/// tokens here with chmod 600. Separate file from config.toml so the
/// user-editable config and the machine-managed credentials don't
/// interleave.
pub fn auth_file() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no user config dir"))?;
    Ok(dir.join("aftercalls").join("auth.json"))
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
    pub display_name: String,
    pub role: String,
    pub org_id: String,
    pub org_slug: String,
    pub org_display_name: String,
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
