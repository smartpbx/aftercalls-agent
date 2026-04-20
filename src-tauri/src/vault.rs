use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Vault;
use crate::summary::Summary;
use crate::transcription::MergedTranscript;

pub fn list_clients(vault: &Vault) -> Result<Vec<String>> {
    let clients_dir = Path::new(&vault.path).join(&vault.clients_subpath);
    if !clients_dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&clients_dir).with_context(|| format!("read {}", clients_dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

pub fn write_note(
    vault: &Vault,
    summary: &Summary,
    transcript: &MergedTranscript,
    session_dir: &Path,
    candidates: &[String],
) -> Result<PathBuf> {
    let client_folder = resolve_client_folder(summary.matched_client.as_deref(), candidates);
    let calls_dir = Path::new(&vault.path)
        .join(&vault.clients_subpath)
        .join(&client_folder)
        .join("calls");
    fs::create_dir_all(&calls_dir)
        .with_context(|| format!("create {}", calls_dir.display()))?;

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let safe_title = sanitize_for_filename(&summary.title);
    let filename = format!("{date} {safe_title}.md");
    let note_path = calls_dir.join(&filename);

    let content = render_markdown(summary, transcript, session_dir);
    fs::write(&note_path, content)
        .with_context(|| format!("write {}", note_path.display()))?;
    Ok(note_path)
}

fn resolve_client_folder(matched: Option<&str>, candidates: &[String]) -> String {
    match matched {
        Some(m) if candidates.iter().any(|c| c == m) => m.to_string(),
        _ => "_Unsorted".to_string(),
    }
}

fn sanitize_for_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "call".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn render_markdown(summary: &Summary, transcript: &MergedTranscript, session_dir: &Path) -> String {
    let mut md = String::new();
    md.push_str("---\n");
    md.push_str(&format!("date: {}\n", Utc::now().format("%Y-%m-%d")));
    md.push_str(&format!(
        "session: {}\n",
        session_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    if let Some(client) = &summary.matched_client {
        md.push_str(&format!("matched_client: \"{}\"\n", client.replace('"', "\\\"")));
    }
    if !summary.participants.is_empty() {
        md.push_str("participants:\n");
        for p in &summary.participants {
            md.push_str(&format!("  - \"{}\"\n", p.replace('"', "\\\"")));
        }
    }
    md.push_str("---\n\n");

    md.push_str(&format!("# {}\n\n", summary.title));

    md.push_str("## Summary\n\n");
    md.push_str(summary.summary.trim());
    md.push_str("\n\n");

    if !summary.action_items.is_empty() {
        md.push_str("## Action Items\n\n");
        for item in &summary.action_items {
            md.push_str(&format!("- {}\n", item.trim()));
        }
        md.push('\n');
    }

    md.push_str("## Transcript\n\n");
    for u in &transcript.timeline {
        md.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            format_ts(u.start_ms),
            u.speaker,
            u.text.trim()
        ));
    }

    md.push_str("---\n\n");
    md.push_str(&format!(
        "Audio session folder: `{}`\n",
        session_dir.display()
    ));
    md
}

fn format_ts(ms: u64) -> String {
    let total_sec = ms / 1000;
    format!("{:02}:{:02}", total_sec / 60, total_sec % 60)
}
