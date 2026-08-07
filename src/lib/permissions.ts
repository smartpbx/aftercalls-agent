// TS mirror of `agent/src-tauri/src/permissions.rs` (#623). Keeps the
// Tauri IPC command names + the `CapturePermissions` / `PermStatus`
// shape in one place so the record-page pre-flight and the onboarding
// permissions slide call the same wrappers.
//
// macOS gates native capture (cpal mic + ScreenCaptureKit loopback)
// behind TCC grants. Off macOS every status is `not_applicable` and
// the request commands no-op — the contract stays uniform so callers
// never need a per-OS branch beyond "is this status `denied`".

import { invoke } from "@tauri-apps/api/core";

/** Per-permission grant state. Wire strings match the Rust
 *  `#[serde(rename_all = "snake_case")]` enum. */
export type PermStatus =
  | "granted"
  | "denied"
  | "undetermined"
  | "not_applicable";

/** Combined snapshot from `check_capture_permissions`. */
export type CapturePermissions = {
  microphone: PermStatus;
  screen_recording: PermStatus;
};

/** Which macOS Privacy & Security pane `openPrivacySettings` targets. */
export type PrivacyPane = "microphone" | "screen";

/** Read the live grant state for mic + screen-recording capture.
 *  Cheap status read (never prompts) so it's safe on the hot Start
 *  path. */
export function checkCapturePermissions(): Promise<CapturePermissions> {
  return invoke<CapturePermissions>("check_capture_permissions");
}

/** Fire the OS mic-permission prompt (macOS) and resolve with the
 *  resulting status. `not_applicable` off macOS. */
export function requestMicPermission(): Promise<PermStatus> {
  return invoke<PermStatus>("request_mic_permission");
}

/** Prompt for screen-recording access (macOS) and resolve true when
 *  granted. Always true off macOS. */
export function requestScreenCaptureAccess(): Promise<boolean> {
  return invoke<boolean>("request_screen_capture_access");
}

/** Open the relevant macOS Privacy & Security pane so the user can
 *  flip a denied grant. Non-fatal: opener failures reject, callers
 *  should swallow.
 *
 *  NOTE: the Rust command is registered but its body is still a stub
 *  (TODO #623 S-4) — this currently resolves without opening anything.
 *  Do not wire an "Open System Settings" affordance to it until S-4
 *  lands the deep-link body. */
export function openPrivacySettings(pane: PrivacyPane): Promise<void> {
  return invoke("open_privacy_settings", { pane });
}
