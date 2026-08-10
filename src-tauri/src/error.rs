//! Structured IPC error shape for Tauri commands that hit the backend.
//!
//! Replaces the previous "anyhow::bail! → e.to_string() → frontend regex
//! sniff" pipeline with a discriminated union that the TS side can match
//! on `kind` directly. See #124 for the motivation: regex on a stringified
//! error message silently regresses every time the backend re-words an
//! error.
//!
//! Tauri serialises any `Result<T, E>` returned from a `#[tauri::command]`
//! through `serde_json` when `E: Serialize`, so the enum below lands in
//! the JS catch block as a plain object with the `kind` discriminator
//! and the variant's fields. The frontend types it as `PortalError` and
//! switches on `err.kind`.

use reqwest::StatusCode;
use serde::Serialize;

/// Discriminated error union surfaced to Tauri command callers. The
/// `tag = "kind"` + `rename_all = "snake_case"` shape gives the TS side
/// a stable string to switch on (e.g. `err.kind === "cooldown"`).
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortalError {
    /// 429 — backend asked us to back off. `retry_after_seconds` comes
    /// from the JSON body when present, falling back to the
    /// `Retry-After` header. The resummarize flow uses this to drive a
    /// countdown ticker.
    Cooldown { retry_after_seconds: u32 },
    /// 400 — backend rejected the request. `message` carries the body
    /// verbatim so the frontend can surface backend-authored validation
    /// strings (e.g. cross-org assignee on action items).
    BadRequest { message: String },
    /// 401 — auth missing / expired. Frontend usually responds by
    /// kicking the user back through the login flow.
    Unauthorized,
    /// 403 — authenticated but not allowed (e.g. non-admin reading a
    /// scope=all list).
    Forbidden,
    /// 404 — resource doesn't exist (or never existed). Some callers
    /// treat this as silent success (delete_action_item).
    NotFound,
    /// reqwest transport failure: DNS, TCP refused, TLS, request timeout.
    /// `message` is the formatted reqwest::Error for logs/debug; UI
    /// copy is keyed on `kind` not `message`.
    Network { message: String },
    /// 5xx — backend is broken or degraded. `status` is the raw status
    /// code, `message` is the response body (may be empty).
    Server { status: u16, message: String },
    /// Any other failure — unexpected status, decode error, missing
    /// auth header, etc. Frontend treats this like Server: "try again
    /// in a moment".
    Other { message: String },
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortalError::Cooldown { retry_after_seconds } => {
                write!(f, "cooldown ({retry_after_seconds}s)")
            }
            PortalError::BadRequest { message } => write!(f, "bad request: {message}"),
            PortalError::Unauthorized => write!(f, "unauthorized"),
            PortalError::Forbidden => write!(f, "forbidden"),
            PortalError::NotFound => write!(f, "not found"),
            PortalError::Network { message } => write!(f, "network: {message}"),
            PortalError::Server { status, message } => {
                write!(f, "server {status}: {message}")
            }
            PortalError::Other { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PortalError {}

/// Render an error together with its `source()` chain, joined by `": "`.
///
/// `reqwest::Error`'s `Display` only writes the outermost layer — for a
/// failed connect that is literally just "error sending request for url
/// (https://…)", which tells support nothing. The part that identifies
/// the fault ("invalid peer certificate: UnknownIssuer" for a TLS
/// inspecting proxy, "dns error", "connection refused", "operation
/// timed out") sits one to three levels down in the source chain and is
/// dropped by `to_string()`.
///
/// Depth is capped and duplicate links are skipped: chains repeat the
/// parent's text often enough that a naive join reads as a stutter.
fn format_with_sources(err: &dyn std::error::Error) -> String {
    const MAX_DEPTH: usize = 8;

    let mut out = err.to_string();
    let mut source = err.source();
    let mut depth = 0;

    while let (Some(e), true) = (source, depth < MAX_DEPTH) {
        let text = e.to_string();
        // Skip links whose text the accumulated message already carries
        // (hyper/rustls wrappers frequently restate their child verbatim).
        if !text.is_empty() && !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = e.source();
        depth += 1;
    }

    out
}

impl From<reqwest::Error> for PortalError {
    /// Map reqwest transport-level failures (no HTTP response received,
    /// or response decode failed) to `Network` for connect/timeout/DNS,
    /// `Other` for everything else (decode, redirect, builder).
    fn from(err: reqwest::Error) -> Self {
        // `is_timeout`, `is_connect`, `is_request` cover the offline /
        // DNS / TCP-refused / connection-reset / read-timeout cases the
        // user sees as "no internet". Anything else (body decode, TLS
        // misconfig, redirect loop) lands in `Other` so we don't lie
        // about what went wrong in the UI copy.
        if err.is_timeout() || err.is_connect() {
            PortalError::Network {
                message: format_with_sources(&err),
            }
        } else if err.is_request() {
            // `is_request` is broader than connect: covers e.g. URL
            // build failures. Treat as network when there's no status
            // attached (transport-level), Other when there is.
            if err.status().is_none() {
                PortalError::Network {
                    message: format_with_sources(&err),
                }
            } else {
                PortalError::Other {
                    message: format_with_sources(&err),
                }
            }
        } else {
            PortalError::Other {
                message: format_with_sources(&err),
            }
        }
    }
}

impl From<anyhow::Error> for PortalError {
    /// Last-resort conversion for portal helpers that still bubble
    /// `anyhow::Error` (auth-header build, config load). These are
    /// not from a backend HTTP response, so they land in `Other` —
    /// the frontend renders them as a generic try-again message.
    ///
    /// `{:#}` rather than `to_string()`: the alternate form renders the
    /// full `.context()` chain, which is where the useful part of these
    /// errors lives (e.g. "parse config toml: …" over a bare "…").
    fn from(err: anyhow::Error) -> Self {
        PortalError::Other {
            message: format!("{err:#}"),
        }
    }
}

impl From<serde_json::Error> for PortalError {
    fn from(err: serde_json::Error) -> Self {
        PortalError::Other {
            message: format!("decode: {err}"),
        }
    }
}

/// Build a `PortalError` from an HTTP response status + body. The body
/// is consumed once for both the message and (on 429) the retry window
/// parse. Used by every helper that classifies a non-2xx response.
pub fn from_status(status: StatusCode, body: String, retry_after_header: Option<u64>) -> PortalError {
    match status {
        StatusCode::TOO_MANY_REQUESTS => {
            // Body shape: `{"error": "cooldown", "retry_after_seconds": N}`
            // when the backend builds its own response; otherwise we
            // fall back to the `Retry-After` header. Default to 0 so
            // the frontend's "retry > 0 ? retry : 30" guard kicks in
            // and shows a sane window even if the server omitted both.
            let retry_body = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("retry_after_seconds").and_then(|n| n.as_u64()));
            let retry = retry_body
                .or(retry_after_header)
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            PortalError::Cooldown { retry_after_seconds: retry }
        }
        StatusCode::BAD_REQUEST => PortalError::BadRequest { message: body },
        StatusCode::UNAUTHORIZED => PortalError::Unauthorized,
        StatusCode::FORBIDDEN => PortalError::Forbidden,
        StatusCode::NOT_FOUND => PortalError::NotFound,
        s if s.is_server_error() => PortalError::Server {
            status: s.as_u16(),
            message: body,
        },
        s => PortalError::Other {
            message: format!("backend {s}: {body}"),
        },
    }
}

/// Convenience: pull the `Retry-After` header off a response as a `u64`,
/// returning `None` when missing or unparseable.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal N-link error chain — stands in for the
    /// reqwest → hyper → rustls nesting we can't construct directly
    /// (reqwest::Error has no public constructor).
    #[derive(Debug)]
    struct Chained {
        text: String,
        source: Option<Box<Chained>>,
    }

    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.text)
        }
    }

    impl std::error::Error for Chained {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref().map(|e| e as &(dyn std::error::Error))
        }
    }

    fn chain(links: &[&str]) -> Chained {
        let mut iter = links.iter().rev();
        let mut node = Chained { text: iter.next().unwrap().to_string(), source: None };
        for text in iter {
            node = Chained { text: text.to_string(), source: Some(Box::new(node)) };
        }
        node
    }

    #[test]
    fn format_with_sources_appends_the_real_cause() {
        // The case this exists for: the outermost layer names only the
        // URL, and the diagnosis is two levels down.
        let e = chain(&[
            "error sending request for url (https://api.aftercalls.io/v1/auth/login)",
            "client error (Connect)",
            "invalid peer certificate: UnknownIssuer",
        ]);
        let s = format_with_sources(&e);
        assert!(s.starts_with("error sending request for url"));
        assert!(
            s.contains("invalid peer certificate: UnknownIssuer"),
            "the TLS-interception signature must survive: {s}"
        );
    }

    #[test]
    fn format_with_sources_skips_restated_links() {
        // hyper/rustls wrappers often restate the child verbatim; the
        // joined string shouldn't stutter.
        let e = chain(&["dns error: no record", "dns error: no record"]);
        assert_eq!(format_with_sources(&e), "dns error: no record");
    }

    #[test]
    fn format_with_sources_handles_a_bare_error() {
        let e = chain(&["operation timed out"]);
        assert_eq!(format_with_sources(&e), "operation timed out");
    }

    #[test]
    fn format_with_sources_caps_depth() {
        // Guard against an absurd chain producing an unbounded banner.
        let links: Vec<String> = (0..40).map(|i| format!("layer {i}")).collect();
        let refs: Vec<&str> = links.iter().map(|s| s.as_str()).collect();
        let s = format_with_sources(&chain(&refs));
        assert!(s.contains("layer 0"));
        assert!(!s.contains("layer 30"), "depth cap should truncate: {s}");
    }

    #[test]
    fn cooldown_serializes_with_kind_tag() {
        let e = PortalError::Cooldown { retry_after_seconds: 30 };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"kind":"cooldown","retry_after_seconds":30}"#);
    }

    #[test]
    fn bad_request_carries_message() {
        let e = PortalError::BadRequest {
            message: "teammate isn't in your workspace".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""kind":"bad_request""#));
        assert!(s.contains("teammate"));
    }

    #[test]
    fn unauthorized_has_no_payload() {
        let s = serde_json::to_string(&PortalError::Unauthorized).unwrap();
        assert_eq!(s, r#"{"kind":"unauthorized"}"#);
    }

    #[test]
    fn server_uses_snake_case_kind() {
        let e = PortalError::Server { status: 503, message: "down".into() };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""kind":"server""#));
    }

    #[test]
    fn from_status_429_parses_body_first() {
        let body = r#"{"error":"cooldown","retry_after_seconds":12}"#.to_string();
        let e = from_status(StatusCode::TOO_MANY_REQUESTS, body, Some(99));
        match e {
            PortalError::Cooldown { retry_after_seconds } => {
                assert_eq!(retry_after_seconds, 12, "body wins over header");
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    #[test]
    fn from_status_429_falls_back_to_header() {
        let e = from_status(StatusCode::TOO_MANY_REQUESTS, String::new(), Some(45));
        match e {
            PortalError::Cooldown { retry_after_seconds } => {
                assert_eq!(retry_after_seconds, 45);
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    #[test]
    fn from_status_500_lands_in_server() {
        let e = from_status(StatusCode::INTERNAL_SERVER_ERROR, "boom".into(), None);
        match e {
            PortalError::Server { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn from_status_404_is_not_found_no_payload() {
        let e = from_status(StatusCode::NOT_FOUND, "ignored".into(), None);
        match e {
            PortalError::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
