//! Shared transcript types, kept on the agent side so the pipeline and
//! vault writer can round-trip them. The actual AssemblyAI work moved
//! to the backend — see backend/src/transcription.rs.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Utterance {
    pub speaker: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MergedTranscript {
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub timeline: Vec<Utterance>,
}
