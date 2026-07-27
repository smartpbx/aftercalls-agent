// #501 — shared in-memory cache for the agent's app prefs.
//
// Multiple call-sites read app prefs via `invoke("get_app_prefs")`:
//   - compliance.ts → consentAnnouncementEnabled (per recording start)
//   - notify.ts     → soundsEnabled (per chime)
//   - +page.svelte  → manualNotesEnabled / shortcuts on mount
//   - settings/+page.svelte → bound state on mount
//
// The Tauri command is cheap (TOML read) but every read used to spin
// a fresh round-trip. This store consolidates them: one async load
// per session, in-memory after that, invalidated explicitly when
// Settings saves so a toggled pref takes effect on the next read.
//
// Mirrors the loadRecordingPrefs() pattern already in compliance.ts
// but covers the strictly-local app prefs (config.toml) instead of
// the server-side org recording_prefs row.

import { invoke } from "@tauri-apps/api/core";

// Mirrors the Rust AppPrefs payload returned by `get_app_prefs`.
// Treated as a partial — older config.toml files that pre-date a
// field still resolve via the Rust default, but TypeScript callers
// should null-check rather than assume presence on a fresh install.
export type AppPrefs = {
  close_to_tray: boolean;
  auto_detect: boolean;
  auto_detect_popup: boolean;
  telemetry_enabled: boolean;
  sounds_enabled: boolean;
  max_recording_minutes: number;
  max_self_note_minutes: number;
  manual_notes_enabled: boolean;
  wayland_hotkey_notice_dismissed: boolean;
  input_device: string | null;
  self_note_shortcut: string | null;
  record_toggle_shortcut: string | null;
  consent_announcement_enabled: boolean;
  /** #596 — auto-record master toggles. Default false on a fresh
   *  install; the Settings → Auto-record section is the only UI that
   *  reads these directly. */
  auto_record_start_enabled: boolean;
  auto_record_stop_enabled: boolean;
  /** #659 P4 — opt-in for the floating always-on-top co-pilot overlay.
   *  Default false. Read by the layout's record-start handler to decide
   *  whether to open the `/overlay` window (Call mode + live transcript on);
   *  toggled from the Settings → Co-pilot row. */
  overlay_enabled: boolean;
};

let cached: AppPrefs | null = null;
let inflight: Promise<AppPrefs | null> | null = null;

/**
 * Returns the cached prefs if present, otherwise issues a single
 * `get_app_prefs` round-trip and caches the result. Concurrent
 * callers share the same in-flight promise so a parallel mount
 * doesn't fan out into N IPC calls.
 *
 * Returns null on IPC failure — callers should fall back to whatever
 * default they previously used. Errors are logged once via console.
 */
export async function getAppPrefs(): Promise<AppPrefs | null> {
  if (cached) return cached;
  if (inflight) return inflight;
  inflight = (async () => {
    try {
      const p = await invoke<AppPrefs>("get_app_prefs");
      cached = p;
      return p;
    } catch (e) {
      console.warn("get_app_prefs failed", e);
      return null;
    } finally {
      inflight = null;
    }
  })();
  return inflight;
}

/**
 * Drop the cache. Call from Settings after a successful
 * `set_app_prefs` so the next consumer reads the updated values.
 */
export function invalidateAppPrefs(): void {
  cached = null;
}

/**
 * Replace the cache wholesale. Optional fast-path for Settings to
 * skip the next round-trip after a save — the form already has the
 * latest values; we just hand them to the cache.
 */
export function primeAppPrefs(prefs: AppPrefs): void {
  cached = prefs;
}
