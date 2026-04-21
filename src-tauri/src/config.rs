use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub api_keys: ApiKeys,
    pub openai: OpenAI,
    pub vault: Vault,
    #[serde(default)]
    pub backend: Option<Backend>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Backend {
    pub url: String,
    pub token: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ApiKeys {
    pub assemblyai: String,
    pub openai: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct OpenAI {
    pub model: String,
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

        if let Ok(v) = env::var("CALLSCRIBE_ASSEMBLYAI_KEY") {
            cfg.api_keys.assemblyai = v;
        }
        if let Ok(v) = env::var("CALLSCRIBE_OPENAI_KEY") {
            cfg.api_keys.openai = v;
        }
        if let Ok(v) = env::var("CALLSCRIBE_OPENAI_MODEL") {
            cfg.openai.model = v;
        }
        if let (Ok(url), Ok(token)) = (
            env::var("CALLSCRIBE_BACKEND_URL"),
            env::var("CALLSCRIBE_BACKEND_TOKEN"),
        ) {
            cfg.backend = Some(Backend { url, token });
        }
        Ok(cfg)
    }
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no user config dir"))?;
    Ok(dir.join("aftercalls").join("config.toml"))
}
