// Short synthesized notification tones for the agent.
//
// Web Audio oscillators so we don't have to ship binary assets
// (keeps the updater swap smaller + avoids rebundling on tone
// changes). Each function is a tiny envelope: one or two notes
// with a short attack/release so the tone reads as a "chime" not
// a sine blip.
//
// The audio context is lazily created on first use because
// browsers require a user gesture before letting us create one.
// The recording-start event is triggered by the user's click on
// Start recording, which counts as a gesture.
//
// `invoke("get_app_prefs")` determines whether sounds are on —
// we read it lazily per call so a user flipping the toggle takes
// effect immediately without reloading.

import { invoke } from "@tauri-apps/api/core";

let ctx: AudioContext | null = null;
function getCtx(): AudioContext | null {
  if (typeof window === "undefined") return null;
  if (ctx) return ctx;
  try {
    // Safari still needs webkitAudioContext; Tauri's webkit2gtk
    // sometimes falls under that too.
    const AC = (window as any).AudioContext ?? (window as any).webkitAudioContext;
    if (!AC) return null;
    ctx = new AC();
    return ctx;
  } catch {
    return null;
  }
}

async function soundsEnabled(): Promise<boolean> {
  try {
    const p = await invoke<{ sounds_enabled: boolean }>("get_app_prefs");
    return p.sounds_enabled ?? true;
  } catch {
    return true;
  }
}

// #92: gate "normal" chimes on busy-state as well as the user pref.
// Reads `is_processing` (lib.rs:237-239, shipped in #79) which is the
// single source of truth for "agent is actively recording OR the
// post-recording pipeline is still running". When busy, suppress the
// tone so:
//   - a stray `pipeline=done` chime from a prior call doesn't play
//     mid-new-recording and get captured by the system loopback,
//   - auto-detect pings don't fire while a user is already in a call.
// Fail-open on IPC error: falls back to pref-only behaviour (same
// discipline as #79) so a Tauri hiccup can't silence the agent
// permanently.
//
// NOTE: `notifyRecordStart(force=true)` deliberately bypasses BOTH
// this helper and the pref — PIPEDA "enforced" compliance cues from
// #48 must remain unsilenceable by local state. Only the four
// "normal" emitters (stop, pipeline done/failed, auto-detect) and
// the `force=false` branch of `notifyRecordStart` route through here.
async function shouldPlayTone(): Promise<boolean> {
  if (!(await soundsEnabled())) return false;
  try {
    const busy = await invoke<boolean>("is_processing");
    return !busy;
  } catch {
    // Fail-open: on IPC error, honour pref-only behaviour.
    return true;
  }
}

type Note = {
  freq: number; // Hz
  at: number;   // start offset seconds
  dur: number;  // note length seconds
  gain?: number; // peak gain (defaults 0.18 — quiet)
};

function playNotes(notes: Note[]) {
  const c = getCtx();
  if (!c) return;
  // Resume on first user gesture — some browsers autosuspend the
  // context, especially if we spawn it before the first click.
  if (c.state === "suspended") c.resume().catch(() => {});
  const now = c.currentTime;
  for (const n of notes) {
    const osc = c.createOscillator();
    const g = c.createGain();
    osc.type = "sine";
    osc.frequency.value = n.freq;
    const peak = n.gain ?? 0.18;
    // Gentle attack + release: avoids click artifacts that a
    // bare start/stop would produce.
    g.gain.setValueAtTime(0, now + n.at);
    g.gain.linearRampToValueAtTime(peak, now + n.at + 0.012);
    g.gain.linearRampToValueAtTime(0, now + n.at + n.dur);
    osc.connect(g).connect(c.destination);
    osc.start(now + n.at);
    osc.stop(now + n.at + n.dur + 0.02);
  }
}

// Await the scheduled note sequence so callers can block until the
// last note has finished playing. Used by the start-cue path before
// starting the recorder so the system loopback doesn't capture the
// beep. Falls through with a no-op wait when no AudioContext is
// available (headless test environments).
function playNotesAwait(notes: Note[]): Promise<void> {
  playNotes(notes);
  const totalSec =
    notes.length === 0
      ? 0
      : Math.max(...notes.map((n) => n.at + n.dur)) + 0.05;
  return new Promise((resolve) => setTimeout(resolve, totalSec * 1000));
}

// `force` bypasses the user's sounds_enabled preference. Used by the
// PIPEDA "enforced" recording-notification mode (#48) where admins
// require the start cue play regardless of local settings — an org's
// compliance posture shouldn't be silenceable from Settings. The
// default path is unchanged so every other caller (stop, pipeline
// done/failed, auto-detect) still honours the local mute toggle.
//
// TODO(#48): speech announcement — "Please note, this call is being
// recorded" — is NOT implemented yet. Web Audio TTS is inconsistent
// across Tauri's webview backends (webkit2gtk in particular). Ship
// the tone + the `force`/mode infrastructure now; add speech when we
// can evaluate a viable path (bundled sample audio OR the Web Speech
// API with a graceful fallback for platforms that lack a voice).
export async function notifyRecordStart(force: boolean = false) {
  // #92: force=true (PIPEDA "enforced" mode, #48) bypasses BOTH the
  // sounds_enabled pref AND the busy gate — compliance cues must play
  // regardless of local state. force=false takes the normal
  // pref + busy-state path.
  if (!force && !(await shouldPlayTone())) return;
  // Two-note ascending: friendly "go" cue. Awaits the full note
  // duration so start_recording can be invoked AFTER the cue plays —
  // otherwise the system loopback captures the beep on every call.
  await playNotesAwait([
    { freq: 523.25, at: 0,    dur: 0.12 }, // C5
    { freq: 783.99, at: 0.13, dur: 0.18 }, // G5
  ]);
}

export async function notifyRecordStop() {
  if (!(await shouldPlayTone())) return;
  // Two-note descending — complement of start.
  playNotes([
    { freq: 783.99, at: 0,    dur: 0.12 }, // G5
    { freq: 523.25, at: 0.13, dur: 0.18 }, // C5
  ]);
}

export async function notifyPipelineDone() {
  if (!(await shouldPlayTone())) return;
  // Three ascending notes — triumphant "done" chord.
  playNotes([
    { freq: 523.25, at: 0,    dur: 0.1 },  // C5
    { freq: 659.25, at: 0.1,  dur: 0.1 },  // E5
    { freq: 783.99, at: 0.2,  dur: 0.22 }, // G5
  ]);
}

export async function notifyPipelineFailed() {
  if (!(await shouldPlayTone())) return;
  // Descending minor — failure cue. Quieter than done.
  playNotes([
    { freq: 440,    at: 0,    dur: 0.12, gain: 0.14 }, // A4
    { freq: 349.23, at: 0.14, dur: 0.22, gain: 0.14 }, // F4
  ]);
}

export async function notifyAutoDetect() {
  if (!(await shouldPlayTone())) return;
  // Double-ping — attention cue. Higher pitch to stand out from
  // the system-audio that's already playing (the call source).
  playNotes([
    { freq: 880, at: 0,    dur: 0.08, gain: 0.14 },
    { freq: 880, at: 0.18, dur: 0.08, gain: 0.14 },
  ]);
}
