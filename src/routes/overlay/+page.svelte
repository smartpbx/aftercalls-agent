<!--
  Overlay route — #659 P4 floating always-on-top co-pilot overlay.

  Rendered in a SECOND Tauri webview (label "overlay"), created on demand from
  Rust (see lib.rs `open_overlay`). It is opt-in (Settings → Co-pilot) and only
  opens on a Call recording start with live transcript on.

  This route renders BARE — +layout.svelte detects `pathname === "/overlay"` and
  skips the app chrome + auth/ToS gates, so this page owns its own minimal
  `live-*` listeners and hydrates once from `get_live_snapshot` (the overlay is
  created mid-call and would otherwise miss every broadcast emitted before it
  existed — coaching is a ~20s cadence).

  MINIMAL by design (plan §3, the anti-"cue-card-dispenser"): a sentiment dot +
  the single highest-priority live cue (glanceable, one line) + expand-on-demand
  detail + the four ask-chips. NOT a mini multi-lane.

  HARD RULES honored:
   - OPAQUE v1 — the window is opaque (no transparency); this is a solid-bg
     panel, not an alpha-composited card. True transparency is a follow-up.
   - All chrome is component-scoped `.ov-*`; design tokens come from app.css
     (imported globally by +layout) → NO app.css touch.
   - Copy is vendor-opaque — no STT/LLM/infra provider name anywhere.
   - Cue / answer text is LLM-derived → plain `{...}` interpolation ONLY, never
     `{@html}`.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import type {
    CoachingUpdate,
    CoachingCard,
    CoachingCardKind,
    CoachingSentimentLabel,
    AskChip,
    AskAnswer,
  } from "@aftercalls/shared/types";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  /** Mirror of the Rust `LiveSnapshot` returned by `get_live_snapshot`. */
  type LiveSnapshot = {
    coaching: CoachingUpdate | null;
    status: string | null;
    session_uuid: string | null;
    recording: boolean;
  };

  let coaching = $state<CoachingUpdate | null>(null);
  let status = $state<LiveStatus>("idle");
  let sessionUuid = $state<string | null>(null);
  let expanded = $state(false);
  let reduceMotion = $state(false);

  // Single inline ask slot + single in-flight guard (mirrors the Record
  // page's `handleAsk`). Degrades CALM: an unavailable ask lands a bone
  // degrade line, never an error surface, never a vendor name.
  let askAnswer = $state<{ chip: AskChip; answer: string } | null>(null);
  let askInFlight = $state<AskChip | null>(null);

  // ── Presentation maps ────────────────────────────────────────────────
  const KIND_LABEL: Record<CoachingCardKind, string> = {
    question: "Question",
    talking_point: "Talking point",
    objection: "Objection",
    next_action: "Next step",
  };

  const ASK_LABEL: Record<AskChip, string> = {
    catch_me_up: "Catch me up",
    summarize: "Summarize",
    what_did_they_ask: "What did they ask",
    action_items: "Action items",
  };
  const ASK_CHIPS: AskChip[] = [
    "catch_me_up",
    "summarize",
    "what_did_they_ask",
    "action_items",
  ];

  // Sentiment dot → design token (plan §3). null (no snapshot) → dim bone.
  const SENTIMENT_COLOR: Record<CoachingSentimentLabel, string> = {
    positive: "var(--olive)",
    neutral: "var(--bone-3)",
    negative: "var(--live)",
    mixed: "var(--sig)",
  };
  let dotColor = $derived(
    coaching ? SENTIMENT_COLOR[coaching.sentiment.label] : "var(--bone-4)",
  );

  // Single highest-priority cue: reuse the IntelligenceLane ordering — sort
  // `high` first (stable, so the server's within-band order is preserved),
  // take cards[0]. No cards → idle surface.
  const rank = (p: string) => (p === "high" ? 0 : 1);
  let topCard = $derived<CoachingCard | null>(
    coaching && coaching.cards.length > 0
      ? [...coaching.cards].sort((a, b) => rank(a.priority) - rank(b.priority))[0]
      : null,
  );
  let hasDetail = $derived(!!topCard?.detail);

  let isEnded = $derived(status === "ended");
  let isLive = $derived(status === "live");

  // Subtle auto-expand ONCE on a fresh high-priority objection (mirror
  // IntelligenceLane's `defaultExpanded`). Tracks the card's identity so the
  // ~20s reconcile never re-forces expand on a card the user collapsed.
  let lastCardKey: string | null = null;
  $effect(() => {
    const c = topCard;
    if (!c) {
      lastCardKey = null;
      return;
    }
    const key = c.kind + ":" + c.title;
    if (key !== lastCardKey) {
      lastCardKey = key;
      if (c.detail && (c.kind === "objection" || c.priority === "high")) {
        expanded = true;
      }
    }
  });

  function toggleExpand() {
    if (hasDetail) expanded = !expanded;
  }

  async function ask(chip: AskChip) {
    if (!sessionUuid || askInFlight) return;
    askInFlight = chip;
    try {
      const res = (await invoke("live_ask", {
        sessionUuid,
        chip,
      })) as AskAnswer;
      askAnswer = { chip, answer: res?.answer ?? "That's not available right now." };
    } catch {
      // No retry-shame, no vendor name — indistinguishable from "hasn't
      // generated yet".
      askAnswer = { chip, answer: "That's not available right now." };
    } finally {
      askInFlight = null;
    }
  }

  function dismissAsk() {
    askAnswer = null;
  }

  function closeOverlay() {
    // × dismisses the overlay; it never stops the recording. Re-opens on the
    // next Call start (or a Settings re-toggle).
    void getCurrentWindow().close();
  }

  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const mqHandler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", mqHandler);

    let unlisteners: UnlistenFn[] = [];
    let disposed = false;

    (async () => {
      // Cold-start hydration: pull the latest cached snapshot so a mid-call
      // overlay renders the top cue + sentiment instantly instead of sitting
      // blank until the next ~20s coaching frame.
      try {
        const snap = await invoke<LiveSnapshot>("get_live_snapshot");
        if (snap) {
          coaching = snap.coaching ?? null;
          status = ((snap.status as LiveStatus) ?? "idle") || "idle";
          sessionUuid = snap.session_uuid ?? null;
        }
      } catch {
        // No snapshot / IPC miss → stay on the calm idle surface; the live
        // broadcast below fills in within one coaching cycle.
      }

      if (disposed) return;

      // Ride the same global broadcast the main window listens to — a
      // separate webview is a separate JS context, so the two surfaces stay
      // in sync purely by subscribing to the same events (no shared store).
      unlisteners.push(
        await listen<CoachingUpdate>("live-coaching", (evt) => {
          coaching = evt.payload;
        }),
      );
      unlisteners.push(
        await listen<{ status?: string }>("live-session", (evt) => {
          const s = evt.payload?.status;
          if (s === "live" || s === "ended" || s === "error") status = s;
        }),
      );
      unlisteners.push(
        await listen<{ recording: boolean; session_uuid?: string | null }>(
          "recording-state",
          (evt) => {
            if (evt.payload.recording) {
              // Fresh session — drop the previous call's cue + answer, seed the
              // new session_uuid so the ask-chips can address it.
              coaching = null;
              askAnswer = null;
              expanded = false;
              status = "live";
              sessionUuid = evt.payload.session_uuid ?? null;
            }
          },
        ),
      );
    })();

    return () => {
      disposed = true;
      mq.removeEventListener("change", mqHandler);
      for (const u of unlisteners) u();
    };
  });
</script>

{#snippet kindGlyph(kind: CoachingCardKind)}
  <span class="ov-glyph" aria-hidden="true">
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

<div class="ov">
  <!-- Header doubles as the drag handle (data-tauri-drag-region). The close
       button carries no drag attribute, so its clicks land normally. -->
  <div class="ov-head" data-tauri-drag-region>
    <span
      class="ov-dot"
      class:blink={isLive && !coaching && !reduceMotion}
      style="background: {dotColor};"
      aria-hidden="true"
    ></span>
    <span class="ov-title">Co-pilot</span>
    <button
      type="button"
      class="ov-close"
      aria-label="Close overlay"
      onclick={closeOverlay}
    >×</button>
  </div>

  <div class="ov-body">
    {#if topCard}
      <div class="ov-cue" class:high={topCard.priority === "high"}>
        {#if hasDetail}
          <button
            type="button"
            class="ov-cue-toggle"
            aria-expanded={expanded}
            onclick={toggleExpand}
          >
            <span class="ov-cue-head">
              {@render kindGlyph(topCard.kind)}
              <span class="ov-kind">{KIND_LABEL[topCard.kind]}</span>
              <svg
                class="ov-chevron"
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
            <span class="ov-cue-title" class:clamped={!expanded}>{topCard.title}</span>
          </button>
          {#if expanded}
            <p class="ov-cue-detail">{topCard.detail}</p>
          {/if}
        {:else}
          <div class="ov-cue-head">
            {@render kindGlyph(topCard.kind)}
            <span class="ov-kind">{KIND_LABEL[topCard.kind]}</span>
          </div>
          <span class="ov-cue-title">{topCard.title}</span>
        {/if}
      </div>
    {:else}
      <div class="ov-idle">
        <p class="ov-idle-primary">
          {isEnded ? "Call ended" : "Listening…"}
        </p>
        {#if isLive}
          <p class="ov-idle-sub">Your top cue shows up here as the call goes.</p>
        {/if}
      </div>
    {/if}

    <!-- Ask-chips row — one tap generates a recap over the live window. Same
         four presets + calm degrade as the in-app panel. Disabled until a
         live session exists / while one is in flight. -->
    {#if askAnswer}
      <div class="ov-answer">
        <button
          type="button"
          class="ov-answer-dismiss"
          aria-label="Dismiss answer"
          onclick={dismissAsk}
        >×</button>
        <span class="ov-answer-label">{ASK_LABEL[askAnswer.chip]}</span>
        <p class="ov-answer-text" aria-live="polite">{askAnswer.answer}</p>
      </div>
    {/if}
    <div class="ov-chips" role="group" aria-label="Quick recap">
      {#each ASK_CHIPS as chip (chip)}
        <button
          type="button"
          class="ov-chip"
          disabled={!sessionUuid || askInFlight !== null}
          onclick={() => ask(chip)}
        >
          {askInFlight === chip ? "…" : ASK_LABEL[chip]}
        </button>
      {/each}
    </div>
  </div>
</div>

<style>
  /* OPAQUE v1: a solid-background panel filling the whole window (no alpha).
     A decorationless window is a rectangle; on compositors that round
     borderless windows (e.g. Hyprland) this reads as a rounded card for free,
     and on every other platform it's a clean framed rectangle. True alpha
     transparency + CSS rounded corners are a documented follow-up. */
  :global(html),
  :global(body) {
    margin: 0;
    height: 100%;
    background: var(--ink-1);
    overflow: hidden;
  }
  .ov {
    box-sizing: border-box;
    height: 100vh;
    display: flex;
    flex-direction: column;
    background: var(--ink-1);
    color: var(--bone-0);
    border: 1px solid var(--hairline-hi);
    user-select: none;
    -webkit-user-select: none;
  }

  /* ── Header / drag handle ───────────────────────────────────────────── */
  .ov-head {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.5rem 0.6rem;
    border-bottom: 1px solid var(--hairline);
    cursor: grab;
    flex-shrink: 0;
  }
  .ov-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .ov-dot.blink {
    animation: ov-pulse 1.4s ease-in-out infinite;
  }
  .ov-title {
    font-size: 0.72rem;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .ov-close {
    margin-left: auto;
    width: 1.3rem;
    height: 1.3rem;
    line-height: 1;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--bone-3);
    font-size: 1.1rem;
    cursor: pointer;
    border-radius: 4px;
  }
  .ov-close:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
  .ov-close:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  /* ── Body ───────────────────────────────────────────────────────────── */
  .ov-body {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    padding: 0.6rem;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }

  .ov-cue {
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--hairline-hi);
    border-radius: var(--radius-sm);
    padding: 0.5rem 0.6rem;
  }
  .ov-cue.high {
    border-left-color: var(--accent);
  }
  .ov-cue-toggle {
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
    color: inherit;
  }
  .ov-cue-toggle:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }
  .ov-cue-head {
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .ov-glyph {
    display: inline-flex;
    align-items: center;
    color: var(--accent);
    flex-shrink: 0;
  }
  .ov-kind {
    font-size: 0.64rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
    white-space: nowrap;
  }
  .ov-chevron {
    margin-left: auto;
    color: var(--bone-3);
    flex-shrink: 0;
    transition: transform 150ms ease;
  }
  .ov-chevron.open {
    transform: rotate(90deg);
  }
  .ov-cue-title {
    margin: 0;
    font-size: 0.86rem;
    font-weight: 550;
    line-height: 1.35;
    color: var(--bone-0);
    overflow-wrap: anywhere;
  }
  .ov-cue-title.clamped {
    display: -webkit-box;
    -webkit-line-clamp: 1;
    line-clamp: 1;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .ov-cue-detail {
    margin: 0.4rem 0 0;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--bone-1);
    overflow-wrap: anywhere;
  }

  /* ── Idle surface ───────────────────────────────────────────────────── */
  .ov-idle {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    padding: 0.4rem 0.2rem;
  }
  .ov-idle-primary {
    margin: 0;
    font-size: 0.85rem;
    color: var(--bone-2);
  }
  .ov-idle-sub {
    margin: 0;
    font-size: 0.76rem;
    line-height: 1.4;
    color: var(--bone-3);
  }

  /* ── Inline ask answer ──────────────────────────────────────────────── */
  .ov-answer {
    position: relative;
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--accent);
    border-radius: var(--radius-sm);
    padding: 0.45rem 1.5rem 0.5rem 0.6rem;
  }
  .ov-answer-dismiss {
    position: absolute;
    top: 0.25rem;
    right: 0.3rem;
    width: 1.15rem;
    height: 1.15rem;
    line-height: 1;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--bone-3);
    font-size: 0.95rem;
    cursor: pointer;
    border-radius: 4px;
  }
  .ov-answer-dismiss:hover {
    color: var(--bone-0);
  }
  .ov-answer-dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .ov-answer-label {
    font-size: 0.6rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .ov-answer-text {
    margin: 0.2rem 0 0;
    font-size: 0.82rem;
    line-height: 1.4;
    color: var(--bone-0);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }

  /* ── Ask chips ──────────────────────────────────────────────────────── */
  .ov-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
  }
  .ov-chip {
    padding: 0.28rem 0.6rem;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-1);
    border-radius: 999px;
    font: inherit;
    font-size: 0.72rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .ov-chip:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .ov-chip:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .ov-chip:disabled {
    opacity: 0.45;
    cursor: default;
  }

  @keyframes ov-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 var(--accent-glow);
    }
    50% {
      box-shadow: 0 0 0 4px rgba(0, 0, 0, 0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .ov-dot.blink {
      animation: none;
    }
    .ov-chevron {
      transition: none;
    }
  }
</style>
