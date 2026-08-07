// macOS capture-permission pre-flight (#623).
//
// Capture is native — `cpal` for the mic, ScreenCaptureKit for the
// system loopback — NOT `getUserMedia`, so the OS gates capture behind
// TCC (Transparency, Consent, and Control) grants rather than a
// webview permission prompt. A *denied* mic permission would otherwise
// surface as a raw `cpal::BuildStreamError` string at the recording
// aha-moment; this module lets the frontend read the grant state
// up-front and short-circuit the already-denied case with an
// actionable message instead of a cryptic error.
//
// This is the seam only — the pre-flight gating on the Start path and
// the onboarding permissions slide that consume it are separate work.
//
// Platform split:
//   - macOS: query AVFoundation (mic) + CoreGraphics (screen recording)
//     through the existing `swift-bridge` FFI seam in
//     `macos_loopback.rs` — no new crate.
//   - Linux / Windows: PipeWire / WASAPI do not gate capture behind a
//     TCC-style per-app grant, so both permissions report
//     `not_applicable` and the request helpers are no-ops. The macOS
//     arm is the point of this module; these arms keep the IPC
//     contract uniform across builds.

use serde::Serialize;

/// Per-permission grant state. Serialised snake_case so the TS mirror
/// in `agent/src/lib/permissions.ts` can match on the wire strings
/// directly.
///
/// Off macOS only `NotApplicable` is ever constructed (PipeWire/WASAPI
/// don't gate capture behind a TCC grant), so the granted/denied/
/// undetermined variants are legitimately dead on those builds — they
/// stay in the enum because they're the cross-platform wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum PermStatus {
    /// Capture is allowed.
    Granted,
    /// The user explicitly denied (or the grant is restricted by MDM).
    Denied,
    /// No decision yet — the OS will prompt on first capture.
    Undetermined,
    /// Platform doesn't gate this capture behind a TCC-style grant
    /// (Linux / Windows), so there is nothing to check or request.
    NotApplicable,
}

/// Combined snapshot returned by `check_capture_permissions`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CapturePermissions {
    pub microphone: PermStatus,
    pub screen_recording: PermStatus,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{CapturePermissions, PermStatus};
    use crate::macos_loopback::ffi;

    // AVFoundation `AVAuthorizationStatus` raw values
    // (notDetermined=0, restricted=1, denied=2, authorized=3) — kept
    // here rather than in Swift so the mapping is auditable on the
    // Rust side. The Swift shim returns the raw `Int`.
    fn map_av_status(raw: i32) -> PermStatus {
        match raw {
            3 => PermStatus::Granted,
            0 => PermStatus::Undetermined,
            // 1 (restricted) + 2 (denied) both leave the user blocked
            // with the same remedy (System Settings), so collapse them.
            _ => PermStatus::Denied,
        }
    }

    // ScreenCaptureKit preflight is a bool: granted (true) or not
    // (false). There is no "undetermined" distinct from "denied" at
    // the CoreGraphics preflight layer, so a `false` maps to denied —
    // screen-recording denial is a soft, dismissible degrade
    // (mic-only), so the granted/denied split is sufficient.
    fn map_screen_status(granted: bool) -> PermStatus {
        if granted {
            PermStatus::Granted
        } else {
            PermStatus::Denied
        }
    }

    pub fn check() -> CapturePermissions {
        CapturePermissions {
            microphone: map_av_status(ffi::micAuthStatus()),
            screen_recording: map_screen_status(ffi::screenCaptureAuthStatus()),
        }
    }

    pub fn request_mic() -> PermStatus {
        // Fires the AVFoundation request prompt (blocks until the user
        // answers), then re-reads the authoritative status.
        ffi::requestMicAccess();
        map_av_status(ffi::micAuthStatus())
    }

    pub fn request_screen() -> bool {
        // `CGRequestScreenCaptureAccess()` prompts (or no-ops if a
        // decision exists) and returns the resulting grant.
        ffi::requestScreenCaptureAccess()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{CapturePermissions, PermStatus};

    pub fn check() -> CapturePermissions {
        CapturePermissions {
            microphone: PermStatus::NotApplicable,
            screen_recording: PermStatus::NotApplicable,
        }
    }

    pub fn request_mic() -> PermStatus {
        PermStatus::NotApplicable
    }

    pub fn request_screen() -> bool {
        // No TCC gate on Linux/Windows — treat as "already allowed" so
        // callers don't surface a spurious denial chip.
        true
    }
}

/// Read the current grant state for both capture permissions. Cheap
/// status read — never blocks on a prompt — so it stays safe to call
/// on the hot Start path.
pub fn check_capture_permissions() -> CapturePermissions {
    imp::check()
}

/// Trigger the OS mic-permission prompt (macOS) and return the
/// resulting status. No-op `not_applicable` off macOS.
pub fn request_mic_permission() -> PermStatus {
    imp::request_mic()
}

/// Trigger the OS screen-recording prompt (macOS) and return whether
/// access is granted. Always `true` off macOS.
pub fn request_screen_capture_access() -> bool {
    imp::request_screen()
}

#[cfg(test)]
mod tests {
    use super::*;

    // On the Linux dev box / CI, both permissions must report
    // `not_applicable` and the request helpers must no-op cleanly.
    // The macOS arm is exercised only by the `build-agent.yml` build
    // leg (Linux `cargo test` cannot reach the `#[cfg(macos)]` code).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_mac_permissions_are_not_applicable() {
        let p = check_capture_permissions();
        assert_eq!(p.microphone, PermStatus::NotApplicable);
        assert_eq!(p.screen_recording, PermStatus::NotApplicable);
        assert_eq!(request_mic_permission(), PermStatus::NotApplicable);
        assert!(request_screen_capture_access());
    }
}
