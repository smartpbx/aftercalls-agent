<!--
  IntelligenceLane — #654 live coaching surface (Coaching lane).

  Grows the #653 honest placeholder into a live, deal-context-aware coaching
  stream fed by `{"type":"coaching"}` WS frames (backend live_coach). Renders a
  glanceable sentiment indicator + a keyed card list (question / talking point /
  objection / next step). Every frame is a FULL snapshot; the list is a keyed
  RECONCILE (stable key = kind + normalized title) so persistent cards keep
  their node and only new/gone/moved cards animate — no flash, never
  auto-scroll. See `.orchestrator/plans/issue-654/ui.md`.

  HARD RULES honored:
   - All chrome is component-scoped `.coach-*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque — no STT/LLM/infra provider name anywhere (HR#2).
   - Card text is LLM-derived → plain `{...}` interpolation ONLY, never
     `{@html}` (injection surface).
-->
<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { flip } from "svelte/animate";
  import LiveChecklist from "$lib/LiveChecklist.svelte";
  import type {
    CoachingUpdate,
    CoachingCard,
    CoachingCardKind,
    CoachingSentimentLabel,
    LiveCue,
    LiveCueCategory,
    ChecklistSnapshot,
    CopilotMode,
    KnowledgeAnswer,
  } from "@aftercalls/shared/types";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    coaching = null,
    status = "idle",
    cues = [],
    checklist = null,
    mode = "sales",
    knowledgeAnswer = null,
    knowledgeInFlight = false,
    onknowledge = undefined,
    ondismissKnowledge = undefined,
  }: {
    /** Latest snapshot; null pre-first-update (and cleared on new session). */
    coaching?: CoachingUpdate | null;
    /** Drives framing (idle/generating vs ended/error freeze). */
    status?: LiveStatus;
    /** #659 (P2) — fast-lane cues (battlecards + deal-risk), rendered in a
     *  flagged, auto-expiring section ABOVE the reflective coaching. Keyed by
     *  `id`; the store owns TTL pruning + the newest-N cap. Sales mode only —
     *  Support mode swaps this section for the cited-knowledge surface. */
    cues?: LiveCue[];
    /** #659 (P3) — auto-checking agenda checklist, pinned at the very TOP of
     *  the lane (above the fast cues + reflective cards). Full snapshot per
     *  frame; null pre-first-frame / cleared on new session. */
    checklist?: ChecklistSnapshot | null;
    /** #659 P5b — the call's active persona. `"support"` swaps the Sales
     *  battlecard section for the cited-knowledge surface below. */
    mode?: CopilotMode;
    /** #659 P5b — the latest Support-mode cited knowledge answer (null until
     *  the rep taps "get an answer"; cleared on new session / dismiss). */
    knowledgeAnswer?: KnowledgeAnswer | null;
    /** #659 P5b — true while a knowledge answer is generating. */
    knowledgeInFlight?: boolean;
    /** #659 P5b — fire one knowledge answer / dismiss the slot. */
    onknowledge?: () => void;
    ondismissKnowledge?: () => void;
  } = $props();

  // #659 P5b — Support mode shows the cited-knowledge surface in place of the
  // Sales battlecards; everything else (checklist, reflective coaching) is
  // persona-agnostic and renders in both.
  let isSupport = $derived(mode === "support");

  // ── #659 (P2) — fast-cue presentation ────────────────────────────────────
  const CUE_LABEL: Record<LiveCueCategory, string> = {
    objection: "OBJECTION",
    pricing: "PRICING",
    competitor: "COMPETITOR",
    pricing_gap: "HEADS UP",
    next_step_gap: "HEADS UP",
  };

  // ── Motion gate (Svelte JS transitions can't be disabled by CSS alone) ──
  let reduceMotion = $state(false);
  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const handler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  });

  // ── Card kind presentation ──────────────────────────────────────────────
  const KIND_LABEL: Record<CoachingCardKind, string> = {
    question: "QUESTION",
    talking_point: "TALKING POINT",
    objection: "OBJECTION",
    next_action: "NEXT STEP",
  };

  const SENTIMENT_WORD: Record<CoachingSentimentLabel, string> = {
    positive: "Positive",
    neutral: "Neutral",
    negative: "Negative",
    mixed: "Mixed",
  };

  // Stable identity key: kind + normalized(title). Persistent cards keep their
  // DOM node across frames; only detail/priority update in place.
  function keyOf(c: CoachingCard): string {
    return c.kind + " " + c.title.toLowerCase().replace(/\s+/g, " ").trim();
  }

  // Deterministic STABLE high-priority-first order. JS sort is stable, so the
  // within-band order the server front-loaded is preserved frame-to-frame (no
  // churn). We never re-rank per frame beyond this.
  const rank = (p: string) => (p === "high" ? 0 : 1);
  let orderedCards = $derived<CoachingCard[]>(
    coaching ? [...coaching.cards].sort((a, b) => rank(a.priority) - rank(b.priority)) : [],
  );

  let hasCards = $derived(!!coaching && coaching.cards.length > 0);
  let isEnded = $derived(status === "ended");
  let isError = $derived(status === "error");
  let isLive = $derived(status === "live");

  // ── #660 — glanceable-expand disclosure (ui.md §2) ───────────────────────
  // Cards default COLLAPSED (glyph + kind + one-line title); objection and
  // high-priority cards default EXPANDED (the response / detail IS the payload;
  // a collapsed objection is a latency regression). User toggles persist across
  // the 20s reconcile via an explicit override map keyed by `keyOf`, so the
  // reconcile never re-forces a default on a card the user opened or closed.
  function defaultExpanded(c: CoachingCard): boolean {
    return c.kind === "objection" || c.priority === "high";
  }
  let expandOverride = $state<Map<string, boolean>>(new Map());
  function isExpanded(key: string, c: CoachingCard): boolean {
    const o = expandOverride.get(key);
    return o ?? defaultExpanded(c);
  }
  function toggleCard(key: string, c: CoachingCard) {
    const next = new Map(expandOverride);
    next.set(key, !isExpanded(key, c));
    expandOverride = next;
  }
  // Prune override entries whose key left the snapshot (bounded memory); read
  // the live keys reactively, mutate the map untracked so this never loops.
  $effect(() => {
    const liveKeys = new Set(orderedCards.map(keyOf));
    untrack(() => {
      let changed = false;
      const next = new Map(expandOverride);
      for (const k of next.keys()) {
        if (!liveKeys.has(k)) {
          next.delete(k);
          changed = true;
        }
      }
      if (changed) expandOverride = next;
    });
  });

  // Idle-surface copy (idle / generating / no-snapshot freeze all share it).
  let idlePrimary = $derived(
    isEnded ? "No coaching for this call." : "Coaching will appear as you talk.",
  );

  // ── Delta-only screen-reader announcements (§7) ──────────────────────────
  // Plain (non-reactive) bookkeeping so writing them can't re-trigger the
  // effect. Only `srMessage` is reactive (drives the live region).
  let srMessage = $state("");
  let prevSeq: number | null = null;
  let prevKeys = new Set<string>();
  let prevLabel: CoachingSentimentLabel | null = null;

  $effect(() => {
    if (!coaching) {
      // New session / cleared — reset so the next activation re-announces.
      prevSeq = null;
      prevKeys = new Set();
      prevLabel = null;
      srMessage = "";
      return;
    }
    if (coaching.seq === prevSeq) return; // no new frame

    const keys = new Set(coaching.cards.map(keyOf));
    const parts: string[] = [];

    if (prevSeq === null) {
      const n = coaching.cards.length;
      parts.push(`Live coaching active. ${n} suggestion${n === 1 ? "" : "s"}.`);
    } else {
      let added = 0;
      for (const k of keys) if (!prevKeys.has(k)) added++;
      if (added > 0) {
        parts.push(`${added} new coaching suggestion${added === 1 ? "" : "s"}.`);
      }
    }
    if (coaching.sentiment.label !== prevLabel && prevLabel !== null) {
      parts.push(`Sentiment: ${coaching.sentiment.label}.`);
    }

    if (parts.length > 0) srMessage = parts.join(" ");

    prevSeq = coaching.seq;
    prevKeys = keys;
    prevLabel = coaching.sentiment.label;
  });

  // ── #659 (P2) — fast-cue delta announcements (only new cues, not the list) ─
  let cueSr = $state("");
  let prevCueIds = new Set<string>();
  $effect(() => {
    const ids = new Set(cues.map((c) => c.id));
    untrack(() => {
      const fresh = cues.filter((c) => !prevCueIds.has(c.id));
      if (fresh.length > 0) {
        const c = fresh[fresh.length - 1];
        cueSr = `${CUE_LABEL[c.category]}: ${c.title}`;
      }
      prevCueIds = ids;
    });
  });

  // ── Card enter/exit: height-expand + fade (reduced-motion → instant) ─────
  function cardMotion(node: HTMLElement, { duration = 180 }: { duration?: number } = {}) {
    const h = node.offsetHeight;
    const s = getComputedStyle(node);
    const pt = parseFloat(s.paddingTop) || 0;
    const pb = parseFloat(s.paddingBottom) || 0;
    const mb = parseFloat(s.marginBottom) || 0;
    return {
      duration: reduceMotion ? 0 : duration,
      css: (t: number) =>
        `overflow:hidden;opacity:${t};height:${t * h}px;` +
        `padding-top:${t * pt}px;padding-bottom:${t * pb}px;margin-bottom:${t * mb}px;`,
    };
  }
</script>

<!-- Kind glyph — shared by the toggle header (expandable cards) and the
     static header (detail-less cards) so the SVG switch lives in one place. -->
{#snippet cardGlyph(kind: CoachingCardKind)}
  <span class="coach-glyph" aria-hidden="true">
    {#if kind === "question"}
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"/><path d="M9.5 9a2.5 2.5 0 0 1 4.9.6c0 1.7-2.4 2.4-2.4 2.4" stroke-width="1.6"/><line x1="12" y1="15.5" x2="12" y2="15.5" stroke-width="2"/></svg>
    {:else if kind === "talking_point"}
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 18h6"/><path d="M10 21h4"/><path d="M12 3a6 6 0 0 0-4 10.5c.5.5 1 1.5 1 2.5h6c0-1 .5-2 1-2.5A6 6 0 0 0 12 3z"/></svg>
    {:else if kind === "objection"}
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>
    {:else}
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="5" y1="12" x2="19" y2="12"/><polyline points="12 5 19 12 12 19"/></svg>
    {/if}
  </span>
{/snippet}

<div class="coach" role="region" aria-label="Live coaching">
  <!-- Delta-only announcements; the card list itself is NOT a live region. -->
  <div class="coach-sr" aria-live="polite">{srMessage}</div>
  <div class="coach-sr" aria-live="polite">{cueSr}</div>

  <!-- #659 (P3) — auto-checking agenda checklist, pinned at the very TOP:
       checklist → fast cues → reflective cards keeps the two-speed model +
       ambient coverage legible. Self-hides until the first snapshot lands. -->
  <LiveChecklist {checklist} {status} />

  <!-- #659 P5b — Support-mode cited-knowledge surface, in place of the Sales
       battlecards. A manual "get an answer" affordance (MVP: no auto-fire);
       the backend grounds a cited answer on the org's knowledge base. Answer +
       citation titles are org-authored/LLM text → plain `{...}` only, never
       `{@html}`. Vendor-opaque copy. -->
  {#if isSupport}
    <div class="kb" role="group" aria-label="Knowledge answers">
      <div class="kb-head">
        <span class="kb-flag" aria-hidden="true">KB</span>
        <span class="kb-label">Knowledge</span>
        <button
          type="button"
          class="kb-ask"
          onclick={() => onknowledge?.()}
          disabled={knowledgeInFlight}
        >
          {knowledgeInFlight ? "Searching…" : "Get an answer"}
        </button>
      </div>
      {#if knowledgeAnswer}
        <div class="kb-answer">
          <button
            type="button"
            class="kb-dismiss"
            aria-label="Dismiss answer"
            onclick={() => ondismissKnowledge?.()}
          >×</button>
          <p class="kb-answer-text" aria-live="polite">{knowledgeAnswer.answer}</p>
          {#if knowledgeAnswer.sources.length > 0}
            <div class="kb-sources">
              <span class="kb-sources-label">SOURCES</span>
              {#each knowledgeAnswer.sources as s (s.id)}
                <span class="kb-source">{s.title}</span>
              {/each}
            </div>
          {/if}
        </div>
      {:else if !knowledgeInFlight}
        <p class="kb-empty">
          Pull a grounded answer from your team's knowledge base — cited to the
          source.
        </p>
      {/if}
    </div>
  {/if}

  <!-- #659 (P2) — FAST-lane cues: flagged, auto-expiring, keyed by id, ABOVE
       the reflective sentiment+cards so the two-speed model is legible. Plain
       `{...}` interpolation only (LLM/canned text → never `{@html}`). Sales
       mode only — Support swaps this for the knowledge surface above. -->
  {#if !isSupport && cues.length > 0}
    <div class="cue-lane" role="group" aria-label="Live cues">
      {#each cues as cue (cue.id)}
        <div
          class="cue-card"
          class:risk={cue.kind === "risk"}
          in:cardMotion={{ duration: 180 }}
          out:cardMotion={{ duration: 150 }}
          animate:flip={{ duration: reduceMotion ? 0 : 200 }}
        >
          <div class="cue-head">
            <span class="cue-flag" aria-hidden="true">LIVE</span>
            <span class="cue-kind">{CUE_LABEL[cue.category]}</span>
          </div>
          <p class="cue-title">{cue.title}</p>
          {#if cue.detail}
            <p class="cue-detail">{cue.detail}</p>
          {/if}
        </div>
      {/each}
    </div>
  {/if}

  {#if coaching}
    <!-- Sentiment pill — pinned; never participates in the card FLIP. -->
    <div class="coach-head">
      <span
        class="coach-sent-pip"
        class:pos={coaching.sentiment.label === "positive" && !isEnded}
        class:neg={coaching.sentiment.label === "negative" && !isEnded}
        class:done={isEnded}
        aria-hidden="true"
      ></span>
      <span class="coach-sent-label">{SENTIMENT_WORD[coaching.sentiment.label]}</span>
      {#if coaching.sentiment.note}
        <span class="coach-sent-sep" aria-hidden="true">·</span>
        <span class="coach-sent-note">{coaching.sentiment.note}</span>
      {/if}
      {#if isEnded}
        <span class="coach-note">Call ended</span>
      {:else if isError}
        <span class="coach-note">Paused</span>
      {/if}
    </div>

    {#if orderedCards.length > 0}
      <!-- Keyed reconcile; tabindex so keyboard users can scroll the list
           (ui.md §7 — MVP cards carry no interactive controls, so the
           container itself is the only tab stop). Never auto-scrolls — the
           user owns scroll position. -->
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <div class="coach-cards" tabindex="0">
        {#each orderedCards as c (keyOf(c))}
          {@const key = keyOf(c)}
          {@const expanded = isExpanded(key, c)}
          {@const hasBody = !!c.detail}
          <div
            class="coach-card"
            class:high={c.priority === "high"}
            in:cardMotion={{ duration: 180 }}
            out:cardMotion={{ duration: 150 }}
            animate:flip={{ duration: reduceMotion ? 0 : 200 }}
          >
            {#if hasBody}
              <!-- Whole header is the disclosure toggle (ui.md §2b): big
                   glanceable target, chevron is the state cue. Native button →
                   Enter/Space toggle for free. -->
              <button
                type="button"
                class="coach-card-toggle"
                aria-expanded={expanded}
                aria-controls={`coach-col-${key}`}
                onclick={() => toggleCard(key, c)}
              >
                <span class="coach-card-tophead">
                  {@render cardGlyph(c.kind)}
                  <span class="coach-kind">{KIND_LABEL[c.kind]}</span>
                  <svg
                    class="coach-chevron"
                    class:open={expanded}
                    viewBox="0 0 24 24"
                    width="12"
                    height="12"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2.2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    aria-hidden="true"><polyline points="9 6 15 12 9 18" /></svg
                  >
                </span>
                <span class="coach-title" class:clamped={!expanded}>{c.title}</span>
              </button>
            {:else}
              <!-- No detail and no objection suggested-response → nothing to
                   expand: static (non-interactive) header, no chevron, no
                   toggle. The title shows in full (never clamped). -->
              <div class="coach-card-head">
                <span class="coach-card-tophead">
                  {@render cardGlyph(c.kind)}
                  <span class="coach-kind">{KIND_LABEL[c.kind]}</span>
                </span>
                <span class="coach-title">{c.title}</span>
              </div>
            {/if}

            <!-- Collapsible body — CSS-only height reveal (grid 0fr→1fr) so it
                 never fights `animate:flip`. Kept in the DOM when collapsed
                 (SR + reconcile stability) — only visually hidden. -->
            {#if hasBody}
              <div
                id="coach-col-{key}"
                class="coach-collapsible"
                class:open={expanded}
              >
                <div class="coach-collapsible-inner">
                  {#if c.kind === "objection"}
                    <div class="coach-response">
                      <span class="coach-response-label">SUGGESTED RESPONSE</span>
                      <p class="coach-response-text">{c.detail}</p>
                    </div>
                  {:else}
                    <p class="coach-detail">{c.detail}</p>
                  {/if}
                </div>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {:else}
      <p class="coach-empty">No suggestions right now — the conversation's on track.</p>
    {/if}
  {:else}
    <!-- No snapshot: idle / generating / degrade-before-first-card / ended-with
         -nothing all share ONE calm surface (a failure is indistinguishable
         from "hasn't generated yet"). Never a red panel, never a hung spinner. -->
    <div class="coach-idle">
      <span class="coach-idle-pip" class:blink={isLive && !reduceMotion}></span>
      <p class="coach-idle-primary">{idlePrimary}</p>
      {#if isLive}
        <p class="coach-idle-sub">
          Talking points and reminders show up here as the call goes.
        </p>
      {/if}
    </div>
  {/if}
</div>

<style>
  .coach {
    display: flex;
    flex-direction: column;
    padding: 0.7rem 0.75rem 0.8rem;
  }

  /* Visually-hidden live region (component-scoped so no app.css touch). */
  .coach-sr {
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

  /* ── #659 (P2) — fast-cue section (flagged, auto-expiring; ABOVE coaching) ─
     Reuses the card chrome but with a distinct signal accent (--sig, the
     "in-flight / attention" token) + a LIVE flag so the two-speed model reads
     as two different things at a glance. */
  .cue-lane {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    margin-bottom: 0.7rem;
  }
  .cue-card {
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--sig);
    border-radius: var(--radius-sm);
    padding: 0.55rem 0.7rem;
  }
  /* Deal-risk cues are quieter than battlecards — a heads-up, not a moment. */
  .cue-card.risk {
    border-left-color: var(--bone-3);
    background: transparent;
  }
  .cue-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-bottom: 0.3rem;
  }
  .cue-flag {
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--sig);
    border: 1px solid var(--sig);
    border-radius: 3px;
    padding: 0.02rem 0.28rem;
    line-height: 1.4;
  }
  .cue-card.risk .cue-flag {
    color: var(--bone-3);
    border-color: var(--hairline-hi);
  }
  .cue-kind {
    font-size: 0.66rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
    white-space: nowrap;
  }
  .cue-title {
    margin: 0;
    font-size: 0.85rem;
    font-weight: 600;
    line-height: 1.35;
    color: var(--bone-0);
    overflow-wrap: anywhere;
  }
  .cue-card.risk .cue-title {
    font-weight: 500;
    color: var(--bone-1);
  }
  .cue-detail {
    margin: 0.25rem 0 0;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--bone-1);
    overflow-wrap: anywhere;
  }

  /* ── #659 P5b — Support-mode knowledge surface (replaces the Sales cues) ──
     A manual "get an answer" affordance + the cited answer card. Accent
     (not --sig) marks it as a rep-initiated pull, distinct from the auto
     battlecards. Component-scoped — no app.css touch. */
  .kb {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    margin-bottom: 0.7rem;
  }
  .kb-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .kb-flag {
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--accent);
    border: 1px solid var(--accent);
    border-radius: 3px;
    padding: 0.02rem 0.28rem;
    line-height: 1.4;
  }
  .kb-label {
    font-size: 0.66rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .kb-ask {
    margin-left: auto;
    padding: 0.3rem 0.7rem;
    border: 1px solid var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
    border-radius: 999px;
    font: inherit;
    font-size: 0.76rem;
    cursor: pointer;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
  }
  .kb-ask:hover:not(:disabled) {
    background: var(--accent);
    color: var(--ink-0);
  }
  .kb-ask:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .kb-ask:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .kb-answer {
    position: relative;
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--accent);
    border-radius: var(--radius-sm);
    padding: 0.55rem 1.6rem 0.6rem 0.7rem;
  }
  .kb-dismiss {
    position: absolute;
    top: 0.3rem;
    right: 0.35rem;
    width: 1.2rem;
    height: 1.2rem;
    line-height: 1;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--bone-3);
    font-size: 1rem;
    cursor: pointer;
    border-radius: 4px;
  }
  .kb-dismiss:hover {
    color: var(--bone-0);
  }
  .kb-dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .kb-answer-text {
    margin: 0;
    font-size: 0.85rem;
    line-height: 1.4;
    color: var(--bone-0);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }
  .kb-sources {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.35rem;
    margin-top: 0.5rem;
  }
  .kb-sources-label {
    font-size: 0.6rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .kb-source {
    font-size: 0.72rem;
    color: var(--bone-1);
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 0.08rem 0.5rem;
    overflow-wrap: anywhere;
  }
  .kb-empty {
    margin: 0;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--bone-3);
  }

  /* ── Sentiment pill (pinned top, never moves) ─────────────────────── */
  .coach-head {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: 0.35rem;
    padding: 0.1rem 0.15rem 0.6rem;
  }
  .coach-sent-pip {
    align-self: center;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--bone-3);
    flex-shrink: 0;
    transition: background 150ms ease;
  }
  .coach-sent-pip.pos {
    background: var(--olive);
  }
  /* Negative: the pip is the ONLY red element, muted on a faint ground —
     never a red panel, never a pulse, label + note stay bone. */
  .coach-sent-pip.neg {
    background: var(--live);
    box-shadow: 0 0 0 3px var(--live-soft);
  }
  .coach-sent-pip.done {
    background: var(--olive);
  }
  .coach-sent-label {
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--bone-1);
  }
  .coach-sent-sep {
    color: var(--bone-3);
  }
  .coach-sent-note {
    font-size: 0.78rem;
    color: var(--bone-2);
    /* wrap to ≤2 lines then ellipsis */
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
    min-width: 0;
  }
  .coach-note {
    margin-left: auto;
    font-size: 0.72rem;
    color: var(--bone-3);
    align-self: center;
  }

  /* ── Card list ────────────────────────────────────────────────────── */
  .coach-cards {
    max-height: min(60vh, 22rem);
    overflow-y: auto;
    /* block flow (not flex-gap) so enter/exit can collapse the spacing */
  }
  .coach-cards:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }

  .coach-card {
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left-width: 1px;
    border-radius: var(--radius-sm);
    padding: 0.55rem 0.7rem;
    margin-bottom: 0.5rem;
  }
  .coach-card:last-child {
    margin-bottom: 0;
  }
  /* Left-edge = priority cue (no accent bar — accent stays on the glyph). */
  .coach-card.high {
    border-left: 2px solid var(--hairline-hi);
  }

  /* ── #660 — disclosure toggle (whole card-header is the button) ─────
     Two tiers: a KIND row (glyph · caps-label · chevron top-right) above
     the title, so an EXPANDED multi-line title wraps freely on its own row
     without the glyph/chevron floating against its centre. */
  .coach-card-toggle {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    width: 100%;
    padding: 0;
    border: none;
    background: transparent;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .coach-card-toggle:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }
  /* Detail-less card: same two-tier layout as the toggle header, but static
     (a plain container — no cursor, no chevron, nothing to expand). */
  .coach-card-head {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .coach-card-tophead {
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .coach-glyph {
    display: inline-flex;
    align-items: center;
    color: var(--accent);
    flex-shrink: 0;
  }
  .coach-kind {
    font-size: 0.68rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
    white-space: nowrap;
  }

  .coach-title {
    margin: 0;
    font-size: 0.85rem;
    font-weight: 500;
    line-height: 1.35;
    color: var(--bone-1);
    overflow-wrap: anywhere;
    min-width: 0;
  }
  /* Collapsed → clamp the title to a single line (glanceable). */
  .coach-title.clamped {
    display: -webkit-box;
    -webkit-line-clamp: 1;
    line-clamp: 1;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .coach-card.high .coach-title {
    color: var(--bone-0);
  }
  /* Chevron — the state cue; rotates 0°→90° on expand. A light affordance,
     lifts to bone-1 on toggle hover (a card is not a CTA — no accent fill). */
  .coach-chevron {
    margin-left: auto;
    color: var(--bone-3);
    flex-shrink: 0;
    align-self: center;
    transition: transform 150ms ease, color 150ms ease;
  }
  .coach-chevron.open {
    transform: rotate(90deg);
  }
  .coach-card-toggle:hover .coach-title {
    color: var(--bone-0);
  }
  .coach-card-toggle:hover .coach-chevron {
    color: var(--bone-1);
  }
  @media (prefers-reduced-motion: reduce) {
    .coach-chevron {
      transition: none;
    }
  }

  /* Collapsible body — CSS-only height reveal via grid rows; the inner
     wrapper clips its overflow. Does NOT compete with `animate:flip` (a
     height change is not a list reorder). */
  .coach-collapsible {
    display: grid;
    grid-template-rows: 0fr;
    transition: grid-template-rows 150ms ease;
  }
  .coach-collapsible.open {
    grid-template-rows: 1fr;
  }
  .coach-collapsible-inner {
    overflow: hidden;
    min-height: 0;
  }
  @media (prefers-reduced-motion: reduce) {
    .coach-collapsible {
      transition: none;
    }
  }
  .coach-detail {
    margin: 0.25rem 0 0;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--bone-2);
    overflow-wrap: anywhere;
  }

  /* Objection two-part: suggested response inset behind a hairline rule. */
  .coach-response {
    margin-top: 0.4rem;
    padding-left: 0.55rem;
    border-left: 1px solid var(--hairline);
  }
  .coach-response-label {
    display: block;
    font-size: 0.64rem;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .coach-response-text {
    margin: 0.15rem 0 0;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--bone-1);
    overflow-wrap: anywhere;
  }

  .coach-empty {
    margin: 0;
    padding: 0.4rem 0.15rem;
    font-size: 0.8rem;
    color: var(--bone-3);
  }

  /* ── Idle / listening surface ─────────────────────────────────────── */
  .coach-idle {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.55rem;
    text-align: center;
    padding: 2rem 1.2rem;
    min-height: 8rem;
  }
  .coach-idle-pip {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--bone-4);
    flex-shrink: 0;
  }
  .coach-idle-pip.blink {
    background: var(--accent);
    animation: coach-pip-pulse 1.4s ease-in-out infinite;
  }
  .coach-idle-primary {
    margin: 0;
    font-size: 0.9rem;
    color: var(--bone-2);
  }
  .coach-idle-sub {
    margin: 0;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--bone-3);
    max-width: 32ch;
  }

  @keyframes coach-pip-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 var(--accent-glow);
    }
    50% {
      box-shadow: 0 0 0 5px rgba(0, 0, 0, 0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .coach-idle-pip.blink {
      animation: none;
    }
    .coach-sent-pip {
      transition: none;
    }
  }
</style>
