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
    CopilotMode,
    LinkedDeal,
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
    onlinkdeal?: (deal: CrmContextDeal) => void;
    onunlinkdeal?: () => void;
  } = $props();

  // ── Hydration state ────────────────────────────────────────────────
  let crm = $state<CrmContext | null>(null);
  let hydrating = $state(false);
  let hydrateError = $state(false);
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

  function openPicker(row: DetectedSpeaker) {
    openPickerKey = speakerIdentityKey(row.channel, row.speakerLabel);
  }
  function closePicker() {
    openPickerKey = null;
  }

  // Commit a pick from the inline picker. A zoho_contact becomes the primary
  // (grounds the card + contact_hint). Optimistic + backend write live in the
  // parent's `onassign`.
  function commitAssign(row: DetectedSpeaker, picked: PickedIdentity) {
    const isZoho = picked.kind === "zoho_contact";
    onassign({
      channel: row.channel,
      speakerLabel: row.speakerLabel,
      kind: picked.kind,
      displayName: picked.display_name,
      contactId: picked.contact_id,
      userId: picked.user_id,
      isPrimary: isZoho ? true : undefined,
    });
    if (isZoho && picked.contact_id) onpick(picked.contact_id);
    openPickerKey = null;
    announce = `${row.diarizationLabel} assigned to ${picked.display_name}.`;
  }

  // Clear one speaker's identity (revert to the diarization label). If it held
  // the primary contact, hand the card + contact_hint to the next remaining
  // zoho_contact (or null).
  function clearAssign(row: DetectedSpeaker) {
    const idn = identityFor(row);
    const wasPrimary = isPrimaryRow(idn);
    onassign({
      channel: row.channel,
      speakerLabel: row.speakerLabel,
      kind: idn?.kind ?? "adhoc",
      displayName: idn?.display_name ?? "",
      clear: true,
    });
    if (wasPrimary) {
      const next = [...speakerIdentities.values()].find(
        (i) =>
          i.kind === "zoho_contact" &&
          i.contact_id &&
          !(
            i.channel === row.channel && i.speaker_label === row.speakerLabel
          ),
      );
      onpick(next?.contact_id ?? null);
    }
    openPickerKey = null;
    announce = `${row.diarizationLabel} identity cleared.`;
  }

  // Promote an already-assigned zoho_contact to primary (re-hydrates the card).
  function makePrimary(row: DetectedSpeaker) {
    const idn = identityFor(row);
    if (!idn || idn.kind !== "zoho_contact" || !idn.contact_id) return;
    onassign({
      channel: row.channel,
      speakerLabel: row.speakerLabel,
      kind: "zoho_contact",
      displayName: idn.display_name,
      contactId: idn.contact_id,
      isPrimary: true,
    });
    onpick(idn.contact_id);
    announce = `${idn.display_name} is now the primary contact.`;
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
    hydrating = true;
    hydrateError = false;
    crm = null;
    try {
      const ctx = (await invoke("live_crm_context", {
        contactId,
        sessionUuid: sessionUuid ?? undefined,
        // #659 P5a — carry the persona so the backend persists it to
        // state.copilot.mode; it does NOT change what's fetched.
        mode,
      })) as CrmContext;
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
      hydrateError = true;
      announce = "Couldn't load contact details.";
      // #662 (N-1) — a failed hydrate leaves no fresh envelope; raise
      // zeroed/unavailable counts so the panel's auto-mode inference stops
      // acting on the PREVIOUS contact's stale counts. Counts-only (no PII).
      raiseCounts(null);
    } finally {
      hydrating = false;
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
  function isLinkedDeal(deal: CrmContextDeal): boolean {
    return !!linkedDeal && linkedDeal.record_id === deal.id;
  }
  function toggleLinkDeal(deal: CrmContextDeal) {
    if (isLinkedDeal(deal)) {
      onunlinkdeal?.();
      announce = `${deal.name} unlinked from this call.`;
    } else {
      onlinkdeal?.(deal);
      announce = `${deal.name} linked to this call.`;
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
    void openUrl(url).catch((e) => console.warn("openUrl failed", e));
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
       One row per detected speaker. "You" (mic recorder) is read-only; the far
       side is one merged "Them" (separation OFF) or the "Speaker A/B/…" rows
       (ON). Assigning opens the inline SpeakerIdentityPicker; the picked
       identity re-labels ALL of that speaker's transcript lines instantly. -->
  <div class="spk-roster" aria-label="Speakers on this call">
    <div class="spk-head">Speakers</div>
    {#each speakers as row (speakerIdentityKey(row.channel, row.speakerLabel))}
      {@const rowKey = speakerIdentityKey(row.channel, row.speakerLabel)}
      {@const idn = identityFor(row)}
      {@const primary = isPrimaryRow(idn)}
      <div class="spk-row" class:picking={openPickerKey === rowKey}>
        {#if openPickerKey === rowKey}
          <SpeakerIdentityPicker
            speakerLabel={row.diarizationLabel}
            onpick={(picked) => commitAssign(row, picked)}
            oncancel={closePicker}
          />
        {:else}
          <div class="spk-id">
            <Avatar name={idn?.display_name ?? row.diarizationLabel} size={22} />
            <span
              class="spk-name"
              title={idn?.display_name ?? row.diarizationLabel}
            >
              {idn?.display_name ?? row.diarizationLabel}
            </span>
            {#if idn}
              <span class="spk-kind">{kindLabel(idn.kind)}</span>
            {/if}
            {#if primary}
              <span class="spk-primary" title="Grounds the deal card below"
                >Primary</span
              >
            {/if}
          </div>
          <div class="spk-actions">
            {#if row.isRecorder}
              <span class="spk-you">You</span>
            {:else if idn}
              {#if idn.kind === "zoho_contact" && !primary}
                <button
                  type="button"
                  class="spk-btn"
                  onclick={() => makePrimary(row)}
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
                onclick={() => clearAssign(row)}
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
          </div>
        {/if}
      </div>
    {/each}
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

        <!-- #659 P5a — Contact grounding swaps with the persona: open Cases
             in Support mode, open Deals in Sales mode. Both come from the one
             crm-context envelope and degrade independently. -->
        {#if mode === "support"}
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
      {/if}
    </div>
  {/if}
</div>

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
  .spk-assign {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .spk-assign:hover {
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
    .crm-card {
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
