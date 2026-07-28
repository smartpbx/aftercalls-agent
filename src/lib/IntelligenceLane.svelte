<!--
  IntelligenceLane — co-pilot BI-revamp: the EPHEMERAL CUE SLOT.

  Reworked (BI-dashboard revamp) from the old scrollable sentiment + 6-card
  coaching list into a single, one-at-a-time cue with an auto-dismiss drain bar.
  The governing principle (competitor review): show only what a rep can act on in
  the next ~30 seconds — one prompt, not a wall. The reference-rail (deal/CRM,
  talk-ratio, checklist, subtle sentiment) and the transcript drawer now live in
  CoPilotPanel; this component owns ONLY the momentary cue.

  Cue sources + priority (higher tier PREEMPTS; equal/lower DROPS — no queue):
    1. battlecard  — objection / pricing / competitor (fast-lane `cues`,
       kind === "battlecard"). The old canned absence "risk" cues were retired
       backend-side (#662); their coverage moved into the checklist.
    2. monologue   — the rep's trailing You-run crossed threshold (talkMetric).
    3. coaching    — the top-ranked reflective coaching card (new `seq`).

  HARD RULES honored:
   - All chrome is component-scoped `.cue-*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque — no STT/LLM/infra provider name anywhere (HR#2).
   - Cue text is LLM- or canned-text → plain `{...}` interpolation ONLY, never
     `{@html}` (injection surface).
-->
<script lang="ts">
  import { onMount, onDestroy, untrack } from "svelte";
  import type {
    CoachingUpdate,
    CoachingCard,
    CoachingCardKind,
    LiveCue,
    LiveCueCategory,
  } from "@aftercalls/shared/types";
  import type { TalkMetric } from "$lib/stores/liveSession.svelte";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    coaching = null,
    status = "idle",
    cues = [],
    talkMetric = null,
  }: {
    /** Latest coaching snapshot; the top-ranked card seeds a tier-3 cue on each
     *  new `seq`. Null pre-first-update / cleared on new session. */
    coaching?: CoachingUpdate | null;
    /** Drives the live gate: cues only surface while the call is live. */
    status?: LiveStatus;
    /** Fast-lane cues; only `kind === "battlecard"` seeds a tier-1 cue (the
     *  retired risk cues are dropped). */
    cues?: LiveCue[];
    /** Trailing You-run drives the tier-2 monologue cue (fires once per run). */
    talkMetric?: TalkMetric | null;
  } = $props();

  // Priority tiers — LOWER number = HIGHER priority (preempts).
  const TIER_BATTLECARD = 1;
  const TIER_MONOLOGUE = 2;
  const TIER_COACHING = 3;

  // Auto-dismiss drain durations (ms). High-signal battlecards linger longest.
  const DUR_BATTLECARD = 16_000;
  const DUR_MONOLOGUE = 9_000;
  const DUR_COACHING = 12_000;

  // Match TalkTimeNudge / plan §5.
  const MONO_THRESHOLD = 90_000;
  const MONOLOGUE_COPY =
    "You've had the floor a while — a question could open things up.";

  const CUE_LABEL: Record<LiveCueCategory, string> = {
    objection: "OBJECTION",
    pricing: "PRICING",
    competitor: "COMPETITOR",
  };
  const KIND_LABEL: Record<CoachingCardKind, string> = {
    question: "QUESTION",
    talking_point: "TALKING POINT",
    objection: "OBJECTION",
    next_action: "NEXT STEP",
  };

  type CueVariant = "battlecard" | "monologue" | "coaching";
  type SlotCue = {
    tier: number;
    label: string;
    title: string;
    detail?: string;
    duration: number;
    variant: CueVariant;
  };

  // Stable high-priority-first pick (JS sort is stable → server front-loading
  // within a band is preserved).
  const rank = (p: string) => (p === "high" ? 0 : 1);
  function topCard(c: CoachingUpdate): CoachingCard | null {
    if (c.cards.length === 0) return null;
    return [...c.cards].sort((a, b) => rank(a.priority) - rank(b.priority))[0];
  }

  // ── Slot state ────────────────────────────────────────────────────────────
  let active = $state<SlotCue | null>(null);
  // Bumped on every fresh cue so the drain bar restarts its CSS animation.
  let drainKey = $state(0);
  let drainDuration = $state(0);
  let drainTimer = 0;
  let announce = $state("");

  // Plain (non-reactive) bookkeeping so writing it never re-triggers the effect.
  let lastBattlecardId: string | null = null;
  let lastCoachSeq: number | null = null;
  let monoFired = false;

  let reduceMotion = $state(false);
  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const handler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  });

  function show(c: SlotCue) {
    active = c;
    drainDuration = c.duration;
    drainKey += 1;
    announce = `${c.label}: ${c.title}`;
    clearTimeout(drainTimer);
    drainTimer = window.setTimeout(() => {
      active = null;
    }, c.duration);
  }

  // Preempt if strictly higher priority; equal/lower drops (no queue).
  function consider(c: SlotCue) {
    const cur = active;
    if (!cur || c.tier < cur.tier) show(c);
  }

  // One effect owns the whole state machine. It depends on the cue SOURCES
  // (cues / coaching.seq / talkMetric.youRunMs / status) and mutates the slot
  // untracked so it never loops on its own writes.
  $effect(() => {
    const cs = cues;
    const seq = coaching?.seq ?? null;
    const runMs = talkMetric?.youRunMs ?? 0;
    const live = status === "live";
    untrack(() => {
      // Session reset — coaching cleared (seq null) re-arms every source so the
      // NEW session's first frame (seq restarts low, ids are fresh) is never
      // mistaken for a repeat of the previous call's last cue.
      if (seq === null) {
        lastBattlecardId = null;
        lastCoachSeq = null;
        monoFired = false;
      }
      if (!live) {
        // Freeze/clear the slot when not live.
        if (active) active = null;
        clearTimeout(drainTimer);
        if (seq === null) announce = "";
        return;
      }
      // Tier 1 — newest battlecard (drop the retired risk cues).
      const bc = cs.filter((c) => c.kind === "battlecard");
      const newest = bc.length ? bc[bc.length - 1] : null;
      if (newest && newest.id !== lastBattlecardId) {
        lastBattlecardId = newest.id;
        consider({
          tier: TIER_BATTLECARD,
          label: CUE_LABEL[newest.category],
          title: newest.title,
          detail: newest.detail,
          duration: DUR_BATTLECARD,
          variant: "battlecard",
        });
      }
      // Tier 2 — monologue (once per run; re-arms when the run drops back).
      if (runMs >= MONO_THRESHOLD) {
        if (!monoFired) {
          monoFired = true;
          consider({
            tier: TIER_MONOLOGUE,
            label: "TALK TIME",
            title: MONOLOGUE_COPY,
            duration: DUR_MONOLOGUE,
            variant: "monologue",
          });
        }
      } else {
        monoFired = false;
      }
      // Tier 3 — top reflective coaching card on each fresh frame.
      if (seq !== null && seq !== lastCoachSeq) {
        lastCoachSeq = seq;
        const top = coaching ? topCard(coaching) : null;
        if (top) {
          consider({
            tier: TIER_COACHING,
            label: KIND_LABEL[top.kind],
            title: top.title,
            detail: top.detail,
            duration: DUR_COACHING,
            variant: "coaching",
          });
        }
      }
    });
  });

  onDestroy(() => clearTimeout(drainTimer));

  let isEnded = $derived(status === "ended");
  let isLive = $derived(status === "live");
  let isError = $derived(status === "error");
  // The cue slot is the primary always-visible surface now (the transcript's own
  // "Live unavailable" note is behind the collapsed drawer), so an errored live
  // pipeline must show a distinct, calm line HERE — the error text wins over the
  // generic idle copy. Plain + vendor-opaque; matches the transcript lane's tone.
  let idleText = $derived(
    isError
      ? "Live coaching unavailable."
      : isEnded
        ? "Call ended."
        : isLive
          ? "Listening…"
          : "Cues appear as you talk.",
  );
</script>

<div class="cue-slot" role="region" aria-label="Live cue">
  <!-- One polite delta region — announces only the active cue (de-duplicated). -->
  <div class="cue-sr" aria-live="polite">{announce}</div>

  {#if active}
    <div class="cue-card cue-{active.variant}">
      <span class="cue-label">{active.label}</span>
      <p class="cue-title">{active.title}</p>
      {#if active.detail}
        <p class="cue-detail">{active.detail}</p>
      {/if}
      <!-- Auto-dismiss drain bar — keyed so it restarts per cue; reduced-motion
           hides it (the timer still dismisses). -->
      {#if !reduceMotion}
        {#key drainKey}
          <span
            class="cue-drain"
            style={`animation-duration:${drainDuration}ms`}
            aria-hidden="true"
          ></span>
        {/key}
      {/if}
    </div>
  {:else}
    <div class="cue-idle">
      <span class="cue-idle-pip" class:blink={isLive && !reduceMotion}></span>
      <span class="cue-idle-text">{idleText}</span>
    </div>
  {/if}
</div>

<style>
  .cue-slot {
    display: flex;
    flex-direction: column;
    min-width: 0;
    padding: 0.7rem 0.75rem 0.8rem;
  }

  /* Visually-hidden live region (component-scoped — no app.css touch). */
  .cue-sr {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
  }

  /* ── The single cue card ──────────────────────────────────────────────
     A left-edge accent carries the variant; NO fill, NO pulse. Enter with a
     gentle fade+rise (reduced-motion → instant). */
  .cue-card {
    position: relative;
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--hairline-hi);
    border-radius: var(--radius-sm);
    padding: 0.6rem 0.75rem 0.7rem;
    overflow: hidden;
    animation: cue-in 160ms ease-out both;
  }
  @media (prefers-reduced-motion: reduce) {
    .cue-card {
      animation: none;
    }
  }
  @keyframes cue-in {
    from {
      opacity: 0;
      transform: translateY(4px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  /* Battlecard = "heads up, act now" → sig gold. Monologue = quiet self-nudge
     → bone. Coaching = ambient → hairline-hi. (Never --live: not destructive.) */
  .cue-battlecard {
    border-left-color: var(--sig);
  }
  .cue-monologue {
    border-left-color: var(--bone-3);
  }
  .cue-coaching {
    border-left-color: var(--hairline-hi);
  }

  .cue-label {
    display: block;
    font-size: 0.62rem;
    font-weight: 700;
    letter-spacing: 0.07em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .cue-battlecard .cue-label {
    color: var(--sig);
  }
  .cue-title {
    margin: 0.3rem 0 0;
    font-size: 0.9rem;
    font-weight: 600;
    line-height: 1.35;
    color: var(--bone-0);
    overflow-wrap: anywhere;
  }
  .cue-monologue .cue-title {
    font-weight: 500;
    color: var(--bone-1);
  }
  .cue-detail {
    margin: 0.3rem 0 0;
    font-size: 0.82rem;
    line-height: 1.4;
    color: var(--bone-1);
    overflow-wrap: anywhere;
  }

  /* Drain bar — a thin foot rule that shrinks to 0 over the cue's lifetime. */
  .cue-drain {
    position: absolute;
    left: 0;
    bottom: 0;
    height: 2px;
    width: 100%;
    transform-origin: left center;
    background: var(--hairline-hi);
    animation-name: cue-drain;
    animation-timing-function: linear;
    animation-fill-mode: forwards;
  }
  .cue-battlecard .cue-drain {
    background: var(--sig);
  }
  @keyframes cue-drain {
    from {
      transform: scaleX(1);
    }
    to {
      transform: scaleX(0);
    }
  }

  /* ── Idle surface — terse, calm, never a marketing line ───────────────── */
  .cue-idle {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.15rem;
  }
  .cue-idle-pip {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--bone-4);
    flex-shrink: 0;
  }
  .cue-idle-pip.blink {
    background: var(--accent);
    animation: cue-pip-pulse 1.4s ease-in-out infinite;
  }
  .cue-idle-text {
    font-size: 0.82rem;
    color: var(--bone-3);
  }
  @keyframes cue-pip-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 var(--accent-glow);
    }
    50% {
      box-shadow: 0 0 0 5px rgba(0, 0, 0, 0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .cue-idle-pip.blink {
      animation: none;
    }
  }
</style>
