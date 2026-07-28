<!--
  TalkTimeNudge — co-pilot BI-revamp: the quiet-rail TALK-RATIO METER.

  Reworked (BI-dashboard revamp) from the old dismissable monologue line into a
  persistent, glanceable talk-share meter that lives in the reference rail. It
  is a GAUGE, not an alarm: a two-segment bar (You / Them) + a bone readout,
  shown whenever the call has produced any final turns. The monologue STATE is
  surfaced here only as a subtle `--sig` tint on the You segment once the rep's
  trailing run passes threshold — the actionable, worded "a question could open
  things up" nudge now lives ONCE, as the ephemeral cue (IntelligenceLane), so
  the alert isn't double-encoded.

  VISUAL PRIVACY (unchanged): this lives ONLY in the rep's own agent window,
  derived on-device from segment DURATIONS (no backend, no affective scoring).
  Nothing is shared to the other party or a manager.

  HARD RULES honored:
   - All chrome is component-scoped `.talk-*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque, self-framed (HR#2). Bone by default; the monologue
     tint uses `--sig` ("heads up"), never `--live` — it informs, never shames.
-->
<script lang="ts">
  import { onMount } from "svelte";

  let {
    /** Talk-share readout — you / them percent of total final speech. Null
     *  pre-first-final; a high You% is NEVER shamed with `--live`. */
    youPct = null,
    themPct = null,
    /** Duration (ms) of the current unbroken trailing You-run; drives the
     *  subtle monologue tint (the worded alert is the cue, not this tile). */
    youRunMs = 0,
    /** Monologue threshold — ~90s continuous per plan §5. */
    thresholdMs = 90_000,
  }: {
    youPct?: number | null;
    themPct?: number | null;
    youRunMs?: number;
    thresholdMs?: number;
  } = $props();

  // Show the meter once the call has produced measurable speech on either side.
  let hasData = $derived(
    youPct !== null && themPct !== null && youPct + themPct > 0,
  );
  // Subtle "you've had the floor a while" gauge state (visual only, no words).
  let mono = $derived(youRunMs >= thresholdMs);

  let reduceMotion = $state(false);
  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const handler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  });
</script>

{#if hasData}
  <div
    class="talk"
    class:mono
    role="img"
    aria-label={`Talk share: you ${youPct} percent, them ${themPct} percent${
      mono ? ", you have had the floor a while" : ""
    }.`}
  >
    <div class="talk-meter" aria-hidden="true">
      <span
        class="talk-seg talk-you"
        class:instant={reduceMotion}
        style={`width:${youPct}%`}
      ></span>
    </div>
    <div class="talk-read" aria-hidden="true">
      <span class="talk-read-you">You {youPct}%</span>
      <span class="talk-read-them">Them {themPct}%</span>
    </div>
  </div>
{/if}

<style>
  .talk {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  /* Two-segment bar: the You fill sits on a Them-toned track. */
  .talk-meter {
    position: relative;
    height: 4px;
    border-radius: 999px;
    background: var(--hairline-hi);
    overflow: hidden;
  }
  .talk-seg {
    display: block;
    height: 100%;
    border-radius: 999px;
    transition: width 260ms ease, background 200ms ease;
  }
  .talk-you {
    background: var(--accent);
  }
  .talk-seg.instant {
    transition: none;
  }
  /* Monologue gauge state — the You fill warms to the "heads up" hue. No
     pulse, no words: the worded nudge is the ephemeral cue, not this gauge. */
  .talk.mono .talk-you {
    background: var(--sig);
  }
  .talk-read {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.7rem;
  }
  .talk-read-you {
    color: var(--bone-2);
  }
  .talk.mono .talk-read-you {
    color: var(--sig);
  }
  .talk-read-them {
    color: var(--bone-3);
  }
</style>
