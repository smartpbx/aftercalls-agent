use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Vault;
use crate::summary::Summary;
use crate::transcription::MergedTranscript;

fn resolve_descendant_dir(root: &Path, relative: &Path, create: bool) -> Result<Option<PathBuf>> {
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolve vault root {}", root.display()))?;
    if !canonical_root.is_dir() {
        anyhow::bail!("vault root is not a directory");
    }
    let mut current = canonical_root.clone();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("vault subfolder contains an unsafe path component");
        };
        let next = current.join(name);
        match next.symlink_metadata() {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    anyhow::bail!("vault subfolder is not a real directory: {}", next.display());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                fs::create_dir(&next)
                    .with_context(|| format!("create vault folder {}", next.display()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect vault folder {}", next.display()))
            }
        }
        let canonical = next
            .canonicalize()
            .with_context(|| format!("resolve vault folder {}", next.display()))?;
        if !canonical.starts_with(&canonical_root) {
            anyhow::bail!("vault subfolder escaped the selected vault");
        }
        current = canonical;
    }
    Ok(Some(current))
}

pub fn list_clients(vault: &Vault) -> Result<Vec<String>> {
    let Some(clients_dir) = resolve_descendant_dir(
        Path::new(&vault.path),
        Path::new(&vault.clients_subpath),
        false,
    )? else {
        return Ok(Vec::new());
    };
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
    let clients_dir = resolve_descendant_dir(
        Path::new(&vault.path),
        Path::new(&vault.clients_subpath),
        true,
    )?
    .ok_or_else(|| anyhow::anyhow!("vault clients directory was not created"))?;
    let client_dir = resolve_descendant_dir(&clients_dir, Path::new(&client_folder), true)?
        .ok_or_else(|| anyhow::anyhow!("vault client directory was not created"))?;
    let calls_dir = resolve_descendant_dir(&client_dir, Path::new("calls"), true)?
        .ok_or_else(|| anyhow::anyhow!("vault calls directory was not created"))?;

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let safe_title = sanitize_for_filename(&summary.title);
    let filename = format!("{date} {safe_title}.md");
    let note_path = calls_dir.join(&filename);

    let content = render_markdown(summary, transcript, session_dir);
    let staged = calls_dir.join(format!(
        ".aftercalls-note-{}.part",
        uuid::Uuid::new_v4().simple()
    ));
    let stage_guard = crate::media_manifest::reserve_private_stage(&staged)?;
    fs::write(&staged, content)
        .with_context(|| format!("write staged note {}", staged.display()))?;
    crate::media_manifest::enforce_private_file(&staged)?;
    crate::media_manifest::sync_staged_file(&staged)
        .with_context(|| format!("sync staged note {}", staged.display()))?;
    crate::media_manifest::atomic_replace_file(&staged, &note_path)
        .with_context(|| format!("publish {}", note_path.display()))?;
    drop(stage_guard);
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
            // Prefix the assignee when the LLM resolved one. Keep the
            // description as-is (still contains any `<name>` markers
            // for OTHER people mentioned in the body — stripping
            // those is intentionally deferred; vault notes go to an
            // Obsidian-like renderer that tolerates loose HTML and
            // the tags stay machine-readable for future tooling).
            let desc = item.description.trim();
            match &item.assignee_name {
                Some(who) if !who.trim().is_empty() => {
                    md.push_str(&format!("- {}: {}\n", who.trim(), desc));
                }
                _ => {
                    md.push_str(&format!("- {}\n", desc));
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aftercalls-vault-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn descendant_creation_stays_under_canonical_vault_root() {
        let scratch = Scratch::new();
        let resolved = resolve_descendant_dir(&scratch.0, Path::new("Clients/Active"), true)
            .unwrap()
            .unwrap();
        assert!(resolved.starts_with(scratch.0.canonicalize().unwrap()));
        assert!(resolved.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn descendant_resolution_rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let outside = scratch.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, scratch.0.join("Clients")).unwrap();
        assert!(resolve_descendant_dir(&scratch.0, Path::new("Clients"), false).is_err());
    }
}
