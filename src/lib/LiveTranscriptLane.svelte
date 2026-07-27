<!--
  LiveTranscriptLane — #653 co-pilot transcript lane, extended for #660 P1.

  Started as a VERBATIM extract of LiveAssistPanel's transcript body (fold
  logic, labelFor, near-bottom autoscroll, role="log" / aria-live) on the
  byte-identical `.live-panel*` / `.live-stream` / `.live-seg*` classes from
  app.css. #660 adds FOUR co-pilot surfaces, all COMPONENT-SCOPED (this lane
  gains its first `<style>` block — HR#1 stays clean, the shared `.live-*`
  classes are reused read-only + only ever narrowed by higher-specificity
  scoped rules, never edited in app.css):
    1. an ask-chip row + single replace-in-place inline answer (ui.md §1);
    2. a per-turn star (highlight) affordance (plan item 4 / ui.md §5 boundary);
    3. pin-to-read + jump-to-live on the stream (ui.md §3);
    4. the self-only TalkTimeNudge mounted at the lane foot (ui.md §4).

  All new copy is vendor-opaque (HR#2); the ask `answer` is LLM-derived and
  rendered as PLAIN TEXT ONLY (never `{@html}` — injection surface).

  LiveAssistPanel.svelte still ships the copilot-OFF path (plain Phase-1 panel);
  this lane is the copilot-ON composition inside CoPilotPanel.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import TalkTimeNudge from "$lib/TalkTimeNudge.svelte";
  import type { LiveSegment, AskChip } from "@aftercalls/shared/types";
  import type {
    AskAnswerState,
    TalkMetric,
  } from "$lib/stores/liveSession.svelte";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    segments = [],
    status = "idle",
    // #660 — co-pilot P1 wiring, threaded from CoPilotPanel / +page. All
    // optional so the plain (non-copilot) composition still type-checks; the
    // ask/star surfaces only render when their callbacks are provided.
    sessionUuid = null,
    askAnswer = null,
    askInFlight = null,
    highlighted = new Set<string>(),
    talkMetric = null,
    onask = undefined,
    ondismissAsk = undefined,
    onhighlight = undefined,
  }: {
    segments?: LiveSegment[];
    status?: LiveStatus;
    sessionUuid?: string | null;
    askAnswer?: AskAnswerState | null;
    askInFlight?: AskChip | null;
    highlighted?: Set<string>;
    talkMetric?: TalkMetric | null;
    onask?: (chip: AskChip) => void;
    ondismissAsk?: () => void;
    onhighlight?: (seg: LiveSegment, starred: boolean) => void;
  } = $props();

  // Fold the raw ordered stream into a stable display list: finals in
  // arrival order, plus the current in-progress provisional per channel
  // appended at the tail. A provisional is replaced by later partials for
  // the same channel and cleared when that channel's final lands.
  let display = $derived.by(() => {
    const finals: LiveSegment[] = [];
    const prov: Record<string, LiveSegment> = {};
    for (const s of segments) {
      if (s.provisional) {
        prov[s.channel] = s;
      } else {
        finals.push(s);
        delete prov[s.channel];
      }
    }
    return [...finals, ...Object.values(prov)];
  });

  // Count of finals only — drives the pinned "new lines behind you" badge
  // (provisionals are drafts, not new lines).
  let finalCount = $derived(segments.reduce((n, s) => n + (s.provisional ? 0 : 1), 0));

  function labelFor(s: LiveSegment): string {
    if (s.speaker) return s.speaker;
    return s.channel === "mic" ? "You" : "Them";
  }

  // ── Ask-chip row (ui.md §1) ─────────────────────────────────────────────
  const ASK_CHIPS: { chip: AskChip; label: string }[] = [
    { chip: "catch_me_up", label: "Catch me up" },
    { chip: "summarize", label: "Summarize so far" },
    { chip: "what_did_they_ask", label: "What did they just ask?" },
    { chip: "action_items", label: "Action items so far" },
  ];
  const ASK_MICRO: Record<AskChip, string> = {
    catch_me_up: "CAUGHT UP",
    summarize: "SUMMARY SO FAR",
    what_did_they_ask: "WHAT THEY ASKED",
    action_items: "ACTION ITEMS",
  };

  // The ask surface is only meaningful with a live session to address and a
  // handler wired. Enabled once there's something to ask about (a live/ended
  // session with a uuid); disabled pre-recording / idle.
  let askReady = $derived(
    !!onask && !!sessionUuid && status !== "idle" && status !== "error",
  );
  let askBusyAny = $derived(askInFlight !== null);

  function fireAsk(chip: AskChip) {
    if (!askReady || askBusyAny) return; // single in-flight + client debounce
    onask?.(chip);
  }

  function onAnswerKeydown(e: KeyboardEvent) {
    // Esc while focus is inside the answer clears the slot (ui.md §1c).
    if (e.key === "Escape") {
      e.stopPropagation();
      ondismissAsk?.();
    }
  }

  // ── Star / highlight (plan item 4, ui.md §5 boundary) ───────────────────
  function segKey(s: LiveSegment): string {
    return s.channel + ":" + s.start_ms;
  }
  function toggleStar(seg: LiveSegment) {
    if (!onhighlight) return;
    onhighlight(seg, !highlighted.has(segKey(seg)));
  }

  // ── Pin-to-read + jump-to-live (ui.md §3) ───────────────────────────────
  let streamEl: HTMLDivElement | undefined = $state();
  let pinned = $state(false);
  let newSincePinned = $state(0);
  // Suppress re-pinning while a programmatic jump-to-bottom is animating —
  // a smooth scroll fires intermediate `scroll` events where distFromBottom
  // is still > threshold, which would otherwise re-pin mid-animation.
  let jumping = $state(false);
  const PIN_THRESHOLD = 60; // px from bottom — reuse the existing hysteresis.

  let reduceMotion = $state(false);
  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const handler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  });

  function onStreamScroll() {
    const el = streamEl;
    if (!el) return;
    const distFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (distFromBottom < PIN_THRESHOLD) {
      // Returned to the bottom → resume follow, clear the backlog badge, end
      // any in-flight programmatic jump.
      jumping = false;
      if (pinned) {
        pinned = false;
        newSincePinned = 0;
      }
    } else if (!pinned && !jumping) {
      // Scrolled up past the hysteresis → stop auto-follow (but not from the
      // intermediate frames of a smooth jump-to-bottom).
      pinned = true;
    }
  }

  // Auto-follow the newest line while UNPINNED; while pinned, count arriving
  // finals into the jump-to-live badge instead of yanking scroll. Reads both
  // `display.length` (re-run on every stream tick incl. partials) and
  // `finalCount` (the badge signal).
  let prevFinal = 0;
  $effect(() => {
    const total = display.length;
    const fc = finalCount;
    const el = streamEl;
    if (!el) {
      prevFinal = fc;
      return;
    }
    if (!pinned) {
      el.scrollTop = el.scrollHeight;
    } else if (fc > prevFinal) {
      newSincePinned += fc - prevFinal;
    }
    prevFinal = fc;
    void total;
  });

  function jumpToLive() {
    const el = streamEl;
    if (!el) return;
    jumping = true;
    el.scrollTo({
      top: el.scrollHeight,
      behavior: reduceMotion ? "auto" : "smooth",
    });
    pinned = false;
    newSincePinned = 0;
    el.focus();
  }

  let jumpActive = $derived(pinned && newSincePinned > 0);
  let jumpMuted = $derived(pinned && newSincePinned === 0);
  let jumpCount = $derived(newSincePinned > 9 ? "9+" : String(newSincePinned));
</script>

<section
  class="live-panel live-status-{status}"
  aria-label="Live transcript (draft)"
>
  <div class="live-panel-head">
    <span class="live-status-pip" aria-hidden="true"></span>
    <span class="live-panel-title">Live transcript</span>
    {#if status === "error"}
      <span class="live-status-note">Live unavailable</span>
    {:else if status === "ended"}
      <span class="live-status-note">Draft</span>
    {:else}
      <span class="live-status-note">Draft — finalized after the call</span>
    {/if}
  </div>

  <!-- Ask-chip row — momentary presets, sits above the stream and never
       overlays it (ui.md §1a). Present whenever the ask handler is wired and
       the surface isn't the degraded error state. -->
  {#if onask && status !== "error"}
    <div class="live-ask-row" role="group" aria-label="Ask about this call">
      {#each ASK_CHIPS as { chip, label } (chip)}
        <button
          type="button"
          class="live-ask-chip"
          class:busy={askInFlight === chip}
          class:reduce={reduceMotion}
          disabled={!askReady || askBusyAny}
          aria-busy={askInFlight === chip}
          title={askBusyAny ? "Give it a moment…" : undefined}
          onclick={() => fireAsk(chip)}
        >
          {#if askInFlight === chip}
            <span class="live-ask-pip" aria-hidden="true"></span>
          {/if}
          {label}
        </button>
      {/each}
    </div>
  {/if}

  <!-- Inline answer slot — single, replace-in-place, plain text (ui.md §1c).
       Live region so the fresh answer is announced once; focus stays on the
       chip (the polite announcement carries it). -->
  {#if askAnswer}
    <div
      class="live-ask-answer"
      aria-live="polite"
      aria-atomic="true"
      onkeydowncapture={onAnswerKeydown}
      role="group"
      aria-label="Ask answer"
      tabindex="-1"
    >
      <div class="live-ask-answer-head">
        <span class="live-ask-answer-kind">{ASK_MICRO[askAnswer.chip]}</span>
        {#if askAnswer.basedOnTurns !== null && askAnswer.basedOnTurns > 0}
          <span class="live-ask-answer-turns"
            >· {askAnswer.basedOnTurns} turns</span
          >
        {/if}
        <button
          type="button"
          class="live-ask-answer-dismiss"
          aria-label="Dismiss answer"
          onclick={() => ondismissAsk?.()}
        >
          <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
        </button>
      </div>
      <!-- tabindex so keyboard users can scroll a long answer in place
           (ui.md §1c) — the body is the only scrollable region here. -->
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <p class="live-ask-answer-body" tabindex="0">{askAnswer.answer}</p>
    </div>
  {/if}

  {#if status === "error"}
    <p class="live-empty">
      The live draft isn't available right now. Your recording is
      unaffected — the full transcript is prepared after the call.
    </p>
  {:else if display.length === 0}
    <p class="live-empty">
      {status === "live" ? "Listening…" : "Waiting for audio…"}
    </p>
  {:else}
    <div class="live-stream-wrap">
      <div
        class="live-stream"
        bind:this={streamEl}
        role="log"
        aria-live="polite"
        tabindex="-1"
        onscroll={onStreamScroll}
      >
        {#each display as seg (seg.channel + ":" + seg.start_ms + ":" + (seg.provisional ? "p" : "f"))}
          {@const starred = highlighted.has(segKey(seg))}
          <div
            class="live-seg live-seg-{seg.channel === 'mic' ? 'you' : 'them'}"
            class:live-seg-provisional={seg.provisional}
          >
            <span class="live-seg-label">{labelFor(seg)}</span>
            <span class="live-seg-text">{seg.text}</span>
            {#if onhighlight && !seg.provisional}
              <button
                type="button"
                class="live-seg-star"
                class:starred
                aria-pressed={starred}
                aria-label={starred ? "Unstar this moment" : "Star this moment"}
                title={starred ? "Starred — click to remove" : "Star this moment"}
                onclick={() => toggleStar(seg)}
              >
                <svg
                  viewBox="0 0 24 24"
                  width="14"
                  height="14"
                  fill={starred ? "currentColor" : "none"}
                  stroke="currentColor"
                  stroke-width="2"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  ><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg
                >
              </button>
            {/if}
          </div>
        {/each}
      </div>

      {#if jumpActive || jumpMuted}
        <button
          type="button"
          class="jump-live"
          class:muted={jumpMuted}
          class:reduce={reduceMotion}
          aria-label={jumpActive
            ? `Jump to live transcript, ${newSincePinned} new lines`
            : "Following paused — jump to live transcript"}
          onclick={jumpToLive}
        >
          {#if jumpActive}
            <span class="jump-live-count" aria-hidden="true">{jumpCount}</span>
            <span class="jump-live-chevron" aria-hidden="true">↓</span>
            <span>Jump to live</span>
          {:else}
            <span>Following paused</span>
          {/if}
        </button>
      {/if}
    </div>
  {/if}

  <!-- Self-only talk-time nudge — pinned at the lane foot, ambient (ui.md §4).
       Renders null until the trailing You-run passes threshold. -->
  {#if talkMetric && status === "live"}
    <TalkTimeNudge
      youRunMs={talkMetric.youRunMs}
      youPct={talkMetric.youPct}
      themPct={talkMetric.themPct}
    />
  {/if}
</section>

<style>
  /* ── Ask-chip row (ui.md §1a/§1b) ─────────────────────────────────────
     The `.scope-pill` / `.lane-tab` idiom, but momentary — no `.active`
     state; asks fire and settle. */
  .live-ask-row {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
  }
  .live-ask-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.3rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .live-ask-chip:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .live-ask-chip:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .live-ask-chip:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  /* Busy chip stays labelled; a small leading accent pulse dot leads it
     (reuse the cp-pip-pulse glow recipe). */
  .live-ask-pip {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
    flex-shrink: 0;
    animation: live-ask-pulse 1.4s ease-in-out infinite;
  }
  .live-ask-chip.reduce .live-ask-pip {
    animation: none;
  }
  @media (prefers-reduced-motion: reduce) {
    .live-ask-pip {
      animation: none;
    }
  }
  @keyframes live-ask-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 var(--accent-glow);
    }
    50% {
      box-shadow: 0 0 0 4px rgba(0, 0, 0, 0);
    }
  }

  /* ── Inline answer slot (ui.md §1c) ───────────────────────────────────
     A soft inset card; the fresh answer cross-fades in (≤120ms). */
  .live-ask-answer {
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    padding: 0.55rem 0.7rem;
    animation: live-ask-answer-in 120ms ease-out both;
  }
  .live-ask-answer:focus-visible {
    outline: none;
  }
  @media (prefers-reduced-motion: reduce) {
    .live-ask-answer {
      animation: none;
    }
  }
  @keyframes live-ask-answer-in {
    from {
      opacity: 0;
    }
    to {
      opacity: 1;
    }
  }
  .live-ask-answer-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .live-ask-answer-kind {
    font-size: 0.64rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .live-ask-answer-turns {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--bone-3);
  }
  .live-ask-answer-dismiss {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    margin-left: auto;
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
  .live-ask-answer-dismiss:hover {
    background: var(--live-soft);
    color: var(--live);
  }
  .live-ask-answer-dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .live-ask-answer-body {
    margin: 0.3rem 0 0;
    font-size: 0.85rem;
    line-height: 1.45;
    color: var(--bone-1);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
    max-height: min(40vh, 14rem);
    overflow-y: auto;
  }
  .live-ask-answer-body:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }

  /* ── Stream wrapper + jump-to-live (ui.md §3) ─────────────────────────
     Positioned so the corner-pinned button anchors inside the stream. */
  .live-stream-wrap {
    position: relative;
  }
  .live-stream:focus-visible {
    outline: none;
  }

  .jump-live {
    position: absolute;
    right: 0.5rem;
    bottom: 0.5rem;
    z-index: 2;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.25rem 0.6rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 999px;
    background: var(--ink-2);
    color: var(--accent-hi);
    font: inherit;
    font-size: 0.72rem;
    cursor: pointer;
    box-shadow: 0 6px 18px -8px rgba(0, 0, 0, 0.45);
    animation: jump-live-in 120ms ease-out both;
  }
  .jump-live.reduce {
    animation: none;
  }
  @media (prefers-reduced-motion: reduce) {
    .jump-live {
      animation: none;
    }
  }
  @keyframes jump-live-in {
    from {
      opacity: 0;
      transform: translateY(4px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  /* Muted "follow paused" variant — bone, no accent, no count. */
  .jump-live.muted {
    color: var(--bone-3);
  }
  .jump-live:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .jump-live-count {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 1.1rem;
    padding: 0 0.25rem;
    border-radius: 999px;
    background: var(--accent-soft);
    color: var(--accent-hi);
    font-family: var(--font-mono);
    font-size: 0.66rem;
    line-height: 1.3;
  }
  .jump-live-chevron {
    font-size: 0.8rem;
    line-height: 1;
  }

  /* ── Per-turn star (plan item 4 / ui.md §5) ───────────────────────────
     Narrow the shared `.live-seg` grid to a third auto column for the star
     (COMPONENT-SCOPED — higher specificity than app.css, applied only in
     this lane; app.css is untouched). Provisional rows render no star, so
     their third column collapses to 0. */
  .live-seg {
    grid-template-columns: 3.2rem 1fr auto;
  }
  .live-seg-star {
    align-self: start;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 1.4rem;
    height: 1.4rem;
    padding: 0;
    border: none;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--bone-3);
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s, color 0.15s, background 0.15s;
  }
  /* Hover/focus-within reveal; always visible once starred (mirrors the
     `.ai-del` hover-reveal opacity rule, ui.md §5 boundary). */
  .live-seg:hover .live-seg-star,
  .live-seg:focus-within .live-seg-star,
  .live-seg-star:focus-visible,
  .live-seg-star.starred {
    opacity: 1;
  }
  .live-seg-star:hover {
    background: var(--hairline);
    color: var(--bone-1);
  }
  .live-seg-star.starred {
    color: var(--sig);
  }
  .live-seg-star:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  /* Touch / coarse pointers can't hover — keep the star visible so it's
     reachable without a hover state. */
  @media (hover: none) {
    .live-seg-star {
      opacity: 0.7;
    }
  }
</style>
