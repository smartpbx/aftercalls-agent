//! Private, collision-proof local recording storage.
//!
//! Session directory names are externally visible as the idempotency key sent
//! to the backend, so allocation must never reuse an existing directory. The
//! timestamp prefix keeps the existing sort/parse behaviour while the random
//! suffix makes two starts in the same second distinct.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const TIMESTAMP_FORMAT: &str = "%Y%m%dT%H%M%SZ";
const TIMESTAMP_LEN: usize = 16;
const ALLOCATION_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Recording,
    Import,
}

pub fn allocate(base_dir: &Path, kind: SessionKind) -> Result<PathBuf> {
    ensure_private_dir(base_dir)?;
    let stamp = Utc::now().format(TIMESTAMP_FORMAT).to_string();
    for _ in 0..ALLOCATION_ATTEMPTS {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let name = match kind {
            SessionKind::Recording => format!("{stamp}_{nonce}"),
            SessionKind::Import => format!("imp_{stamp}_{nonce}"),
        };
        let path = base_dir.join(name);
        match create_private_leaf(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create session {}", path.display()))
            }
        }
    }
    anyhow::bail!("could not allocate a unique recording session directory")
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create private directory {}", path.display()))?;
    enforce_private_dir(path)
}

fn create_private_leaf(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn enforce_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("set private permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub fn create_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create private file {}", path.display()))
}

pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open private file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set private permissions on {}", path.display()))?;
    }
    file.write_all(bytes)
        .with_context(|| format!("write private file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync private file {}", path.display()))
}

pub fn parse_timestamp(session_id: &str) -> Option<DateTime<Utc>> {
    let value = session_id.strip_prefix("imp_").unwrap_or(session_id);
    let prefix = value.get(..TIMESTAMP_LEN)?;
    NaiveDateTime::parse_from_str(prefix, TIMESTAMP_FORMAT)
        .ok()
        .map(|value| value.and_utc())
}

/// Session identifiers cross the IPC boundary and must remain exactly one
/// normal path component.  Keep this validator shared by notes, playback and
/// recovery so a future command cannot accidentally reintroduce traversal.
pub fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && !session_id.chars().any(char::is_control)
        && !session_id.contains('/')
        && !session_id.contains('\\')
        && Path::new(session_id)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && Path::new(session_id).file_name().and_then(|value| value.to_str()) == Some(session_id)
}

/// Resolve an existing, real directory directly beneath `base_dir`.
/// Canonicalizing both sides rejects traversal and symlink escapes while the
/// `symlink_metadata` check rejects a symlink leaf even when it points back
/// inside the recording root.
pub fn resolve_existing_dir(base_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    let canonical_base = base_dir.canonicalize().ok()?;
    let candidate = base_dir.join(session_id);
    let metadata = candidate.symlink_metadata().ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if canonical.parent() != Some(canonical_base.as_path()) {
        return None;
    }
    Some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "aftercalls-session-fs-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn allocations_in_the_same_second_never_reuse_a_directory() {
        let scratch = Scratch::new();
        let first = allocate(&scratch.0, SessionKind::Recording).unwrap();
        let second = allocate(&scratch.0, SessionKind::Recording).unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
    }

    #[test]
    fn timestamp_parser_accepts_legacy_suffixed_and_import_ids() {
        let legacy = parse_timestamp("20260731T101112Z").unwrap();
        assert_eq!(
            parse_timestamp("20260731T101112Z_0123456789abcdef"),
            Some(legacy)
        );
        assert_eq!(
            parse_timestamp("imp_20260731T101112Z_0123456789abcdef"),
            Some(legacy)
        );
        assert!(parse_timestamp("not-a-session").is_none());
    }

    #[test]
    fn session_id_is_exactly_one_safe_path_component() {
        assert!(valid_session_id("20260731T101112Z_0123456789abcdef"));
        assert!(valid_session_id("imp_20260731T101112Z_0123456789abcdef"));
        for unsafe_id in ["", ".", "..", "../other", "sub/other", "sub\\other", "/tmp/x"] {
            assert!(!valid_session_id(unsafe_id), "accepted {unsafe_id:?}");
        }
    }

    #[test]
    fn existing_session_resolver_rejects_non_direct_children() {
        let scratch = Scratch::new();
        let session = allocate(&scratch.0, SessionKind::Recording).unwrap();
        let id = session.file_name().unwrap().to_str().unwrap();
        assert_eq!(resolve_existing_dir(&scratch.0, id), session.canonicalize().ok());
        assert!(resolve_existing_dir(&scratch.0, "../outside").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn session_directories_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let session = allocate(&scratch.0, SessionKind::Recording).unwrap();
        let media = session.join("mic.wav");
        create_private_file(&media).unwrap();
        assert_eq!(
            std::fs::metadata(&scratch.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&session).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&media).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
