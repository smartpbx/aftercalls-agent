use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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
    let calls_dir = resolve_calls_dir(vault, summary, candidates)?;

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let note_path = calls_dir.join(note_filename(&calls_dir, &date, &summary.title, None));

    let content = render_markdown(summary, transcript, &NoteOrigin::Local { session_dir });
    let staged = stage_note(&calls_dir, &content)?;
    crate::media_manifest::atomic_replace_file(&staged.path, &note_path)
        .with_context(|| format!("publish {}", note_path.display()))?;
    drop(staged);
    Ok(note_path)
}

/// Where a note's content came from. Local recordings have a session
/// folder on this machine; server-imported calls (SmartPBX, Zoho Meeting,
/// …) have no local audio at all, so their note must not point at one.
pub enum NoteOrigin<'a> {
    Local {
        session_dir: &'a Path,
    },
    Imported {
        call_id: &'a str,
        ingest_source: &'a str,
        recorded_at: DateTime<Utc>,
    },
}

/// Result of [`write_imported_note`].
#[derive(Debug, PartialEq, Eq)]
pub enum ImportedNote {
    /// This call wrote a new note file.
    Created(PathBuf),
    /// A note for this call id was already in the calls folder (an earlier
    /// tick that failed to record the path, a renamed title, or the same
    /// vault synced from another machine). Left untouched.
    AlreadyExists(PathBuf),
}

/// Write the vault note for a call imported server-side.
///
/// Same folder layout and client matching as [`write_note`], with these
/// differences for calls that never passed through this machine:
///   * the date in the file name and front matter is the call's
///     `recorded_at`, not today (imports can arrive hours later);
///   * the file name ends in ` (<short call id>).md`, and idempotency is
///     keyed on that suffix alone: any existing note in the calls folder
///     with this call's suffix counts, whatever its date or title, so a
///     re-titled summary never produces a second note;
///   * publishing never overwrites: an existing note is reported as
///     [`ImportedNote::AlreadyExists`] and left alone.
pub fn write_imported_note(
    vault: &Vault,
    summary: &Summary,
    transcript: &MergedTranscript,
    candidates: &[String],
    call_id: &str,
    ingest_source: &str,
    recorded_at: DateTime<Utc>,
) -> Result<ImportedNote> {
    let calls_dir = resolve_calls_dir(vault, summary, candidates)?;
    let short_id = short_call_id(call_id);
    if let Some(existing) = find_note_by_call_suffix(&calls_dir, &short_id)? {
        return Ok(ImportedNote::AlreadyExists(existing));
    }
    let date = recorded_at.format("%Y-%m-%d").to_string();
    let note_path = calls_dir.join(note_filename(&calls_dir, &date, &summary.title, Some(&short_id)));
    let content = render_markdown(
        summary,
        transcript,
        &NoteOrigin::Imported {
            call_id,
            ingest_source,
            recorded_at,
        },
    );
    let staged = stage_note(&calls_dir, &content)?;
    // Hard-linking the fully written, synced stage onto the final name is
    // an atomic create-if-absent: it fails with AlreadyExists instead of
    // replacing a file that appeared since the check above.
    match fs::hard_link(&staged.path, &note_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(ImportedNote::AlreadyExists(note_path));
        }
        Err(_) => {
            // Some filesystems (certain network or FAT volumes) refuse hard
            // links. Fall back to create_new, which is still no-clobber.
            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&note_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Ok(ImportedNote::AlreadyExists(note_path));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create {}", note_path.display()))
                }
            };
            use std::io::Write;
            file.write_all(content.as_bytes())
                .and_then(|()| file.sync_all())
                .with_context(|| format!("write {}", note_path.display()))?;
        }
    }
    drop(staged);
    Ok(ImportedNote::Created(note_path))
}

fn short_call_id(call_id: &str) -> String {
    call_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// An existing note in `calls_dir` whose name ends in ` (<short_id>).md`.
/// Hidden files (our own stages) and anything that isn't a regular file
/// are ignored.
fn find_note_by_call_suffix(calls_dir: &Path, short_id: &str) -> Result<Option<PathBuf>> {
    let suffix = format!(" ({short_id}).md");
    for entry in fs::read_dir(calls_dir).with_context(|| format!("read {}", calls_dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.to_ascii_lowercase().ends_with(&suffix) {
            continue;
        }
        if entry.file_type()?.is_file() {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn resolve_calls_dir(vault: &Vault, summary: &Summary, candidates: &[String]) -> Result<PathBuf> {
    let client_folder = resolve_client_folder(summary.matched_client.as_deref(), candidates);
    let clients_dir = resolve_descendant_dir(
        Path::new(&vault.path),
        Path::new(&vault.clients_subpath),
        true,
    )?
    .ok_or_else(|| anyhow::anyhow!("vault clients directory was not created"))?;
    let client_dir = resolve_descendant_dir(&clients_dir, Path::new(&client_folder), true)?
        .ok_or_else(|| anyhow::anyhow!("vault client directory was not created"))?;
    resolve_descendant_dir(&client_dir, Path::new("calls"), true)?
        .ok_or_else(|| anyhow::anyhow!("vault calls directory was not created"))
}

/// A private, fully written and synced stage file next to the final note.
/// Dropping it removes the stage path if it is still there (it is renamed
/// away by `atomic_replace_file`, or left behind as the second link after
/// `hard_link`) via the reservation's own `Drop`.
struct StagedNote {
    path: PathBuf,
    _guard: crate::media_manifest::PrivateStage,
}

fn stage_note(calls_dir: &Path, content: &str) -> Result<StagedNote> {
    let path = calls_dir.join(format!(
        ".aftercalls-note-{}.part",
        uuid::Uuid::new_v4().simple()
    ));
    let guard = crate::media_manifest::reserve_private_stage(&path)?;
    fs::write(&path, content).with_context(|| format!("write staged note {}", path.display()))?;
    crate::media_manifest::enforce_private_file(&path)?;
    crate::media_manifest::sync_staged_file(&path)
        .with_context(|| format!("sync staged note {}", path.display()))?;
    Ok(StagedNote {
        path,
        _guard: guard,
    })
}

fn resolve_client_folder(matched: Option<&str>, candidates: &[String]) -> String {
    match matched {
        Some(m) if candidates.iter().any(|c| c == m) => m.to_string(),
        _ => "_Unsorted".to_string(),
    }
}

/// Longest full note path we produce, in characters. Windows' classic
/// MAX_PATH is 260 including the terminating NUL; staying under it keeps
/// notes openable by tools that aren't long-path aware.
const MAX_NOTE_PATH_CHARS: usize = 259;
/// Longest title portion of a file name, whatever the folder depth.
const MAX_TITLE_CHARS: usize = 80;

/// `{date} {title}{tail}` where tail is `.md` or ` (<short id>).md`. The
/// title is sanitized and shortened so the full path stays within
/// [`MAX_NOTE_PATH_CHARS`]; with no room left it is dropped entirely.
fn note_filename(calls_dir: &Path, date: &str, title: &str, short_id: Option<&str>) -> String {
    let tail = match short_id {
        Some(id) => format!(" ({id}).md"),
        None => ".md".to_string(),
    };
    let fixed = calls_dir.to_string_lossy().chars().count() + 1 + date.chars().count() + tail.chars().count();
    let budget = MAX_NOTE_PATH_CHARS.saturating_sub(fixed + 1).min(MAX_TITLE_CHARS);
    let safe_title = sanitize_for_filename(title, budget);
    if safe_title.is_empty() {
        format!("{date}{tail}")
    } else {
        format!("{date} {safe_title}{tail}")
    }
}

/// File-name-safe title: path and Windows-reserved punctuation become
/// `-`, control characters are dropped, whitespace runs collapse, and the
/// result is cut to `max_chars` and trimmed of trailing dots/spaces
/// (invalid at the end of a Windows file name). Empty input → `call`
/// when there is room for it.
fn sanitize_for_filename(s: &str, max_chars: usize) -> String {
    let mut cleaned = String::new();
    for c in s.chars() {
        let mapped = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_whitespace() => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if mapped == ' ' && (cleaned.is_empty() || cleaned.ends_with(' ')) {
            continue;
        }
        cleaned.push(mapped);
    }
    let trimmed = cleaned.trim();
    let base = if trimmed.is_empty() { "call" } else { trimmed };
    base.chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end_matches(['.', ' '])
        .to_string()
}

// ── Rendering ────────────────────────────────────────────────────────
//
// Note content is untrusted: transcripts, titles and summaries can carry
// text chosen by a caller or an outside meeting/chat participant, and
// Obsidian renders (and with plugins, executes) Markdown. Two rules:
//   * front matter values are YAML double-quoted scalars produced by
//     `yaml_quote`, so no value can end the block or add keys;
//   * body text goes through `md_text` / `md_block`, which keep harmless
//     formatting (emphasis, line-start headings, list markers, quotes)
//     and backslash-escape everything that can load or execute something:
//     see `md_text` for the exact rules and why the allowed characters
//     can't be recombined into anything dangerous.
// Applied to local and imported notes alike, so the summarizer's own
// **bold**, `## headings` and `- bullets` render as before.

/// YAML 1.2 double-quoted scalar for any string. Escapes `\`, `"`, and
/// every character outside YAML's printable set (C0/C1 controls, DEL,
/// NEL, line/paragraph separators, BOM, U+FFFE/U+FFFF), so the value
/// stays on one line and inside its quotes.
fn yaml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{0}' => out.push_str("\\0"),
            '\u{85}' => out.push_str("\\N"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c if yaml_printable(c) && c != '\u{feff}' => out.push(c),
            c if (c as u32) <= 0xFFFF => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push_str(&format!("\\U{:08X}", c as u32)),
        }
    }
    out.push('"');
    out
}

fn yaml_printable(c: char) -> bool {
    matches!(c as u32,
        0x20..=0x7E | 0xA0..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

/// Characters escaped wherever they appear in untrusted body text. Each is
/// required by some construct that loads or executes content:
///   `[` `]`  links, images `![…](…)`, wikilinks/embeds `[[…]]`/`![[…]]`,
///            footnotes `[^…]`, callouts `> [!…]`, Dataview `[k:: v]`;
///   `<`      raw HTML (`<img>`, `<iframe>`, `<script>`), autolinks `<url>`,
///            Templater `<% … %>`;
///   `&`      HTML entities that could smuggle the above;
///   `` ` ``  code spans and fences: ```` ```dataviewjs ````, inline
///            Dataview `` `= …` `` / `` `$= …` ``;
///   `~`      `~~~` fences (same code-block processors);
///   `$`      math (`$…$`, `$$…$$`);
///   `%`      Obsidian `%%` comments (hidden content);
///   `^`      block references / footnote markers;
///   `|`      tables (harmless, but keeps lines from turning into layout);
///   `=`      `==` highlights next to `` ` ``-free inline fields;
///   `\`      so input can't pre-escape (and thereby disarm) our escapes.
/// Deliberately NOT escaped: `*` and `_` (emphasis), `!`, `(` `)`, `{` `}`,
/// `>` and `-`/`+`/digits (lists, quotes). Without `[`, `<` or a backtick
/// none of them can open a link, embed, HTML block or code block, so they
/// can only ever produce emphasis, lists, quotes or plain text.
const MD_ACTIVE: &[char] = &[
    '\\', '`', '[', ']', '<', '&', '~', '$', '%', '^', '|', '=',
];

/// One line of untrusted text as inert Markdown, keeping harmless
/// formatting:
///   * control characters dropped (tabs become spaces);
///   * every [`MD_ACTIVE`] character backslash-escaped;
///   * `#` kept only as a heading marker (1–6 `#` then a space, after at most
///     three spaces of indent); any other `#` is escaped so it can't become
///     an Obsidian tag;
///   * the second `:` of any `::` escaped, so Dataview inline fields
///     (`Key:: value`, `(k:: v)`) never form.
/// A line-leading `>` stays a blockquote; it can't become a callout because
/// `[` is escaped.
fn md_text(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 8);
    let indent = line.len() - line.trim_start_matches(' ').len();
    let heading_hashes = if indent <= 3 {
        let rest = &line[indent..];
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        let after = rest[hashes..].chars().next();
        if (1..=6).contains(&hashes) && matches!(after, Some(' ') | None) {
            indent + hashes
        } else {
            0
        }
    } else {
        0
    };
    let mut prev: Option<char> = None;
    for (byte, c) in line.char_indices() {
        match c {
            '\t' => out.push(' '),
            c if c.is_control() => continue,
            '#' if byte < heading_hashes => out.push('#'),
            '#' => out.push_str("\\#"),
            ':' if prev == Some(':') => out.push_str("\\:"),
            c if MD_ACTIVE.contains(&c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
        prev = Some(c);
    }
    out
}

/// A label we follow with our own `:` (speaker, assignee). Trailing colons
/// and spaces are dropped (an escaped `\:` right before our `:` would still
/// read as `::` in the raw text Dataview scans), every remaining `:` and
/// `#` is escaped, plus the [`MD_ACTIVE`] set.
fn md_label(label: &str) -> String {
    let label = label.trim_end_matches(|c: char| c == ':' || c.is_whitespace());
    let mut out = String::with_capacity(label.len() + 4);
    for c in label.chars() {
        match c {
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c if c == ':' || c == '#' || MD_ACTIVE.contains(&c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Multi-line untrusted text (a summary): each line through [`md_text`],
/// so headings, list markers and quotes the summarizer writes keep their
/// meaning while everything dangerous stays escaped.
fn md_block(text: &str) -> String {
    text.trim()
        .lines()
        .map(md_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_markdown(summary: &Summary, transcript: &MergedTranscript, origin: &NoteOrigin) -> String {
    let mut md = String::new();
    md.push_str("---\n");
    match origin {
        NoteOrigin::Local { session_dir } => {
            md.push_str(&format!("date: {}\n", Utc::now().format("%Y-%m-%d")));
            md.push_str(&format!(
                "session: {}\n",
                yaml_quote(
                    &session_dir
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )
            ));
        }
        NoteOrigin::Imported {
            call_id,
            ingest_source,
            recorded_at,
        } => {
            md.push_str(&format!("date: {}\n", recorded_at.format("%Y-%m-%d")));
            md.push_str(&format!("aftercalls_call_id: {}\n", yaml_quote(call_id)));
            md.push_str(&format!("source: {}\n", yaml_quote(source_label(ingest_source))));
        }
    }
    if let Some(client) = &summary.matched_client {
        md.push_str(&format!("matched_client: {}\n", yaml_quote(client)));
    }
    if !summary.participants.is_empty() {
        md.push_str("participants:\n");
        for p in &summary.participants {
            md.push_str(&format!("  - {}\n", yaml_quote(p)));
        }
    }
    md.push_str("---\n\n");

    md.push_str(&format!("# {}\n\n", md_text(summary.title.lines().next().unwrap_or(""))));

    md.push_str("## Summary\n\n");
    md.push_str(&md_block(&summary.summary));
    md.push_str("\n\n");

    if !summary.action_items.is_empty() {
        md.push_str("## Action Items\n\n");
        for item in &summary.action_items {
            // Prefix the assignee when the LLM resolved one. Any `<name>`
            // markers in the description are escaped with the rest of the
            // text, so they show literally instead of being parsed as HTML.
            let desc = md_text(&item.description.trim().replace('\n', " "));
            match &item.assignee_name {
                Some(who) if !who.trim().is_empty() => {
                    md.push_str(&format!("- {}: {}\n", md_label(who.trim()), desc));
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
            "**\\[{}\\] {}:** {}\n\n",
            format_ts(u.start_ms),
            md_label(&u.speaker),
            md_text(&u.text.trim().replace(['\r', '\n'], " "))
        ));
    }

    md.push_str("---\n\n");
    match origin {
        NoteOrigin::Local { session_dir } => {
            md.push_str(&format!(
                "Audio session folder: `{}`\n",
                session_dir.display().to_string().replace('`', "'")
            ));
        }
        NoteOrigin::Imported { ingest_source, .. } => {
            // No local audio exists for an imported call; don't render a
            // path or embed that would be broken. The recording lives in
            // aftercalls.
            md.push_str(&format!(
                "Imported from {}. The recording is in aftercalls, not on this machine.\n",
                md_text(source_label(ingest_source))
            ));
        }
    }
    md
}

/// Human label for a server-side ingest source, as shown to users.
fn source_label(ingest_source: &str) -> &str {
    match ingest_source {
        "smartpbx" => "SmartPBX",
        "zoho_meeting" => "Zoho Meeting",
        "zoho_cliq" => "Zoho Cliq",
        "imported_file" => "an imported file",
        other => other,
    }
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

    fn test_vault(root: &Path) -> Vault {
        Vault {
            path: root.to_string_lossy().into_owned(),
            clients_subpath: "Clients".into(),
            imported_notes: true,
            imported_notes_since: None,
            imported_notes_account: None,
        }
    }

    fn test_summary(title: &str) -> Summary {
        Summary {
            title: title.into(),
            matched_client: None,
            summary: "Talked about the quote.".into(),
            action_items: Vec::new(),
            participants: vec!["Nick".into()],
        }
    }

    fn test_transcript() -> MergedTranscript {
        MergedTranscript {
            duration_ms: 4_000,
            timeline: vec![crate::transcription::Utterance {
                speaker: "Nick".into(),
                start_ms: 1_000,
                end_ms: 3_000,
                text: "Hello there".into(),
            }],
        }
    }

    fn recorded() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-08T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn imported(md_origin_call: &str) -> NoteOrigin<'_> {
        NoteOrigin::Imported {
            call_id: md_origin_call,
            ingest_source: "smartpbx",
            recorded_at: recorded(),
        }
    }

    fn write(vault: &Vault, title: &str, call_id: &str) -> ImportedNote {
        write_imported_note(
            vault,
            &test_summary(title),
            &test_transcript(),
            &[],
            call_id,
            "smartpbx",
            recorded(),
        )
        .unwrap()
    }

    fn created_path(note: &ImportedNote) -> PathBuf {
        match note {
            ImportedNote::Created(path) => path.clone(),
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[test]
    fn imported_note_has_no_local_audio_reference() {
        let md = render_markdown(
            &test_summary("Quote follow-up"),
            &test_transcript(),
            &imported("0b5f6a2c-1111-2222-3333-444455556666"),
        );
        assert!(md.contains("date: 2026-10-08\n"), "{md}");
        assert!(md.contains("aftercalls_call_id: \"0b5f6a2c-1111-2222-3333-444455556666\""));
        assert!(md.contains("source: \"SmartPBX\""));
        assert!(md.contains("Imported from SmartPBX."));
        assert!(!md.contains("Audio session folder"));
        assert!(!md.contains("session:"));
        assert!(!md.contains("![["), "no embeds for audio that isn't local");
        assert!(md.contains("**\\[00:01\\] Nick:** Hello there"), "{md}");
    }

    #[test]
    fn local_note_keeps_session_folder() {
        let md = render_markdown(
            &test_summary("Local"),
            &test_transcript(),
            &NoteOrigin::Local {
                session_dir: Path::new("/tmp/sessions/2026-10-08-143000"),
            },
        );
        assert!(md.contains("session: \"2026-10-08-143000\"\n"), "{md}");
        assert!(md.contains("Audio session folder: `/tmp/sessions/2026-10-08-143000`"));
    }

    // ── Fix 2: untrusted content can't break front matter or inject ──

    #[test]
    fn yaml_values_stay_quoted_on_one_line() {
        assert_eq!(yaml_quote("plain"), "\"plain\"");
        assert_eq!(yaml_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(
            yaml_quote("x\n---\nevil: true\r\u{7}\u{85}\u{2028}\u{feff}"),
            "\"x\\n---\\nevil: true\\r\\u0007\\N\\L\\uFEFF\""
        );
        assert_eq!(yaml_quote("café 😀"), "\"café 😀\"");
    }

    #[test]
    fn hostile_metadata_cannot_add_front_matter_keys() {
        let mut summary = test_summary("t");
        summary.matched_client = Some("Acme\"\ncssclasses: evil\n---".into());
        summary.participants = vec!["Bob\n- injected".into()];
        let md = render_markdown(&summary, &test_transcript(), &imported("c1"));
        let front: Vec<&str> = md.split("---\n").nth(1).unwrap().lines().collect();
        assert!(front.iter().all(|l| !l.starts_with("cssclasses")), "{md}");
        assert_eq!(front.iter().filter(|l| l.starts_with("  - ")).count(), 1, "{md}");
        assert!(md.contains("matched_client: \"Acme\\\"\\ncssclasses: evil\\n---\""), "{md}");
    }

    /// The summarizer's ordinary formatting must render exactly as before.
    #[test]
    fn typical_summary_formatting_is_unchanged() {
        let typical = "## Key points\n\
                       **Billing.** The customer owes *two* months.\n\
                       - Follow up on _Tuesday_ (before noon)!\n\
                       * Confirm the new {plan} > old plan\n\
                       1. Send the invoice\n\
                       > Customer asked us to call back.";
        let mut summary = test_summary("Weekly sync");
        summary.summary = typical.into();
        let md = render_markdown(&summary, &test_transcript(), &imported("c1"));
        assert!(md.contains(&format!("## Summary\n\n{typical}\n\n")), "{md}");
        assert!(md.contains("# Weekly sync\n"));
        assert_eq!(md_text("**bold** and _it_ and ### h"), "**bold** and _it_ and \\#\\#\\# h");
        assert_eq!(md_text("### Heading"), "### Heading");
        assert_eq!(md_text("   # Indented heading"), "   # Indented heading");
        assert_eq!(md_text("####### seven"), "\\#\\#\\#\\#\\#\\#\\# seven");
        assert_eq!(md_text("#tag"), "\\#tag");
    }

    /// Everything that can load or execute stays neutralized.
    #[test]
    fn hostile_body_text_renders_inert() {
        let hostile = "![x](https://evil.example/p.png) <img src=x onerror=1> &#60;b&#62; \
                       [[Secret]] ![[Secret]] `$= dv.page()` `= this.file` %%c%% $$x$$ $y$ \
                       #tag ==hl== ^ref [^1] <% tp.file %> Key:: value (k:: v) [k:: v] \
                       <https://evil.example> ~~~";
        let mut summary = test_summary(hostile);
        summary.summary = format!(
            "```dataviewjs\nalert(1)\n```\n~~~js\nx\n~~~\n> [!warning] callout\n\
             Key:: value\n[^1]: footnote\n{hostile}"
        );
        summary.action_items = vec![crate::summary::ActionItem {
            description: hostile.into(),
            assignee_name: Some("<b>Eve</b>:".into()),
        }];
        let transcript = MergedTranscript {
            duration_ms: 1,
            timeline: vec![crate::transcription::Utterance {
                speaker: "[[Mallory]] Key:".into(),
                start_ms: 0,
                end_ms: 1,
                text: format!("{hostile}\n```dataview\nTABLE x\n```"),
            }],
        };
        let md = render_markdown(&summary, &transcript, &imported("c1"));
        let body = md.splitn(3, "---\n").nth(2).unwrap();
        for raw in [
            "![", "[[", "[!", "[^", "<img", "<b>", "<%", "<http", "&#", "`", "$$", "$y", "%%",
            "==", "^ref", "~~~", "#tag",
        ] {
            let unescaped = body.match_indices(raw).any(|(i, _)| !(i > 0 && &body[i - 1..i] == "\\"));
            assert!(!unescaped, "unescaped `{raw}` in:\n{body}");
        }
        // No Dataview inline field anywhere: every `::` is broken up, including
        // a speaker/assignee label ending in `:` next to our own `:`.
        assert!(!body.contains("::"), "{body}");
        // A callout can't form: the quote stays, `[` is escaped.
        assert!(body.contains("> \\[!warning\\] callout"), "{body}");
    }

    /// Allowed characters can't be recombined into a link, image or embed.
    #[test]
    fn allowed_formatting_cannot_rebuild_links() {
        for input in [
            "*[*x*](*https://e.example*)*",
            "!_[_x_](y)_",
            "> ![a](b)",
            "- [[x]]",
            "## [link](u)",
        ] {
            let out = md_text(input);
            assert!(!out.contains("[") || out.matches('[').count() == out.matches("\\[").count(), "{input} -> {out}");
            assert!(!out.contains("]") || out.matches(']').count() == out.matches("\\]").count(), "{input} -> {out}");
        }
    }

    #[test]
    fn dataview_inline_fields_never_form() {
        assert_eq!(md_text("Key:: value"), "Key:\\: value");
        assert_eq!(md_text("a::::b"), "a:\\:\\:\\:b");
        assert_eq!(md_text("ratio 3:1, time 10:30"), "ratio 3:1, time 10:30");
        assert_eq!(md_label("Nick:"), "Nick");
        assert_eq!(md_label("Key:: "), "Key");
        assert_eq!(md_label("a:b"), "a\\:b");
        assert_eq!(md_label(r"a\:"), r"a\\");
    }

    // ── Fix 4: file names ────────────────────────────────────────────

    #[test]
    fn file_names_drop_control_chars_and_fit_max_path() {
        assert_eq!(sanitize_for_filename("a\u{0}b\u{1b}[31m c\n\td", 80), "ab[31m c d");
        assert_eq!(sanitize_for_filename("  ", 80), "call");
        assert_eq!(sanitize_for_filename("trailing dots...", 80), "trailing dots");
        let deep = PathBuf::from(format!("/{}", "d".repeat(200)));
        let name = note_filename(&deep, "2026-10-08", &"t".repeat(300), Some("0b5f6a2c"));
        assert!(name.ends_with(" (0b5f6a2c).md"), "{name}");
        assert!(deep.join(&name).to_string_lossy().chars().count() <= MAX_NOTE_PATH_CHARS);
        let too_deep = PathBuf::from(format!("/{}", "d".repeat(250)));
        assert_eq!(note_filename(&too_deep, "2026-10-08", "Title", Some("0b5f6a2c")), "2026-10-08 (0b5f6a2c).md");
    }

    // ── Fix 6: idempotency on the call-id suffix ─────────────────────

    #[test]
    fn imported_note_is_keyed_on_call_id_and_never_clobbers() {
        let scratch = Scratch::new();
        let vault = test_vault(&scratch.0);
        let call_id = "0b5f6a2c-1111-2222-3333-444455556666";

        let path = created_path(&write(&vault, "Quote: follow-up", call_id));
        assert!(path.ends_with("Clients/_Unsorted/calls/2026-10-08 Quote- follow-up (0b5f6a2c).md"));
        std::fs::write(&path, "edited by the user").unwrap();

        // Retry with the same title: same file, user's edits kept.
        assert_eq!(write(&vault, "Quote: follow-up", call_id), ImportedNote::AlreadyExists(path.clone()));
        // Re-summarized under a new title: still the same note, no duplicate.
        assert_eq!(write(&vault, "A completely new title", call_id), ImportedNote::AlreadyExists(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited by the user");

        // Same title and day, different call: its own file.
        let other = created_path(&write(&vault, "Quote: follow-up", "9c0d1e2f-aaaa-bbbb-cccc-ddddeeeeffff"));
        assert_ne!(other, path);

        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".part")), "stage files leaked: {names:?}");
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
