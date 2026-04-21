//! Shared summary type. OpenAI call moved server-side; this stays on the
//! agent so the vault writer can consume it without a JSON dance.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Summary {
    pub title: String,
    #[serde(default)]
    pub matched_client: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub action_items: Vec<String>,
    #[serde(default)]
    pub participants: Vec<String>,
}
