<!--
  CoPilotPanel — #653 in-call co-pilot surface, #662 dashboard + auto-mode,
  reworked into the BI-dashboard revamp (three buckets, retiring the old
  Transcript/Contact/Coaching 3-lane model).

  The founder-approved revamp (competitor review: show only what a rep can act on
  in the next ~30 seconds) restructures the panel into three buckets that share
  ONE mounted set of live-state components (BC-1 "lanes render once" — CSS
  repositions between compact / dashboard, never remounts):

    1. QUIET REFERENCE RAIL (cap 3 tiles) — deal/CRM context (CrmContextLane),
       a talk-ratio meter (TalkTimeNudge), the discovery checklist (LiveChecklist,
       meter-only), plus ONE subtle sentiment indicator. Compact = a horizontal
       3-cell strip; dashboard = a ~300px vertical left column.
    2. EPHEMERAL CUE SLOT — a single one-at-a-time cue with an auto-dismiss drain
       bar (IntelligenceLane): battlecard > monologue > coaching, higher preempts.
    3. ON-DEMAND TRANSCRIPT DRAWER (collapsed by default) — LiveTranscriptLane +
       a one-click "Catch me up" shortcut + (Support) the Knowledge affordance.

  #662 — two shapes, ONE set of live-state instances:
    • COMPACT (default): the in-page panel; rail strip + cue + collapsed drawer.
    • DASHBOARD: an in-app maximized layer (`position:fixed`, portaled to <body>
      so its z-index competes at the document root — the shipped full-screen fix)
      under a slim command strip; rail column + cue + drawer.
  The layout switch is a PURE CSS restyle of the SAME mounted components — they
  NEVER unmount on toggle, so expand/collapse / drawer-open keep the picked
  contact, hydrated Deals/Cases, transcript scroll, and the live cue's drain
  (zero re-fetch). This is why the body is always-mounted, not `{#if layout}`.

  #662 — auto-mode: the persona (Sales/Support) is INFERRED here (single owner)
  from open Deals/Cases counts (raised by CrmContextLane's counts-only
  `oncounts`, no PII) + the coach frame's `posture`. A quiet indicator + a small
  override popover; a manual pick freezes inference for the call.

  All chrome is component-scoped (`.cp-*`/`.cpd-*`); the only shared classes are
  the transcript lane's `.live-*` family (read-only) and `.avatar`. No app.css
  touch → hard-rule #1 never engaged.
-->
<script lang="ts" module>
  import type { CopilotMode, CoachingPosture } from "@aftercalls/shared/types";

  /** #662 — counts-only CRM signal raised from CrmContextLane (NO PII: counts
   *  + degrade status only, consistent with the deal-facts-only posture). */
  export type CrmCounts = {
    dealsOpen: number;
    casesOpen: number;
    dealsStatus: "ok" | "empty" | "unavailable";
    casesStatus: "ok" | "empty" | "unavailable";
  };

  /**
   * #662 — pure one-shot mode inference (extracted so it stays testable + the
   * hysteresis glue in the component stays thin).
   *
   *  1. A DECISIVE conversational posture ("sales"/"support") wins over CRM
   *     inventory — a rep may run a support call on a contact who also has an
   *     open deal. ("mixed" is not decisive → falls through to the CRM lean.)
   *  2. Else lean on the OPEN counts: cases-only → support, deals-only → sales,
   *     both → the greater count (tie → null).
   *  3. Neither / no signal → null (no decision; the caller falls back to the
   *     org default via `effectiveMode`'s `?? defaultMode`).
   */
  export function inferOnce(
    crm: CrmCounts | null,
    posture: CoachingPosture | null | undefined,
  ): CopilotMode | null {
    if (posture === "sales") return "sales";
    if (posture === "support") return "support";
    const deals = crm?.dealsOpen ?? 0;
    const cases = crm?.casesOpen ?? 0;
    if (cases > 0 && deals === 0) return "support";
    if (deals > 0 && cases === 0) return "sales";
    if (deals > 0 && cases > 0) {
      if (cases > deals) return "support";
      if (deals > cases) return "sales";
      return null; // tie → no decision
    }
    return null; // nothing open → no decision
  }
</script>

<script lang="ts">
  import { untrack } from "svelte";
  import LiveTranscriptLane from "$lib/LiveTranscriptLane.svelte";
  import CrmContextLane from "$lib/CrmContextLane.svelte";
  import IntelligenceLane from "$lib/IntelligenceLane.svelte";
  import LiveChecklist from "$lib/LiveChecklist.svelte";
  import TalkTimeNudge from "$lib/TalkTimeNudge.svelte";
  import { portal } from "$lib/actions/portal";
  import type {
    LiveSegment,
    CoachingUpdate,
    LiveCue,
    ChecklistSnapshot,
    AskChip,
    KnowledgeAnswer,
    CoachingSentimentLabel,
    SpeakerIdentity,
    SpeakerIdentityAssignArgs,
    LinkedDeal,
    CrmContextDeal,
  } from "@aftercalls/shared/types";
  import type {
    AskAnswerState,
    TalkMetric,
  } from "$lib/stores/liveSession.svelte";
  import { deriveDetectedSpeakers } from "$lib/stores/liveSession.svelte";

  type LiveStatus = "idle" | "live" | "ended" | "error";
  type Layout = "compact" | "dashboard";

  let {
    segments = [],
    status = "idle",
    recording = false,
    isAdmin = false,
    defaultMode = "sales",
    liveTranscriptEnabled = false,
    sessionUuid = null,
    coaching = null,
    cues = [],
    checklist = null,
    elapsedMs = 0,
    onToggleRecording = undefined,
    onnotes = undefined,
    askAnswer = null,
    askInFlight = null,
    knowledgeAnswer = null,
    knowledgeInFlight = false,
    highlighted = new Set<string>(),
    speakerIdentities = new Map<string, SpeakerIdentity>(),
    talkMetric = null,
    onask = undefined,
    ondismissAsk = undefined,
    onknowledge = undefined,
    ondismissKnowledge = undefined,
    onhighlight = undefined,
    onassignSpeaker,
    onpick,
    linkedDeal = null,
    onlinkdeal = undefined,
    onunlinkdeal = undefined,
  }: {
    segments?: LiveSegment[];
    status?: LiveStatus;
    recording?: boolean;
    isAdmin?: boolean;
    /** #659 P5a — the org's default co-pilot persona (from /auth/me). The
     *  bias/fallback when auto-inference has no decision. */
    defaultMode?: CopilotMode;
    /** #654 — latest live coaching snapshot (null pre-first-update / cleared
     *  on new session). Feeds the cue slot + the rail sentiment; #662 also
     *  reads `coaching.posture` for auto-mode inference. */
    coaching?: CoachingUpdate | null;
    /** #659 (P2) — fast-lane cues (battlecards); feed the ephemeral cue slot. */
    cues?: LiveCue[];
    /** #659 (P3) — auto-checking agenda checklist; rendered in the rail. */
    checklist?: ChecklistSnapshot | null;
    /** Whether the org's live_transcript flag is ON. The transcript drawer is
     *  only meaningful when it is (the relay is gated on that flag alone), so
     *  with copilot ON + live_transcript OFF we drop the transcript. */
    liveTranscriptEnabled?: boolean;
    sessionUuid?: string | null;
    /** #662 — recorder elapsed clock (ms), formatted mono in the command
     *  strip. Ignored in compact. */
    elapsedMs?: number;
    /** #662 — flip the recorder (Stop/Start) from the dashboard command strip. */
    onToggleRecording?: () => void;
    /** #662 — reach Notes from the command strip: collapses + opens notes. */
    onnotes?: () => void;
    askAnswer?: AskAnswerState | null;
    askInFlight?: AskChip | null;
    /** #659 P5b — Support-mode cited knowledge answer + its in-flight guard,
     *  rendered in the transcript drawer. */
    knowledgeAnswer?: KnowledgeAnswer | null;
    knowledgeInFlight?: boolean;
    highlighted?: Set<string>;
    /** #646 (Phase 2) — the speaker→identity map, threaded into BOTH the rail
     *  CRM tile (roster + primary card) and the transcript drawer (labelFor). */
    speakerIdentities?: Map<string, SpeakerIdentity>;
    talkMetric?: TalkMetric | null;
    onask?: (chip: AskChip) => void;
    ondismissAsk?: () => void;
    /** #659 P5b — fire one Support-mode knowledge answer / dismiss the slot. */
    onknowledge?: () => void;
    ondismissKnowledge?: () => void;
    onhighlight?: (seg: LiveSegment, starred: boolean) => void;
    /** #646 (Phase 2) — commit one speaker identity assign / clear. +page owns
     *  the `live_speaker_identity` invoke + the optimistic/reconcile store write. */
    onassignSpeaker?: (
      args: Omit<SpeakerIdentityAssignArgs, "sessionUuid">,
    ) => void;
    // Raised up from CrmContextLane; +page threads it into start_recording.
    onpick: (contactId: string | null) => void;
    /** Phase 3 — the ONE deal linked to this call (or null), threaded into the
     *  rail CRM tile so it can render the per-deal "Link to call" / "Linked"
     *  state. */
    linkedDeal?: LinkedDeal | null;
    /** Phase 3 — link (one deal at a time) / unlink raised from the CRM tile;
     *  +page owns the `live_linked_deal` invoke + the optimistic/reconcile store
     *  write. */
    onlinkdeal?: (deal: CrmContextDeal) => void;
    onunlinkdeal?: () => void;
  } = $props();

  // #646 (Phase 2) — the distinct-speaker roster set, derived from the live
  // segments (mic recorder + far-side speakers). Passed to the rail CRM tile.
  let detectedSpeakers = $derived(deriveDetectedSpeakers(segments));

  // ── Layout (#662) ─────────────────────────────────────────────────────────
  // Panel-local, NOT persisted: default compact each session (expand is a
  // deliberate "go big" gesture). Never `{#if}`-swap the buckets on toggle — a
  // pure CSS restyle keeps every component instance mounted (zero re-fetch,
  // scroll + drain + picked-contact state preserved).
  let layout = $state<Layout>("compact");
  let dashboard = $derived(layout === "dashboard");
  let expandBtnEl = $state<HTMLButtonElement | null>(null);
  let collapseBtnEl = $state<HTMLButtonElement | null>(null);
  let layerEl = $state<HTMLElement | null>(null);

  // The dashboard layer is portaled to <body> (see `use:portal` below), so its
  // top offset must clear the REAL app topstrip. `--topbar-h` (40px) is only the
  // floor: the topstrip is `min-height` and GROWS when its pill cluster wraps on
  // a narrow window. Measure the live topstrip while the layer is up and feed its
  // height to the layer's `top` via a component-local `--cpd-top`.
  let topOffset = $state<number | null>(null);
  $effect(() => {
    if (!dashboard) {
      topOffset = null;
      return;
    }
    const strip = document.querySelector<HTMLElement>(".topstrip");
    if (!strip) return;
    const measure = () => {
      topOffset = Math.round(strip.getBoundingClientRect().height);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(strip);
    return () => ro.disconnect();
  });

  function expand() {
    layout = "dashboard";
    queueMicrotask(() => collapseBtnEl?.focus());
  }
  function collapse() {
    layout = "compact";
    modeMenuOpen = false;
    queueMicrotask(() => expandBtnEl?.focus());
  }
  // Escape collapses (menu first if open); Tab is trapped within the layer.
  function onLayerKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (modeMenuOpen) {
        modeMenuOpen = false;
        e.stopPropagation();
        return;
      }
      e.preventDefault();
      collapse();
      return;
    }
    if (e.key === "Tab" && layerEl) {
      const nodes = layerEl.querySelectorAll<HTMLElement>(
        'button:not([disabled]), a[href], input:not([disabled]), [tabindex]:not([tabindex="-1"])',
      );
      const list = Array.from(nodes).filter((el) => el.getClientRects().length > 0);
      if (list.length === 0) return;
      const first = list[0];
      const last = list[list.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey && active === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    }
  }

  // ── Auto-mode brain (#662, single owner) ──────────────────────────────────
  const MODE_LABELS: Record<CopilotMode, string> = {
    sales: "Sales",
    support: "Support",
  };
  const MODE_OPTIONS: CopilotMode[] = ["sales", "support"];

  let modeOverride = $state<CopilotMode | null>(null);
  let userSet = $state(false);
  let inferredMode = $state<CopilotMode | null>(null);
  let effectiveMode = $derived<CopilotMode>(
    modeOverride ?? inferredMode ?? defaultMode,
  );
  let isSupport = $derived(effectiveMode === "support");

  // Counts-only CRM signal raised from CrmContextLane (NO PII).
  let crmCounts = $state<CrmCounts | null>(null);
  function handleCounts(c: CrmCounts) {
    crmCounts = c;
  }

  let pendingCandidate: CopilotMode | null = null;

  // Inference effect — re-runs on the CRM counts OR a fresh coaching frame.
  $effect(() => {
    const counts = crmCounts;
    const posture = coaching?.posture ?? null;
    void coaching?.seq;
    untrack(() => {
      if (userSet) return;
      const candidate = inferOnce(counts, posture);
      if (candidate === null) {
        pendingCandidate = null;
        return;
      }
      if (inferredMode === null) {
        inferredMode = candidate;
        pendingCandidate = null;
        return;
      }
      if (candidate === inferredMode) {
        pendingCandidate = null;
        return;
      }
      if (pendingCandidate === candidate) {
        inferredMode = candidate;
        pendingCandidate = null;
      } else {
        pendingCandidate = candidate;
      }
    });
  });

  // ── Demoted mode control + override popover ────────────────────────────────
  let modeMenuOpen = $state(false);
  let modeWrapEl = $state<HTMLDivElement | null>(null);
  let modeBtnEl = $state<HTMLButtonElement | null>(null);
  let modeMenuEls: HTMLButtonElement[] = [];
  let modeMenuFocus = $state(0);

  let detectedLabel = $derived(
    inferredMode ? MODE_LABELS[inferredMode] : "No clear signal",
  );

  function openModeMenu() {
    modeMenuOpen = true;
    modeMenuFocus = 0;
    queueMicrotask(() => modeMenuEls[0]?.focus());
  }
  function toggleModeMenu() {
    if (modeMenuOpen) closeModeMenu();
    else openModeMenu();
  }
  function closeModeMenu() {
    modeMenuOpen = false;
    queueMicrotask(() => modeBtnEl?.focus());
  }
  function setMode(m: CopilotMode) {
    modeOverride = m;
    userSet = true;
    pendingCandidate = null;
    closeModeMenu();
  }
  function followDetection() {
    modeOverride = null;
    userSet = false;
    pendingCandidate = null;
    inferredMode = inferOnce(crmCounts, coaching?.posture ?? null);
    closeModeMenu();
  }
  function onModeMenuKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      closeModeMenu();
      return;
    }
    const n = modeMenuEls.filter(Boolean).length || 3;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      modeMenuFocus = (modeMenuFocus + 1) % n;
      modeMenuEls[modeMenuFocus]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      modeMenuFocus = (modeMenuFocus - 1 + n) % n;
      modeMenuEls[modeMenuFocus]?.focus();
    }
  }
  function onOutsidePointerDown(e: PointerEvent) {
    if (!modeMenuOpen) return;
    const t = e.target as Node | null;
    if (!t) return;
    if (modeWrapEl && modeWrapEl.contains(t)) return;
    modeMenuOpen = false;
  }
  $effect(() => {
    document.addEventListener("pointerdown", onOutsidePointerDown, true);
    return () =>
      document.removeEventListener("pointerdown", onOutsidePointerDown, true);
  });

  // Polite announcement of an auto- or manual mode change.
  let modeAnnounce = $state("");
  let prevAnnouncedMode: CopilotMode | null = null;
  $effect(() => {
    const m = effectiveMode;
    untrack(() => {
      if (prevAnnouncedMode !== null && prevAnnouncedMode !== m) {
        modeAnnounce = `Mode: ${MODE_LABELS[m]}`;
      }
      prevAnnouncedMode = m;
    });
  });

  // ── Rail sentiment (ONE subtle indicator; de-duplicated from the old ~4) ──
  const SENTIMENT_WORD: Record<CoachingSentimentLabel, string> = {
    positive: "Positive",
    neutral: "Neutral",
    negative: "Negative",
    mixed: "Mixed",
  };
  let sentiment = $derived(coaching?.sentiment ?? null);
  let isEnded = $derived(status === "ended");
  // Delta-only SR announce so the single indicator isn't spammed each frame.
  let sentimentAnnounce = $state("");
  let prevSentLabel: CoachingSentimentLabel | null = null;
  $effect(() => {
    const label = coaching?.sentiment.label ?? null;
    untrack(() => {
      if (label !== null && label !== prevSentLabel && prevSentLabel !== null) {
        sentimentAnnounce = `Sentiment: ${label}.`;
      }
      prevSentLabel = label;
    });
  });

  // ── Transcript drawer (collapsed by default) ───────────────────────────────
  let drawerOpen = $state(false);
  function toggleDrawer() {
    drawerOpen = !drawerOpen;
  }
  // The drawer hosts the transcript (+ ask chips) and, in Support, Knowledge.
  let hasDrawer = $derived(liveTranscriptEnabled || isSupport);
  // Count of finals behind the collapsed drawer — a quiet "there's transcript".
  let finalCount = $derived(
    segments.reduce((n, s) => n + (s.provisional ? 0 : 1), 0),
  );

  // "Catch me up" one-click shortcut — reuses the ask-chip mechanism. Ready once
  // there's a live session to address; opens the drawer so the answer shows.
  let askReady = $derived(
    !!onask &&
      liveTranscriptEnabled &&
      !!sessionUuid &&
      status !== "idle" &&
      status !== "error",
  );
  let askBusy = $derived(askInFlight !== null);
  function catchMeUp() {
    if (!askReady || askBusy) return;
    onask?.("catch_me_up");
    drawerOpen = true;
  }

  // ── Notes (command strip) ──────────────────────────────────────────────────
  function openNotes() {
    collapse();
    onnotes?.();
  }

  // Mono call timer for the command strip.
  function fmtTimer(ms: number): string {
    const s = Math.floor(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const r = s % 60;
    const pad = (n: number) => String(n).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(r)}` : `${pad(m)}:${pad(r)}`;
  }
</script>

<!-- ── Quiet mode indicator + override popover (shared by both layouts) ────── -->
{#snippet modeControl(variant: "chip" | "strip")}
  <div class="cp-mode cp-mode-{variant}" bind:this={modeWrapEl}>
    <button
      bind:this={modeBtnEl}
      type="button"
      class="cp-mode-indicator"
      aria-haspopup="menu"
      aria-expanded={modeMenuOpen}
      aria-label={`Mode: ${MODE_LABELS[effectiveMode]}, ${
        modeOverride === null ? "detected" : "set manually"
      }. Activate to change.`}
      onclick={toggleModeMenu}
    >
      <span class="cp-mode-glyph" aria-hidden="true">
        <svg viewBox="0 0 24 24" width="11" height="11" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9" /><polygon points="15.5 8.5 11 11 8.5 15.5 13 13 15.5 8.5" fill="currentColor" stroke="none" /></svg>
      </span>
      <span class="cp-mode-word">{MODE_LABELS[effectiveMode]}</span>
      <span class="cp-mode-src" aria-hidden="true"
        >{modeOverride === null ? "auto" : "manual"}</span
      >
      <svg
        class="cp-mode-caret"
        viewBox="0 0 24 24"
        width="10"
        height="10"
        fill="none"
        stroke="currentColor"
        stroke-width="2.2"
        stroke-linecap="round"
        stroke-linejoin="round"
        aria-hidden="true"><polyline points="6 9 12 15 18 9" /></svg
      >
    </button>

    {#if modeMenuOpen}
      <div
        class="cp-mode-menu"
        role="menu"
        aria-label="Co-pilot mode"
        tabindex="-1"
        onkeydown={onModeMenuKeydown}
      >
        <p class="cp-mode-menu-head">Detected: {detectedLabel}</p>
        <button
          bind:this={modeMenuEls[0]}
          type="button"
          role="menuitemradio"
          aria-checked={modeOverride === null}
          class="cp-mode-item"
          class:on={modeOverride === null}
          onmouseenter={() => (modeMenuFocus = 0)}
          onclick={followDetection}
        >
          <span class="cp-mode-check" aria-hidden="true"
            >{modeOverride === null ? "✓" : ""}</span
          >
          Follow detection
        </button>
        {#each MODE_OPTIONS as m, i (m)}
          <button
            bind:this={modeMenuEls[i + 1]}
            type="button"
            role="menuitemradio"
            aria-checked={modeOverride === m}
            class="cp-mode-item"
            class:on={modeOverride === m}
            onmouseenter={() => (modeMenuFocus = i + 1)}
            onclick={() => setMode(m)}
          >
            <span class="cp-mode-check" aria-hidden="true"
              >{modeOverride === m ? "✓" : ""}</span
            >
            {MODE_LABELS[m]}
          </button>
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

<!-- ── The three buckets — ONE mounted set (BC-1); CSS repositions them ─────
     Rail (reference) · cue (ephemeral) · drawer (on-demand transcript). The
     live-state components (CrmContextLane, LiveTranscriptLane, IntelligenceLane
     cue) are mounted exactly once and NEVER cross an `{#if}` on layout/drawer,
     so expand/collapse + drawer-open are pure CSS restyles (no remount). -->
{#snippet buckets()}
  <!-- 1 — Quiet reference rail (cap 3 tiles + a subtle sentiment read). -->
  <aside class="cp-rail" class:dashboard aria-label="Reference">
    <div class="cp-tile cp-tile-crm">
      <CrmContextLane
        speakers={detectedSpeakers}
        {speakerIdentities}
        onassign={onassignSpeaker}
        {onpick}
        oncounts={handleCounts}
        {sessionUuid}
        {isAdmin}
        mode={effectiveMode}
        {linkedDeal}
        {onlinkdeal}
        {onunlinkdeal}
      />
    </div>
    {#if talkMetric && talkMetric.youPct + talkMetric.themPct > 0}
      <div class="cp-tile cp-tile-talk">
        <div class="cp-tile-head">Talk share</div>
        <TalkTimeNudge
          youPct={talkMetric.youPct}
          themPct={talkMetric.themPct}
          youRunMs={talkMetric.youRunMs}
        />
      </div>
    {/if}
    {#if checklist}
      <div class="cp-tile cp-tile-check">
        <LiveChecklist {checklist} {status} />
      </div>
    {/if}
    {#if sentiment}
      <!-- ONE subtle sentiment indicator (founder-kept; de-duplicated). -->
      <div
        class="cp-sent"
        aria-label={`Sentiment: ${SENTIMENT_WORD[sentiment.label]}`}
      >
        <span
          class="cp-sent-dot"
          class:pos={sentiment.label === "positive" && !isEnded}
          class:neg={sentiment.label === "negative" && !isEnded}
          class:done={isEnded}
          aria-hidden="true"
        ></span>
        <span class="cp-sent-word">{SENTIMENT_WORD[sentiment.label]}</span>
      </div>
    {/if}
  </aside>

  <!-- 2 + 3 — cue slot (prominent) over the on-demand transcript drawer. -->
  <div class="cp-main" class:dashboard>
    <div class="cp-cue-host">
      <IntelligenceLane {coaching} {status} {cues} {talkMetric} />
    </div>

    {#if hasDrawer}
      <div class="cp-drawer" class:open={drawerOpen} class:dashboard>
        <div class="cp-drawer-bar">
          <button
            type="button"
            class="cp-drawer-toggle"
            aria-expanded={drawerOpen}
            aria-controls="cp-drawer-body"
            onclick={toggleDrawer}
          >
            <svg
              class="cp-drawer-chevron"
              class:open={drawerOpen}
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
            <span>Transcript</span>
            {#if !drawerOpen && finalCount > 0}
              <span class="cp-drawer-count">{finalCount}</span>
            {/if}
          </button>
          {#if onask && liveTranscriptEnabled}
            <button
              type="button"
              class="cp-catchup"
              disabled={!askReady || askBusy}
              title={askBusy ? "Give it a moment…" : "Catch me up on the last minute"}
              onclick={catchMeUp}
            >
              Catch me up
            </button>
          {/if}
        </div>

        <div id="cp-drawer-body" class="cp-drawer-body" hidden={!drawerOpen}>
          {#if liveTranscriptEnabled}
            <LiveTranscriptLane
              {segments}
              {status}
              {sessionUuid}
              {askAnswer}
              {askInFlight}
              {highlighted}
              {speakerIdentities}
              {onask}
              {ondismissAsk}
              {onhighlight}
            />
          {/if}
          {#if isSupport}
            <!-- Support-mode grounded knowledge (rep-initiated pull). Vendor-
                 opaque; answer + citations are LLM/org text → plain `{...}`. -->
            <div class="cp-kb" role="group" aria-label="Knowledge answers">
              <div class="cp-kb-head">
                <span class="cp-kb-label">Knowledge</span>
                <button
                  type="button"
                  class="cp-kb-ask"
                  onclick={() => onknowledge?.()}
                  disabled={knowledgeInFlight}
                >
                  {knowledgeInFlight ? "Searching…" : "Get an answer"}
                </button>
              </div>
              {#if knowledgeAnswer}
                <div class="cp-kb-answer">
                  <button
                    type="button"
                    class="cp-kb-dismiss"
                    aria-label="Dismiss answer"
                    onclick={() => ondismissKnowledge?.()}>×</button
                  >
                  <p class="cp-kb-answer-text" aria-live="polite">
                    {knowledgeAnswer.answer}
                  </p>
                  {#if knowledgeAnswer.sources.length > 0}
                    <div class="cp-kb-sources">
                      {#each knowledgeAnswer.sources as s (s.id)}
                        <span class="cp-kb-source">{s.title}</span>
                      {/each}
                    </div>
                  {/if}
                </div>
              {/if}
            </div>
          {/if}
        </div>
      </div>
    {/if}
  </div>
{/snippet}

<section class="copilot-panel" class:is-dashboard={dashboard} style="--i: 2.5">
  <!-- Net-new polite regions for the whole panel (mode + sentiment change);
       the buckets own their own live regions and must not be duplicated. -->
  <span class="cp-sr" aria-live="polite">{modeAnnounce}</span>
  <span class="cp-sr" aria-live="polite">{sentimentAnnounce}</span>

  {#if !dashboard}
    <!-- COMPACT chrome — a slim header (quiet mode chip + Expand ⤢). The
         buckets live in the always-mounted shell below, so toggling layout is
         a pure CSS restyle and never remounts them. -->
    <div class="copilot-tabrow">
      <span class="cp-title">Co-pilot</span>
      <div class="copilot-tabrow-right">
        {@render modeControl("chip")}
        <button
          bind:this={expandBtnEl}
          type="button"
          class="cp-expand"
          aria-label="Expand co-pilot to full-screen dashboard"
          title="Expand dashboard"
          onclick={expand}
        >
          <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 3 21 3 21 9" /><polyline points="9 21 3 21 3 15" /><line x1="21" y1="3" x2="14" y2="10" /><line x1="3" y1="21" x2="10" y2="14" /></svg>
        </button>
      </div>
    </div>
  {/if}

  <!-- ── Persistent lane shell (#662 BC-1 + shipped full-screen portal fix) ──
       ONE home for the buckets across BOTH layouts, OUTSIDE the layout `{#if}`.
       Compact: a transparent passthrough (`display:contents`) so `.copilot-body`
       renders inline. Dashboard: this SAME element is portaled to <body>
       (`use:portal={dashboard}`) and promoted to the in-app maximized layer
       (`position:fixed`, `z-index:90` competing at document root — the fix),
       with `--cpd-top` measured off the live topstrip. Because the buckets'
       single render site never crosses an if/else boundary, expand/collapse
       keeps the SAME instances (picked contact, hydrated Deals/Cases, cue drain,
       transcript scroll all survive; zero re-fetch). -->
  <div
    class="lane-shell"
    class:dashboard
    bind:this={layerEl}
    use:portal={dashboard}
    style={dashboard && topOffset !== null
      ? `--cpd-top:${topOffset}px`
      : undefined}
    role={dashboard ? "dialog" : undefined}
    aria-modal={dashboard ? "true" : undefined}
    aria-label={dashboard ? "Co-pilot dashboard" : undefined}
    tabindex="-1"
    onkeydown={dashboard ? onLayerKeydown : undefined}
  >
    {#if dashboard}
      <!-- DASHBOARD chrome — the slim command strip over the buckets. -->
      <header class="cpd-strip">
        <div class="cpd-record">
          <span
            class="cpd-pip"
            class:live={recording}
            class:done={status === "ended"}
            aria-hidden="true"
          ></span>
          {#if recording}
            <span class="cpd-timer">{fmtTimer(elapsedMs)}</span>
          {/if}
          {#if onToggleRecording}
            <button
              type="button"
              class="cpd-stop"
              class:live={recording}
              onclick={() => onToggleRecording?.()}
            >
              {recording ? "Stop recording" : "Start recording"}
            </button>
          {/if}
        </div>

        {@render modeControl("strip")}

        <div class="cpd-right">
          {#if onnotes}
            <button type="button" class="cpd-tool" onclick={openNotes}>
              Notes
            </button>
          {/if}
          <button
            bind:this={collapseBtnEl}
            type="button"
            class="cpd-tool cpd-collapse"
            aria-label="Collapse dashboard"
            title="Collapse"
            onclick={collapse}
          >
            <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 14 10 14 10 20" /><polyline points="20 10 14 10 14 4" /><line x1="14" y1="10" x2="21" y2="3" /><line x1="3" y1="21" x2="10" y2="14" /></svg>
            Collapse
          </button>
        </div>
      </header>
    {/if}

    <!-- ONE bucket host — rendered exactly once; class toggles switch it between
         the compact stacked block and the dashboard rail+main split. The
         components inside are the SAME instances in both layouts. -->
    <div class="copilot-body" class:dashboard>
      {@render buckets()}
    </div>
  </div>
</section>

<style>
  .copilot-panel {
    width: 100%;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
    /* NOT overflow:hidden — the CRM contact-search dropdown is absolutely
       positioned and extends below the (short) CRM tile; clipping would hide
       it. Each scroll region manages its own overflow. */
    overflow: visible;
    position: relative;
    z-index: 5;
  }
  /* When the dashboard layer is up the compact shell has no visible content —
     drop the border so the section doesn't paint a stray outline behind it. */
  .copilot-panel.is-dashboard {
    border-color: transparent;
    background: transparent;
  }

  /* Visually-hidden live region (component-scoped — no app.css touch). */
  .cp-sr {
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

  /* ── Compact header (title · mode chip · Expand) ──────────────────────── */
  .copilot-tabrow {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.55rem 0.7rem;
    border-bottom: 1px solid var(--hairline);
  }
  .cp-title {
    font-size: 0.64rem;
    letter-spacing: 0.07em;
    text-transform: uppercase;
    color: var(--bone-3);
    min-width: 0;
  }
  .copilot-tabrow-right {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-left: auto;
    flex-shrink: 0;
  }

  /* ── Expand / collapse controls ───────────────────────────────────────── */
  .cp-expand,
  .cpd-collapse {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }
  .cp-expand {
    justify-content: center;
    width: 1.9rem;
    height: 1.9rem;
    padding: 0;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    background: var(--ink-1);
    color: var(--bone-2);
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .cp-expand:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .cp-expand:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* ── Quiet mode indicator (demoted from the P5a segmented pills) ───────── */
  .cp-mode {
    position: relative;
    display: inline-flex;
  }
  .cp-mode-indicator {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0.28rem 0.5rem;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--bone-1);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s, color 0.15s;
  }
  .cp-mode-indicator:hover {
    background: var(--ink-2);
    border-color: var(--hairline);
  }
  .cp-mode-indicator[aria-expanded="true"] {
    background: var(--ink-2);
    border-color: var(--hairline-hi);
  }
  .cp-mode-indicator:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cp-mode-glyph {
    display: inline-flex;
    align-items: center;
    color: var(--bone-3);
    flex-shrink: 0;
  }
  .cp-mode-word {
    color: var(--bone-0);
    white-space: nowrap;
  }
  .cp-mode-src {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--bone-3);
    white-space: nowrap;
  }
  .cp-mode-caret {
    color: var(--bone-3);
    flex-shrink: 0;
  }

  /* Override popover — the ChipMenu/CrmContext popover vocabulary, scoped. */
  .cp-mode-menu {
    position: absolute;
    top: calc(100% + 0.35rem);
    right: 0;
    z-index: 30;
    min-width: 190px;
    padding: 0.3rem;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-1);
    box-shadow: 0 14px 30px -10px rgba(0, 0, 0, 0.55);
    animation: cp-menu-in 100ms ease-out both;
  }
  .cp-mode-strip .cp-mode-menu {
    right: auto;
    left: 0;
  }
  @media (prefers-reduced-motion: reduce) {
    .cp-mode-menu {
      animation: none;
    }
  }
  @keyframes cp-menu-in {
    from {
      opacity: 0;
      transform: translateY(-2px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  .cp-mode-menu-head {
    margin: 0.1rem 0.45rem 0.35rem;
    font-size: 0.64rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .cp-mode-item {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    width: 100%;
    padding: 0.4rem 0.5rem;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: var(--bone-1);
    font: inherit;
    font-size: 0.82rem;
    text-align: left;
    cursor: pointer;
    transition: background 120ms linear, color 120ms linear;
  }
  .cp-mode-item:hover,
  .cp-mode-item:focus-visible {
    background: var(--ink-2);
    color: var(--bone-0);
    outline: none;
  }
  .cp-mode-item.on {
    color: var(--accent-hi);
  }
  .cp-mode-check {
    display: inline-flex;
    justify-content: center;
    width: 0.9rem;
    flex-shrink: 0;
    color: var(--accent);
  }

  /* ── Persistent lane shell (#662 BC-1 + full-screen portal fix) ──────────
     Compact: a transparent passthrough (`display:contents`). Dashboard: this
     SAME element is portaled to <body> and promoted to the maximized layer. */
  .lane-shell {
    display: contents;
  }
  .lane-shell.dashboard {
    position: fixed;
    /* Below the topstrip so crumbs + window controls stay live. `--cpd-top`
       is the MEASURED topstrip height; fall back to the token floor. */
    top: var(--cpd-top, var(--topbar-h));
    left: 0;
    right: 0;
    bottom: 0;
    /* Portaled to <body> (see `use:portal={dashboard}`), so this z-index
       competes at the document ROOT — out-ranking the recording floater (80)
       and toasts/modals (60-70). This is the shipped full-screen fix; DO NOT
       lower it or un-portal the layer. */
    z-index: 90;
    display: flex;
    flex-direction: column;
    min-height: 0;
    background: var(--ink-0);
    animation: cpd-layer-in 140ms ease-out both;
  }
  @keyframes cpd-layer-in {
    from {
      opacity: 0;
      transform: scale(0.98);
    }
    to {
      opacity: 1;
      transform: scale(1);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .lane-shell.dashboard {
      animation: none;
    }
  }

  /* Command strip — sticky, non-shrinking header over the buckets. */
  .cpd-strip {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex-shrink: 0;
    padding: 0.5rem 0.9rem;
    border-bottom: 1px solid var(--hairline);
    background: var(--ink-0);
  }
  .cpd-record {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    min-width: 0;
  }
  .cpd-pip {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--bone-4);
    flex-shrink: 0;
  }
  .cpd-pip.live {
    background: var(--live);
    animation: cpd-live-pulse 1.4s ease-in-out infinite;
  }
  .cpd-pip.done {
    background: var(--olive);
  }
  @keyframes cpd-live-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 var(--live-soft);
    }
    50% {
      box-shadow: 0 0 0 5px rgba(0, 0, 0, 0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .cpd-pip.live {
      animation: none;
    }
  }
  .cpd-timer {
    font-family: var(--font-mono);
    font-size: 0.82rem;
    color: var(--bone-1);
    font-variant-numeric: tabular-nums;
  }
  .cpd-stop {
    display: inline-flex;
    align-items: center;
    padding: 0.32rem 0.8rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .cpd-stop:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .cpd-stop.live {
    border-color: var(--live);
    color: var(--live);
    background: var(--live-soft);
  }
  .cpd-stop:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  /* Centre the mode control in the strip. */
  .cpd-strip .cp-mode {
    margin: 0 auto;
  }
  .cpd-right {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .cpd-tool {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.32rem 0.65rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .cpd-tool:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .cpd-tool:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* ── Bucket host — compact stacked block; dashboard rail+main split ────── */
  .copilot-body {
    display: flex;
    flex-direction: column;
  }
  .copilot-body.dashboard {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 300px minmax(0, 1fr);
    gap: 0;
  }

  /* The transcript lane re-uses the shared `.live-panel` surface (its own
     border + bg). Inside the drawer that would double-chrome, so neutralize
     the outer border/bg contextually (scoped :global — app.css untouched). */
  .copilot-body :global(.live-panel) {
    border: none;
    border-radius: 0;
    background: transparent;
  }

  /* ── 1 · Reference rail ───────────────────────────────────────────────── */
  .cp-rail {
    display: flex;
    flex-direction: row;
    flex-wrap: wrap;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.7rem;
    border-bottom: 1px solid var(--hairline);
  }
  .cp-rail.dashboard {
    flex-direction: column;
    flex-wrap: nowrap;
    align-items: stretch;
    min-height: 0;
    overflow-y: auto;
    border-bottom: none;
    border-right: 1px solid var(--hairline);
  }
  /* Compact strip: CRM takes the lead cell; talk + checklist share the rest.
     Stretch the tile boxes to a common height per row so the compact strip
     reads as an even row rather than ragged bottoms (the rail's own
     `align-items` is flex-start compact / stretch dashboard; this override
     keeps the tiles even in both, and leaves the sentiment read untouched). */
  .cp-tile {
    min-width: 0;
    align-self: stretch;
  }
  .cp-tile-crm {
    flex: 2 1 15rem;
  }
  .cp-tile-talk,
  .cp-tile-check {
    flex: 1 1 11rem;
  }
  .cp-rail.dashboard .cp-tile {
    flex: none;
  }
  .cp-tile-head {
    font-size: 0.62rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    margin-bottom: 0.4rem;
  }
  .cp-tile-talk {
    padding: 0.2rem 0.15rem;
  }
  .cp-rail.dashboard .cp-tile-talk {
    padding: 0.2rem 0.75rem 0.6rem;
  }

  /* Subtle sentiment — ONE quiet dot + word (founder-kept, de-duplicated). */
  .cp-sent {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.15rem 0.15rem;
  }
  .cp-rail.dashboard .cp-sent {
    margin-top: auto;
    padding: 0.5rem 0.75rem;
    border-top: 1px solid var(--hairline);
  }
  .cp-sent-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--bone-3);
    flex-shrink: 0;
    transition: background 150ms ease;
  }
  .cp-sent-dot.pos {
    background: var(--olive);
  }
  .cp-sent-dot.neg {
    background: var(--live);
    box-shadow: 0 0 0 3px var(--live-soft);
  }
  .cp-sent-dot.done {
    background: var(--olive);
  }
  .cp-sent-word {
    font-size: 0.74rem;
    color: var(--bone-2);
  }
  @media (prefers-reduced-motion: reduce) {
    .cp-sent-dot {
      transition: none;
    }
  }

  /* ── 2 + 3 · Main (cue over drawer) ───────────────────────────────────── */
  .cp-main {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .cp-main.dashboard {
    min-height: 0;
    overflow: hidden;
  }
  .cp-cue-host {
    flex-shrink: 0;
  }
  /* Dashboard: with the drawer collapsed (or absent) the cue host absorbs the
     leftover column height so a short/idle cue doesn't leave a dead gap beneath
     it (the grid cell is as tall as the rail). An OPEN drawer takes the space
     instead (`.cp-drawer.open { flex: 1 }` below), so revert the cue host to its
     content size then. */
  .cp-main.dashboard .cp-cue-host {
    flex: 1;
    min-height: 0;
    border-bottom: 1px solid var(--hairline);
  }
  .cp-main.dashboard:has(.cp-drawer.open) .cp-cue-host {
    flex: 0 0 auto;
  }

  /* Drawer — collapsed by default; the transcript stays MOUNTED (the body is
     hidden via the `hidden` attribute, not `{#if}`), so scroll/pin survive. */
  .cp-drawer {
    display: flex;
    flex-direction: column;
    min-width: 0;
    border-top: 1px solid var(--hairline);
  }
  .cp-drawer-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.45rem 0.7rem;
    flex-shrink: 0;
  }
  .cp-drawer-toggle {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.28rem 0.4rem;
    border: none;
    background: transparent;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    border-radius: var(--radius-sm);
    transition: color 0.15s;
  }
  .cp-drawer-toggle:hover {
    color: var(--bone-0);
  }
  .cp-drawer-toggle:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cp-drawer-chevron {
    color: var(--bone-3);
    flex-shrink: 0;
    transition: transform 150ms ease;
  }
  .cp-drawer-chevron.open {
    transform: rotate(90deg);
  }
  @media (prefers-reduced-motion: reduce) {
    .cp-drawer-chevron {
      transition: none;
    }
  }
  .cp-drawer-count {
    font-family: var(--font-mono);
    font-size: 0.66rem;
    color: var(--bone-3);
    background: var(--ink-2);
    border-radius: 999px;
    padding: 0.02rem 0.4rem;
  }
  .cp-catchup {
    margin-left: auto;
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    padding: 0.3rem 0.75rem;
    border: 1px solid var(--accent);
    border-radius: 999px;
    background: var(--accent-soft);
    color: var(--accent-hi);
    font: inherit;
    font-size: 0.76rem;
    cursor: pointer;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
  }
  .cp-catchup:hover:not(:disabled) {
    background: var(--accent);
    color: var(--ink-0);
  }
  .cp-catchup:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cp-catchup:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .cp-drawer-body {
    display: flex;
    flex-direction: column;
    min-width: 0;
    padding: 0 0.4rem 0.5rem;
  }
  /* `hidden` must win over the flex display above (attribute selector out-ranks
     the class), so the collapsed drawer keeps the transcript mounted but unseen. */
  .cp-drawer-body[hidden] {
    display: none;
  }

  /* Dashboard: an open drawer fills the remaining height and the transcript
     stream scrolls internally (ask-chips pinned at top). Contextual :global
     restyles only — no lane-internal change, no app.css touch. */
  .cp-main.dashboard .cp-drawer.open {
    flex: 1;
    min-height: 0;
  }
  .cp-main.dashboard .cp-drawer.open .cp-drawer-body {
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }
  .cp-main.dashboard .cp-drawer.open :global(.live-panel) {
    flex: 1;
    min-height: 0;
    height: 100%;
  }
  .cp-main.dashboard .cp-drawer.open :global(.live-stream-wrap) {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .cp-main.dashboard .cp-drawer.open :global(.live-stream) {
    flex: 1;
    min-height: 0;
    max-height: none;
  }

  /* ── Support-mode Knowledge (in the drawer) ───────────────────────────── */
  .cp-kb {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.4rem 0.35rem 0.2rem;
  }
  .cp-kb-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .cp-kb-label {
    font-size: 0.62rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .cp-kb-ask {
    margin-left: auto;
    padding: 0.28rem 0.7rem;
    border: 1px solid var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
    border-radius: 999px;
    font: inherit;
    font-size: 0.74rem;
    cursor: pointer;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
  }
  .cp-kb-ask:hover:not(:disabled) {
    background: var(--accent);
    color: var(--ink-0);
  }
  .cp-kb-ask:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cp-kb-ask:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .cp-kb-answer {
    position: relative;
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--accent);
    border-radius: var(--radius-sm);
    padding: 0.55rem 1.6rem 0.6rem 0.7rem;
  }
  .cp-kb-dismiss {
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
  .cp-kb-dismiss:hover {
    color: var(--bone-0);
  }
  .cp-kb-dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .cp-kb-answer-text {
    margin: 0;
    font-size: 0.85rem;
    line-height: 1.4;
    color: var(--bone-0);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }
  .cp-kb-sources {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.35rem;
    margin-top: 0.5rem;
  }
  .cp-kb-source {
    font-size: 0.72rem;
    color: var(--bone-1);
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 0.08rem 0.5rem;
    overflow-wrap: anywhere;
  }

  /* ── Responsive (≤680px) — both shapes converge to a single column ────── */
  @media (max-width: 680px) {
    .copilot-body.dashboard {
      display: flex;
      flex-direction: column;
    }
    .cp-rail,
    .cp-rail.dashboard {
      flex-direction: column;
      flex-wrap: nowrap;
      align-items: stretch;
      overflow-y: visible;
      border-right: none;
      border-bottom: 1px solid var(--hairline);
    }
    .cp-tile-crm,
    .cp-tile-talk,
    .cp-tile-check {
      flex: none;
    }
    .cp-rail.dashboard .cp-sent {
      margin-top: 0;
    }
    /* On a single narrow column the transcript scrolls with the drawer too. */
    .cp-main.dashboard {
      overflow: visible;
    }
    .cp-main.dashboard .cp-drawer.open :global(.live-stream) {
      max-height: 50vh;
    }
  }
</style>
