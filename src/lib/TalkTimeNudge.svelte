<!--
  TalkTimeNudge — #660 co-pilot P1, self-only talk-time nudge.

  A subtle, dismissable, SELF-COACHING line fed the derived trailing You-run
  metric from `liveSession.svelte.ts` (plan §5 / ui.md §4). When the rep (You)
  has monologued past a threshold it surfaces one calm line inviting a question.

  VISUAL PRIVACY (critical, ui.md §4d): this line lives ONLY in the rep's own
  agent window. The counterpart never sees the agent UI, so the nudge is
  inherently private — self-coaching, verbal-only, on-device (derived from
  segment DURATIONS, no backend, no affective/camera scoring). Nothing here is
  shared to the other party or a manager.

  HARD RULES honored:
   - All chrome is component-scoped `.talk-nudge*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque, self-framed, non-judgmental (HR#2). Bone only — no
     `--live`, no `--accent` fill, no pulse: it informs, it never alarms/shames.
-->
<script lang="ts">
  import { onMount } from "svelte";

  let {
    /** Duration (ms) of the current unbroken trailing You-run. */
    youRunMs = 0,
    /** Optional talk-share readout (ui.md §4e — OPTIONAL, neutral). Rendered
     *  only when both are provided; a high You% is NEVER colored (no shame). */
    youPct = null,
    themPct = null,
    /** Surface threshold — ~90s continuous per plan §5. */
    thresholdMs = 90_000,
  }: {
    youRunMs?: number;
    youPct?: number | null;
    themPct?: number | null;
    thresholdMs?: number;
  } = $props();

  // Dismiss latch, scoped to the CURRENT monologue run. Cleared when the run
  // resets (the counterpart speaks → youRunMs drops back below threshold), so a
  // fresh long You-run can surface the nudge again — but it never re-fires
  // within the same unbroken monologue (ui.md §4c re-arm).
  let dismissed = $state(false);
  $effect(() => {
    if (youRunMs < thresholdMs && dismissed) dismissed = false;
  });

  let show = $derived(youRunMs >= thresholdMs && !dismissed);
  let hasShare = $derived(youPct !== null && themPct !== null);

  // Motion gate — the fade-in is CSS, but keep parity with the lane's posture.
  let reduceMotion = $state(false);
  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const handler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  });
</script>

{#if show}
  <div
    class="talk-nudge"
    class:reduce={reduceMotion}
    role="status"
    aria-live="polite"
  >
    <span class="talk-nudge-glyph" aria-hidden="true">
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>
    </span>
    <span class="talk-nudge-text"
      >You've been talking a while — a question could open things up.</span
    >
    {#if hasShare}
      <span class="talk-nudge-share" aria-hidden="true"
        >You {youPct}% · Them {themPct}%</span
      >
    {/if}
    <button
      type="button"
      class="talk-nudge-dismiss"
      aria-label="Dismiss"
      onclick={() => (dismissed = true)}
    >
      <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
    </button>
  </div>
{/if}

<style>
  /* Slim single line, ambient in the rep's peripheral vision. A faint top
     hairline rule separates it from the stream above; bone only. */
  .talk-nudge {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.5rem;
    padding: 0.4rem 0.55rem;
    border-top: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    background: var(--ink-2);
    animation: talk-nudge-in 180ms ease-out both;
  }
  .talk-nudge.reduce {
    animation: none;
  }
  @media (prefers-reduced-motion: reduce) {
    .talk-nudge {
      animation: none;
    }
  }
  @keyframes talk-nudge-in {
    from {
      opacity: 0;
    }
    to {
      opacity: 1;
    }
  }

  .talk-nudge-glyph {
    display: inline-flex;
    align-items: center;
    color: var(--bone-3);
    flex-shrink: 0;
  }
  .talk-nudge-text {
    flex: 1 1 auto;
    min-width: 0;
    font-size: 0.8rem;
    line-height: 1.35;
    color: var(--bone-2);
    overflow-wrap: anywhere;
  }
  /* Optional share readout — neutral mono, never colored (a high You% is not
     a shame signal). */
  .talk-nudge-share {
    flex-shrink: 0;
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--bone-3);
  }
  .talk-nudge-dismiss {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    width: 1.15rem;
    height: 1.15rem;
    padding: 0;
    border: none;
    border-radius: 50%;
    background: transparent;
    color: var(--bone-3);
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }
  .talk-nudge-dismiss:hover {
    background: var(--hairline);
    color: var(--bone-1);
  }
  .talk-nudge-dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
