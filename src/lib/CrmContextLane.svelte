<!--
  CrmContextLane — #653 co-pilot "Contact" lane.

  The "who are you calling?" combobox (Zoho contact typeahead) + the
  resulting contact card and the contact's OPEN Deals. Reuses
  `invoke("zoho_search_records", { module: "Contacts", q })` for search and
  `invoke("live_crm_context", { contactId })` to hydrate. The picked
  contact id is raised to `+page.svelte` via `onpick` (threaded into
  `start_recording` as `contactHint`).

  Degrade posture (hard-rule #2 + design.md semantic-colour rule):
    • "Zoho" is a sanctioned name and MAY appear in copy.
    • Errors are PLAIN BONE TEXT, never a red (`--live`) panel.
    • The lane never blocks the transcript — every failure is soft.

  All chrome is component-scoped (`.crm-*`); the only shared class is
  `.avatar` via Avatar.svelte (markup-only). No app.css touch.
-->
<script lang="ts">
  import { untrack } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import Avatar from "@aftercalls/shared/ui/Avatar.svelte";
  import { zohoStore } from "$lib/stores/zoho.svelte";
  import type { CrmContext, CopilotMode } from "@aftercalls/shared/types";

  type ContactHit = { id: string; name: string; secondary?: string | null };

  let {
    onpick,
    oncounts = undefined,
    sessionUuid = null,
    isAdmin = false,
    mode = "sales",
  }: {
    // Raised on every committed pick (contact id) and on clear (null). The
    // parent stores it as `contactHint` and threads it into start_recording.
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
    // pre-call (no session yet) — the MVP contact_id path doesn't need it.
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
  } = $props();

  // ── Picker state ───────────────────────────────────────────────────
  let query = $state("");
  let results = $state<ContactHit[]>([]);
  let searching = $state(false);
  let searchError = $state(false);
  let dropdownOpen = $state(false);
  let activeIdx = $state(-1);
  let searchTimer = 0;
  // Guards against an out-of-order search: only the latest issued query's
  // response is allowed to land (a slow early request can't clobber a fast
  // later one).
  let searchSeq = 0;

  // ── Selection + hydration state ────────────────────────────────────
  let selected = $state<ContactHit | null>(null);
  // `editing` shows the combobox; false shows the collapsed card. Starts
  // true (picker is the first action). A commit flips it false; the
  // "Change" affordance flips it back true.
  let editing = $state(true);
  let crm = $state<CrmContext | null>(null);
  let hydrating = $state(false);
  let hydrateError = $state(false);

  let inputEl = $state<HTMLInputElement | null>(null);
  let containerEl = $state<HTMLDivElement | null>(null);
  let cardEl = $state<HTMLDivElement | null>(null);
  // Polite SR announcement for pick → hydrate transitions.
  let announce = $state("");

  const listboxId = `crm-list-${Math.random().toString(36).slice(2, 10)}`;
  const DEAL_CAP = 5;

  // Cross-tab-aware Zoho status (client-only — $effect never runs during
  // SSR). If an admin connects in another window, the store's storage
  // listener flips `connected` and the lane re-renders out of the "connect
  // Zoho" prompt without a reload.
  $effect(() => {
    zohoStore.ensureCrossTabListener();
    void zohoStore.refresh();
  });

  // ── Search ─────────────────────────────────────────────────────────
  function scheduleSearch() {
    clearTimeout(searchTimer);
    searchError = false;
    const q = query.trim();
    if (q.length < 2) {
      // Below the fetch threshold — no request. Clear stale rows but keep
      // the input responsive.
      results = [];
      activeIdx = -1;
      searching = false;
      return;
    }
    // 150ms debounce — mirrors the SendToZohoModal / tag-popover idiom.
    searchTimer = window.setTimeout(runSearch, 150);
  }

  async function runSearch() {
    const q = query.trim();
    if (q.length < 2) return;
    const seq = ++searchSeq;
    searching = true;
    searchError = false;
    dropdownOpen = true;
    try {
      const out = (await invoke("zoho_search_records", {
        module: "Contacts",
        q,
      })) as { results?: ContactHit[] };
      if (seq !== searchSeq) return; // superseded
      results = out.results ?? [];
      activeIdx = -1;
    } catch {
      if (seq !== searchSeq) return;
      // Errors are plain bone text, never a red panel.
      searchError = true;
      results = [];
      activeIdx = -1;
    } finally {
      if (seq === searchSeq) searching = false;
    }
  }

  function retrySearch() {
    searchError = false;
    void runSearch();
  }

  // ── Pick / change / clear ──────────────────────────────────────────
  function pick(hit: ContactHit) {
    selected = hit;
    query = hit.name;
    results = [];
    activeIdx = -1;
    dropdownOpen = false;
    editing = false;
    onpick(hit.id);
    announce = `Selected ${hit.name}. Loading deals.`;
    // Focus the card container so keyboard users land on the freshly
    // rendered card (Start Recording stays reachable by Tab).
    queueMicrotask(() => cardEl?.focus());
    void hydrate(hit.id);
  }

  function changeContact() {
    editing = true;
    dropdownOpen = false;
    queueMicrotask(() => {
      inputEl?.focus();
      inputEl?.select();
    });
  }

  function clearSelection() {
    selected = null;
    crm = null;
    hydrateError = false;
    query = "";
    results = [];
    activeIdx = -1;
    editing = true;
    onpick(null);
    // #662 — a cleared contact has no CRM signal; reset the panel's inference.
    raiseCounts(null);
    queueMicrotask(() => inputEl?.focus());
  }

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
        announce = `${selected?.name ?? "Contact"} selected. Zoho is not connected.`;
      } else if (mode === "support") {
        const caseCount = ctx.cases?.items?.length ?? 0;
        announce = `${selected?.name ?? "Contact"} loaded. ${caseCount} open ${
          caseCount === 1 ? "case" : "cases"
        }.`;
      } else {
        const dealCount = ctx.deals?.items?.length ?? 0;
        announce = `${selected?.name ?? "Contact"} loaded. ${dealCount} open ${
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
    if (selected) void hydrate(selected.id);
  }

  // #662 — persist `state.copilot.mode` so the backend coach loop re-targets
  // the checklist template to the active persona. The write-through is the
  // existing `live_crm_context` (reused — NO new endpoint); it re-fetches
  // Deals/Cases but the returned envelope is identical (mode changes only what
  // is PERSISTED, not what is fetched), so we fire-and-forget and never disturb
  // the already-hydrated `crm`. Fires when there IS a live session + committed
  // contact AND either the persona flipped (auto OR manual) OR the session was
  // just established (seeding a pre-call inferred persona at record start).
  // Flips are rare (the panel's hysteresis gates them), so the redundant fetch
  // is an accepted cost. `hydrate` already persists the mode for a mid-call
  // re-pick, so `selected` is read UNTRACKED here to avoid a double write.
  let lastPersistedMode: CopilotMode | null = null;
  let lastPersistedSession: string | null = null;
  $effect(() => {
    const m = mode;
    const su = sessionUuid;
    untrack(() => {
      const sel = selected;
      if (!sel || !su) {
        // No live session / no contact yet — nothing to persist; reset the
        // cursor so the first real persist always fires.
        lastPersistedMode = null;
        lastPersistedSession = su;
        return;
      }
      const sessionChanged = su !== lastPersistedSession;
      if (!sessionChanged && m === lastPersistedMode) return;
      lastPersistedMode = m;
      lastPersistedSession = su;
      void invoke("live_crm_context", {
        contactId: sel.id,
        sessionUuid: su,
        mode: m,
      }).catch((e) =>
        console.warn("live_crm_context mode re-persist failed", e),
      );
    });
  });

  // ── Keyboard (combobox) ────────────────────────────────────────────
  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      dropdownOpen = true;
      if (results.length === 0) return;
      activeIdx = activeIdx + 1 >= results.length ? 0 : activeIdx + 1;
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      dropdownOpen = true;
      if (results.length === 0) return;
      activeIdx = activeIdx <= 0 ? results.length - 1 : activeIdx - 1;
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      if (activeIdx >= 0 && activeIdx < results.length) {
        pick(results[activeIdx]);
      }
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      // First Escape closes the dropdown (keeps the typed text); a second
      // Escape on an already-closed list clears the field.
      if (dropdownOpen) {
        dropdownOpen = false;
      } else if (query.length > 0) {
        query = "";
        results = [];
        activeIdx = -1;
      }
      return;
    }
  }

  // Capture-phase outside-click dismissal (mirrors SpeakerRenamePicker).
  // Only closes the dropdown — never clears the typed value.
  function onOutsidePointerDown(e: PointerEvent) {
    const t = e.target as Node | null;
    if (!t) return;
    if (containerEl && containerEl.contains(t)) return;
    dropdownOpen = false;
  }
  $effect(() => {
    document.addEventListener("pointerdown", onOutsidePointerDown, true);
    return () =>
      document.removeEventListener("pointerdown", onOutsidePointerDown, true);
  });

  // ── Deals helpers ──────────────────────────────────────────────────
  let visibleDeals = $derived(crm?.deals.items.slice(0, DEAL_CAP) ?? []);
  let extraDeals = $derived(
    Math.max(0, (crm?.deals.items.length ?? 0) - DEAL_CAP),
  );

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
    if (!firstUrl || !selected) return "";
    const idx = firstUrl.indexOf("/crm/tab/");
    if (idx <= 0) return "";
    return `${firstUrl.slice(0, idx)}/crm/tab/Contacts/${selected.id}`;
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

<div class="crm-lane" bind:this={containerEl} aria-label="Contact and deals">
  <span class="sr-only" aria-live="polite">{announce}</span>

  {#if zohoOff}
    <!-- Whole-lane degrade — Zoho not connected. Soft, never red. -->
    {@render connectPrompt()}
  {:else if editing || !selected}
    <!-- ── Combobox ── -->
    <div class="crm-picker">
      <input
        bind:this={inputEl}
        class="crm-input"
        type="text"
        role="combobox"
        placeholder="Who are you calling?"
        autocomplete="off"
        aria-label="Search your Zoho contacts"
        aria-expanded={dropdownOpen}
        aria-controls={listboxId}
        aria-autocomplete="list"
        aria-activedescendant={activeIdx >= 0
          ? `crm-opt-${activeIdx}`
          : undefined}
        bind:value={query}
        oninput={scheduleSearch}
        onfocus={() => {
          if (results.length > 0) dropdownOpen = true;
        }}
        onkeydown={handleKeydown}
      />

      {#if query.trim().length === 0}
        <p class="crm-sub">
          Search your Zoho contacts to see their open {mode === "support"
            ? "cases"
            : "deals"} during the call.
        </p>
      {:else if query.trim().length < 2}
        <p class="crm-hint">Keep typing…</p>
      {/if}

      {#if dropdownOpen && query.trim().length >= 2}
        <div
          class="crm-dropdown"
          id={listboxId}
          role="listbox"
          aria-busy={searching}
        >
          {#if searching}
            <div class="crm-status">Searching…</div>
          {:else if searchError}
            <div class="crm-error-row">
              <span>Couldn't reach Zoho just now.</span>
              <button type="button" class="ghost-btn" onclick={retrySearch}>
                Retry
              </button>
            </div>
          {:else if results.length === 0}
            <div class="crm-status crm-nomatch">
              No contacts match "{query.trim()}". Check the spelling or try an
              email.
            </div>
          {:else}
            {#each results as hit, i (hit.id)}
              <button
                type="button"
                id="crm-opt-{i}"
                class="crm-option"
                class:active={i === activeIdx}
                role="option"
                aria-selected={i === activeIdx}
                onmouseenter={() => (activeIdx = i)}
                onclick={() => pick(hit)}
              >
                <Avatar name={hit.name} size={20} />
                <span class="crm-opt-body">
                  <span class="crm-opt-name">{hit.name}</span>
                  {#if hit.secondary}
                    <span class="crm-opt-sec">{hit.secondary}</span>
                  {/if}
                </span>
              </button>
            {/each}
          {/if}
        </div>
      {/if}
    </div>
  {:else}
    <!-- ── Collapsed contact card ── -->
    <div class="crm-card" bind:this={cardEl} tabindex="-1">
      <div class="crm-card-head">
        <Avatar
          name={crm?.contact.name ?? selected.name}
          size={28}
        />
        <span class="crm-card-name">{crm?.contact.name ?? selected.name}</span>
        <button type="button" class="crm-change" onclick={changeContact}>
          Change
        </button>
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
                <div class="crm-deal">
                  <button
                    type="button"
                    class="crm-deal-name"
                    onclick={() => openDeal(deal.url)}
                    title={deal.name}
                  >
                    <span class="crm-deal-name-text">{deal.name}</span>
                    <span class="crm-deal-arrow" aria-hidden="true">↗</span>
                  </button>
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

  /* ── Combobox ── */
  .crm-picker {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .crm-input {
    width: 100%;
    padding: 0.55rem 0.7rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.9rem;
  }
  .crm-input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-color: var(--accent);
  }
  .crm-sub {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.4;
    color: var(--bone-3);
  }
  .crm-hint {
    margin: 0;
    font-size: 0.76rem;
    color: var(--bone-4);
  }

  .crm-dropdown {
    position: absolute;
    top: calc(100% + 0.3rem);
    left: 0;
    right: 0;
    z-index: 20;
    max-height: 240px;
    overflow-y: auto;
    padding: 0.3rem 0;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-1);
    box-shadow: 0 10px 24px -8px rgba(0, 0, 0, 0.45);
    animation: crm-pop 100ms ease-out both;
  }
  @keyframes crm-pop {
    from {
      opacity: 0;
      transform: translateY(-4px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .crm-dropdown {
      animation: crm-fade-in 100ms ease-out both;
    }
  }
  @keyframes crm-fade-in {
    from {
      opacity: 0;
    }
    to {
      opacity: 1;
    }
  }

  .crm-status {
    padding: 0.5rem 0.7rem;
    font-size: 0.8rem;
    color: var(--bone-3);
  }
  .crm-nomatch {
    line-height: 1.4;
  }

  .crm-option {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 0.6rem;
    width: 100%;
    padding: 0.4rem 0.7rem;
    border: none;
    background: transparent;
    color: var(--bone-1);
    text-align: left;
    cursor: pointer;
    font: inherit;
    transition: background 0.15s, color 0.15s;
  }
  .crm-option:hover,
  .crm-option.active {
    background: var(--accent-soft);
    color: var(--bone-0);
  }
  .crm-opt-body {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 0;
  }
  .crm-opt-name {
    font-size: 0.86rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .crm-opt-sec {
    font-size: 0.72rem;
    color: var(--bone-3);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  /* ── Contact card ── */
  .crm-card {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    animation: crm-fade-in 150ms ease-out both;
  }
  .crm-card:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
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
  .crm-change {
    margin-left: auto;
    flex-shrink: 0;
    background: transparent;
    border: none;
    padding: 0.2rem 0.3rem;
    font: inherit;
    font-size: 0.76rem;
    color: var(--bone-3);
    cursor: pointer;
    transition: color 0.15s;
  }
  .crm-change:hover {
    color: var(--accent);
  }
  .crm-change:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
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
    transition: color 0.15s;
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
