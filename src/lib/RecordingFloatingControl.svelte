<script lang="ts">
  // Floating screen-recording control pill (#216).
  //
  // Always visible while a screen recording is in flight for the
  // ReportIssueDialog. Sits in the bottom-right corner above the
  // toast host so it can never be hidden by an unrelated
  // notification. Shows:
  //   - red recording dot (pulsing)
  //   - mm:ss elapsed counter
  //   - running size estimate (XX MB / 100 MB)
  //   - "Open report" affordance to bring the report dialog back without
  //     stopping
  //   - Stop button (primary action)
  //
  // Mounted once at the layout root. Renders nothing when state is
  // not "recording" or "finalising". The dialog owns the actual
  // MediaRecorder; the pill only signals via
  // `screenRecording.requestStop()`.
  //
  // Vendor-opacity: nothing here mentions a vendor name.

  import { screenRecording } from "$lib/stores/recording.svelte";

  const MAX_BYTES = 100 * 1024 * 1024;

  function fmtTime(ms: number): string {
    const total = Math.floor(ms / 1000);
    const mm = Math.floor(total / 60);
    const ss = total % 60;
    return `${mm.toString().padStart(2, "0")}:${ss.toString().padStart(2, "0")}`;
  }

  function fmtMB(n: number): string {
    return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  }

  function onStop() {
    screenRecording.requestStop();
  }

  function onOpenForm() {
    screenRecording.showDialog();
  }
</script>

{#if screenRecording.state === "recording" || screenRecording.state === "finalising"}
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
      onclick={onOpenForm}
      aria-label="Open report form"
      title="Open report form"
    >
      Open report
    </button>
    <button
      type="button"
      class="rec-stop-btn"
      onclick={onStop}
      aria-label="Stop recording"
      disabled={screenRecording.state === "finalising"}
    >
      {screenRecording.state === "finalising" ? "Finishing…" : "Stop"}
    </button>
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

  .rec-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--live);
    animation: rec-dot-pulse 1.4s ease-in-out infinite;
    flex-shrink: 0;
  }
  @keyframes rec-dot-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.35;
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

  .rec-form-btn,
  .rec-stop-btn {
    appearance: none;
    font-size: 0.78rem;
    font-weight: 600;
    padding: 0.3rem 0.7rem;
    border-radius: 999px;
    cursor: pointer;
    transition: filter 0.15s, background 0.15s, color 0.15s;
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
    .rec-floater {
      animation: none;
    }
    .rec-dot {
      animation: none;
      opacity: 1;
    }
  }
</style>
