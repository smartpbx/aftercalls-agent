//! Shared summary type. OpenAI call moved server-side; this stays on the
//! agent so the vault writer can consume it without a JSON dance.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Summary {
    pub title: String,
    #[serde(default)]
    pub matched_client: Option<String>,
    pub summary: String,
    /// Phase 1 of the v0.4.0 action-items bundle (#10 #19 #104 #105)
    /// flips this from `Vec<String>` to `Vec<ActionItem>` so each
    /// item carries a structured `assignee_name` alongside the
    /// description. The `#[serde(untagged)]` fallback handles a
    /// regressing backend that still ships plain strings — routed
    /// through the same path with `assignee_name: None` so the vault
    /// renderer keeps working regardless of which shape comes back.
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    #[serde(default)]
    pub participants: Vec<String>,
}

/// Action item as persisted by the backend and read by the local vault
/// writer. Mirrors the server-side `ActionItemLLM` shape, with the same
/// string-fallback tolerance for backward compatibility with older
/// backends.
#[derive(Serialize, Clone, Debug)]
pub struct ActionItem {
    pub description: String,
    #[serde(default)]
    pub assignee_name: Option<String>,
}

impl<'de> Deserialize<'de> for ActionItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Struct {
                description: String,
                #[serde(default)]
                assignee_name: Option<String>,
            },
            PlainString(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Struct {
                description,
                assignee_name,
            } => Ok(ActionItem {
                description,
                assignee_name: assignee_name.and_then(|s| {
                    let t = s.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                }),
            }),
            Wire::PlainString(s) => Ok(ActionItem {
                description: s,
                assignee_name: None,
            }),
        }
    }
}
