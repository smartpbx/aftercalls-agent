// Shared helpers for the PIPEDA compliance flow (#44 + #48 + #56).
// Used by both the Record page (manual start) and the layout-level
// auto-detect slide-out (#59), so the recording-ack check, the
// prefs cache, and the start-cue behaviour stay consistent across
// both entry points.

import { invoke } from "@tauri-apps/api/core";
import { notifyConsentAnnouncement, notifyRecordStart } from "./notify";
import { getAppPrefs } from "./stores/appPrefs.svelte";

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

// #56 — per-machine pref controlling whether the spoken consent
// announcement plays alongside the chime. Default OFF for `user` mode
// (chime alone is enough for most users); always-on for `enforced`
// mode regardless of this pref so an admin's compliance posture isn't
// silenceable from local Settings. #501 — reads route through the
// shared appPrefs cache so a series of recordings within one session
// only pays the round-trip once; Settings invalidates on save so a
// toggled pref takes effect immediately.
async function consentAnnouncementEnabled(): Promise<boolean> {
  const p = await getAppPrefs();
  // Fail-closed on cache miss: better to skip the speech than to
  // surprise users with audio they didn't opt into.
  return !!p?.consent_announcement_enabled;
}

export type StartCueOpts = {
  // #56 — true for note-to-self captures. The chime still plays as
  // usual but the spoken announcement is suppressed: a self-note has
  // only one participant (the user dictating) so consent disclosure
  // is meaningless. Mirrors the existing PIPEDA-ack bypass for
  // self-notes in +page.svelte.
  selfNote?: boolean;
};

// Play the start-of-recording cue according to the org's policy.
// 'off'      → silent
// 'user'     → plays iff the user's local sounds_enabled pref is on
// 'enforced' → plays unconditionally (force-overrides sounds_enabled)
//
// #56 — when the policy says "play the chime" AND this is a real
// call (not a self-note), the spoken announcement is layered after
// the chime per the listener's "tone → meaning" framing:
//   • mode 'enforced' → announcement always plays (force).
//   • mode 'user'     → announcement plays iff the per-machine
//                       consent_announcement_enabled toggle is on.
//   • mode 'off'      → no chime, no announcement.
// Self-notes skip the announcement entirely (selfNote: true).
export async function playStartCueIfEnabled(
  opts: StartCueOpts = {},
): Promise<void> {
  const prefs = await loadRecordingPrefs();
  const mode = prefs?.recording_notification_mode ?? "user";
  if (mode === "off") return;
  const enforced = mode === "enforced";
  // Chime first — short (~350ms), reliable Web Audio oscillator.
  await notifyRecordStart(enforced);
  if (opts.selfNote) return;
  // Then the spoken announcement, gated on the relevant toggle.
  if (!enforced && !(await consentAnnouncementEnabled())) return;
  await notifyConsentAnnouncement(enforced);
}
