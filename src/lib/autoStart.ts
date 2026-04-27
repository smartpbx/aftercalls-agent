// autoStart.ts — shared helper for the auto-detect "Record this call" path.
//
// Both +layout.svelte (slide-out) and +page.svelte (inline banner) need to
// play the start cue and then invoke confirm_auto_start. Extracting the core
// mechanics here avoids copy-paste drift between the two surfaces (#465).
//
// Callers are responsible for their own ack-gating, prompt-state cleanup, and
// error surfacing — those are route-local concerns. This module only owns the
// shared atomic action: "play cue, then confirm".

import { invoke } from "@tauri-apps/api/core";
import { playStartCueIfEnabled } from "$lib/compliance";

/**
 * Play the recording start cue then call `confirm_auto_start` in the backend.
 *
 * The cue plays BEFORE confirming so the system loopback doesn't capture the
 * beep. The cue failure is swallowed — we'd rather start the recording than
 * miss it because of a flaky audio subsystem.
 *
 * Throws if `confirm_auto_start` itself throws; callers should catch and log.
 */
export async function confirmAutoStart(): Promise<void> {
  try {
    await playStartCueIfEnabled();
  } catch (e) {
    console.warn("start cue failed", e);
  }
  await invoke("confirm_auto_start");
}
