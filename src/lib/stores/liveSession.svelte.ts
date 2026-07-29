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
  QuestionsSnapshot,
  AskChip,
  KnowledgeAnswer,
  SpeakerIdentity,
  LinkedDeal,
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

/** #646 (Phase 2) — natural key for a speaker identity: `channel + speaker_label`.
 *  The transcript lane (`labelFor`), the reference-rail roster, and the store's
 *  identity map all key on this exact shape so they agree without a shared object
 *  reference. The merged far side (separation OFF) keys on the CANONICAL
 *  diarization label the backend emits for far segments — `"Them"`, NOT `""` — so
 *  a pre-call "Them → contact" assignment matches the far lines the instant they
 *  arrive AND survives the `POST /v1/live/speaker-identity` non-empty check. */
export function speakerIdentityKey(channel: string, label: string): string {
  return channel + ":" + label;
}

/** #646 (Phase 2) — one detected speaker in the live stream, the unit the
 *  reference-rail roster renders one row per. Derived purely from the segment
 *  buffer via `deriveDetectedSpeakers`. `speakerLabel` is the CANONICAL
 *  diarization label the backend emits (the identity-map key half — `"Them"` for
 *  the merged far side, `"Speaker A"` when separated); `diarizationLabel` is the
 *  display fallback when unassigned ("You" / "Them" / "Speaker A"); `isRecorder`
 *  marks the mic row, which is the read-only "You". */
export type DetectedSpeaker = {
  channel: "mic" | "system";
  speakerLabel: string;
  diarizationLabel: string;
  isRecorder: boolean;
};

/** #646 (Phase 2) — pure derivation of the roster's distinct-speaker set from
 *  the live segment buffer. Always leads with the mic recorder ("You",
 *  read-only). For the far side: the distinct non-empty `system` diarization
 *  labels when speaker separation is ON ("Speaker A/B/…"); a single merged
 *  "Them" row when OFF or pre-call (no system audio yet). The merged row keys on
 *  the CANONICAL `"Them"` label the backend emits for far segments when
 *  separation is OFF — NOT an empty string — so it's assignable BEFORE the far
 *  side speaks and the assignment matches the far lines (and the endpoint's
 *  non-empty `speaker_label` check) once they arrive. A stray empty-label early
 *  partial is dropped once named labels exist so it can't add a duplicate row
 *  beside the named speakers. Extracted (not inlined in the panel) so it stays
 *  testable + the lane/roster share one truth. */
export function deriveDetectedSpeakers(segments: LiveSegment[]): DetectedSpeaker[] {
  const rows: DetectedSpeaker[] = [
    { channel: "mic", speakerLabel: "", diarizationLabel: "You", isRecorder: true },
  ];
  const seen = new Set<string>();
  const labels: string[] = [];
  for (const s of segments) {
    if (s.channel !== "system") continue;
    const label = s.speaker ?? "";
    if (seen.has(label)) continue;
    seen.add(label);
    labels.push(label);
  }
  const named = labels.filter((l) => l.length > 0);
  // Separation ON → the named speakers; OFF / pre-call → one merged "Them"
  // (the canonical far label the backend emits + the assignable/lookup key).
  const effective = named.length > 0 ? named : ["Them"];
  for (const label of effective) {
    rows.push({
      channel: "system",
      speakerLabel: label,
      diarizationLabel: label || "Them",
      isRecorder: false,
    });
  }
  return rows;
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

  // Phase 4 (live↔after-call continuity) — the latest auto-extracted questions
  // snapshot (full replace per frame, like `checklist`). Rendered in the
  // transcript drawer (`LiveQuestions`) with an open-count badge on the drawer
  // toggle. Session-sticky ON THE BACKEND (a question never un-answers once
  // answered); the agent just mirrors the newest snapshot. Retained through
  // `ended` (frozen final list); cleared on the next session start.
  let questions = $state<QuestionsSnapshot | null>(null);

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

  // #646 (Phase 2) — per-speaker identity map, keyed `channel + speaker_label`
  // (`speakerIdentityKey`). A plain Map, REASSIGNED on every change so the rune
  // tracks it (Svelte doesn't deep-track Map mutations). Set optimistically on an
  // assign click, then RECONCILED wholesale from the `live_speaker_identity`
  // response (the endpoint is the source of truth, incl. which zoho_contact is
  // primary). Cleared on a new session.
  let speakerIdentities = $state<Map<string, SpeakerIdentity>>(new Map());

  // #646 (Phase 2) — PRE-CALL staged assignments, keyed `channel + speaker_label`.
  // An assign made BEFORE the session mints (no session_uuid yet) can't POST, so
  // it's held here and REPLAYED to the backend the instant recording starts (the
  // layout drains this via `takePendingIdentities`). It deliberately SURVIVES
  // `resetForNewSession` — which re-seeds the display map from it — so a contact
  // the rep assigned for THIS starting call isn't wiped alongside the PREVIOUS
  // call's assignments (which only ever lived in `speakerIdentities`). In-call
  // assigns POST directly and never touch this buffer.
  let pendingIdentities = $state<Map<string, SpeakerIdentity>>(new Map());

  // Phase 3 — the ONE Zoho deal linked to this call (mid-call), driving the
  // call-end push prompt/confirmation. A single scalar (one linked deal at a
  // time — a new link replaces it). Set OPTIMISTICALLY the instant the rep links
  // a deal, then RECONCILED from the `live_linked_deal` response (the backend is
  // the source of truth). Cleared on a new session (`resetForNewSession`) — the
  // ended card reads it BEFORE the next record-start, the acknowledged
  // prompt-mode envelope (auto mode is backend-driven, reset-independent).
  let linkedDeal = $state<LinkedDeal | null>(null);

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
    get questions(): QuestionsSnapshot | null {
      return questions;
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
    get speakerIdentities(): Map<string, SpeakerIdentity> {
      return speakerIdentities;
    },
    get linkedDeal(): LinkedDeal | null {
      return linkedDeal;
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

    /** Phase 4 (live↔after-call continuity) — store the latest FULL questions
     *  snapshot verbatim; `LiveQuestions` renders it wholesale. Answers are
     *  sticky on the backend (a question never un-answers), so a later snapshot
     *  only ever gains answers. Retained through `ended` (frozen final list);
     *  cleared on the next session start. */
    setQuestions(next: QuestionsSnapshot | null) {
      questions = next;
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

    /** #646 (Phase 2) — OPTIMISTIC set of one speaker identity, applied the
     *  instant the user commits a pick so every existing + future line of that
     *  speaker re-labels without waiting on the round-trip. Reassigns a fresh
     *  Map so the rune tracks it. When the new identity claims `is_primary`, the
     *  flag is cleared off any other entry first (only one primary at a time) —
     *  matching what the backend then reconciles. */
    setSpeakerIdentity(identity: SpeakerIdentity) {
      const next = new Map(speakerIdentities);
      if (identity.is_primary) {
        for (const [k, v] of next) {
          if (v.is_primary) next.set(k, { ...v, is_primary: false });
        }
      }
      next.set(
        speakerIdentityKey(identity.channel, identity.speaker_label),
        identity,
      );
      speakerIdentities = next;
    },

    /** #646 (Phase 2) — OPTIMISTIC clear of one speaker's identity (revert to
     *  the diarization label). Reassigns a fresh Map so the rune tracks it. */
    clearSpeakerIdentity(channel: string, label: string) {
      const key = speakerIdentityKey(channel, label);
      if (!speakerIdentities.has(key)) return;
      const next = new Map(speakerIdentities);
      next.delete(key);
      speakerIdentities = next;
    },

    /** #646 (Phase 2) — RECONCILE the whole map from the `live_speaker_identity`
     *  response (the endpoint returns the full set, incl. the authoritative
     *  primary). Replaces wholesale so an optimistic guess that diverged from the
     *  server (e.g. a primary re-shuffle) settles to the truth. */
    reconcileSpeakerIdentities(identities: SpeakerIdentity[]) {
      const next = new Map<string, SpeakerIdentity>();
      for (const idn of identities) {
        next.set(speakerIdentityKey(idn.channel, idn.speaker_label), idn);
      }
      speakerIdentities = next;
    },

    /** Phase 3 — set (or clear with `null`) the call's linked Zoho deal. Used
     *  both for the OPTIMISTIC set the instant the rep links/unlinks a deal and
     *  for the RECONCILE from the `live_linked_deal` response (the backend is the
     *  source of truth; the endpoint returns the reconciled scalar, or `null`
     *  after a clear). One linked deal at a time — a new value replaces the prior
     *  scalar wholesale. */
    setLinkedDeal(deal: LinkedDeal | null) {
      linkedDeal = deal;
    },

    /** #646 (Phase 2) — STAGE a PRE-CALL assignment (no session yet) for replay
     *  at record-start. Mirrors `setSpeakerIdentity`'s single-primary strip so
     *  the staged set never carries two primaries. */
    stagePendingIdentity(identity: SpeakerIdentity) {
      const next = new Map(pendingIdentities);
      if (identity.is_primary) {
        for (const [k, v] of next) {
          if (v.is_primary) next.set(k, { ...v, is_primary: false });
        }
      }
      next.set(
        speakerIdentityKey(identity.channel, identity.speaker_label),
        identity,
      );
      pendingIdentities = next;
    },

    /** #646 (Phase 2) — drop a PRE-CALL staged assignment (a pre-call clear), so
     *  a cleared row isn't replayed at record-start. */
    unstagePendingIdentity(channel: string, label: string) {
      const key = speakerIdentityKey(channel, label);
      if (!pendingIdentities.has(key)) return;
      const next = new Map(pendingIdentities);
      next.delete(key);
      pendingIdentities = next;
    },

    /** #646 (Phase 2) — DRAIN the staged pre-call assignments: return them and
     *  empty the buffer. The layout calls this right after the session_uuid is
     *  minted and replays each to the backend (best-effort), so a contact
     *  assigned before/at call-connect actually lands in
     *  `state.copilot.speaker_identities` instead of being lost. */
    takePendingIdentities(): SpeakerIdentity[] {
      if (pendingIdentities.size === 0) return [];
      const out = [...pendingIdentities.values()];
      pendingIdentities = new Map();
      return out;
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
      // Phase 4 — drop the previous call's extracted questions so a new call
      // starts from an empty ledger.
      questions = null;
      status = liveEnabled ? "live" : "idle";
      sessionUuid = null;
      askAnswer = null;
      askInFlight = null;
      // #659 P5b — drop the previous call's Support-mode knowledge answer.
      knowledgeAnswer = null;
      knowledgeInFlight = false;
      highlighted = new Set();
      // #646 (Phase 2) — drop the previous call's speaker→identity assignments
      // (they only ever lived in `speakerIdentities`) but RE-SEED from the
      // PRE-CALL staged set so an identity the rep assigned for THIS starting
      // call survives the reset; the layout replays that staged set to the
      // backend right after. In-call assigns reconcile in as the rep makes them.
      speakerIdentities = new Map(pendingIdentities);
      // #659 (P2) — clear fast-lane cues + stop the prune timer so a new call
      // never inherits the previous call's battlecards or risk reminders.
      liveCues = [];
      stopCuePruneTimer();
      // Phase 3 — drop the previous call's linked Zoho deal so a fresh call
      // never inherits it (the ended card reads it before this reset fires).
      linkedDeal = null;
    },
  };
}

/** Singleton — one live session is in flight at a time. */
export const liveSession = createStore();
