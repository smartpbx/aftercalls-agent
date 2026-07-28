<script lang="ts">
  // Persistent live-recording floater (#216 + #327).
  //
  // One pill, two recording kinds. Originally built for the screen-
  // recording capture inside `ReportIssueDialog` (#216), now also
  // surfaces during call recording + self-note capture (#327) so the
  // user always has a visible Stop button — even when the main
  // window is minimized to tray, even on Hyprland/Sway with tray
  // popups disabled. Recording with no on-screen feedback is a
  // privacy footgun; this is the primary always-visible signal.
  //
  // The component reads two stores:
  //   - `screenRecording` — owned by `ReportIssueDialog`. The dialog
  //     holds the MediaRecorder; the pill only signals via
  //     `screenRecording.requestStop()` and re-opens the dialog via
  //     `screenRecording.showDialog()`.
  //   - `callRecording`   — owned by the layout's `recording-state`
  //     and `pipeline` listeners. The Tauri backend holds the actual
  //     mic/loopback handles; the pill signals stop via
  //     `callRecording.requestStop()` which the layout observes.
  //
  // Branches on `kind` for label / icon / stop-action. Render
  // priority when both happen at once: screen-recording wins (it has
  // a hard 100 MB cap to surface) and the call-recording pill is
  // suppressed so the user only ever sees one floater. In practice
  // simultaneous capture is unusual — the support-issue dialog flow
  // is its own modal route — but the priority rule keeps the layout
  // rule deterministic.
  //
  // Self-note prominence: when `mode === "self_note"`, the pill picks
  // up a subtle border pulse and a clearer label so the user can
  // tell at a glance which recording kind is in flight. The chime
  // suppression mentioned in the issue body lives in compliance.ts;
  // this floater doesn't gate on it.
  //
  // Vendor opacity: nothing here mentions a vendor name.

  import { invoke } from "@tauri-apps/api/core";
  import { screenRecording } from "$lib/stores/recording.svelte";
  import { callRecording } from "$lib/stores/callRecording.svelte";

  const MAX_BYTES = 100 * 1024 * 1024;

  // #302 Slice C — poll screen-capture status while a CALL is live so the
  // floater can widen to "Recording call + screen". No dedicated backend
  // event this slice; a light 2s poll is enough (the floater is the one
  // always-visible privacy signal). Self-notes never screen-capture, so
  // the poll only writes while `mode === "call"` and the store guards on
  // the live state. The effect tears the interval down when the call
  // leaves the recording state.
  $effect(() => {
    if (callRecording.state !== "recording") return;
    let cancelled = false;
    const probe = async () => {
      try {
        const st = await invoke<{
          available: boolean;
          capturing: boolean;
          consented: boolean | null;
          source_kind: string | null;
        }>("screen_capture_status");
        // #302 follow-up — source_kind is null when no capture is running
        // (never picked, or it died mid-call) so the cue drops cleanly.
        if (!cancelled) callRecording.setScreenSource(st.source_kind ?? null);
      } catch {
        // Advisory only — a failed probe never surfaces an error.
      }
    };
    void probe();
    const handle = setInterval(probe, 2000);
    return () => {
      cancelled = true;
      clearInterval(handle);
    };
  });

  // The screen cue rides the call floater only for real calls that are
  // actively screen-capturing.
  let screenCue = $derived(
    callRecording.state === "recording" &&
      callRecording.mode === "call" &&
      callRecording.screenActive,
  );

  // #302 follow-up — the captured source kind, as plain user-facing words
  // (vendor-opaque). Widens the floater to "+ screen / + window / + area".
  let screenKindLabel = $derived.by(() => {
    switch (callRecording.screenKind) {
      case "window":
        return "window";
      case "region":
        return "area";
      default:
        return "screen";
    }
  });

  function fmtTime(ms: number): string {
    const total = Math.floor(ms / 1000);
    const mm = Math.floor(total / 60);
    const ss = total % 60;
    return `${mm.toString().padStart(2, "0")}:${ss.toString().padStart(2, "0")}`;
  }

  function fmtMB(n: number): string {
    return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  }

  // Screen-recording shows when its state is "recording" or
  // "finalising"; call-recording shows for any non-idle state so the
  // post-stop pipeline status (transcribing → summarizing → done)
  // stays visible. The screen-recording flow takes precedence to
  // avoid stacked pills (see component-level comment).
  let screenVisible = $derived(
    screenRecording.state === "recording" ||
      screenRecording.state === "finalising",
  );
  let callVisible = $derived(callRecording.isVisible && !screenVisible);

  // Post-stop label for the call floater. The live-recording label is
  // mode-aware ("Recording call" vs "Self-note recording"); the
  // post-stop label tracks pipeline progress.
  let callLabel = $derived.by(() => {
    if (callRecording.state === "recording") {
      if (callRecording.mode === "self_note") return "Self-note recording";
      return screenCue
        ? `Recording call + ${screenKindLabel}`
        : "Recording call";
    }
    if (callRecording.state === "stopping") return "Wrapping up…";
    if (callRecording.state === "done") return "Saved";
    if (callRecording.state === "failed") return "Pipeline failed";
    if (callRecording.stage === "transcribing") return "Transcribing…";
    if (callRecording.stage === "summarizing") return "Summarizing…";
    if (callRecording.stage === "uploading") return "Saving…";
    return "Recording";
  });

  // Pip class drives the colour + animation of the leading dot. Live
  // states pulse red; pipeline stages glow amber; done is olive;
  // failed is solid red. Mirrors the `.pip` vocabulary already in
  // app.css (design.md §Pipe / pip indicator).
  let callPipClass = $derived.by(() => {
    if (callRecording.state === "recording") return "live";
    if (callRecording.state === "done") return "done";
    if (callRecording.state === "failed") return "failed";
    // stopping / pipeline-in-flight all read as "working" amber.
    if (callRecording.stage === "transcribing") return "transcribing";
    if (callRecording.stage === "summarizing") return "summarizing";
    if (callRecording.stage === "uploading") return "uploading";
    return "live";
  });

  function onScreenStop() {
    screenRecording.requestStop();
  }

  function onScreenOpenForm() {
    screenRecording.showDialog();
  }

  function onCallStop() {
    callRecording.requestStop();
  }

  function onCallRetry() {
    void callRecording.openCallOrList();
  }
</script>

{#if screenVisible}
  <div
    class="rec-floater"
    role="status"
    aria-label="Screen recording in progress"
    aria-live="polite"
  >
    <span class="rec-dot" aria-hidden="true"></span>
    <span class="rec-meta">
      <span class="rec-time">{fmtTime(screenRecording.elapsedMs)}</span>
      <span class="rec-size"
        >{fmtMB(screenRecording.bytes)} / {fmtMB(MAX_BYTES)}</span
      >
    </span>
    <button
      type="button"
      class="rec-form-btn"
      onclick={onScreenOpenForm}
      aria-label="Open report form"
      title="Open report form"
    >
      Open report
    </button>
    <button
      type="button"
      class="rec-stop-btn"
      onclick={onScreenStop}
      aria-label="Stop recording"
      disabled={screenRecording.state === "finalising"}
    >
      {screenRecording.state === "finalising" ? "Finishing…" : "Stop"}
    </button>
  </div>
{:else if callVisible}
  <div
    class="rec-floater rec-floater-call"
    class:rec-floater-self-note={callRecording.mode === "self_note" &&
      callRecording.state === "recording"}
    class:rec-floater-failed={callRecording.state === "failed"}
    class:rec-floater-done={callRecording.state === "done"}
    role="status"
    aria-label={callRecording.mode === "self_note"
      ? "Self-note recording in progress"
      : screenCue
        ? `Call and ${screenKindLabel} recording in progress`
        : "Call recording in progress"}
    aria-live="polite"
  >
    {#if screenCue}
      <svg
        class="rec-screen-glyph"
        viewBox="0 0 20 20"
        width="15"
        height="15"
        aria-hidden="true"
      >
        <rect
          x="2.2"
          y="3.5"
          width="15.6"
          height="10"
          rx="1.4"
          fill="none"
          stroke="currentColor"
          stroke-width="1.4"
        />
        <path
          d="M7.5 16.5 h5 M10 13.5 v3"
          stroke="currentColor"
          stroke-width="1.4"
          stroke-linecap="round"
          fill="none"
        />
      </svg>
    {/if}
    <span class="rec-dot rec-dot-{callPipClass}" aria-hidden="true"></span>
    <span class="rec-meta">
      <span class="rec-label">{callLabel}</span>
      {#if screenCue}
        <span class="rec-screen-badge">SCREEN</span>
      {/if}
      {#if callRecording.state === "recording" || callRecording.state === "stopping"}
        <span class="rec-time">{fmtTime(callRecording.elapsedMs)}</span>
      {/if}
    </span>
    {#if callRecording.state === "recording"}
      <button
        type="button"
        class="rec-stop-btn"
        onclick={onCallStop}
        aria-label="Stop recording"
      >
        Stop
      </button>
    {:else if callRecording.state === "failed"}
      <button
        type="button"
        class="rec-form-btn rec-retry-btn"
        onclick={onCallRetry}
        aria-label="Open call detail to retry"
      >
        Open
      </button>
    {/if}
  </div>
{/if}

<style>
  /* Bottom-right placement, above the toast host (z-index 70 in
   * app.css). Sits at the same right offset as the toast stack but
   * above it so a stack of toasts doesn't push the recording pill
   * out of sight. Below modal backdrops (.rn-backdrop) since those
   * default to a higher implicit stacking via DOM order on top of
   * the layout — an accidentally re-opened dialog still wins. */
  .rec-floater {
    position: fixed;
    right: 1rem;
    bottom: 1rem;
    z-index: 80;
    display: inline-flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.5rem 0.7rem;
    background: var(--ink-1);
    border: 1px solid var(--live);
    border-radius: 999px;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.35);
    color: var(--bone-1);
    font-family: var(--font-sans);
    animation: rec-floater-in 0.18s ease-out;
  }
  @keyframes rec-floater-in {
    from {
      opacity: 0;
      transform: translateY(8px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }

  /* Self-note variant — slightly more prominent so the user doesn't
   * miss it when no chime layered on top. Soft outer halo + a
   * gentler border pulse. The dot still pulses on its own; the
   * border pulse is the second-order cue. */
  .rec-floater-self-note {
    box-shadow:
      0 6px 18px rgba(0, 0, 0, 0.35),
      0 0 0 0 var(--live-soft);
    animation:
      rec-floater-in 0.18s ease-out,
      rec-floater-self-note-pulse 1.6s ease-in-out infinite;
  }
  @keyframes rec-floater-self-note-pulse {
    0%, 100% {
      box-shadow:
        0 6px 18px rgba(0, 0, 0, 0.35),
        0 0 0 0 var(--live-soft);
    }
    50% {
      box-shadow:
        0 6px 18px rgba(0, 0, 0, 0.35),
        0 0 0 6px var(--live-soft);
    }
  }

  /* Post-stop variants. Failed holds (no fade) and recolours the
   * border to amber-ish so the user can tell something needs their
   * attention. Done picks up the olive border for the brief moment
   * before the layout-side timer fires the reset. */
  .rec-floater-failed {
    border-color: var(--sig);
  }
  .rec-floater-done {
    border-color: var(--olive);
  }

  .rec-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .rec-dot-live {
    background: var(--live);
    animation: rec-dot-pulse 1.4s ease-in-out infinite;
  }
  .rec-dot-transcribing,
  .rec-dot-summarizing,
  .rec-dot-uploading {
    background: var(--sig);
    animation: rec-dot-blink 1s ease-in-out infinite;
  }
  .rec-dot-done {
    background: var(--olive);
  }
  .rec-dot-failed {
    background: var(--live);
  }
  /* Default screen-recording dot (no kind class) keeps the original
   * red pulse for backwards compatibility with the existing flow. */
  .rec-floater:not(.rec-floater-call) .rec-dot {
    background: var(--live);
    animation: rec-dot-pulse 1.4s ease-in-out infinite;
  }
  @keyframes rec-dot-pulse {
    0%, 100% {
      opacity: 1;
    }
    50% {
      opacity: 0.35;
    }
  }
  @keyframes rec-dot-blink {
    0%, 100% {
      opacity: 1;
    }
    50% {
      opacity: 0.5;
    }
  }

  .rec-meta {
    display: inline-flex;
    flex-direction: column;
    line-height: 1.15;
    font-variant-numeric: tabular-nums;
  }
  .rec-time {
    color: var(--bone-0);
    font-family: var(--font-mono);
    font-size: 0.85rem;
    font-weight: 500;
  }
  .rec-size {
    color: var(--bone-3);
    font-family: var(--font-mono);
    font-size: 0.7rem;
  }
  /* Call-floater label: prose, not mono — distinguishes the two
   * kinds visually beyond the leading dot. */
  .rec-label {
    color: var(--bone-0);
    font-size: 0.82rem;
    font-weight: 600;
    letter-spacing: -0.005em;
  }

  /* #302 Slice C — screen-capture cue. The monitor glyph sits before
   * the pulsing dot; the SCREEN mini-chip reads the screen state at a
   * glance. `--live` on `--live-soft` (the on-air law), mono uppercase. */
  .rec-screen-glyph {
    color: var(--live);
    flex-shrink: 0;
  }
  .rec-screen-badge {
    display: inline-block;
    margin-top: 0.1rem;
    padding: 0.02rem 0.32rem;
    border-radius: var(--radius-sm);
    background: var(--live-soft);
    color: var(--live);
    font-family: var(--font-mono);
    font-size: 0.6rem;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    align-self: flex-start;
  }

  .rec-form-btn,
  .rec-stop-btn {
    appearance: none;
    font-size: 0.78rem;
    font-weight: 600;
    padding: 0.3rem 0.7rem;
    border-radius: 999px;
    cursor: pointer;
    transition:
      filter 0.15s,
      background 0.15s,
      color 0.15s;
  }
  .rec-form-btn {
    border: 1px solid var(--hairline-hi);
    background: transparent;
    color: var(--bone-2);
  }
  .rec-form-btn:hover {
    color: var(--bone-0);
    border-color: var(--bone-2);
  }
  .rec-retry-btn {
    border-color: var(--sig);
    color: var(--sig);
  }
  .rec-retry-btn:hover {
    color: var(--bone-0);
    background: rgba(201, 162, 74, 0.18);
    border-color: var(--sig);
  }
  .rec-stop-btn {
    border: 1px solid var(--live);
    background: var(--live);
    color: var(--ink-0);
  }
  .rec-stop-btn:hover:not(:disabled) {
    filter: brightness(1.1);
  }
  .rec-stop-btn:disabled {
    opacity: 0.7;
    cursor: progress;
  }

  @media (prefers-reduced-motion: reduce) {
    .rec-floater,
    .rec-floater-self-note {
      animation: none;
    }
    .rec-dot,
    .rec-dot-live,
    .rec-dot-transcribing,
    .rec-dot-summarizing,
    .rec-dot-uploading {
      animation: none;
      opacity: 1;
    }
  }
</style>
