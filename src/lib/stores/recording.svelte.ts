// Screen-recording status store (#216).
//
// Owns the *status* of an in-flight screen recording so the
// `ReportIssueDialog` and the floating `RecordingFloatingControl`
// can both read and react. The recording lifecycle itself
// (getDisplayMedia, MediaRecorder, finalise) still lives in the
// dialog — only one component owns the OS handles, and that's the
// only one that needs to. This store is the bridge that lets the
// dialog hide while the floater takes over the visible UI.
//
// Exported as `screenRecording` (not `recording`) to avoid a name
// clash with the call-recording state in `+layout.svelte`.
//
// State machine:
//   idle → starting → recording → idle    (normal)
//   idle → starting → idle                (user cancels picker)
//   recording → finalising → idle         (after stop)
//
// `dialogHidden` lets the dialog go off-screen while recording is
// live without unmounting. Setting this back to false re-renders
// the dialog so the user can finalise the report.
//
// `requestStop` is a one-shot signal: the floating control bumps a
// counter when the user clicks Stop, and the dialog observes via
// `$effect` to call its local `stopRecording()`. This avoids
// importing the dialog's MediaRecorder reference into the store.

export type ScreenRecordingState =
  | "idle"
  | "starting"
  | "recording"
  | "finalising";

function createStore() {
  let state = $state<ScreenRecordingState>("idle");
  let elapsedMs = $state(0);
  let bytes = $state(0);
  let dialogHidden = $state(false);
  let stopRequestId = $state(0);

  return {
    get state(): ScreenRecordingState {
      return state;
    },
    get isLive(): boolean {
      return state === "recording";
    },
    get elapsedMs(): number {
      return elapsedMs;
    },
    get bytes(): number {
      return bytes;
    },
    get dialogHidden(): boolean {
      return dialogHidden;
    },
    get stopRequestId(): number {
      return stopRequestId;
    },

    /** Called by the dialog when getDisplayMedia is awaited. */
    markStarting() {
      state = "starting";
      elapsedMs = 0;
      bytes = 0;
    },
    /** Called by the dialog once the MediaRecorder is live. */
    markRecording() {
      state = "recording";
      // Hide the dialog by default once recording starts so the rest
      // of the app becomes interactive. The dialog reads this flag
      // and renders nothing while it's true (still mounted, draft
      // state preserved).
      dialogHidden = true;
    },
    /** Called by the dialog when the recorder enters its onstop. */
    markFinalising() {
      state = "finalising";
    },
    /** Called by the dialog after finaliseRecording wraps up. */
    markIdle() {
      state = "idle";
      elapsedMs = 0;
      bytes = 0;
      dialogHidden = false;
    },
    /** Called by the dialog if start aborts before recording begins
     *  (user cancels picker, capability error, etc.). */
    markIdleAfterAbort() {
      state = "idle";
      elapsedMs = 0;
      bytes = 0;
      // Don't touch dialogHidden — it was never flipped on this path.
    },
    /** Called by the dialog's tick interval. */
    setProgress(ms: number, b: number) {
      elapsedMs = ms;
      bytes = b;
    },
    /** Floater calls this when the user clicks Stop. The dialog's
     *  effect watches stopRequestId and invokes stopRecording(). */
    requestStop() {
      stopRequestId += 1;
    },
    /** Floater calls this when the user clicks the pill body to
     *  bring the dialog back without stopping. */
    showDialog() {
      dialogHidden = false;
    },
  };
}

/** Singleton — only one screen recording is in flight at a time. */
export const screenRecording = createStore();
