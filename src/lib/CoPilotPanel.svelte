<!--
  CoPilotPanel — #653 in-call co-pilot surface, #662 dashboard + auto-mode.

  Composes three lanes on the Record page (Call mode, copilot flag ON):
  Transcript (verbatim Phase-1 draft), Contact (Zoho CRM context + "who are you
  calling?" picker), and Coaching (live coaching + checklist + cues/KB).

  #662 — two shapes, ONE set of lane instances:
    • COMPACT (default): today's tabbed view — only the active lane is shown
      (the others carry `display:none` + `hidden`), tab row + a QUIET mode chip
      + an Expand (⤢) control.
    • DASHBOARD: an in-app maximized layer (`position:fixed` below the topstrip)
      showing all lanes at once in a 3-column grid under a slim command strip;
      a Collapse (⤡) control returns to compact.
  The layout switch is a PURE CSS restyle of the same mounted lanes — the lanes
  NEVER unmount on toggle, so expand/collapse is instant and preserves the
  picked contact, hydrated Deals/Cases, transcript scroll, and coaching
  expand-overrides (zero re-fetch). This is why the body is always-mounted, not
  `{#if activeTab}`.

  #662 — auto-mode: the persona (Sales/Support) is INFERRED here (single owner)
  from open Deals/Cases counts (raised by CrmContextLane's counts-only
  `oncounts`, no PII) + the coach frame's `posture`. The prominent P5a segmented
  pills are DEMOTED to a quiet indicator + a small override popover. A manual
  pick freezes inference for the call; "Follow detection" resumes it.

  Mount/width are owned by +page.svelte (compact panel is width:100% inside the
  widened `.page.copilot-active` column; the dashboard is its own fixed layer).
  All chrome is component-scoped; the only shared classes are the transcript
  lane's `.live-*` family (read-only) and `.avatar`. No app.css touch → hard-rule
  #1 never engaged.
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
  import { onDestroy, untrack } from "svelte";
  import LiveTranscriptLane from "$lib/LiveTranscriptLane.svelte";
  import CrmContextLane from "$lib/CrmContextLane.svelte";
  import IntelligenceLane from "$lib/IntelligenceLane.svelte";
  import type {
    LiveSegment,
    CoachingUpdate,
    LiveCue,
    ChecklistSnapshot,
    AskChip,
    KnowledgeAnswer,
  } from "@aftercalls/shared/types";
  import { liveSession } from "$lib/stores/liveSession.svelte";
  import type {
    AskAnswerState,
    TalkMetric,
  } from "$lib/stores/liveSession.svelte";

  type LiveStatus = "idle" | "live" | "ended" | "error";
  type Tab = "transcript" | "contact" | "coaching";
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
    // #662 — command-strip record cluster (Stop/Start + mono timer). Threaded
    // from +page (which owns the recorder toggle + elapsed clock).
    elapsedMs = 0,
    onToggleRecording = undefined,
    onnotes = undefined,
    // #660 co-pilot P1 — ask/highlight/talk state + callbacks, threaded from
    // +page (which owns the Tauri invokes + store writes) straight into the
    // Transcript lane. All optional so the panel still mounts when the P1
    // surfaces aren't wired.
    askAnswer = null,
    askInFlight = null,
    knowledgeAnswer = null,
    knowledgeInFlight = false,
    highlighted = new Set<string>(),
    talkMetric = null,
    onask = undefined,
    ondismissAsk = undefined,
    onknowledge = undefined,
    ondismissKnowledge = undefined,
    onhighlight = undefined,
    onpick,
  }: {
    segments?: LiveSegment[];
    status?: LiveStatus;
    recording?: boolean;
    isAdmin?: boolean;
    /** #659 P5a — the org's default co-pilot persona (from /auth/me). The
     *  bias/fallback when auto-inference has no decision. */
    defaultMode?: CopilotMode;
    /** #654 — latest live coaching snapshot (null pre-first-update / cleared
     *  on new session). Threaded straight into the Coaching lane; #662 also
     *  reads `coaching.posture` for auto-mode inference. */
    coaching?: CoachingUpdate | null;
    /** #659 (P2) — fast-lane cues (battlecards + deal-risk), threaded into the
     *  Coaching lane's live-cue section above the reflective cards. */
    cues?: LiveCue[];
    /** #659 (P3) — auto-checking agenda checklist, threaded into the Coaching
     *  lane's pinned checklist section (above the cues). */
    checklist?: ChecklistSnapshot | null;
    // Whether the org's live_transcript flag is ON. The Transcript lane is
    // only meaningful when it is: the live relay is gated on that flag alone
    // (backend lib.rs begin()), so with copilot ON + live_transcript OFF the
    // lane would strand on "Listening…" forever. When false we drop the
    // Transcript tab / column and never auto-advance to it.
    liveTranscriptEnabled?: boolean;
    sessionUuid?: string | null;
    /** #662 — recorder elapsed clock (ms), formatted mono in the command
     *  strip. Ignored in compact. */
    elapsedMs?: number;
    /** #662 — flip the recorder (Stop/Start) from the dashboard command strip.
     *  When absent, the strip's Stop/Start control is hidden. */
    onToggleRecording?: () => void;
    /** #662 — reach Notes from the command strip: collapses the dashboard and
     *  asks +page to open the notes panel. When absent, the Notes control is
     *  hidden. */
    onnotes?: () => void;
    askAnswer?: AskAnswerState | null;
    askInFlight?: AskChip | null;
    /** #659 P5b — Support-mode cited knowledge answer + its in-flight guard,
     *  threaded into the Coaching lane. Rendered only in Support mode. */
    knowledgeAnswer?: KnowledgeAnswer | null;
    knowledgeInFlight?: boolean;
    highlighted?: Set<string>;
    talkMetric?: TalkMetric | null;
    onask?: (chip: AskChip) => void;
    ondismissAsk?: () => void;
    /** #659 P5b — fire one Support-mode knowledge answer / dismiss the slot. */
    onknowledge?: () => void;
    ondismissKnowledge?: () => void;
    onhighlight?: (seg: LiveSegment, starred: boolean) => void;
    // Raised up from CrmContextLane; +page threads it into start_recording.
    onpick: (contactId: string | null) => void;
  } = $props();

  // ── Layout (#662) ─────────────────────────────────────────────────────────
  // Panel-local, NOT persisted: default compact each session (expand is a
  // deliberate "go big" gesture). Never `{#if}`-swap the lanes on toggle — a
  // pure CSS restyle keeps every lane instance mounted (zero re-fetch, scroll +
  // expand-override state preserved).
  let layout = $state<Layout>("compact");
  let dashboard = $derived(layout === "dashboard");
  let expandBtnEl = $state<HTMLButtonElement | null>(null);
  let collapseBtnEl = $state<HTMLButtonElement | null>(null);
  let layerEl = $state<HTMLElement | null>(null);

  function expand() {
    layout = "dashboard";
    // Move focus into the layer (the Collapse control) once it paints.
    queueMicrotask(() => collapseBtnEl?.focus());
  }
  function collapse() {
    layout = "compact";
    modeMenuOpen = false;
    // Restore focus to the Expand control on the compact panel.
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

  // Display order: Transcript · Contact · Coaching. The Transcript tab/column
  // is present only when live_transcript is ON (else the relay never starts and
  // the lane is dead). Default active lane = Contact pre-call (the picker is the
  // first action the flow needs), and it is always present.
  const LABELS: Record<Tab, string> = {
    transcript: "Transcript",
    contact: "Contact",
    coaching: "Coaching",
  };
  let TABS: Tab[] = $derived(
    liveTranscriptEnabled
      ? ["transcript", "contact", "coaching"]
      : ["contact", "coaching"],
  );
  let activeTab = $state<Tab>("contact");
  // A lane wrapper is hidden ONLY in compact when it isn't the active tab; in
  // the dashboard every lane is shown at once.
  function laneHidden(t: Tab): boolean {
    return !dashboard && activeTab !== t;
  }

  // ── Auto-mode brain (#662, single owner) ──────────────────────────────────
  const MODE_LABELS: Record<CopilotMode, string> = {
    sales: "Sales",
    support: "Support",
  };
  const MODE_OPTIONS: CopilotMode[] = ["sales", "support"];

  // `modeOverride` is null until the user pins a persona (then `userSet`
  // freezes inference for the call). `inferredMode` is the committed inferred
  // value (null until the first decision). `effectiveMode` reads override →
  // inferred → org default, and flows into BOTH lanes exactly as `mode` did.
  let modeOverride = $state<CopilotMode | null>(null);
  let userSet = $state(false);
  let inferredMode = $state<CopilotMode | null>(null);
  let effectiveMode = $derived<CopilotMode>(
    modeOverride ?? inferredMode ?? defaultMode,
  );

  // Counts-only CRM signal raised from CrmContextLane (NO PII). Available at
  // contact-pick, pre-transcript — bootstraps the inference before any frame.
  let crmCounts = $state<CrmCounts | null>(null);
  function handleCounts(c: CrmCounts) {
    crmCounts = c;
  }

  // Confirm-gate cursor for a posture-driven change. Plain (non-reactive) so
  // writing it never re-triggers the inference effect.
  let pendingCandidate: CopilotMode | null = null;

  // Inference effect — re-runs on the CRM counts OR a fresh coaching frame
  // (posture may refine). Bootstraps immediately from CRM (pre-transcript);
  // a posture-driven CHANGE away from the committed value requires TWO agreeing
  // evaluations (~40s / two frames) before it flips; a manual override freezes
  // the whole thing. CRM inventory is static after hydrate, so the practical
  // flip source is posture — a single anomalous read can't swing the persona.
  $effect(() => {
    const counts = crmCounts;
    const posture = coaching?.posture ?? null;
    // Read `seq` so a same-VALUE posture on a NEW frame still ticks the gate.
    void coaching?.seq;
    untrack(() => {
      if (userSet) return; // a manual choice sticks for the whole call
      const candidate = inferOnce(counts, posture);
      if (candidate === null) {
        pendingCandidate = null; // no decision — keep current, clear the gate
        return;
      }
      if (inferredMode === null) {
        inferredMode = candidate; // bootstrap immediately (first paint)
        pendingCandidate = null;
        return;
      }
      if (candidate === inferredMode) {
        pendingCandidate = null; // reaffirmed — clear any pending flip
        return;
      }
      // Change away from the committed value → 2-frame confirm.
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

  // Detection currently sees … (null when no clear signal / no contact yet).
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
    // A manual pick pins the persona and freezes inference for the call.
    modeOverride = m;
    userSet = true;
    pendingCandidate = null;
    closeModeMenu();
  }
  function followDetection() {
    // Clear the override → back to auto; re-seed the inferred value now from
    // the current signals so the indicator updates instantly (don't wait a
    // coaching cycle).
    modeOverride = null;
    userSet = false;
    pendingCandidate = null;
    inferredMode = inferOnce(crmCounts, coaching?.posture ?? null);
    closeModeMenu();
  }
  // Roving arrow-nav within the 3-item popover (Follow detection / Sales /
  // Support); Enter/Space activate on the item's own onclick; Escape closes.
  function onModeMenuKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      // Don't let the layer's Escape handler ALSO collapse the dashboard —
      // one Escape closes the menu only.
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
  // Capture-phase outside-click dismissal (mirrors CrmContextLane / ChipMenu).
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

  // Polite announcement of an auto- or manual mode change (single net-new live
  // region for the whole panel; the lanes own their own regions).
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

  // ── Notes (command strip) ──────────────────────────────────────────────────
  function openNotes() {
    // Collapse to the record page so the existing NotesPanel is reachable.
    collapse();
    onnotes?.();
  }

  // Contact-tab pip: filled once a contact is committed. Tracked here so the
  // pip stays lit while the picker is re-opened via "Change".
  let hasContact = $state(false);
  function handlePick(contactId: string | null) {
    hasContact = contactId !== null;
    onpick(contactId);
  }

  // On the FIRST transition into recording, auto-select Transcript once
  // (the live surface). Respect any manual switch thereafter — no repeated
  // yanking (ui.md §2). Only when live_transcript is ON — otherwise there is
  // no relay and no Transcript tab to advance to, so Contact stays put. Only
  // matters in compact (the dashboard shows every lane at once).
  let advancedOnce = $state(false);
  $effect(() => {
    if (liveTranscriptEnabled && recording && !advancedOnce) {
      activeTab = "transcript";
      advancedOnce = true;
    }
  });

  let tabEls: Record<string, HTMLButtonElement | null> = {};

  function selectTab(t: Tab) {
    activeTab = t;
  }

  // Roving tabindex + arrow-key nav. Selection follows focus (automatic
  // activation) — the lanes are cheap to switch, so there's no reason to
  // require a separate Enter/Space to commit.
  function onTabKeydown(e: KeyboardEvent, t: Tab) {
    const idx = TABS.indexOf(t);
    let next = -1;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") next = (idx + 1) % TABS.length;
    else if (e.key === "ArrowLeft" || e.key === "ArrowUp")
      next = (idx - 1 + TABS.length) % TABS.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = TABS.length - 1;
    else return;
    e.preventDefault();
    const nt = TABS[next];
    activeTab = nt;
    tabEls[nt]?.focus();
  }

  // Transcript pip mirrors the live-status vocabulary.
  let transcriptPip = $derived(
    status === "error"
      ? "err"
      : status === "ended"
        ? "done"
        : status === "live"
          ? "live"
          : "idle",
  );

  // #654 — Coaching-tab pip. "This lane has content" (steady accent) once cards
  // exist; a brief one-shot pulse on each fresh `seq` to pull attention when
  // the user is on another tab; olive on ended (if cards). NO sentiment tint on
  // the tab pip (that would over-alarm + collide with Transcript's red=error).
  let coachingHasCards = $derived(!!coaching && coaching.cards.length > 0);
  // #659 (P5c-F2) — EFFECTIVE checklist progress, so BOTH personas can light the
  // pip. Discovery (auto-tick) uses the wire `covered_count`. Compliance is
  // `confirm_required`, so its items ship as `"likely"` and the wire
  // `covered_count` is ALWAYS 0 — count a model-suggested `"likely"` item OR one
  // the rep confirmed (the `checklistConfirmed` overlay) as activity worth a
  // glance. Reading the raw `covered_count` alone (the old check) meant a Support
  // call's checklist never contributed to the pip.
  let checklistProgress = $derived.by(() => {
    if (!checklist) return 0;
    if (!checklist.confirm_required) return checklist.covered_count;
    const confirmed = liveSession.checklistConfirmed;
    return checklist.items.filter(
      (it) => it.state === "likely" || confirmed.has(it.id),
    ).length;
  });
  // #659 (P2/P3/P5c) — the Coaching lane also hosts fast cues + the agenda
  // checklist, so the tab pip lights on reflective cards, live cues, OR actual
  // checklist progress (a covered/confirmed/likely item — an all-pending agenda
  // alone doesn't count as "content worth glancing at").
  let coachingLaneHasContent = $derived(
    coachingHasCards || cues.length > 0 || checklistProgress > 0,
  );
  let coachPulse = $state(false);
  let prevCoachSeq: number | null = null;
  let coachPulseTimer = 0;
  $effect(() => {
    const seq = coaching?.seq ?? null;
    if (seq === null) {
      prevCoachSeq = null;
      return;
    }
    if (seq !== prevCoachSeq) {
      prevCoachSeq = seq;
      coachPulse = true;
      clearTimeout(coachPulseTimer);
      coachPulseTimer = window.setTimeout(() => (coachPulse = false), 1400);
    }
  });
  // #659 (P2) — a separate one-shot pulse when a FRESH fast cue arrives (a
  // new id), so a battlecard that lands while the user is on another tab still
  // pulls the eye. Tracks the newest cue id; a resend of the same id is inert.
  let cuePulse = $state(false);
  let prevTopCueId: string | null = null;
  let cuePulseTimer = 0;
  $effect(() => {
    const topId = cues.length > 0 ? cues[cues.length - 1].id : null;
    if (topId === null) {
      prevTopCueId = null;
      return;
    }
    if (topId !== prevTopCueId) {
      prevTopCueId = topId;
      cuePulse = true;
      clearTimeout(cuePulseTimer);
      cuePulseTimer = window.setTimeout(() => (cuePulse = false), 1400);
    }
  });
  onDestroy(() => {
    clearTimeout(coachPulseTimer);
    clearTimeout(cuePulseTimer);
  });

  // Mono call timer for the command strip (compact panel keeps the record
  // controls on the record page; only the dashboard strip renders this).
  function fmtTimer(ms: number): string {
    const s = Math.floor(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const r = s % 60;
    const pad = (n: number) => String(n).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(r)}` : `${pad(m)}:${pad(r)}`;
  }
</script>

<!-- ── Quiet mode indicator + override popover (shared by both layouts) ──────
     A neutral label (compass glyph + persona word + caret + · auto/· manual),
     NOT a segmented toggle. Activating opens a small popover: Follow detection
     / Sales / Support. Reuses the ChipMenu/CrmContext popover vocabulary,
     component-scoped. `variant` only tweaks the chrome (chip vs strip). -->
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
        >· {modeOverride === null ? "auto" : "manual"}</span
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

<!-- ── Always-mounted lanes (ONE instance each; CSS repositions them) ────────
     Compact = the active lane only (others display:none + hidden). Dashboard =
     all three in the grid. The lanes NEVER unmount on layout toggle. -->
{#snippet lanes()}
  {#if liveTranscriptEnabled}
    <section
      class="cp-lane cp-lane-transcript"
      class:dashboard
      id="cp-panel-transcript"
      role={dashboard ? "region" : "tabpanel"}
      aria-label={dashboard ? "Transcript" : undefined}
      aria-labelledby={dashboard ? undefined : "cp-tab-transcript"}
      hidden={laneHidden("transcript")}
    >
      {#if dashboard}<div class="cpd-col-head">Transcript</div>{/if}
      <LiveTranscriptLane
        {segments}
        {status}
        {sessionUuid}
        {askAnswer}
        {askInFlight}
        {highlighted}
        {talkMetric}
        {onask}
        {ondismissAsk}
        {onhighlight}
      />
    </section>
  {/if}
  <section
    class="cp-lane cp-lane-contact"
    class:dashboard
    id="cp-panel-contact"
    role={dashboard ? "region" : "tabpanel"}
    aria-label={dashboard ? "Contact" : undefined}
    aria-labelledby={dashboard ? undefined : "cp-tab-contact"}
    hidden={laneHidden("contact")}
  >
    {#if dashboard}<div class="cpd-col-head">Contact</div>{/if}
    <CrmContextLane
      onpick={handlePick}
      oncounts={handleCounts}
      {sessionUuid}
      {isAdmin}
      mode={effectiveMode}
    />
  </section>
  <section
    class="cp-lane cp-lane-coaching"
    class:dashboard
    id="cp-panel-coaching"
    role={dashboard ? "region" : "tabpanel"}
    aria-label={dashboard ? "Coaching" : undefined}
    aria-labelledby={dashboard ? undefined : "cp-tab-coaching"}
    hidden={laneHidden("coaching")}
  >
    {#if dashboard}<div class="cpd-col-head">Coaching</div>{/if}
    <IntelligenceLane
      {coaching}
      {status}
      {cues}
      {checklist}
      mode={effectiveMode}
      {knowledgeAnswer}
      {knowledgeInFlight}
      {onknowledge}
      ondismissKnowledge={ondismissKnowledge}
    />
  </section>
{/snippet}

<section class="copilot-panel" class:is-dashboard={dashboard} style="--i: 2.5">
  <!-- One net-new polite region for the whole panel (mode change); the lanes
       own their own live regions and must not be duplicated. -->
  <span class="cp-sr" aria-live="polite">{modeAnnounce}</span>

  {#if !dashboard}
    <!-- COMPACT chrome — the tab row demotes the mode control to a quiet chip
         and gains an Expand (⤢) affordance on the right. The lanes do NOT live
         here; they're in the always-mounted shell below, so toggling layout is
         a pure CSS restyle and never remounts them. -->
    <div class="copilot-tabrow">
      <div class="copilot-tabs" role="tablist" aria-label="Co-pilot lanes">
        {#each TABS as t (t)}
          <button
            bind:this={tabEls[t]}
            type="button"
            role="tab"
            id="cp-tab-{t}"
            class="lane-tab"
            class:active={activeTab === t}
            aria-selected={activeTab === t}
            aria-controls="cp-panel-{t}"
            tabindex={activeTab === t ? 0 : -1}
            onclick={() => selectTab(t)}
            onkeydown={(e) => onTabKeydown(e, t)}
          >
            <span
              class="lane-pip"
              class:live={t === "transcript" && transcriptPip === "live"}
              class:err={t === "transcript" && transcriptPip === "err"}
              class:done={(t === "transcript" && transcriptPip === "done") ||
                (t === "coaching" && status === "ended" && coachingLaneHasContent)}
              class:on={(t === "contact" && hasContact) ||
                (t === "coaching" && coachingLaneHasContent && status !== "ended")}
              class:coach-pulse={t === "coaching" && (coachPulse || cuePulse)}
              aria-hidden="true"
            ></span>
            {LABELS[t]}
          </button>
        {/each}
      </div>

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

  <!-- ── Persistent lane shell (#662 BC-1) ────────────────────────────────
       ONE home for the lanes across BOTH layouts, OUTSIDE the layout `{#if}`.
       Compact: a transparent passthrough (`display:contents`) so `.copilot-body`
       renders inline exactly as a direct panel child. Dashboard: this SAME
       element is promoted to the in-app maximized layer (`position:fixed` below
       the topstrip; role=dialog + Escape/Tab trap + focus move/restore). Because
       the lanes' single render site never crosses an if/else boundary,
       expand/collapse keeps the SAME lane instances — picked contact, hydrated
       Deals/Cases, coaching expand-overrides, and transcript scroll all survive
       (zero re-fetch). -->
  <div
    class="lane-shell"
    class:dashboard
    bind:this={layerEl}
    role={dashboard ? "dialog" : undefined}
    aria-modal={dashboard ? "true" : undefined}
    aria-label={dashboard ? "Co-pilot dashboard" : undefined}
    tabindex="-1"
    onkeydown={dashboard ? onLayerKeydown : undefined}
  >
    {#if dashboard}
      <!-- DASHBOARD chrome — the slim command strip over the grid. -->
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
              <svg viewBox="0 0 14 14" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 2h10v10H2zM2 5h10M5 5v7" /></svg>
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

    <!-- ONE lane host — rendered exactly once; class toggles switch it between
         the compact tabbed block and the dashboard 3-column grid. The lanes
         inside are the SAME instances in both layouts (never remounted). -->
    <div
      class="copilot-body"
      class:dashboard
      class:cpd-grid={dashboard}
      class:no-transcript={dashboard && !liveTranscriptEnabled}
    >
      {@render lanes()}
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
       positioned and extends below the (short) Contact lane; clipping the
       panel would hide it. Each lane manages its own scroll (transcript /
       coaching), so the shell doesn't need to clip. */
    overflow: visible;
    /* Establish a stacking context that beats the later reveal-animated
       `.status` sibling below the panel, so the escaped dropdown paints
       above it instead of the status text bleeding through. */
    position: relative;
    z-index: 5;
  }
  /* When the dashboard layer is up the compact shell has no visible content —
     drop the border so the section doesn't paint a stray outline behind the
     fixed layer. */
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

  /* ── Compact tab row (tabs · mode chip · Expand) ──────────────────────── */
  .copilot-tabrow {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.6rem 0.7rem;
    border-bottom: 1px solid var(--hairline);
  }
  .copilot-tabs {
    display: flex;
    gap: 0.3rem;
    min-width: 0;
    flex-wrap: wrap;
  }
  .copilot-tabrow-right {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-left: auto;
    flex-shrink: 0;
  }

  /* Lane tab — the .scope-pill idiom (component-scoped per existing
     per-route precedent), plus a leading state pip. */
  .lane-tab {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.35rem 0.85rem;
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-2);
    border-radius: 999px;
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .lane-tab:hover:not(.active) {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .lane-tab.active {
    border-color: var(--accent);
    color: var(--accent-hi);
    background: var(--accent-soft);
  }
  .lane-tab:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .lane-pip {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--bone-4);
    flex-shrink: 0;
  }
  .lane-pip.on {
    background: var(--accent);
  }
  .lane-pip.live {
    background: var(--accent);
    animation: cp-pip-pulse 1.4s ease-in-out infinite;
  }
  .lane-pip.err {
    background: var(--live);
  }
  .lane-pip.done {
    background: var(--olive);
  }
  /* #654 — one-shot attention pulse on a fresh coaching frame; settles to the
     steady `.on` accent (the class is dropped after ~1.4s). */
  .lane-pip.coach-pulse {
    background: var(--accent);
    animation: cp-pip-pulse 1.4s ease-in-out;
  }
  @media (prefers-reduced-motion: reduce) {
    .lane-pip.coach-pulse {
      animation: none;
    }
  }
  @keyframes cp-pip-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 var(--accent-glow);
    }
    50% {
      box-shadow: 0 0 0 5px rgba(0, 0, 0, 0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .lane-pip.live {
      animation: none;
    }
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
  /* Neutral glyph — mode is a quiet label, not a semantic-hued state. */
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
  /* In the compact chip the menu opens under a right-aligned control; in the
     strip it is centred, so anchor it under the indicator's left edge. */
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

  /* ── Body (compact block; the dashboard grid restyles the SAME element) ── */
  .copilot-body {
    /* ≤120ms opacity cross-fade on lane switch — disciplined, not showy. */
    animation: cp-body-fade 120ms ease-out both;
  }
  @keyframes cp-body-fade {
    from {
      opacity: 0.7;
    }
    to {
      opacity: 1;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .copilot-body {
      animation: none;
    }
  }

  /* Lane wrapper — a plain block in compact; the grid column in dashboard. */
  .cp-lane[hidden] {
    display: none;
  }

  /* The transcript lane re-uses the shared `.live-panel` surface (its own
     border + bg). Inside the co-pilot shell that would double-chrome, so
     neutralize the outer border/bg contextually. Scoped to this component's
     `.copilot-body` — the shared class is unchanged (no app.css edit); this
     is a contextual override, same posture as any component-scoped tweak. */
  .copilot-body :global(.live-panel) {
    border: none;
    border-radius: 0;
    background: transparent;
  }

  /* ── Persistent lane shell (#662 BC-1) ────────────────────────────────────
     ONE home for the lanes across both layouts. Compact: a transparent
     passthrough (`display:contents`) so `.copilot-body` renders inline as a
     direct panel child (no extra box, no layout change vs. the pre-BC-1 shape).
     Dashboard: this SAME element is promoted to the in-app maximized layer.
     The lanes live inside it in both layouts and NEVER remount on toggle. */
  .lane-shell {
    display: contents;
  }
  .lane-shell.dashboard {
    position: fixed;
    /* Below the topstrip so the crumbs + window controls stay live. */
    top: var(--topbar-h);
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 55; /* above ChipMenu (30) + note overlay (50); below app modals (60) + toasts (70) */
    display: flex;
    flex-direction: column;
    min-height: 0;
    background: var(--ink-0);
    /* Fade + a gentle scale-up on open (reduced-motion → instant). */
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

  /* Command strip — sticky, non-shrinking header over the grid. overflow
     visible so the mode popover can escape it. */
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

  /* Grid — 3 reading columns, each its own scroll container, hairline
     dividers. The strip is the header; the grid fills the rest of the layer. */
  .cpd-grid {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns:
      minmax(340px, 1.5fr)
      minmax(300px, 1fr)
      minmax(340px, 1.15fr);
    gap: 0;
  }
  .cpd-grid.no-transcript {
    grid-template-columns: minmax(320px, 1fr) minmax(340px, 1.15fr);
  }
  .cp-lane.dashboard {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
    overflow-y: auto;
    border-right: 1px solid var(--hairline);
  }
  .cp-lane.dashboard:last-child {
    border-right: none;
  }
  /* Column sub-head — the design's caps-label idiom, sticky at the column top. */
  .cpd-col-head {
    position: sticky;
    top: 0;
    z-index: 1;
    padding: 0.55rem 0.9rem 0.45rem;
    font-size: 0.64rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    background: var(--ink-0);
    border-bottom: 1px solid var(--hairline);
  }

  /* Transcript = the spine: fill the column height and let the lane's own
     stream scroll internally (ask-chips pinned at top, talk-time at foot),
     rather than the column scrolling as a whole. Contextual `:global`
     restyles only — no lane-internal change, no app.css touch. */
  .cp-lane-transcript.dashboard {
    overflow: hidden;
  }
  .cp-lane-transcript.dashboard :global(.live-panel) {
    flex: 1;
    min-height: 0;
    height: 100%;
  }
  .cp-lane-transcript.dashboard :global(.live-stream-wrap) {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .cp-lane-transcript.dashboard :global(.live-stream) {
    flex: 1;
    min-height: 0;
    max-height: none;
  }

  /* ── Responsive (ui.md §5) — the layer sizes to the agent window. Below the
       3-col comfort width, drop to 2 columns (transcript spine + a stacked
       Contact/Coaching right side), then a single stacked column. CSS-only
       reflow; the compact panel stays the small-window home. ── */
  @media (max-width: 900px) {
    .cpd-grid,
    .cpd-grid.no-transcript {
      grid-template-columns: minmax(300px, 1.35fr) minmax(280px, 1fr);
      grid-auto-rows: 1fr;
    }
    /* Coaching drops under Contact in the right column. */
    .cpd-grid .cp-lane-coaching.dashboard {
      grid-column: 2;
    }
    /* Transcript keeps the spine. */
    .cpd-grid .cp-lane-transcript.dashboard {
      grid-row: 1 / span 2;
    }
  }
  @media (max-width: 680px) {
    .cpd-grid,
    .cpd-grid.no-transcript {
      grid-template-columns: 1fr;
    }
    .cpd-grid .cp-lane.dashboard {
      grid-column: 1 !important;
      grid-row: auto !important;
      border-right: none;
      border-bottom: 1px solid var(--hairline);
    }
    /* On a single narrow column the transcript scrolls with the column too. */
    .cp-lane-transcript.dashboard {
      overflow-y: auto;
    }
  }
</style>
