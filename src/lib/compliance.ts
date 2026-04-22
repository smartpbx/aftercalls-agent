// Shared helpers for the PIPEDA compliance flow (#44 + #48).
// Used by both the Record page (manual start) and the layout-level
// auto-detect slide-out (#59), so the recording-ack check, the
// prefs cache, and the start-cue behaviour stay consistent across
// both entry points.

import { invoke } from "@tauri-apps/api/core";
import { notifyRecordStart } from "./notify";

export type RecordingNotificationMode = "off" | "user" | "enforced";
export type RecordingPrefs = {
  recording_purpose: string;
  recording_notification_mode: RecordingNotificationMode;
};

// Report the platform to the backend verbatim for the recording-ack
// audit row (#44). Tauri doesn't expose the OS directly to the
// webview so we sniff the user-agent; navigator.platform is
// deprecated. Three allowed values on the backend side.
export function detectPlatform(): "windows" | "linux" | "macos" {
  if (typeof navigator === "undefined") return "macos";
  const ua = navigator.userAgent;
  if (/windows/i.test(ua)) return "windows";
  if (/linux/i.test(ua) && !/android/i.test(ua)) return "linux";
  return "macos";
}

// Module-level cache so the Copy-notice button, the Record-page
// start flow, and the auto-detect slide-out share a single in-memory
// copy of the org's recording prefs — no per-click roundtrip.
let cached: RecordingPrefs | null = null;

export async function loadRecordingPrefs(): Promise<RecordingPrefs | null> {
  if (cached) return cached;
  try {
    const p = await invoke<RecordingPrefs>("get_recording_prefs");
    cached = p;
    return p;
  } catch (e) {
    console.warn("get_recording_prefs failed", e);
    return null;
  }
}

// Play the start-of-recording cue according to the org's policy.
// 'off'      → silent
// 'user'     → plays iff the user's local sounds_enabled pref is on
// 'enforced' → plays unconditionally (force-overrides sounds_enabled)
export async function playStartCueIfEnabled(): Promise<void> {
  const prefs = await loadRecordingPrefs();
  const mode = prefs?.recording_notification_mode ?? "user";
  if (mode === "off") return;
  await notifyRecordStart(mode === "enforced");
}
