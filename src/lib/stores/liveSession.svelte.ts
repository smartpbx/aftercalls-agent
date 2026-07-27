// Live-session transcript store (#659).
//
// Bug B: `liveSegments` / `liveStatus` / `liveCoaching` used to be
// component-local `$state` on the Record page (`+page.svelte`), with
// their `live-*` Tauri listeners registered in the page's onMount and
// torn down in onDestroy. Navigating away from the Record route
// unmounted the page → the buffer and listeners died, while the Rust
// `LiveRelay` singleton kept running and the WS kept emitting
// `live-segment` / `live-session` / `live-coaching` frames. On return
// the page restarted from an empty buffer, dropping everything the
// call had produced so far.
//
// Lifting the stream into this persistent rune store — initialised at
// the layout level, which never unmounts across route nav — makes the
// live transcript survive navigation. The layout owns the single set
// of `live-*` listeners (they live for the app's lifetime) and drives
// `resetForNewSession()` from the `recording-state` start transition,
// exactly as the page used to.
//
// Pattern mirrors `recording.svelte.ts` / `callRecording.svelte.ts`.

import type {
  LiveSegment,
  CoachingUpdate,
  LiveCue,
  ChecklistSnapshot,
  AskChip,
  KnowledgeAnswer,
} from "@aftercalls/shared/types";

export type LiveSessionStatus = "idle" | "live" | "ended" | "error";

/** #659 (P2) — a fast-lane cue plus its client-side expiry. `expiresAt` is
 *  `Date.now() + ttl_ms` for battlecards (auto-pruned) and `null` for risk cues
 *  (they stand until the next session). The store owns all TTL pruning; there
 *  is NO server write-through for cues (they're moment-specific + stale on
 *  reload → nothing to persist, keeping the SQL surface at zero). */
export type LiveCueEntry = LiveCue & { expiresAt: number | null };

/** #660 — the single, replace-in-place inline ask answer (ui.md §1).
 *  `chip` names which preset produced it (drives the answer's micro-label);
 *  `answer` is plain text (real answer OR a calm degrade line — the two are
 *  posture-identical). `basedOnTurns` is omitted from the caption when null. */
export type AskAnswerState = {
  chip: AskChip;
  answer: string;
  basedOnTurns: number | null;
  /** #660 P1 — the backend's honest "nothing yet" flag. When true the lane
   *  renders a calm empty-state treatment (muted body + "nothing yet" tag),
   *  visually distinct from a real answer or an error/unavailable line. Defaults
   *  false (a normal answer or a degrade line). */
  empty?: boolean;
};

/** #660 — derived talk-time metric (plan §5, client-side, from segment
 *  durations). Per-channel Σ(end−start) for the share; a SAME-channel
 *  trailing delta for the You-run so the mic/system independent-clock
 *  caveat never bites (never cross-subtract mic vs system offsets). */
export type TalkMetric = {
  youMs: number;
  themMs: number;
  youPct: number;
  themPct: number;
  /** Duration of the current unbroken trailing You (mic) run; 0 when the
   *  last final was the counterpart. Drives the talk-time nudge. */
  youRunMs: number;
};

/** Same cap the Record page used (`+page.svelte` §live-segment) — the
 *  draft only needs the recent tail on screen, so a very long call
 *  can't grow this buffer unbounded. */
const SEGMENT_CAP = 500;

/** #660 — natural wire key for a highlighted turn: `channel + start_ms`
 *  (the agent never sees the backend `seq`; the UI already keys turns on
 *  this pair). Shared so the lane + store agree on the key shape. */
export function highlightKey(channel: string, startMs: number): string {
  return channel + ":" + startMs;
}

/** #659 (P2) — cap concurrent on-screen battlecards to the newest few (plan
 *  §5 — keep the surface glanceable). Risk cues are uncapped (there are at most
 *  two, and they persist). */
const MAX_BATTLECARDS = 3;

function createStore() {
  let segments = $state<LiveSegment[]>([]);
  let status = $state<LiveSessionStatus>("idle");
  let coaching = $state<CoachingUpdate | null>(null);

  // #659 (P3) — the latest auto-checking agenda checklist snapshot (full
  // replace per frame, like `coaching`). Rendered in the IntelligenceLane
  // ABOVE the fast cues + reflective cards. Session-sticky ON THE BACKEND
  // (covered ids only ever accumulate); the agent just mirrors the newest
  // snapshot. Retained through `ended` (frozen final summary); cleared on the
  // next session start.
  let checklist = $state<ChecklistSnapshot | null>(null);

  // #659 (P5c) — the set of checklist item ids the USER has confirmed as
  // covered, for a `confirm_required` (compliance) template. The backend emits
  // model-matched compliance items as `"likely"` (a suggestion, never an
  // auto-tick); the lane overlays this set to promote them to `"covered"`. Held
  // in the store (not the LiveChecklist component) because the Coaching lane
  // unmounts on a tab switch — local component state would be lost. Cleared on a
  // new session AND when the checklist template changes (a shared id like
  // `next_step` must not carry a confirm across personas). Client-only + per
  // call: a compliance confirm is a self-coaching aid, not a system of record,
  // so it is intentionally NOT persisted — a reconnect reverts items to
  // `"likely"` (the SAFE direction: never a false "done").
  let checklistConfirmed = $state<Set<string>>(new Set());

  // #659 (P2) — the fast-lane cue list (battlecards + deal-risk cues), rendered
  // in the IntelligenceLane ABOVE the reflective coaching. A single interval
  // prunes expired battlecards; it runs only while a TTL'd cue is present and
  // stops itself otherwise.
  let liveCues = $state<LiveCueEntry[]>([]);
  let cuePruneTimer: ReturnType<typeof setInterval> | null = null;

  function pruneCuesNow() {
    const now = Date.now();
    const next = liveCues.filter((c) => c.expiresAt === null || c.expiresAt > now);
    if (next.length !== liveCues.length) liveCues = next;
    // Nothing left to expire → stop the timer (restarted on the next TTL'd cue).
    if (cuePruneTimer && !next.some((c) => c.expiresAt !== null)) {
      clearInterval(cuePruneTimer);
      cuePruneTimer = null;
    }
  }
  function ensureCuePruneTimer() {
    if (cuePruneTimer) return;
    cuePruneTimer = setInterval(pruneCuesNow, 1000);
  }
  function stopCuePruneTimer() {
    if (cuePruneTimer) {
      clearInterval(cuePruneTimer);
      cuePruneTimer = null;
    }
  }

  // #660 — the live session_uuid surfaced from Rust (recording-state
  // event) so the ask/highlight surfaces can address the live session.
  let sessionUuid = $state<string | null>(null);

  // #660 — single inline ask slot + single in-flight guard (ui.md §1).
  let askAnswer = $state<AskAnswerState | null>(null);
  let askInFlight = $state<AskChip | null>(null);

  // #659 P5b — Support-mode cited knowledge answer. A single inline slot +
  // boolean in-flight guard (one "get an answer" affordance, not a chip set).
  // The answer carries its citations; both survive `ended` and clear on the
  // next session start.
  let knowledgeAnswer = $state<KnowledgeAnswer | null>(null);
  let knowledgeInFlight = $state<boolean>(false);

  // #660 — highlighted-turn set, keyed `channel + start_ms`. A plain Set,
  // reassigned on change so the rune tracks it (Svelte doesn't deep-track
  // Set mutations).
  let highlighted = $state<Set<string>>(new Set());

  // #660 — talk-share + trailing-monologue metric, derived from FINAL
  // segments only (provisionals are drafts). Per-channel durations for the
  // share; a same-channel trailing delta for the You-run.
  const talkMetric = $derived.by<TalkMetric>(() => {
    let youMs = 0;
    let themMs = 0;
    const finals: LiveSegment[] = [];
    for (const s of segments) {
      if (s.provisional) continue;
      finals.push(s);
      const d = Math.max(0, s.end_ms - s.start_ms);
      if (s.channel === "mic") youMs += d;
      else themMs += d;
    }
    // Trailing unbroken You (mic) run — walk the tail while channel stays
    // mic; duration = last mic end_ms − first mic start_ms OF THAT RUN
    // (same-channel delta; never subtract across the two upstream clocks).
    let youRunMs = 0;
    let i = finals.length - 1;
    if (i >= 0 && finals[i].channel === "mic") {
      const lastEnd = finals[i].end_ms;
      let firstStart = finals[i].start_ms;
      while (i >= 0 && finals[i].channel === "mic") {
        firstStart = finals[i].start_ms;
        i--;
      }
      youRunMs = Math.max(0, lastEnd - firstStart);
    }
    const total = youMs + themMs;
    return {
      youMs,
      themMs,
      youPct: total > 0 ? Math.round((youMs / total) * 100) : 0,
      themPct: total > 0 ? Math.round((themMs / total) * 100) : 0,
      youRunMs,
    };
  });

  return {
    get segments(): LiveSegment[] {
      return segments;
    },
    get status(): LiveSessionStatus {
      return status;
    },
    get coaching(): CoachingUpdate | null {
      return coaching;
    },
    get checklist(): ChecklistSnapshot | null {
      return checklist;
    },
    get checklistConfirmed(): Set<string> {
      return checklistConfirmed;
    },
    get liveCues(): LiveCueEntry[] {
      return liveCues;
    },
    get sessionUuid(): string | null {
      return sessionUuid;
    },
    get askAnswer(): AskAnswerState | null {
      return askAnswer;
    },
    get askInFlight(): AskChip | null {
      return askInFlight;
    },
    get knowledgeAnswer(): KnowledgeAnswer | null {
      return knowledgeAnswer;
    },
    get knowledgeInFlight(): boolean {
      return knowledgeInFlight;
    },
    get highlighted(): Set<string> {
      return highlighted;
    },
    get talkMetric(): TalkMetric {
      return talkMetric;
    },

    /** Append one live segment, capping the buffer at SEGMENT_CAP so a
     *  long call can't grow it unbounded.
     *
     *  #659 (Phase L) — keyed-reconcile for finals: a FINAL that repeats an
     *  existing final's `(channel, start_ms)` is an in-place correction (e.g.
     *  a far-side speaker label that settled from "Speaker A" to "Speaker B"),
     *  NOT a new line. Replace the existing row so the label/text mutate
     *  without a duplicate row; the stable `#each` key in the lanes then
     *  re-renders the text in place (no remount, flash, or scroll jump).
     *  Provisionals keep blind-append — the lanes' display fold already
     *  collapses to one provisional per channel. */
    pushSegment(seg: LiveSegment) {
      let next: LiveSegment[];
      if (!seg.provisional) {
        const idx = segments.findIndex(
          (s) =>
            !s.provisional &&
            s.channel === seg.channel &&
            s.start_ms === seg.start_ms,
        );
        if (idx === -1) {
          next = [...segments, seg];
        } else {
          next = segments.slice();
          next[idx] = seg;
        }
      } else {
        next = [...segments, seg];
      }
      segments =
        next.length > SEGMENT_CAP ? next.slice(next.length - SEGMENT_CAP) : next;
    },

    /** Set the session status from a `live-session` event
     *  (live / ended / error). */
    setStatus(next: LiveSessionStatus) {
      status = next;
    },

    /** #659 (P2) — append one fast-lane cue from a `live-cue` event. Battlecards
     *  get a client-side expiry (`ttl_ms`); risk cues persist (`expiresAt` null).
     *  De-dupes a resent `id`, drops any already-expired entries, and caps the
     *  battlecard count to the newest `MAX_BATTLECARDS` so the surface stays
     *  glanceable. No server write-through — cues are moment-specific. */
    pushCue(cue: LiveCue) {
      const now = Date.now();
      const expiresAt =
        cue.ttl_ms && cue.ttl_ms > 0 ? now + cue.ttl_ms : null;
      const entry: LiveCueEntry = { ...cue, expiresAt };
      let next = liveCues.filter(
        (c) => c.id !== cue.id && (c.expiresAt === null || c.expiresAt > now),
      );
      next = [...next, entry];
      // Cap battlecards (risk cues are uncapped + persistent).
      const cards = next.filter((c) => c.kind === "battlecard");
      if (cards.length > MAX_BATTLECARDS) {
        const drop = new Set(
          cards.slice(0, cards.length - MAX_BATTLECARDS).map((c) => c.id),
        );
        next = next.filter((c) => !drop.has(c.id));
      }
      liveCues = next;
      if (expiresAt !== null) ensureCuePruneTimer();
    },

    /** Store the latest FULL coaching snapshot verbatim; the Coaching
     *  lane reconciles its card list. Retained through `ended`;
     *  cleared on the next session start. */
    setCoaching(next: CoachingUpdate | null) {
      coaching = next;
    },

    /** #659 (P3) — store the latest FULL checklist snapshot verbatim; the
     *  IntelligenceLane renders it wholesale. Coverage is append-only on the
     *  backend, so a later snapshot never has FEWER covered items than an
     *  earlier one. Retained through `ended` (frozen "covered X/N" summary);
     *  cleared on the next session start. */
    setChecklist(next: ChecklistSnapshot | null) {
      // #659 (P5c) — drop the confirmations overlay when the template changes
      // (a mid-call Sales↔Support switch, or clear), so a shared id can never
      // carry a confirm across personas. Same-template snapshots (the ~20s
      // swaps) keep the user's confirmations intact.
      if (next?.template_id !== checklist?.template_id && checklistConfirmed.size) {
        checklistConfirmed = new Set();
      }
      checklist = next;
    },

    /** #659 (P5c) — toggle one compliance item's user-confirmation (tap to
     *  confirm a `"likely"` item as covered; tap again to undo). Reassigns a new
     *  Set so the rune tracks the change and the lane re-renders immediately.
     *  No backend round-trip — the confirmation is a client-side overlay on the
     *  backend's `"likely"` suggestion (see `checklistConfirmed`). */
    toggleChecklistConfirm(id: string) {
      const next = new Set(checklistConfirmed);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      checklistConfirmed = next;
    },

    /** #660 — record the live session_uuid surfaced from Rust. Called by
     *  the layout's `recording-state` handler right after
     *  `resetForNewSession` so the fresh uuid isn't clobbered by the
     *  reset. `null` for self-notes / flag-off / stop transitions. */
    setSessionUuid(next: string | null) {
      sessionUuid = next ?? null;
    },

    /** #660 — mark one ask preset in flight (single in-flight; the lane
     *  disables all four chips while set). Clears the previous answer's
     *  slot? No — the old answer stays visible until the new one lands so
     *  the rep is never staring at an empty box mid-request. */
    setAskBusy(chip: AskChip) {
      askInFlight = chip;
    },

    /** #660 — settle the inline answer slot (replace-in-place). Clears the
     *  in-flight guard. `answer` may be a real answer, a calm "nothing yet"
     *  (`empty:true`), or an error/unavailable degrade line — the lane styles
     *  each distinctly (P1). */
    setAskAnswer(
      chip: AskChip,
      answer: string,
      basedOnTurns: number | null,
      empty = false,
    ) {
      askAnswer = { chip, answer, basedOnTurns, empty };
      askInFlight = null;
    },

    /** #660 — clear the in-flight guard without touching the slot (used
     *  when a request is abandoned). */
    clearAskBusy() {
      askInFlight = null;
    },

    /** #660 — dismiss the inline answer slot (× / Esc / new session). */
    dismissAsk() {
      askAnswer = null;
    },

    /** #659 P5b — mark the knowledge answer in flight (single affordance;
     *  the lane disables the button while set). The previous answer stays
     *  visible until the new one lands so the rep is never staring at an
     *  empty box mid-request. */
    setKnowledgeBusy() {
      knowledgeInFlight = true;
    },

    /** #659 P5b — settle the inline knowledge slot (replace-in-place). Clears
     *  the in-flight guard. `answer` may be a real grounded answer or a calm
     *  no-match / degrade line (empty `sources`) — both render identically. */
    setKnowledgeAnswer(next: KnowledgeAnswer) {
      knowledgeAnswer = next;
      knowledgeInFlight = false;
    },

    /** #659 P5b — dismiss the inline knowledge slot (× / new session). */
    dismissKnowledge() {
      knowledgeAnswer = null;
    },

    /** #660 — optimistic set/clear of one highlighted turn. Reassigns the
     *  Set so the rune tracks the change; the star reflects it immediately
     *  and the caller reverts on a backend failure. */
    setHighlighted(channel: string, startMs: number, starred: boolean) {
      const key = highlightKey(channel, startMs);
      const next = new Set(highlighted);
      if (starred) next.add(key);
      else next.delete(key);
      highlighted = next;
    },

    /** Clear the buffer + coaching on a NEW session start, seeding the
     *  status optimistically to `live` when the live-transcript feature
     *  is on (the relay confirms via `live-session`, or flips to
     *  `error` if it can't reach the backend) or `idle` otherwise.
     *  Mirrors the per-session clear the Record page used to run in its
     *  own `recording-state` handler. Also clears the #660 co-pilot P1
     *  surfaces (session_uuid, inline ask, highlights) so a new call
     *  never inherits the previous call's answer or stars; the fresh
     *  session_uuid arrives immediately after via `setSessionUuid`. */
    resetForNewSession(liveEnabled: boolean) {
      segments = [];
      coaching = null;
      // #659 (P3) — drop the previous call's agenda checklist so a new call
      // starts from a fresh, all-pending agenda.
      checklist = null;
      // #659 (P5c) — and its compliance-confirmation overlay.
      checklistConfirmed = new Set();
      status = liveEnabled ? "live" : "idle";
      sessionUuid = null;
      askAnswer = null;
      askInFlight = null;
      // #659 P5b — drop the previous call's Support-mode knowledge answer.
      knowledgeAnswer = null;
      knowledgeInFlight = false;
      highlighted = new Set();
      // #659 (P2) — clear fast-lane cues + stop the prune timer so a new call
      // never inherits the previous call's battlecards or risk reminders.
      liveCues = [];
      stopCuePruneTimer();
    },
  };
}

/** Singleton — one live session is in flight at a time. */
export const liveSession = createStore();
