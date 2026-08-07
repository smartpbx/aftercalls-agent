<!--
  CrmContextLane — #653 co-pilot "Contact" lane, reworked into the #646
  (Phase 2) SPEAKER → IDENTITY ROSTER.

  Top: one row per DETECTED speaker (distinct `channel + speaker` from the live
  segments, derived by CoPilotPanel + passed as `speakers`). Each row shows its
  current label (assigned identity name, else the diarization label) and an
  ASSIGN affordance that opens the agent-local `SpeakerIdentityPicker`
  (Zoho contact · teammate · adhoc name). "You" (the mic recorder) is
  pre-assigned + read-only. Separation OFF → one merged "Them" row; ON → the
  "Speaker A/B/…" rows; unassigned rows keep their diarization label. The PRIMARY
  zoho_contact grounds the deal/case card beneath the roster + the recording's
  `contact_hint` (raised via `onpick`); switching primary re-hydrates the card
  via the SAME `live_crm_context` fetch (no new endpoint).

  #676 adds a THIRD row state: SEEDED-UNCONFIRMED. When the rep pre-picked a
  contact, the backend re-keys that identity onto the first far label the
  diarizer establishes and marks it `source:"suggested"`. Such a row renders as
  a visible guess (dimmed avatar, italic "{name}?", mono "Pre-selected contact"
  caption) with Confirm / Edit / × — and, critically, does NOT rename the
  transcript until confirmed (`LiveTranscriptLane.labelFor` skips it). The CRM
  card IS grounded by it immediately: that part already worked, and gating it
  would be a regression. Absent `source` ⇒ rep-assigned, so nothing about the
  pre-#676 render changes.

  Bottom: the existing matched-contact card + open Deals (Sales) / Cases
  (Support), hydrated from the primary contact. Reuses
  `invoke("live_crm_context", { contactId })`.

  Degrade posture (hard-rule #2 + design.md semantic-colour rule):
    • "Zoho" is a sanctioned name and MAY appear in copy.
    • Errors are PLAIN BONE TEXT, never a red (`--live`) panel.
    • The lane never blocks the transcript — every failure is soft.

  All chrome is component-scoped (`.crm-*` / `.spk-*`); the only shared class is
  `.avatar` via Avatar.svelte (markup-only). No app.css touch.
-->
<script lang="ts">
  import { untrack } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { externalHttpsUrl } from "$lib/externalPush";
  import Avatar from "@aftercalls/shared/ui/Avatar.svelte";
  import { zohoStore } from "$lib/stores/zoho.svelte";
  import SpeakerIdentityPicker from "$lib/SpeakerIdentityPicker.svelte";
  import type { PickedIdentity } from "$lib/SpeakerIdentityPicker.svelte";
  import {
    speakerIdentityKey,
    type DetectedSpeaker,
  } from "$lib/stores/liveSession.svelte";
  import type {
    CrmContext,
    CrmContextDeal,
    CrmContextTicket,
    CopilotMode,
    LinkedDeal,
    LinkedTicket,
    SpeakerIdentity,
    SpeakerIdentityAssignArgs,
    SpeakerIdentityKind,
  } from "@aftercalls/shared/types";

  let {
    speakers = [],
    speakerIdentities = new Map<string, SpeakerIdentity>(),
    onassign = () => {},
    onpick,
    oncounts = undefined,
    sessionUuid = null,
    isAdmin = false,
    mode = "sales",
    linkedDeal = null,
    onlinkdeal = undefined,
    onunlinkdeal = undefined,
    zohoDesk = false,
    linkedTicket = null,
    onlinkticket = undefined,
    onunlinkticket = undefined,
    extraSpeakerLabels = [],
    onaddspeaker = undefined,
    onremovespeaker = undefined,
  }: {
    // #646 (Phase 2) — the detected-speaker roster rows (mic recorder + the
    // distinct far-side speakers), derived by CoPilotPanel from the segments.
    speakers?: DetectedSpeaker[];
    // #646 (Phase 2) — the identity map (keyed `channel + speaker_label`). The
    // roster reads it for each row's current label + primary state.
    speakerIdentities?: Map<string, SpeakerIdentity>;
    // #646 (Phase 2) — commit one assign (or a `clear`). The parent owns the
    // `live_speaker_identity` invoke + the optimistic/reconcile store write.
    onassign?: (args: Omit<SpeakerIdentityAssignArgs, "sessionUuid">) => void;
    // Raised with the PRIMARY zoho_contact id (or null). The parent stores it as
    // `contactHint` and threads it into start_recording — the start-frame
    // counterpart is the primary contact.
    onpick: (contactId: string | null) => void;
    // #662 — counts-only CRM signal for the panel's auto-mode inference. Fired
    // after each hydrate (and on clear) with open Deals/Cases COUNTS + their
    // degrade status ONLY — NO names/subjects/PII, consistent with the
    // deal-facts-only posture. The panel weighs these against the coach
    // `posture` to pick Sales vs Support.
    oncounts?: (counts: {
      dealsOpen: number;
      casesOpen: number;
      dealsStatus: "ok" | "empty" | "unavailable";
      casesStatus: "ok" | "empty" | "unavailable";
    }) => void;
    // Optional live-session anchor for the crm-context write-through. Null
    // pre-call (no session yet) — the contact_id path doesn't need it.
    sessionUuid?: string | null;
    // Admin viewers get the "Connect Zoho" button; members get the muted
    // "ask an admin" text (they can't connect the integration themselves).
    isAdmin?: boolean;
    // #659 P5a — co-pilot persona. `"sales"` renders the contact's open
    // Deals (today's behaviour); `"support"` renders the contact's open
    // Cases from the SAME crm-context envelope (the backend always fetches
    // both, degrading each independently — so the toggle is a client-side
    // section swap, no re-fetch). Passed to `live_crm_context` so the backend
    // best-effort persists the active persona to `state.copilot.mode`.
    mode?: CopilotMode;
    // Phase 3 — the ONE deal currently linked to this call (or null). Drives the
    // per-deal "Link to call" / "Linked" affordance in the open-Deals list.
    linkedDeal?: LinkedDeal | null;
    // Phase 3 — raise a link (one deal at a time — a new link replaces the prior)
    // / unlink to the parent, which owns the `live_linked_deal` invoke + store.
    // Both are only surfaced mid-call (a session must exist to persist the link).
    onlinkdeal?: (deal: CrmContextDeal) => void | Promise<void>;
    onunlinkdeal?: () => void | Promise<void>;
    // Zoho Desk — whether `features.zoho_desk` is on. Gates the whole Tickets
    // section (rendered BESIDE Deals/Cases, NOT swapped by the mode toggle). Off
    // → no ticket chrome at all, byte-identical to the pre-Desk lane.
    zohoDesk?: boolean;
    // Zoho Desk — the ONE ticket currently linked to this call (or null). Drives
    // the per-ticket "Link to call" / "Linked" affordance. Coexists with a
    // linked deal.
    linkedTicket?: LinkedTicket | null;
    // Zoho Desk — raise a link (one ticket at a time — a new link replaces the
    // prior) / unlink to the parent, which owns the `live_linked_ticket` invoke
    // + store. Only surfaced mid-call (a session must exist to persist the link).
    onlinkticket?: (ticket: CrmContextTicket) => void | Promise<void>;
    onunlinkticket?: () => void | Promise<void>;
    // Far-side roster rows the rep added by hand (already folded into
    // `speakers` by the parent). Held here only so the row can offer "Remove"
    // for a manual row while a genuinely-detected row can't be removed.
    extraSpeakerLabels?: string[];
    // Claim the next free far-side label / drop a manual row. The parent owns
    // the store write; `onaddspeaker` gets the labels already on the roster so
    // the store picks the next unused one.
    onaddspeaker?: (taken: string[]) => void;
    onremovespeaker?: (label: string) => void;
  } = $props();

  // ── Hydration state ────────────────────────────────────────────────
  let crm = $state<CrmContext | null>(null);
  let hydrating = $state(false);
  let hydrateError = $state(false);
  let hydrateGeneration = 0;
  // Polite SR announcement for assign → hydrate transitions.
  let announce = $state("");

  const DEAL_CAP = 5;

  // Cross-tab-aware Zoho status (client-only — $effect never runs during
  // SSR). If an admin connects in another window, the store's storage
  // listener flips `connected` and the lane re-renders out of the "connect
  // Zoho" prompt without a reload.
  $effect(() => {
    zohoStore.ensureCrossTabListener();
    void zohoStore.refresh();
  });

  // ── Speaker roster ─────────────────────────────────────────────────
  // Which row (by `channel:speaker_label`) currently has its inline picker open.
  let openPickerKey = $state<string | null>(null);

  function identityFor(row: DetectedSpeaker): SpeakerIdentity | undefined {
    return speakerIdentities.get(
      speakerIdentityKey(row.channel, row.speakerLabel),
    );
  }
  function kindLabel(k: SpeakerIdentityKind): string {
    return k === "zoho_contact"
      ? "Contact"
      : k === "internal_user"
        ? "Teammate"
        : "Name";
  }
  function isPrimaryRow(idn: SpeakerIdentity | undefined): boolean {
    return (
      !!idn &&
      idn.kind === "zoho_contact" &&
      !!idn.contact_id &&
      idn.contact_id === primaryContactId
    );
  }
  /** #676 — a row the BACKEND bound, not the rep: the contact picked before
   *  dialing, re-keyed onto the first far label the diarizer established. It
   *  renders as a visibly-unconfirmed suggestion (dimmed avatar, italic
   *  "{name}?", Confirm / Edit / ×) and does NOT rename the transcript until
   *  confirmed. Absent `source` ⇒ rep-assigned, so every pre-#676 row and every
   *  older backend renders exactly as before. */
  function isSeeded(idn: SpeakerIdentity | undefined): boolean {
    return idn?.source === "suggested";
  }

  function openPicker(row: DetectedSpeaker) {
    openPickerKey = speakerIdentityKey(row.channel, row.speakerLabel);
  }
  function closePicker() {
    openPickerKey = null;
  }

  // ── Merge voices that resolve to the SAME person ───────────────────
  // Diarization over-segments: one speaker who pauses, changes tone, or shares
  // a noisy line routinely comes back as "Speaker A" + "Speaker B" + "Speaker
  // C". Naming each of them is the rep telling us they are one person — so the
  // roster has to collapse them, not keep three identical rows (which also made
  // three rows each claim the PRIMARY chip, since that test keys on the shared
  // contact id).
  //
  // The group key is the IDENTITY, not the label: same Zoho contact, same
  // teammate, or same typed name (case-insensitively) → one row. Unassigned
  // rows never merge — an anonymous "Speaker B" is not yet known to be anyone.
  // The recorder ("You") is always its own row.
  type SpeakerGroup = {
    key: string;
    rows: DetectedSpeaker[];
    lead: DetectedSpeaker;
    identity: SpeakerIdentity | undefined;
  };

  function identityGroupKey(
    row: DetectedSpeaker,
    idn: SpeakerIdentity | undefined,
  ): string {
    if (row.isRecorder) return "self";
    if (!idn) return "un:" + speakerIdentityKey(row.channel, row.speakerLabel);
    if (idn.kind === "zoho_contact" && idn.contact_id)
      return "contact:" + idn.contact_id;
    if (idn.kind === "internal_user" && idn.user_id)
      return "user:" + idn.user_id;
    return "name:" + (idn.display_name ?? "").trim().toLowerCase();
  }

  let speakerGroups = $derived.by<SpeakerGroup[]>(() => {
    const out: SpeakerGroup[] = [];
    const byKey = new Map<string, SpeakerGroup>();
    for (const row of speakers) {
      const idn = identityFor(row);
      const key = identityGroupKey(row, idn);
      const existing = byKey.get(key);
      if (existing) {
        existing.rows.push(row);
        continue;
      }
      const group: SpeakerGroup = { key, rows: [row], lead: row, identity: idn };
      byKey.set(key, group);
      out.push(group);
    }
    return out;
  });

  // ── Manual roster rows ─────────────────────────────────────────────
  // The roster is derived from speakers the diarizer actually SPLIT OUT. When
  // two people share a handset — or the diarizer merges them onto one label —
  // there is no second row to assign, so the second person can't be named at
  // all. "Add person" claims the next free label in the same space the diarizer
  // uses, giving an assignable row now; if that letter later starts arriving on
  // real turns, the assignment already owns those lines. Either way the person
  // lands on the durable call roster via their identity.
  let rosterLabels = $derived(
    speakers.filter((s) => !s.isRecorder).map((s) => s.speakerLabel),
  );
  // 26 letters in the label space; past that there is nothing left to claim.
  let canAddSpeaker = $derived(!!onaddspeaker && rosterLabels.length < 26);
  function isManualRow(row: DetectedSpeaker): boolean {
    return extraSpeakerLabels.includes(row.speakerLabel);
  }
  function addSpeaker() {
    onaddspeaker?.(rosterLabels);
    announce = "Added a person to the call. Assign who they are.";
  }
  // Dropping a manual row also clears any identity assigned to it, so a
  // half-removed row can't leave a phantom name on the after-call roster.
  function removeSpeaker(row: DetectedSpeaker) {
    // Clear only THIS label, not the whole merged group: removing one
    // hand-added voice must leave the other voices of that person named.
    const idn = identityFor(row);
    if (idn) {
      onassign({
        channel: row.channel,
        speakerLabel: row.speakerLabel,
        kind: idn.kind,
        displayName: idn.display_name,
        clear: true,
      });
    }
    onremovespeaker?.(row.speakerLabel);
    openPickerKey = null;
    announce = `${row.diarizationLabel} removed.`;
  }

  // Commit a pick from the inline picker. A zoho_contact becomes the primary
  // (grounds the card + contact_hint). Optimistic + backend write live in the
  // parent's `onassign`.
  function commitAssign(group: SpeakerGroup, picked: PickedIdentity) {
    const isZoho = picked.kind === "zoho_contact";
    // Assign EVERY voice already merged into this row. Re-naming a merged row
    // must not silently strand the other labels on the old identity — they'd
    // split back apart on the next snapshot.
    for (const row of group.rows) {
      onassign({
        channel: row.channel,
        speakerLabel: row.speakerLabel,
        kind: picked.kind,
        displayName: picked.display_name,
        contactId: picked.contact_id,
        userId: picked.user_id,
        isPrimary: isZoho ? true : undefined,
      });
    }
    if (isZoho && picked.contact_id) onpick(picked.contact_id);
    openPickerKey = null;
    announce = `${groupLabel(group)} assigned to ${picked.display_name}.`;
  }

  // Clear one speaker's identity (revert to the diarization label). If it held
  // the primary contact, hand the card + contact_hint to the next remaining
  // zoho_contact (or null).
  function clearAssign(group: SpeakerGroup) {
    const idn = group.identity;
    const wasPrimary = isPrimaryRow(idn);
    // Clearing a merged row un-names every voice in it, splitting them back
    // into the separate anonymous speakers they were detected as.
    const cleared = new Set(
      group.rows.map((row) => speakerIdentityKey(row.channel, row.speakerLabel)),
    );
    for (const row of group.rows) {
      onassign({
        channel: row.channel,
        speakerLabel: row.speakerLabel,
        kind: idn?.kind ?? "adhoc",
        displayName: idn?.display_name ?? "",
        clear: true,
      });
    }
    if (wasPrimary) {
      const next = [...speakerIdentities.values()].find(
        (i) =>
          i.kind === "zoho_contact" &&
          i.contact_id &&
          !cleared.has(speakerIdentityKey(i.channel, i.speaker_label)),
      );
      onpick(next?.contact_id ?? null);
    }
    openPickerKey = null;
    announce = `${groupLabel(group)} identity cleared.`;
  }

  // #676 — CONFIRM a backend-seeded suggestion. There is no confirm endpoint:
  // the rep re-picking the same identity IS the confirmation, so this hands the
  // seeded row's own identity straight to `commitAssign`. That stamps
  // `source:"assigned"` server-side, flips the row to the ordinary confirmed
  // look, and lets `LiveTranscriptLane.labelFor` start applying the name — all
  // through the path the picker already uses. No new invoke, no new prop.
  function confirmSeeded(group: SpeakerGroup) {
    const idn = group.identity;
    if (!idn) return;
    commitAssign(group, {
      kind: idn.kind,
      display_name: idn.display_name,
      contact_id: idn.contact_id,
      user_id: idn.user_id,
    });
  }

  // #676 — announce a suggestion ONCE, the first time its row appears. The
  // identity map is reconciled wholesale on every write, so keying the
  // announcement off the row alone would re-read the same suggestion aloud on
  // each reconcile; the seen-set makes it arrival-only. Confirm and Clear reuse
  // `commitAssign` / `clearAssign`'s existing announcements verbatim.
  const announcedSeeded = new Set<string>();
  // …but only once PER CALL. The lane stays mounted for the life of the window,
  // so an un-cleared set would mute the announcement on every call after the
  // first that seeds the same `system:Speaker A`. `resetForNewSession` nulls the
  // store's `sessionUuid` on the recording-state start edge and the fresh one
  // arrives immediately after, so this turns over exactly once per new session.
  let announcedSeededSession: string | null = null;
  $effect(() => {
    if (sessionUuid !== announcedSeededSession) {
      announcedSeededSession = sessionUuid;
      announcedSeeded.clear();
    }
    for (const group of speakerGroups) {
      const idn = group.identity;
      if (!isSeeded(idn) || !idn) continue;
      const key = speakerIdentityKey(
        group.lead.channel,
        group.lead.speakerLabel,
      );
      if (announcedSeeded.has(key)) continue;
      announcedSeeded.add(key);
      announce = `We think this is ${idn.display_name}. Confirm or edit in Speakers.`;
    }
  });

  // Promote an already-assigned zoho_contact to primary (re-hydrates the card).
  function makePrimary(group: SpeakerGroup) {
    const idn = group.identity;
    if (!idn || idn.kind !== "zoho_contact" || !idn.contact_id) return;
    onassign({
      channel: group.lead.channel,
      speakerLabel: group.lead.speakerLabel,
      kind: "zoho_contact",
      displayName: idn.display_name,
      contactId: idn.contact_id,
      isPrimary: true,
    });
    onpick(idn.contact_id);
    announce = `${idn.display_name} is now the primary contact.`;
  }

  /** What to call a group in an announcement: the person's name once assigned,
   *  else the diarization label(s) it covers. */
  function groupLabel(group: SpeakerGroup): string {
    if (group.identity?.display_name) return group.identity.display_name;
    return group.rows.map((row) => row.diarizationLabel).join(" + ");
  }

  // ── Primary contact (grounds the card) ─────────────────────────────
  // The one is_primary zoho_contact, else the first zoho_contact assigned.
  let primaryContact = $derived.by<SpeakerIdentity | null>(() => {
    const zoho = [...speakerIdentities.values()].filter(
      (i) => i.kind === "zoho_contact" && i.contact_id,
    );
    if (zoho.length === 0) return null;
    return zoho.find((i) => i.is_primary) ?? zoho[0];
  });
  let primaryContactId = $derived(primaryContact?.contact_id ?? null);
  let primaryName = $derived(primaryContact?.display_name ?? "Contact");

  // #662 — raise the counts-only CRM signal for the panel's auto-mode
  // inference. Open COUNTS + degrade status ONLY — never names/subjects/PII.
  // An "ok" section's item count is the open count; "empty"/"unavailable"
  // contribute 0 (status tells the panel the section degraded).
  function sectionCount(status: "ok" | "empty" | "unavailable", n: number) {
    return status === "ok" ? n : 0;
  }
  function raiseCounts(ctx: CrmContext | null) {
    if (!oncounts) return;
    if (!ctx) {
      oncounts({
        dealsOpen: 0,
        casesOpen: 0,
        dealsStatus: "unavailable",
        casesStatus: "unavailable",
      });
      return;
    }
    oncounts({
      dealsOpen: sectionCount(ctx.deals.status, ctx.deals.items.length),
      casesOpen: sectionCount(ctx.cases.status, ctx.cases.items.length),
      dealsStatus: ctx.deals.status,
      casesStatus: ctx.cases.status,
    });
  }

  // ── Hydrate ────────────────────────────────────────────────────────
  async function hydrate(contactId: string) {
    const generation = ++hydrateGeneration;
    const requestedSession = sessionUuid;
    const requestedMode = mode;
    hydrating = true;
    hydrateError = false;
    crm = null;
    try {
      const ctx = (await invoke("live_crm_context", {
        contactId,
        sessionUuid: requestedSession ?? undefined,
        // #659 P5a — carry the persona so the backend persists it to
        // state.copilot.mode; it does NOT change what's fetched.
        mode: requestedMode,
      })) as CrmContext;
      if (
        generation !== hydrateGeneration ||
        primaryContactId !== contactId
      ) {
        return;
      }
      crm = ctx;
      // #662 — feed the panel's inference from the fresh envelope.
      raiseCounts(ctx);
      if (ctx.zoho === "not_connected") {
        announce = `${primaryName} selected. Zoho is not connected.`;
      } else if (mode === "support") {
        const caseCount = ctx.cases?.items?.length ?? 0;
        announce = `${primaryName} loaded. ${caseCount} open ${
          caseCount === 1 ? "case" : "cases"
        }.`;
      } else {
        const dealCount = ctx.deals?.items?.length ?? 0;
        announce = `${primaryName} loaded. ${dealCount} open ${
          dealCount === 1 ? "deal" : "deals"
        }.`;
      }
    } catch {
      if (
        generation !== hydrateGeneration ||
        primaryContactId !== contactId
      ) {
        return;
      }
      hydrateError = true;
      announce = "Couldn't load contact details.";
      // #662 (N-1) — a failed hydrate leaves no fresh envelope; raise
      // zeroed/unavailable counts so the panel's auto-mode inference stops
      // acting on the PREVIOUS contact's stale counts. Counts-only (no PII).
      raiseCounts(null);
    } finally {
      if (generation === hydrateGeneration) hydrating = false;
    }
  }

  function retryHydrate() {
    if (primaryContactId) void hydrate(primaryContactId);
  }

  // ── Re-hydrate the card when the primary contact changes ───────────
  // Reactively fetch the primary contact's Deals/Cases. Guarded on the last
  // hydrated id so a re-render that doesn't change the primary is a no-op; a
  // switch of primary (assign / clear / make-primary) re-hydrates. When there's
  // no primary the card clears + counts reset. `onpick` is raised imperatively
  // from the assign handlers (NOT here), so the session-start map reset can't
  // wipe the already-captured `contact_hint`.
  let lastHydratedContact: string | null = null;
  $effect(() => {
    const cid = primaryContactId;
    untrack(() => {
      if (cid === lastHydratedContact) return;
      lastHydratedContact = cid;
      if (cid) {
        void hydrate(cid);
      } else {
        hydrateGeneration += 1;
        hydrating = false;
        crm = null;
        hydrateError = false;
        raiseCounts(null);
      }
    });
  });

  // #662 — persist `state.copilot.mode` so the backend coach loop re-targets
  // the checklist template to the active persona. The write-through is the
  // existing `live_crm_context` (reused — NO new endpoint); it re-fetches
  // Deals/Cases but the returned envelope is identical (mode changes only what
  // is PERSISTED, not what is fetched), so we fire-and-forget and never disturb
  // the already-hydrated `crm`. Fires when there IS a live session + a primary
  // contact AND either the persona flipped (auto OR manual) OR the session was
  // just established. `hydrate` already persists the mode for a fresh primary,
  // so `primaryContact` is read UNTRACKED here to avoid a double write.
  let lastPersistedMode: CopilotMode | null = null;
  let lastPersistedSession: string | null = null;
  $effect(() => {
    const m = mode;
    const su = sessionUuid;
    untrack(() => {
      const cid = primaryContactId;
      if (!cid || !su) {
        // No live session / no primary contact yet — nothing to persist; reset
        // the cursor so the first real persist always fires.
        lastPersistedMode = null;
        lastPersistedSession = su;
        return;
      }
      const sessionChanged = su !== lastPersistedSession;
      if (!sessionChanged && m === lastPersistedMode) return;
      lastPersistedMode = m;
      lastPersistedSession = su;
      void invoke("live_crm_context", {
        contactId: cid,
        sessionUuid: su,
        mode: m,
      }).catch((e) =>
        console.warn("live_crm_context mode re-persist failed", e),
      );
    });
  });

  // ── Deals helpers ──────────────────────────────────────────────────
  let visibleDeals = $derived(crm?.deals.items.slice(0, DEAL_CAP) ?? []);
  let extraDeals = $derived(
    Math.max(0, (crm?.deals.items.length ?? 0) - DEAL_CAP),
  );

  // ── Link-to-call (Phase 3) ─────────────────────────────────────────
  // The "Link to call" affordance is a MID-CALL action, so it's only shown
  // when a live session exists (`sessionUuid`); pre-call there's nothing to
  // persist the link to and it would be wiped at record-start. One deal at a
  // time — linking a new deal replaces the prior one (the parent + backend own
  // that; here we only raise the intent).
  let canLinkDeal = $derived(!!sessionUuid && !!onlinkdeal);
  let dealLinkBusy = $state(false);
  function isLinkedDeal(deal: CrmContextDeal): boolean {
    return !!linkedDeal && linkedDeal.record_id === deal.id;
  }
  async function toggleLinkDeal(deal: CrmContextDeal) {
    if (dealLinkBusy) return;
    const unlinking = isLinkedDeal(deal);
    dealLinkBusy = true;
    announce = "Updating the call link.";
    try {
      if (unlinking) {
        await onunlinkdeal?.();
        announce = `${deal.name} unlinked from this call.`;
      } else {
        await onlinkdeal?.(deal);
        announce = `${deal.name} linked to this call.`;
      }
    } catch {
      announce = "Couldn't update the call link.";
    } finally {
      dealLinkBusy = false;
    }
  }

  // ── Cases helpers (#659 P5a, Support mode) ─────────────────────────
  const CASE_CAP = 5;
  let visibleCases = $derived(crm?.cases.items.slice(0, CASE_CAP) ?? []);
  let extraCases = $derived(
    Math.max(0, (crm?.cases.items.length ?? 0) - CASE_CAP),
  );
  // A case's display title: the Subject when present, else the Case Number,
  // else a plain fallback. Subject may carry PII — it renders in the lane
  // only (the backend never writes it into AI-grounding state).
  function caseTitle(c: { subject?: string; case_number?: string }): string {
    return c.subject || (c.case_number ? `Case #${c.case_number}` : "(untitled case)");
  }

  // ── Tickets helpers (Zoho Desk) ────────────────────────────────────
  // Where Tickets render depends on whether the org actually runs Desk:
  //   • Desk org, SUPPORT mode → Tickets ARE the support section, in place of
  //     CRM Cases. A shop with a Desk queue does not track live support work in
  //     CRM Cases, so showing Cases there is showing the wrong system of record.
  //   • Desk org, SALES mode   → Tickets render BESIDE Deals, as secondary
  //     context ("heads up, they have two open tickets").
  //   • No Desk               → no ticket chrome at all; Support mode falls
  //     back to CRM Cases exactly as before.
  // `"unavailable"` still counts as a Desk org (Desk is set up, this fetch just
  // failed) — it renders the degraded ticket lane rather than silently swapping
  // the rep to a different system of record.
  const TICKET_CAP = 5;
  let visibleTickets = $derived(crm?.tickets?.items.slice(0, TICKET_CAP) ?? []);
  let extraTickets = $derived(
    Math.max(0, (crm?.tickets?.items.length ?? 0) - TICKET_CAP),
  );
  // A ticket's display title: the Subject when present, else the Ticket Number,
  // else a plain fallback. Subject may carry PII — it renders in the lane only
  // (the backend never writes it into AI-grounding state).
  function ticketTitle(t: { subject?: string; ticket_number?: string }): string {
    return (
      t.subject ||
      (t.ticket_number ? `Ticket #${t.ticket_number}` : "(untitled ticket)")
    );
  }
  // Mid-call "Link to call" is session-gated (same as deals). One ticket at a
  // time — linking a new ticket replaces the prior (parent + backend own that).
  let canLinkTicket = $derived(!!sessionUuid && !!onlinkticket);
  let ticketLinkBusy = $state(false);

  // Does this org actually run Desk? Flag on + the envelope reports anything
  // other than "not_connected".
  let hasDesk = $derived(
    zohoDesk && !!crm?.tickets && crm.tickets.status !== "not_connected",
  );
  // Support mode on a Desk org → Tickets replace Cases as the primary section.
  let ticketsArePrimary = $derived(hasDesk && mode === "support");
  function isLinkedTicket(ticket: CrmContextTicket): boolean {
    return !!linkedTicket && linkedTicket.ticket_id === ticket.id;
  }
  async function toggleLinkTicket(ticket: CrmContextTicket) {
    if (ticketLinkBusy) return;
    const unlinking = isLinkedTicket(ticket);
    ticketLinkBusy = true;
    announce = "Updating the ticket link.";
    try {
      if (unlinking) {
        await onunlinkticket?.();
        announce = `${ticketTitle(ticket)} unlinked from this call.`;
      } else {
        await onlinkticket?.(ticket);
        announce = `${ticketTitle(ticket)} linked to this call.`;
      }
    } catch {
      announce = "Couldn't update the ticket link.";
    } finally {
      ticketLinkBusy = false;
    }
  }

  function fmtClose(s?: string): string {
    if (!s) return "";
    const d = new Date(s);
    if (Number.isNaN(d.getTime())) return s;
    return d.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric",
    });
  }

  // Reconstruct the org's Zoho host from a deal/case deep-link so the "+N
  // more" row can point at the contact record. Backend builds urls as
  // `{crm_host}/crm/tab/{Deals|Cases}/{id}`, so the prefix before `/crm/tab/`
  // is the host. Returns "" when it can't be derived (row renders unlinked).
  function contactMoreUrl(firstUrl?: string): string {
    if (!firstUrl || !primaryContactId) return "";
    const idx = firstUrl.indexOf("/crm/tab/");
    if (idx <= 0) return "";
    return `${firstUrl.slice(0, idx)}/crm/tab/Contacts/${primaryContactId}`;
  }

  function openDeal(url: string) {
    const safeUrl = externalHttpsUrl(url);
    if (!safeUrl) return;
    void openUrl(safeUrl).catch((e) => console.warn("openUrl failed", e));
  }

  function connectZoho() {
    // The agent links out to the portal admin surface (no in-app connect);
    // mirrors the settings page's `/admin/*` openUrl pattern.
    void openUrl("https://app.aftercalls.io/admin/zoho").catch((e) =>
      console.warn("openUrl failed", e),
    );
  }

  // Whole-lane no-Zoho gate: only assert "not connected" once the store has
  // resolved, so we don't flash the connect prompt during the initial probe.
  let zohoOff = $derived(zohoStore.loaded && !zohoStore.connected);
  // Envelope-level not-connected (rare mid-session disconnect) surfaces the
  // same prompt inside the card area.
  let envelopeOff = $derived(crm?.zoho === "not_connected");
</script>

<div class="crm-lane" aria-label="Speakers and contact">
  <span class="sr-only" aria-live="polite">{announce}</span>

  <!-- ── Speaker → identity roster ──
       One row per PERSON, not per detected voice. "You" (mic recorder) is
       read-only. Diarization routinely splits one speaker into several labels,
       so rows that resolve to the same identity are merged into one (see
       `speakerGroups`) and carry a "N voices" chip so the merge is visible
       rather than silent. Assigning opens the inline SpeakerIdentityPicker and
       applies to every voice in the row; the picked identity re-labels all of
       their transcript lines instantly. -->
  <div class="spk-roster" aria-label="Speakers on this call">
    <div class="spk-head">Speakers</div>
    {#each speakerGroups as group (group.key)}
      {@const row = group.lead}
      {@const rowKey = speakerIdentityKey(row.channel, row.speakerLabel)}
      {@const idn = group.identity}
      {@const primary = isPrimaryRow(idn)}
      {@const seeded = isSeeded(idn) && openPickerKey !== rowKey}
      <div
        class="spk-row"
        class:picking={openPickerKey === rowKey}
        class:spk-row-seeded={seeded}
      >
        {#if openPickerKey === rowKey}
          <SpeakerIdentityPicker
            speakerLabel={row.diarizationLabel}
            onpick={(picked) => commitAssign(group, picked)}
            oncancel={closePicker}
          />
        {:else}
          <div class="spk-id">
            <Avatar name={idn?.display_name ?? row.diarizationLabel} size={22} />
            <span
              class="spk-name"
              title={idn?.display_name ?? row.diarizationLabel}
            >
              {#if seeded}{idn?.display_name}?{:else}{idn?.display_name ??
                  row.diarizationLabel}{/if}
            </span>
            {#if idn && !seeded}
              <span class="spk-kind">{kindLabel(idn.kind)}</span>
            {/if}
            {#if group.rows.length > 1}
              <span
                class="spk-voices"
                title={`Merged from ${group.rows
                  .map((member) => member.diarizationLabel)
                  .join(", ")}`}>{group.rows.length} voices</span
              >
            {/if}
            {#if primary}
              <span class="spk-primary" title="Grounds the deal card below"
                >Primary</span
              >
            {/if}
          </div>
          {#if seeded}
            <!-- Names the actual mechanism: this is the contact the rep picked
                 before dialing, not a transcript name-match. Wraps to its own
                 line when the rail is too narrow to carry it beside the name. -->
            <span class="spk-suggest-meta">Pre-selected contact</span>
          {/if}
          <div class="spk-actions">
            {#if row.isRecorder}
              <span class="spk-you">You</span>
            {:else if seeded && idn}
              <!-- #676 — one click each: it's right / it's the wrong person /
                   it's nobody. No dialog on the exit path — the row is showing
                   a stranger's name to the rep the moment it's wrong. -->
              <button
                type="button"
                class="spk-btn spk-confirm"
                onclick={() => confirmSeeded(group)}
              >
                Confirm
              </button>
              <button
                type="button"
                class="spk-btn"
                aria-label={`Edit suggested identity for ${row.diarizationLabel}`}
                onclick={() => openPicker(row)}
              >
                Edit
              </button>
              <button
                type="button"
                class="spk-btn spk-clear"
                title="Not this person"
                aria-label={`Clear suggested identity for ${idn.display_name}`}
                onclick={() => clearAssign(group)}
              >
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
              </button>
            {:else if idn}
              {#if idn.kind === "zoho_contact" && !primary}
                <button
                  type="button"
                  class="spk-btn"
                  onclick={() => makePrimary(group)}
                >
                  Make primary
                </button>
              {/if}
              <button
                type="button"
                class="spk-btn"
                onclick={() => openPicker(row)}
              >
                Change
              </button>
              <button
                type="button"
                class="spk-btn spk-clear"
                aria-label="Clear identity"
                title="Clear identity"
                onclick={() => clearAssign(group)}
              >
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
              </button>
            {:else}
              <button
                type="button"
                class="spk-btn spk-assign"
                onclick={() => openPicker(row)}
              >
                Assign
              </button>
            {/if}
            {#if isManualRow(row) && onremovespeaker}
              <!-- Only a HAND-ADDED row can be removed. A row the far side
                   genuinely spoke under is a fact about the call, not a
                   preference. -->
              <button
                type="button"
                class="spk-btn spk-clear"
                aria-label="Remove this person"
                title="Remove this person"
                onclick={() => removeSpeaker(row)}
              >
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4h8v2"/><path d="M6 6l1 14h10l1-14"/></svg>
              </button>
            {/if}
          </div>
        {/if}
      </div>
    {/each}

    {#if canAddSpeaker}
      <button type="button" class="spk-add" onclick={addSpeaker}>
        <span class="spk-add-glyph" aria-hidden="true">+</span>
        Add person
      </button>
    {/if}
  </div>

  <!-- ── Contact card — grounded on the PRIMARY zoho_contact ── -->
  {#if zohoOff}
    <!-- Zoho not connected — can't fetch deals; roster teammate/adhoc still work. -->
    {@render connectPrompt()}
  {:else if !primaryContact}
    <p class="crm-empty">
      Assign a speaker to a Zoho contact to see their open deals.
    </p>
  {:else}
    <div class="crm-card">
      <div class="crm-card-head">
        <Avatar name={crm?.contact.name ?? primaryName} size={28} />
        <span class="crm-card-name">{crm?.contact.name ?? primaryName}</span>
      </div>

      {#if hydrating}
        <div class="crm-loading" aria-live="polite">
          <span class="crm-skel"></span>
          <span class="crm-status">Loading deals…</span>
        </div>
      {:else if hydrateError}
        <div class="crm-error-row">
          <span>Couldn't load contact details.</span>
          <button type="button" class="ghost-btn" onclick={retryHydrate}>
            Retry
          </button>
        </div>
      {:else if envelopeOff}
        {@render connectPrompt()}
      {:else if crm}
        <!-- Detail rows — each omitted entirely when its field is absent. -->
        {#if crm.contact.email}
          <p class="crm-detail">{crm.contact.email}</p>
        {/if}
        {#if crm.contact.phone}
          <p class="crm-detail">{crm.contact.phone}</p>
        {/if}
        {#if crm.contact.account_name}
          <p class="crm-detail">
            <span class="crm-chip">at {crm.contact.account_name}</span>
          </p>
        {/if}

        <!-- Contact grounding swaps with the persona: the support queue in
             Support mode, open Deals in Sales mode. On a Desk org the support
             queue IS Desk tickets (rendered by the shared `ticketList` snippet
             below); otherwise it falls back to CRM Cases. All three come from
             the one crm-context envelope and degrade independently. -->
        {#if ticketsArePrimary}
          <div class="crm-deals" aria-live="polite">
            {@render ticketList()}
          </div>
        {:else if mode === "support"}
          <!-- Open Cases -->
          <div class="crm-deals" aria-live="polite">
            {#if crm.cases.status === "unavailable"}
              <div class="crm-error-row">
                <span>Cases didn't load.</span>
                <button type="button" class="ghost-btn" onclick={retryHydrate}>
                  Retry
                </button>
              </div>
            {:else if crm.cases.status === "empty"}
              <p class="crm-status">No open cases.</p>
            {:else}
              {#each visibleCases as c (c.id)}
                <div class="crm-deal">
                  <button
                    type="button"
                    class="crm-deal-name"
                    onclick={() => openDeal(c.url)}
                    title={caseTitle(c)}
                  >
                    <span class="crm-deal-name-text">{caseTitle(c)}</span>
                    <span class="crm-deal-arrow" aria-hidden="true">↗</span>
                  </button>
                  <div class="crm-deal-meta">
                    {#if c.case_number && c.subject}
                      <span class="crm-stage">#{c.case_number}</span>
                    {/if}
                    {#if c.status}
                      <span class="crm-stage">{c.status}</span>
                    {/if}
                    {#if c.priority}
                      <span class="crm-stage">{c.priority}</span>
                    {/if}
                    {#if c.created_time}
                      <span class="crm-close">{fmtClose(c.created_time)}</span>
                    {/if}
                  </div>
                </div>
              {/each}
              {#if extraCases > 0}
                {#if contactMoreUrl(crm?.cases.items[0]?.url)}
                  <button
                    type="button"
                    class="crm-more"
                    onclick={() => openDeal(contactMoreUrl(crm?.cases.items[0]?.url))}
                  >
                    +{extraCases} more in Zoho ↗
                  </button>
                {:else}
                  <p class="crm-more crm-more-plain">+{extraCases} more open cases</p>
                {/if}
              {/if}
            {/if}
          </div>
        {:else}
          <!-- Open Deals -->
          <div class="crm-deals" aria-live="polite">
            {#if crm.deals.status === "unavailable"}
              <div class="crm-error-row">
                <span>Deals didn't load.</span>
                <button type="button" class="ghost-btn" onclick={retryHydrate}>
                  Retry
                </button>
              </div>
            {:else if crm.deals.status === "empty"}
              <p class="crm-status">No open deals.</p>
            {:else}
              {#each visibleDeals as deal (deal.id)}
                <div class="crm-deal" class:linked={isLinkedDeal(deal)}>
                  <div class="crm-deal-top">
                    <button
                      type="button"
                      class="crm-deal-name"
                      onclick={() => openDeal(deal.url)}
                      title={deal.name}
                    >
                      <span class="crm-deal-name-text">{deal.name}</span>
                      <span class="crm-deal-arrow" aria-hidden="true">↗</span>
                    </button>
                    {#if canLinkDeal}
                      <!-- Mid-call "Link to call" toggle — one deal at a time.
                           Linking a new deal replaces the prior; clicking the
                           linked deal unlinks. -->
                      <button
                        type="button"
                        class="crm-deal-link"
                        class:on={isLinkedDeal(deal)}
                        aria-pressed={isLinkedDeal(deal)}
                        onclick={() => toggleLinkDeal(deal)}
                        disabled={dealLinkBusy}
                      >
                        {#if isLinkedDeal(deal)}
                          <span class="crm-deal-link-check" aria-hidden="true">✓</span>
                          Linked to call
                        {:else}
                          Link to call
                        {/if}
                      </button>
                    {/if}
                  </div>
                  <div class="crm-deal-meta">
                    {#if deal.stage}
                      <span class="crm-stage">{deal.stage}</span>
                    {/if}
                    {#if deal.amount}
                      <span class="crm-amount">{deal.amount}</span>
                    {/if}
                    {#if deal.close_date}
                      <span class="crm-close">{fmtClose(deal.close_date)}</span>
                    {/if}
                  </div>
                </div>
              {/each}
              {#if extraDeals > 0}
                {#if contactMoreUrl(crm?.deals.items[0]?.url)}
                  <button
                    type="button"
                    class="crm-more"
                    onclick={() => openDeal(contactMoreUrl(crm?.deals.items[0]?.url))}
                  >
                    +{extraDeals} more in Zoho ↗
                  </button>
                {:else}
                  <p class="crm-more crm-more-plain">+{extraDeals} more open deals</p>
                {/if}
              {/if}
            {/if}
          </div>
        {/if}

        <!-- Zoho Desk — Open Tickets as the SECONDARY section (Sales mode on a
             Desk org): "heads up, they have open tickets" beside the deals. In
             Support mode the same `ticketList` snippet renders ABOVE as the
             primary section instead of CRM Cases, so it is not repeated here. -->
        {#if hasDesk && !ticketsArePrimary}
          <div class="crm-tickets" aria-live="polite">
            <div class="crm-section-head">Support tickets</div>
            {@render ticketList()}
          </div>
        {/if}
      {/if}
    </div>
  {/if}
</div>

<!-- The open-Desk-tickets list. Rendered as the PRIMARY support section in
     Support mode and as a secondary section in Sales mode — one definition so
     the two placements can't drift. Each ticket gets the same "Link to call"
     affordance as a deal (one linked ticket at a time, coexisting with a linked
     deal). Subject may carry PII — lane-only. "Zoho" is the sanctioned name;
     the deep-link uses the Desk web_url. -->
{#snippet ticketList()}
  {#if crm?.tickets}
    {#if crm.tickets.status === "unavailable"}
      <div class="crm-error-row">
        <span>Tickets didn't load.</span>
        <button type="button" class="ghost-btn" onclick={retryHydrate}>
          Retry
        </button>
      </div>
    {:else if crm.tickets.status === "empty"}
      <p class="crm-status">No open tickets.</p>
    {:else}
      {#each visibleTickets as t (t.id)}
                <div class="crm-deal" class:linked={isLinkedTicket(t)}>
                  <div class="crm-deal-top">
                    <button
                      type="button"
                      class="crm-deal-name"
                      onclick={() => openDeal(t.web_url)}
                      title={ticketTitle(t)}
                    >
                      <span class="crm-deal-name-text">{ticketTitle(t)}</span>
                      <span class="crm-deal-arrow" aria-hidden="true">↗</span>
                    </button>
                    {#if canLinkTicket}
                      <!-- Mid-call "Link to call" toggle — one ticket at a time.
                           Linking a new ticket replaces the prior; clicking the
                           linked ticket unlinks. Independent of the linked deal. -->
                      <button
                        type="button"
                        class="crm-deal-link"
                        class:on={isLinkedTicket(t)}
                        aria-pressed={isLinkedTicket(t)}
                        onclick={() => toggleLinkTicket(t)}
                        disabled={ticketLinkBusy}
                      >
                        {#if isLinkedTicket(t)}
                          <span class="crm-deal-link-check" aria-hidden="true">✓</span>
                          Linked to call
                        {:else}
                          Link to call
                        {/if}
                      </button>
                    {/if}
                  </div>
          <div class="crm-deal-meta">
            {#if t.ticket_number}
              <span class="crm-stage">#{t.ticket_number}</span>
            {/if}
            {#if t.status}
              <span class="crm-stage">{t.status}</span>
            {/if}
            {#if t.priority}
              <span class="crm-stage">{t.priority}</span>
            {/if}
            {#if t.created_time}
              <span class="crm-close">{fmtClose(t.created_time)}</span>
            {/if}
          </div>
        </div>
      {/each}
      {#if extraTickets > 0}
        <p class="crm-more crm-more-plain">
          +{extraTickets} more open tickets
        </p>
      {/if}
    {/if}
  {/if}
{/snippet}

{#snippet connectPrompt()}
  <div class="crm-connect">
    {#if isAdmin}
      <p class="crm-connect-text">
        Connect Zoho to see who you're calling and their open deals.
      </p>
      <button type="button" class="ghost-btn" onclick={connectZoho}>
        Connect Zoho
      </button>
    {:else}
      <p class="crm-connect-text">Ask an admin to connect Zoho.</p>
    {/if}
  </div>
{/snippet}

<style>
  .crm-lane {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
    padding: 0.9rem 0.95rem 1rem;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  @keyframes crm-fade-in {
    from {
      opacity: 0;
    }
    to {
      opacity: 1;
    }
  }

  /* ── Speaker → identity roster ── */
  .spk-roster {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .spk-head {
    font-size: 0.62rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    margin-bottom: 0.1rem;
  }
  .spk-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    min-height: 1.9rem;
  }
  /* When the inline picker is open the row becomes a column so the picker
     spans the full tile width. */
  .spk-row.picking {
    flex-direction: column;
    align-items: stretch;
  }
  /* #676 — seeded-unconfirmed row. Compresses the after-call
     `.speaker-suggestion` visual language (design.md §"Speaker-suggestion chip
     + banner") into the roster's one-row-per-speaker grammar: there is no
     separate chip to leave untouched beneath a sub-row here, so the row itself
     carries the treatment. `--sig` left-marker = "in flight / needs a decision",
     the same semantic it carries on the update pill. */
  .spk-row-seeded {
    flex-wrap: wrap;
    padding: 0.4rem 0.5rem;
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--sig);
    border-radius: var(--radius-sm);
    background: var(--accent-soft);
    animation: crm-fade-in 150ms ease-out both;
  }
  /* The suggestion is a GUESS, so it reads as one: dimmed avatar, italic name,
     trailing "?" (rendered in the markup). Same recipe as the after-call
     `.speaker-suggestion :global(.avatar)` rule. */
  .spk-row-seeded :global(.avatar) {
    opacity: 0.6;
  }
  .spk-row-seeded .spk-name {
    font-style: italic;
  }
  /* Mono caption naming the provenance. Metadata, not a status — no ground, no
     semantic colour. */
  .spk-suggest-meta {
    flex-shrink: 0;
    font-family: var(--font-mono);
    font-size: 0.66rem;
    letter-spacing: 0.02em;
    color: var(--bone-3);
  }
  /* Three actions never fit beside the name in a rail this narrow, so they take
     their own line unconditionally. This is the one deliberate divergence from
     the after-call chip, which only wraps below 520px: the co-pilot tile is
     narrower than that at EVERY width it ships in, so a media query would just
     be a breakpoint that is always true. */
  .spk-row-seeded .spk-actions {
    width: 100%;
    margin-left: 0;
    justify-content: flex-start;
  }
  .spk-id {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    min-width: 0;
    flex: 1 1 auto;
  }
  .spk-name {
    font-size: 0.85rem;
    color: var(--bone-0);
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* Kind chip — neutral ground, NOT a semantic-colour signal (design.md
     §Never-do-this palette rule). */
  .spk-kind {
    flex-shrink: 0;
    padding: 0.02rem 0.4rem;
    border-radius: var(--radius-sm);
    background: var(--ink-2);
    color: var(--bone-2);
    font-size: 0.66rem;
    letter-spacing: 0.02em;
  }
  /* Primary badge — the single grounding contact. Accent-tinted (it IS a
     meaningful "this drives the card" signal, not decorative colour). */
  .spk-primary {
    flex-shrink: 0;
    padding: 0.02rem 0.45rem;
    border-radius: var(--radius-sm);
    background: var(--accent-soft);
    color: var(--accent-hi);
    font-size: 0.64rem;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .spk-actions {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    flex-shrink: 0;
    margin-left: auto;
  }
  .spk-you {
    font-size: 0.7rem;
    color: var(--bone-3);
    padding: 0.02rem 0.4rem;
    border-radius: var(--radius-sm);
    background: var(--ink-2);
  }
  .spk-btn {
    padding: 0.22rem 0.55rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.72rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .spk-btn:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .spk-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  /* #676 — Confirm is the seeded row's CTA and Assign is the anonymous row's,
     so they share one accent recipe. One primary action per row. */
  .spk-assign,
  .spk-confirm {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .spk-assign:hover,
  .spk-confirm:hover {
    background: var(--accent);
    color: var(--ink-0);
    border-color: var(--accent);
  }
  .spk-clear {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0.22rem 0.35rem;
    color: var(--bone-3);
  }
  .spk-clear:hover {
    color: var(--bone-0);
  }

  /* "Add person" — a dashed, quiet affordance under the roster rows. Dashed
     because the row it creates is a CLAIM on a voice, not an observed one. */
  .spk-add {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    align-self: flex-start;
    margin-top: 0.1rem;
    padding: 0.18rem 0.5rem 0.18rem 0.35rem;
    border: 1px dashed var(--hairline);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--bone-3);
    font: inherit;
    font-size: 0.72rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
  }
  .spk-add:hover {
    color: var(--bone-1);
    border-color: var(--hairline-hi);
  }
  .spk-add:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .spk-add-glyph {
    color: var(--accent);
    font-size: 0.95rem;
    font-weight: 600;
    line-height: 1;
  }

  /* "N voices" — how many diarization labels this one person absorbed. Quiet
     and mono, so it reads as metadata rather than a status. */
  .spk-voices {
    flex-shrink: 0;
    padding: 0.02rem 0.4rem;
    border-radius: 999px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-3);
    font-family: var(--font-mono);
    font-size: 0.62rem;
    white-space: nowrap;
  }

  /* No-primary hint beneath the roster (soft, muted). */
  .crm-empty {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.4;
    color: var(--bone-3);
  }

  .crm-status {
    padding: 0.5rem 0.7rem;
    font-size: 0.8rem;
    color: var(--bone-3);
  }

  /* ── Contact card ── */
  .crm-card {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    animation: crm-fade-in 150ms ease-out both;
  }
  @media (prefers-reduced-motion: reduce) {
    .crm-card,
    .spk-row-seeded {
      animation: none;
    }
  }
  .crm-card-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .crm-card-name {
    font-size: 0.95rem;
    font-weight: 600;
    color: var(--bone-0);
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .crm-detail {
    margin: 0;
    font-size: 0.8rem;
    color: var(--bone-2);
  }
  .crm-chip {
    display: inline-block;
    padding: 0.1rem 0.45rem;
    border-radius: var(--radius-sm);
    background: var(--ink-2);
    color: var(--bone-1);
    font-size: 0.76rem;
  }

  .crm-loading {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  .crm-skel {
    width: 40%;
    height: 0.7rem;
    border-radius: var(--radius-sm);
    background: var(--ink-2);
  }

  /* ── Deals ── */
  .crm-deals {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    margin-top: 0.25rem;
    max-height: 240px;
    overflow-y: auto;
  }

  /* ── Tickets (Zoho Desk) — same list treatment as .crm-deals, but its own
     section so it can sit BESIDE Deals/Cases with a distinguishing caps
     label. Reuses the .crm-deal* row classes (name/meta/link toggle). ── */
  .crm-tickets {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    margin-top: 0.4rem;
    padding-top: 0.4rem;
    border-top: 1px solid var(--hairline);
    max-height: 240px;
    overflow-y: auto;
  }
  /* Section caps label — distinguishes the Tickets list from the Deals/Cases
     list above it. Neutral bone, NOT a semantic-colour signal (design.md
     §Never-do-this palette rule). */
  .crm-section-head {
    font-size: 0.62rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    margin-bottom: 0.1rem;
  }
  .crm-deal {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    min-width: 0;
  }
  /* Row holding the deal name (grows) + the "Link to call" toggle (right). */
  .crm-deal-top {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    min-width: 0;
  }
  .crm-deal-name {
    display: flex;
    align-items: baseline;
    gap: 0.3rem;
    background: transparent;
    border: none;
    padding: 0;
    font: inherit;
    font-size: 0.85rem;
    font-weight: 500;
    color: var(--bone-0);
    cursor: pointer;
    text-align: left;
    min-width: 0;
    flex: 1 1 auto;
    transition: color 0.15s;
  }
  /* Link-to-call toggle — a subtle hairline pill at rest that flips to the
     accent-tinted "linked" state. Accent IS a meaningful "this deal will
     receive the call" signal (design.md §Semantic — not decorative). */
  .crm-deal-link {
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    padding: 0.16rem 0.5rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: transparent;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.7rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .crm-deal-link:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .crm-deal-link:disabled {
    cursor: wait;
    opacity: 0.58;
  }
  .crm-deal-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .crm-deal-link.on {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .crm-deal-link.on:hover {
    /* On hover a linked pill hints the unlink action (softens toward live). */
    border-color: var(--live);
    color: var(--live);
    background: var(--live-soft);
  }
  .crm-deal-link-check {
    font-size: 0.68rem;
    line-height: 1;
  }
  .crm-deal-name-text {
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .crm-deal-arrow {
    flex-shrink: 0;
    color: var(--bone-3);
    font-size: 0.78rem;
    transition: color 0.15s;
  }
  /* Lock hover colour on both the name and the arrow so a global a:hover
     can't re-tint (design.md CTA note). Buttons, not <a>, so we're already
     clear of the global anchor rule — this keeps the intent explicit. */
  .crm-deal-name:hover,
  .crm-deal-name:hover .crm-deal-arrow {
    color: var(--accent);
  }
  .crm-deal-name:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }
  .crm-deal-meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.72rem;
  }
  /* Stage is progress info, NOT a semantic-colour signal — neutral ground,
     no accent/sig/live (design.md §Never-do-this palette rule). */
  .crm-stage {
    padding: 0.05rem 0.4rem;
    border-radius: var(--radius-sm);
    background: var(--ink-2);
    color: var(--bone-1);
  }
  .crm-amount {
    font-family: var(--font-mono);
    color: var(--bone-0);
  }
  .crm-close {
    font-family: var(--font-mono);
    color: var(--bone-2);
  }
  .crm-more {
    align-self: flex-start;
    background: transparent;
    border: none;
    padding: 0.1rem 0;
    font: inherit;
    font-size: 0.76rem;
    color: var(--bone-3);
    cursor: pointer;
    transition: color 0.15s;
  }
  .crm-more:hover {
    color: var(--accent);
  }
  .crm-more:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }
  .crm-more-plain {
    margin: 0;
    cursor: default;
  }
  .crm-more-plain:hover {
    color: var(--bone-3);
  }

  /* ── Degrade rows ── */
  .crm-error-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex-wrap: wrap;
    padding: 0.4rem 0.1rem;
    font-size: 0.8rem;
    color: var(--bone-2);
  }
  .crm-connect {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.55rem;
    padding: 0.4rem 0.1rem;
  }
  .crm-connect-text {
    margin: 0;
    font-size: 0.82rem;
    line-height: 1.4;
    color: var(--bone-2);
    max-width: 40ch;
  }

  /* Ghost button — component-scoped (no `.ghost-btn` in app.css); follows
     the SpeakerRenamePicker "documented-here" precedent. */
  .ghost-btn {
    flex-shrink: 0;
    padding: 0.35rem 0.8rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .ghost-btn:hover {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }
  .ghost-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
