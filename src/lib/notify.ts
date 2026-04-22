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

export async function notifyRecordStart() {
  if (!(await soundsEnabled())) return;
  // Two-note ascending: friendly "go" cue.
  playNotes([
    { freq: 523.25, at: 0,    dur: 0.12 }, // C5
    { freq: 783.99, at: 0.13, dur: 0.18 }, // G5
  ]);
}

export async function notifyRecordStop() {
  if (!(await soundsEnabled())) return;
  // Two-note descending — complement of start.
  playNotes([
    { freq: 783.99, at: 0,    dur: 0.12 }, // G5
    { freq: 523.25, at: 0.13, dur: 0.18 }, // C5
  ]);
}

export async function notifyPipelineDone() {
  if (!(await soundsEnabled())) return;
  // Three ascending notes — triumphant "done" chord.
  playNotes([
    { freq: 523.25, at: 0,    dur: 0.1 },  // C5
    { freq: 659.25, at: 0.1,  dur: 0.1 },  // E5
    { freq: 783.99, at: 0.2,  dur: 0.22 }, // G5
  ]);
}

export async function notifyPipelineFailed() {
  if (!(await soundsEnabled())) return;
  // Descending minor — failure cue. Quieter than done.
  playNotes([
    { freq: 440,    at: 0,    dur: 0.12, gain: 0.14 }, // A4
    { freq: 349.23, at: 0.14, dur: 0.22, gain: 0.14 }, // F4
  ]);
}

export async function notifyAutoDetect() {
  if (!(await soundsEnabled())) return;
  // Double-ping — attention cue. Higher pitch to stand out from
  // the system-audio that's already playing (the call source).
  playNotes([
    { freq: 880, at: 0,    dur: 0.08, gain: 0.14 },
    { freq: 880, at: 0.18, dur: 0.08, gain: 0.14 },
  ]);
}
