//! Security boundary for values that cross from the webview into native IPC.
//!
//! A path-looking string is not authority to read a local file.  Native file
//! dialogs approve canonical paths for a narrow purpose; later commands must
//! present one of those exact paths.  The same rule applies to downloadable
//! audio URLs: only URLs returned by the authenticated backend are accepted.

use reqwest::Url;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const MAX_APPROVED_PATHS: usize = 32;
const MAX_APPROVED_URLS: usize = 32;
const MAX_PATH_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPurpose {
    ImportAudio,
    SupportAttachment,
    VaultRoot,
}

#[derive(Default)]
pub struct IpcSecurity {
    paths: Mutex<VecDeque<(PathPurpose, PathBuf)>>,
    audio_urls: Mutex<VecDeque<String>>,
}

impl IpcSecurity {
    pub fn approve_path(&self, purpose: PathPurpose, path: PathBuf) {
        let mut approved = self.paths.lock().unwrap();
        if let Some(position) = approved
            .iter()
            .position(|(existing_purpose, existing_path)| {
                *existing_purpose == purpose && *existing_path == path
            })
        {
            approved.remove(position);
        }
        approved.push_back((purpose, path));
        while approved.len() > MAX_APPROVED_PATHS {
            approved.pop_front();
        }
    }

    pub fn require_approved_file(
        &self,
        purpose: PathPurpose,
        supplied: &str,
    ) -> Result<PathBuf, String> {
        let canonical = canonical_existing_file(supplied)?;
        let approved = self.paths.lock().unwrap();
        if approved
            .iter()
            .any(|(p, path)| *p == purpose && path == &canonical)
        {
            Ok(canonical)
        } else {
            Err("file was not selected in the native dialog".into())
        }
    }

    pub fn consume_approved_file(
        &self,
        purpose: PathPurpose,
        supplied: &str,
    ) -> Result<PathBuf, String> {
        let canonical = canonical_existing_file(supplied)?;
        let mut approved = self.paths.lock().unwrap();
        let position = approved
            .iter()
            .position(|(p, path)| *p == purpose && path == &canonical)
            .ok_or_else(|| "file was not selected in the native dialog".to_string())?;
        approved.remove(position);
        Ok(canonical)
    }

    pub fn require_approved_dir(
        &self,
        purpose: PathPurpose,
        supplied: &str,
    ) -> Result<PathBuf, String> {
        let canonical = canonical_existing_dir(supplied)?;
        let approved = self.paths.lock().unwrap();
        if approved
            .iter()
            .any(|(p, path)| *p == purpose && path == &canonical)
        {
            Ok(canonical)
        } else {
            Err("folder was not selected in the native dialog".into())
        }
    }

    /// Replace neither existing approvals nor caller-visible values.  Signed
    /// URLs can rotate while a detail page is open, so keep a small FIFO of
    /// exact canonical URLs returned by recent authenticated responses.
    pub fn approve_audio_urls(&self, body: &Value) {
        let mut approved = self.audio_urls.lock().unwrap();
        for key in ["mic", "system", "mixed"] {
            let Some(raw) = body.get(key).and_then(Value::as_str) else {
                continue;
            };
            let Ok(url) = validate_download_url(raw) else {
                continue;
            };
            let canonical = url.to_string();
            if let Some(position) = approved.iter().position(|item| item == &canonical) {
                approved.remove(position);
            }
            approved.push_back(canonical);
        }
        while approved.len() > MAX_APPROVED_URLS {
            approved.pop_front();
        }
    }

    pub fn require_approved_audio_url(&self, supplied: &str) -> Result<Url, String> {
        let url = validate_download_url(supplied)?;
        let canonical = url.to_string();
        if self
            .audio_urls
            .lock()
            .unwrap()
            .iter()
            .any(|item| item == &canonical)
        {
            Ok(url)
        } else {
            Err("audio URL was not issued by the authenticated backend".into())
        }
    }
}

fn validate_path_text(input: &str) -> Result<&Path, String> {
    if input.is_empty() || input.len() > MAX_PATH_BYTES {
        return Err("invalid local path length".into());
    }
    if input.chars().any(char::is_control) {
        return Err("local path contains control characters".into());
    }
    let path = Path::new(input);
    if !path.is_absolute() {
        return Err("local path must be absolute".into());
    }
    Ok(path)
}

pub fn canonical_existing_file(input: &str) -> Result<PathBuf, String> {
    let path = validate_path_text(input)?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("resolve selected file: {e}"))?;
    let metadata = canonical
        .metadata()
        .map_err(|e| format!("inspect selected file: {e}"))?;
    if !metadata.is_file() {
        return Err("selected path is not a regular file".into());
    }
    Ok(canonical)
}

pub fn canonical_existing_dir(input: &str) -> Result<PathBuf, String> {
    let path = validate_path_text(input)?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("resolve selected folder: {e}"))?;
    let metadata = canonical
        .metadata()
        .map_err(|e| format!("inspect selected folder: {e}"))?;
    if !metadata.is_dir() {
        return Err("selected path is not a folder".into());
    }
    Ok(canonical)
}

/// Normalize the optional vault child path while rejecting all path escape
/// forms.  Backslashes are accepted as separators so Windows settings remain
/// portable, but absolute paths, prefixes, dot segments and empty interior
/// components are not.
pub fn normalize_relative_subpath(input: &str) -> Result<String, String> {
    let normalized = input.trim().replace('\\', "/");
    if normalized.is_empty() {
        return Ok(String::new());
    }
    if normalized.len() > 512 || normalized.chars().any(char::is_control) {
        return Err("vault subfolder is too long or contains control characters".into());
    }
    if normalized.starts_with('/') || normalized.ends_with('/') {
        return Err("vault subfolder must be a relative folder path".into());
    }
    if normalized
        .split('/')
        .next()
        .is_some_and(|segment| segment.ends_with(':'))
    {
        return Err("vault subfolder must not contain a drive prefix".into());
    }
    if normalized
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("vault subfolder contains an unsafe path segment".into());
    }
    let path = Path::new(&normalized);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| "vault subfolder must be valid UTF-8".to_string())?;
                if value.is_empty() || value == "." || value == ".." {
                    return Err("vault subfolder contains an unsafe path segment".into());
                }
                parts.push(value);
            }
            _ => return Err("vault subfolder must stay inside the selected vault".into()),
        }
    }
    if parts.is_empty() {
        return Err("vault subfolder contains an empty path segment".into());
    }
    Ok(parts.join("/"))
}

pub fn safe_suggested_filename(input: &str, fallback: &str) -> String {
    let mut result = String::with_capacity(input.len().min(120));
    for ch in input.chars().take(120) {
        if ch.is_control() || matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            result.push('_');
        } else {
            result.push(ch);
        }
    }
    let result = result.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    if result.is_empty() {
        fallback.to_string()
    } else {
        result.to_string()
    }
}

fn validate_download_url(input: &str) -> Result<Url, String> {
    if input.len() > 8_192 || input.chars().any(char::is_control) {
        return Err("invalid audio URL".into());
    }
    let url = Url::parse(input).map_err(|_| "invalid audio URL".to_string())?;
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return Err("audio URL must not contain credentials".into());
    }
    match url.scheme() {
        "https" => {}
        "http" => {
            // `Url::host_str` preserves brackets around IPv6 literals, while
            // `IpAddr::from_str` expects the bare address.
            let raw_host = url.host_str().unwrap_or_default();
            let host = raw_host
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(raw_host);
            if host != "localhost"
                && host
                    .parse::<std::net::IpAddr>()
                    .map(|ip| !ip.is_loopback())
                    .unwrap_or(true)
            {
                return Err("plain HTTP audio URLs are limited to local development".into());
            }
        }
        _ => return Err("audio URL must use HTTPS".into()),
    }
    if url.fragment().is_some() {
        return Err("audio URL must not contain a fragment".into());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_subpaths_are_normalized_and_fenced() {
        assert_eq!(normalize_relative_subpath("").unwrap(), "");
        assert_eq!(
            normalize_relative_subpath("Clients\\Active").unwrap(),
            "Clients/Active"
        );
        for unsafe_value in [
            "../outside",
            "Clients/../../outside",
            "/absolute",
            "Clients//Active",
            "Clients/./Active",
            "C:\\outside",
        ] {
            assert!(
                normalize_relative_subpath(unsafe_value).is_err(),
                "accepted {unsafe_value:?}"
            );
        }
    }

    #[test]
    fn download_urls_require_secure_or_loopback_http_origins() {
        assert!(validate_download_url("https://audio.example/object?sig=abc").is_ok());
        assert!(validate_download_url("http://127.0.0.1:9000/object").is_ok());
        assert!(validate_download_url("http://[::1]:9000/object").is_ok());
        for unsafe_value in [
            "file:///etc/passwd",
            "http://169.254.169.254/latest/meta-data",
            "https://user:pass@example.com/object",
            "javascript:alert(1)",
        ] {
            assert!(validate_download_url(unsafe_value).is_err());
        }
    }

    #[test]
    fn audio_urls_must_have_been_returned_by_backend() {
        let security = IpcSecurity::default();
        security.approve_audio_urls(&serde_json::json!({
            "mixed": "https://audio.example/object?sig=one"
        }));
        assert!(security
            .require_approved_audio_url("https://audio.example/object?sig=one")
            .is_ok());
        assert!(security
            .require_approved_audio_url("https://audio.example/object?sig=two")
            .is_err());
    }

    #[test]
    fn suggested_names_cannot_escape_the_dialog_directory() {
        assert_eq!(
            safe_suggested_filename("../../secret.txt", "export.txt"),
            "_.._secret.txt"
        );
        assert_eq!(safe_suggested_filename("...", "export.txt"), "export.txt");
    }
}
