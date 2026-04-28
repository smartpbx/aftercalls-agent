// Call-recording status store (#327).
//
// Sister to `screenRecording` (issue #216) — owns the *status* of an
// in-flight call recording (or self-note) so the persistent floating
// control can surface during normal calls, not only screen captures.
//
// Why a discriminated-union "recording" surface but two stores under
// the hood? Two reasons:
//   1. Truth lives in two different places. Screen recording's
//      lifecycle is owned by `ReportIssueDialog` (its MediaRecorder
//      handles the OS handles). Call recording's lifecycle is owned
//      by the Tauri backend; the layout is the bridge that listens
//      for `recording-state` + `pipeline` events. Forcing them into
//      one store would couple two unrelated owners.
//   2. The screen-recording store already ships in production with a
//      stable API consumed by `ReportIssueDialog` and the existing
//      `RecordingFloatingControl`. Renaming it would touch the
//      MediaRecorder owner for purely cosmetic reasons. Better to
//      keep `screenRecording` as-is and add `callRecording` next to
//      it.
//
// The unified UI lives in `LiveRecordingFloater.svelte` and reads
// both stores; consumers of the *floater* see one component, but the
// state graphs stay separate.
//
// State machine:
//   idle → recording                        (start)
//   recording → stopping → done             (normal stop, fades out)
//   recording → stopping → failed           (pipeline failure, holds)
//
// `stage` reflects the post-stop pipeline progression. While
// `state === "recording"` the stage is always "live" (the live timer
// is the user's feedback). Once the user (or the timer cap) stops
// the recording, the floater stays mounted to surface
// "Transcribing → Summarizing → Saved" before fading out.

import { goto } from "$app/navigation";

export type CallRecordingState =
  | "idle"
  | "recording"
  | "stopping"
  | "done"
  | "failed";

export type CallRecordingMode = "call" | "self_note";

/** Pipeline stages that the floater renders post-stop. The full set
 *  the backend emits is wider (`started`, `transcribing`,
 *  `summarizing`, `writing_note`, `uploading`, `done`, `failed`,
 *  `transcribed` carrying a call_id); we collapse them into a small
 *  status vocabulary the floater can label cleanly. */
export type CallRecordingStage =
  | "live"
  | "transcribing"
  | "summarizing"
  | "uploading"
  | "done"
  | "failed";

function createStore() {
  let state = $state<CallRecordingState>("idle");
  let mode = $state<CallRecordingMode>("call");
  let stage = $state<CallRecordingStage>("live");
  let startedAtMs = $state<number | null>(null);
  let elapsedMs = $state(0);
  let stopRequestId = $state(0);
  /** Latest known call id (set on the pipeline `transcribed` event,
   *  re-set on `done`). Used by the failed-state pointer so the user
   *  can deep-link into the partial transcript even on pipeline
   *  failure. Empty when the pipeline hasn't reported one yet. */
  let callId = $state<string>("");
  /** Sanitized error string for the failed surface. Plain text. */
  let errorMessage = $state<string>("");

  let timerHandle: ReturnType<typeof setInterval> | null = null;

  function startTimer(seedAtMs: number | null) {
    stopTimer();
    startedAtMs = seedAtMs ?? Date.now();
    elapsedMs = Math.max(0, Date.now() - startedAtMs);
    timerHandle = setInterval(() => {
      if (startedAtMs !== null) {
        elapsedMs = Math.max(0, Date.now() - startedAtMs);
      }
    }, 500);
  }

  function stopTimer() {
    if (timerHandle) {
      clearInterval(timerHandle);
      timerHandle = null;
    }
  }

  return {
    get state(): CallRecordingState {
      return state;
    },
    get mode(): CallRecordingMode {
      return mode;
    },
    get stage(): CallRecordingStage {
      return stage;
    },
    get isLive(): boolean {
      return state === "recording";
    },
    get isVisible(): boolean {
      return state !== "idle";
    },
    get elapsedMs(): number {
      return elapsedMs;
    },
    get startedAtMs(): number | null {
      return startedAtMs;
    },
    get stopRequestId(): number {
      return stopRequestId;
    },
    get callId(): string {
      return callId;
    },
    get errorMessage(): string {
      return errorMessage;
    },

    /** Transition into the live recording state. Called from the
     *  layout's `recording-state` listener when `recording=true`.
     *  `seedAtMs` lets a re-mounted webview pick up an in-flight
     *  session via `is_recording` without losing elapsed time. */
    markRecording(nextMode: CallRecordingMode, seedAtMs: number | null) {
      mode = nextMode;
      stage = "live";
      callId = "";
      errorMessage = "";
      state = "recording";
      startTimer(seedAtMs);
    },

    /** Transition into the post-stop stopping state. The recorder has
     *  released its handles (or is about to); the pipeline has not
     *  yet reported a stage. We pin the elapsed timer at the final
     *  value so the user sees the duration of what they captured. */
    markStopping() {
      if (state === "idle") return;
      stopTimer();
      state = "stopping";
      stage = "transcribing"; // best-guess until the pipeline reports
    },

    /** Pipeline event from the layout. Maps the wider backend
     *  vocabulary onto the floater's small status set. `transcribed`
     *  carries a call_id; we capture it without changing stage so
     *  the floater can deep-link on failure. */
    notePipelineStage(s: string, payloadCallId?: string) {
      if (state === "idle") return;
      if (payloadCallId) callId = payloadCallId;
      if (s === "transcribing" || s === "started") {
        stage = "transcribing";
      } else if (s === "summarizing" || s === "writing_note") {
        stage = "summarizing";
      } else if (s === "uploading") {
        stage = "uploading";
      } else if (s === "done") {
        stage = "done";
        state = "done";
        // Caller schedules the fade-out via dismissAfterDone.
      } else if (s === "failed") {
        stage = "failed";
        state = "failed";
      }
      // `transcribed` doesn't move the stage forward — it's the
      // mid-pipeline signal that `call_id` is now known.
    },

    /** Set on `pipeline:failed`. Plain-text, vendor-redacted by the
     *  caller. */
    setError(msg: string) {
      errorMessage = msg;
    },

    /** Called by the layout's auto-fade `setTimeout` after `done`.
     *  Idempotent — a second call from a stale timer is a no-op. */
    reset() {
      state = "idle";
      stage = "live";
      mode = "call";
      stopTimer();
      startedAtMs = null;
      elapsedMs = 0;
      callId = "";
      errorMessage = "";
    },

    /** Floater calls this when the user clicks Stop. The Record page
     *  / layout watch `stopRequestId` and invoke `stop_recording`. */
    requestStop() {
      stopRequestId += 1;
    },

    /** Floater calls this on the failed-state retry pointer to deep-
     *  link into the call detail (if a call id was reported pre-
     *  failure) or the calls list (if not). Wraps `goto` so the
     *  floater stays decoupled from `$app/navigation`. */
    async openCallOrList() {
      const target = callId ? `/calls/${callId}` : "/calls";
      this.reset();
      await goto(target);
    },
  };
}

/** Singleton — only one call recording is in flight at a time. */
export const callRecording = createStore();
