<script lang="ts" module>
  // Public types — exported so call-sites on either surface can
  // type the props they pass in. Mirrors the portal's
  // `api.ZohoSearchResult` / `ZohoPushResponse` shapes; agent passes
  // Tauri-bridge results that satisfy these.
  export type SzmSearchResult = {
    id: string;
    name: string;
    secondary?: string | null;
  };
  export type SzmTag = { kind: string; value: string };
  export type SzmPushResponse = {
    push_id: string;
    zoho_call_record_id: string;
    zoho_url: string;
    tags_added: SzmTag[];
  };
  export type SzmPriorPush = {
    module: string;
    record_id: string;
    record_name: string;
    pushed_at: string;
    pushed_by_display_name?: string | null;
    zoho_url?: string | null;
  };
  export type SzmApi = {
    searchRecords: (
      module: string,
      q: string,
    ) => Promise<SzmSearchResult[]>;
    pushCall: (
      callId: string,
      body: {
        module: string;
        record_id: string;
        record_name: string;
        extra_tags?: string[];
      },
    ) => Promise<SzmPushResponse>;
  };
</script>

<script lang="ts">
  // Send-to-CRM (Zoho) modal — mirror pair (#186).
  //
  // Two surfaces share this byte-identically. Surface-specific
  // operations (HTTP transport, external-link open) ride in via
  // props on the `api` object + `zohoUrlHandler`. The portal calls
  // its REST helpers; the agent goes through Tauri commands that
  // proxy to the same backend.
  //
  // Steps (see ui.md §Surface 2):
  //   0 — already pushed (only when prior push exists)
  //   1 — record-type radio picker (Contact/Account/Deal/Lead/Quote)
  //   2 — debounced search + select
  //   3 — review + push (record card + tag editor + attribution)
  //   4 — pushing in flight (aria-busy=true)
  //   5 — success
  //   5-alt — error
  //
  // All Zoho-returned strings render via {value} interpolation only.
  // NEVER `{@html}`. Per learnings #97 / #102 XSS-safe pattern.

  import { onMount, tick } from "svelte";

  interface Props {
    callId: string;
    /** Existing tags on the call. Pre-populates the Step-3 chip list. */
    existingTags?: SzmTag[];
    /** Most-recent prior push, when one exists. Drives Step 0. */
    priorPush?: SzmPriorPush | null;
    /** Display name of the pusher's matched Zoho user (email match);
     *  null when no match — modal renders attribution-in-description
     *  fallback string. */
    matchedZohoDisplayName?: string | null;
    /** Display name of the admin whose token will own the Call when
     *  no email match exists. */
    fallbackOwnerDisplayName?: string | null;
    /**
     * Surface-specific transport. Portal injects HTTP-backed
     * functions; agent injects Tauri-command-backed functions. Same
     * shape, same return contract — that's how the modal stays
     * byte-identical between the two surfaces.
     */
    api: SzmApi;
    /**
     * External-link opener. Portal → window.open. Agent →
     * invoke("open_url"). Mirror-pair invariant: the modal never
     * owns the divergent code path; the call-site does.
     */
    zohoUrlHandler: (url: string) => void;
    onClose: () => void;
    /** Fires after a successful push so the call-detail page can
     *  refresh tags + show "already pushed" state. */
    onPushed?: (resp: SzmPushResponse) => void;
  }

  let {
    callId,
    existingTags = [],
    priorPush = null,
    matchedZohoDisplayName = null,
    fallbackOwnerDisplayName = null,
    api,
    zohoUrlHandler,
    onClose,
    onPushed,
  }: Props = $props();

  type Step = 0 | 1 | 2 | 3 | 4 | 5 | 51;
  // 51 == 5-alt error step. Numeric so the pip-row index math stays
  // simple; the title-id and aria-live changes treat it like step 5.

  // Step-0 vs Step-1 entry depends on whether the parent passed a
  // prior-push record. We deliberately read priorPush ONCE at mount
  // — opening the modal is always a fresh decision; if the parent
  // wants to re-render with a different prior push, it can unmount
  // and remount via {#if zohoModalOpen}.
  let step = $state<Step>((priorPush ? 0 : 1) as Step);
  let modalEl = $state<HTMLDivElement | null>(null);
  const titleId = `szm-title-${Math.random().toString(36).slice(2, 9)}`;
  const liveId = `szm-live-${Math.random().toString(36).slice(2, 9)}`;
  let liveAnnouncement = $state("");

  // Step-1 record type picker. v1 ships these 5 hardcoded options.
  type RecordType = {
    api_name: string;
    label: string;
    kicker: string;
  };
  const RECORD_TYPES: RecordType[] = [
    { api_name: "Contacts", label: "Contact", kicker: "Person" },
    { api_name: "Deals", label: "Deal", kicker: "Opportunity" },
    { api_name: "Accounts", label: "Account", kicker: "Company" },
    { api_name: "Leads", label: "Lead", kicker: "Unqualified lead" },
    { api_name: "Quotes", label: "Quote", kicker: "Quote document" },
  ];
  let chosenType = $state<RecordType>(RECORD_TYPES[0]);

  // Step-2 search.
  let query = $state("");
  let results = $state<SzmSearchResult[]>([]);
  let searchInFlight = $state(false);
  let searchError = $state<string | null>(null);
  let searchToken = 0;
  let chosenRecord = $state<SzmSearchResult | null>(null);

  // Step-3 tags.
  // Pre-populate ONLY custom-kind tags, per ui.md §Step 3 sub-2.
  // Client/purpose/topic tags are call metadata, not CRM labels; the
  // user can add them manually if they want them on the Zoho side.
  // Snapshot once at mount — the modal owns the editable list from
  // that moment forward.
  let extraTags = $state<string[]>(seedExtraTags(existingTags));
  let newTagInput = $state("");

  function seedExtraTags(seed: SzmTag[]): string[] {
    return seed.filter((t) => t.kind === "custom").map((t) => t.value);
  }

  // Step-4 + Step-5 push state.
  let pushInFlight = $state(false);
  let pushResponse = $state<SzmPushResponse | null>(null);
  let pushErrorMsg = $state<string | null>(null);

  $effect(() => {
    // Reactive search effect. Re-runs on query change with a 250ms
    // debounce; older in-flight requests get superseded by checking
    // `searchToken` after await.
    if (step !== 2) return;
    const q = query.trim();
    if (q.length === 0) {
      results = [];
      searchError = null;
      searchInFlight = false;
      return;
    }
    const token = ++searchToken;
    searchInFlight = true;
    searchError = null;
    const handle = setTimeout(async () => {
      try {
        const out = await api.searchRecords(chosenType.api_name, q);
        if (token !== searchToken) return;
        results = out;
      } catch (e: any) {
        if (token !== searchToken) return;
        results = [];
        searchError = "Search unavailable. Check your connection and try again.";
      } finally {
        if (token === searchToken) searchInFlight = false;
      }
    }, 250);
    return () => clearTimeout(handle);
  });

  $effect(() => {
    // Announce step transitions to assistive tech via the visually
    // hidden live region. Only fires when step actually changes.
    let msg = "";
    switch (step) {
      case 0:
        msg = "Previously pushed";
        break;
      case 1:
        msg = "Step 1 of 3: choose record type";
        break;
      case 2:
        msg = `Step 2 of 3: search ${chosenType.label}`;
        break;
      case 3:
        msg = "Step 3 of 3: review and push";
        break;
      case 4:
        msg = "Pushing to Zoho";
        break;
      case 5:
        msg = "Pushed successfully";
        break;
      case 51:
        msg = "Push failed";
        break;
    }
    liveAnnouncement = msg;
  });

  onMount(async () => {
    await tick();
    focusFirstInteractive();
  });

  async function focusFirstInteractive() {
    if (!modalEl) return;
    if (step === 0) {
      const first = modalEl.querySelector<HTMLElement>("[data-step0-first]");
      first?.focus();
    } else if (step === 1) {
      const first = modalEl.querySelector<HTMLInputElement>('input[name="szm-rtype"]:checked');
      first?.focus();
    } else if (step === 2) {
      const input = modalEl.querySelector<HTMLInputElement>("input[type='search']");
      input?.focus();
    } else if (step === 3) {
      const push = modalEl.querySelector<HTMLButtonElement>("[data-step3-push]");
      push?.focus();
    } else if (step === 4) {
      modalEl.focus();
    } else if (step === 5 || step === 51) {
      const close = modalEl.querySelector<HTMLButtonElement>("[data-step5-close]");
      close?.focus();
    }
  }

  function dismiss() {
    // Step 4: in-flight push — Escape is intentionally swallowed so
    // the user can't abandon mid-push.
    if (step === 4) return;
    onClose();
  }

  async function gotoStep(next: Step) {
    step = next;
    await tick();
    focusFirstInteractive();
  }

  function selectRecord(r: SzmSearchResult) {
    if (chosenRecord && chosenRecord.id === r.id) {
      chosenRecord = null;
    } else {
      chosenRecord = r;
    }
  }

  function addTag(raw: string) {
    const v = raw.trim();
    if (v.length === 0 || v.length > 120) return;
    if (extraTags.includes(v)) return;
    if (extraTags.length >= 16) return;
    extraTags = [...extraTags, v];
  }

  function removeTag(idx: number) {
    extraTags = extraTags.filter((_, i) => i !== idx);
  }

  function onTagInputKey(e: KeyboardEvent) {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      addTag(newTagInput);
      newTagInput = "";
    } else if (e.key === "Backspace" && newTagInput.length === 0 && extraTags.length > 0) {
      e.preventDefault();
      extraTags = extraTags.slice(0, -1);
    }
  }

  async function doPush() {
    if (!chosenRecord) return;
    pushInFlight = true;
    pushErrorMsg = null;
    pushResponse = null;
    await gotoStep(4);

    try {
      const out = await api.pushCall(callId, {
        module: chosenType.api_name,
        record_id: chosenRecord.id,
        record_name: chosenRecord.name,
        extra_tags: extraTags,
      });
      pushResponse = out;
      onPushed?.(out);
      await gotoStep(5);
    } catch (e: any) {
      const status = e?.status as number | undefined;
      if (status === 429) {
        pushErrorMsg =
          "You've pushed too many calls recently. Try again in a few minutes.";
      } else if (status === 404) {
        pushErrorMsg =
          "Your org's Zoho connection is no longer active. Ask an admin to reconnect.";
      } else if (status === 502) {
        pushErrorMsg = "Zoho returned an error. The call was not pushed.";
      } else if (!status) {
        pushErrorMsg =
          "Unable to reach the server. Your call was not pushed.";
      } else {
        pushErrorMsg = String(e?.message ?? e);
      }
      await gotoStep(51);
    } finally {
      pushInFlight = false;
    }
  }

  function pipClass(idx: 1 | 2 | 3): string {
    // Active / done / pending pip styling. Step 0 + 4+ steps have
    // the indicator hidden via the {#if} block.
    if (step === idx) return "pip working";
    if (step > idx) return "pip done";
    return "pip pending";
  }

  // Title per step.
  const titles: Record<Step, string> = {
    0: "Previously pushed",
    1: "Choose record type",
    2: "Find a record",
    3: "Review and push",
    4: "Pushing…",
    5: "Pushed.",
    51: "Push failed",
  };

  function formatPushedAt(iso: string): string {
    try {
      return new Date(iso).toLocaleString();
    } catch {
      return iso;
    }
  }

  function attrLine(): string {
    if (matchedZohoDisplayName) {
      return `Will be posted as Call by ${matchedZohoDisplayName}`;
    }
    const owner = fallbackOwnerDisplayName ?? "your Zoho admin";
    return `Will be posted as Call by ${owner} · your name will appear in the description`;
  }

  function secondaryFor(r: SzmSearchResult): string {
    return r.secondary ?? "";
  }
</script>

<div
  class="rn-backdrop"
  role="button"
  tabindex="-1"
  onclick={dismiss}
  onkeydown={(e) => {
    if (e.key === "Escape") dismiss();
  }}
>
  <div
    class="rn-modal szm"
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    aria-busy={step === 4 ? "true" : undefined}
    bind:this={modalEl}
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => {
      if (e.key === "Escape") dismiss();
      e.stopPropagation();
    }}
    tabindex="-1"
  >
    <div class="rn-head">
      <h2 id={titleId}>{titles[step]}</h2>
      {#if step === 1 || step === 2 || step === 3}
        <div
          class="szm-steps"
          role="group"
          aria-label={`Step ${step} of 3`}
        >
          <span class={pipClass(1)} aria-hidden="true"></span>
          <span class={pipClass(2)} aria-hidden="true"></span>
          <span class={pipClass(3)} aria-hidden="true"></span>
        </div>
      {/if}
    </div>

    <p id={liveId} class="szm-live" aria-live="polite">{liveAnnouncement}</p>

    <div class="rn-body">
      {#if step === 0 && priorPush}
        <!-- Step 0: prior push summary -->
        <div class="szm-prior">
          <div class="szm-prior-row">
            <span class="szm-caps">{priorPush.module.replace(/s$/, "")}</span>
            <span class="szm-prior-name">{priorPush.record_name}</span>
          </div>
          <div class="szm-prior-meta">
            <span class="szm-prior-when">
              Pushed {formatPushedAt(priorPush.pushed_at)}
            </span>
            {#if priorPush.pushed_by_display_name}
              <span class="szm-prior-by"
                >by {priorPush.pushed_by_display_name}</span
              >
            {/if}
          </div>
        </div>
      {:else if step === 1}
        <fieldset class="szm-rtype">
          <legend>Record type</legend>
          {#each RECORD_TYPES as rt (rt.api_name)}
            <label class="szm-rtype-opt">
              <input
                type="radio"
                name="szm-rtype"
                value={rt.api_name}
                checked={chosenType.api_name === rt.api_name}
                onchange={() => (chosenType = rt)}
              />
              <span class="szm-rtype-text">
                <span class="szm-rtype-name">{rt.label}</span>
                <span class="szm-rtype-kicker">{rt.kicker}</span>
              </span>
            </label>
          {/each}
        </fieldset>
      {:else if step === 2}
        <div class="szm-search">
          <label class="szm-caps" for="szm-q">
            Search {chosenType.label}…
          </label>
          <input
            id="szm-q"
            type="search"
            class="szm-q"
            placeholder={`Search ${chosenType.label}…`}
            bind:value={query}
            aria-label={`Search ${chosenType.label}`}
          />
          <div class="szm-results">
            {#if searchInFlight}
              <p class="szm-results-hint">Searching…</p>
            {:else if searchError}
              <p class="szm-results-err" role="alert">{searchError}</p>
            {:else if query.trim().length === 0}
              <p class="szm-results-hint">
                Type to search {chosenType.label} in your Zoho account.
              </p>
            {:else if results.length === 0}
              <p class="szm-results-hint">
                No {chosenType.label} found for "{query}".
              </p>
            {:else}
              <ul class="szm-results-list">
                {#each results as r (r.id)}
                  <li>
                    <button
                      type="button"
                      class="szm-result"
                      class:szm-result-selected={chosenRecord?.id === r.id}
                      aria-pressed={chosenRecord?.id === r.id ? "true" : "false"}
                      onclick={() => selectRecord(r)}
                    >
                      <span class="szm-result-name">{r.name}</span>
                      {#if secondaryFor(r)}
                        <span class="szm-result-secondary"
                          >{secondaryFor(r)}</span
                        >
                      {/if}
                    </button>
                  </li>
                {/each}
              </ul>
            {/if}
          </div>
        </div>
      {:else if step === 3 && chosenRecord}
        <div class="szm-review">
          <div class="szm-record-card">
            <span class="szm-caps">{chosenType.label}</span>
            <span class="szm-record-name">{chosenRecord.name}</span>
            {#if secondaryFor(chosenRecord)}
              <span class="szm-record-secondary"
                >{secondaryFor(chosenRecord)}</span
              >
            {/if}
          </div>

          <div class="szm-tags">
            <span class="szm-caps">Tags</span>
            {#if extraTags.length === 0}
              <span class="szm-tags-empty">No tags — optional</span>
            {/if}
            <div class="szm-tags-row">
              {#each extraTags as t, i (t)}
                <span class="szm-chip">
                  {t}
                  <button
                    type="button"
                    class="szm-chip-x"
                    aria-label={`Remove tag ${t}`}
                    onclick={() => removeTag(i)}
                  >×</button>
                </span>
              {/each}
              <input
                type="text"
                class="szm-tag-input"
                placeholder={extraTags.length === 0 ? "Add tag" : "Add another"}
                bind:value={newTagInput}
                onkeydown={onTagInputKey}
                onblur={() => {
                  if (newTagInput.trim()) {
                    addTag(newTagInput);
                    newTagInput = "";
                  }
                }}
                aria-label="Add a tag"
              />
            </div>
          </div>

          <div class="szm-attr">
            <span aria-hidden="true">›</span>
            <span>{attrLine()}</span>
          </div>
        </div>
      {:else if step === 4}
        <div class="szm-pushing">
          <span class="pip working" aria-hidden="true"></span>
          <p>Pushing to Zoho CRM…</p>
        </div>
      {:else if step === 5 && pushResponse}
        <div class="szm-success">
          <span class="pip done" aria-hidden="true"></span>
          <p class="szm-success-title">Pushed.</p>
          <p class="szm-success-sub">
            Linked to {chosenRecord?.name ?? "the record"} as a call
            activity.
          </p>
          <button
            type="button"
            class="szm-link-btn"
            onclick={() => zohoUrlHandler(pushResponse!.zoho_url)}
          >
            View in Zoho ↗
          </button>
        </div>
      {:else if step === 51}
        <div class="szm-error" role="alert">
          {pushErrorMsg ?? "The push did not complete. Zoho was not modified."}
        </div>
        <p class="szm-error-hint">
          The push did not complete. Zoho was not modified.
        </p>
      {/if}
    </div>

    <div class="rn-actions">
      {#if step === 0 && priorPush}
        <button
          type="button"
          class="rn-link"
          data-step0-first
          onclick={() =>
            priorPush?.zoho_url && zohoUrlHandler(priorPush.zoho_url)}
          disabled={!priorPush.zoho_url}
        >View in Zoho ↗</button>
        <button
          type="button"
          class="rn-dismiss"
          onclick={() => gotoStep(1)}
        >Push again</button>
      {:else if step === 1}
        <button type="button" class="rn-link" onclick={dismiss}>Cancel</button>
        <button
          type="button"
          class="rn-dismiss"
          onclick={() => gotoStep(2)}
        >Next</button>
      {:else if step === 2}
        <button
          type="button"
          class="rn-link"
          onclick={() => gotoStep(1)}
        >Back</button>
        <button
          type="button"
          class="rn-dismiss"
          onclick={() => gotoStep(3)}
          disabled={!chosenRecord}
        >Next</button>
      {:else if step === 3}
        <button
          type="button"
          class="rn-link"
          onclick={() => gotoStep(2)}
          disabled={pushInFlight}
        >Back</button>
        <button
          type="button"
          class="rn-dismiss"
          data-step3-push
          onclick={doPush}
          disabled={pushInFlight}
        >Push</button>
      {:else if step === 4}
        <span></span>
        <button type="button" class="rn-dismiss" disabled>Pushing…</button>
      {:else if step === 5}
        <span></span>
        <button
          type="button"
          class="rn-dismiss"
          data-step5-close
          onclick={onClose}
        >Close</button>
      {:else if step === 51}
        <button
          type="button"
          class="rn-link"
          onclick={() => gotoStep(3)}
        >Retry</button>
        <button
          type="button"
          class="rn-dismiss"
          data-step5-close
          onclick={onClose}
        >Cancel</button>
      {/if}
    </div>
  </div>
</div>

<style>
  /* All szm-* classes are component-scoped. Reused: .rn-backdrop,
     .rn-modal, .rn-head, .rn-body, .rn-actions, .rn-link, .rn-dismiss,
     .pip from app.css. */

  .szm {
    width: 480px;
    max-width: calc(100vw - 2rem);
    max-height: min(85vh, 600px);
    animation: szm-entry 150ms ease-out both;
  }
  @keyframes szm-entry {
    from {
      opacity: 0;
      transform: scale(0.97);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .szm {
      animation: szm-entry-noscale 120ms ease-out both;
    }
  }
  @keyframes szm-entry-noscale {
    from {
      opacity: 0;
    }
  }

  /* Mobile bottom-sheet — sub-520px the modal slides up from the
     bottom, full-width, with rounded top corners only. */
  @media (max-width: 519px) {
    .szm {
      width: 100%;
      max-width: 100%;
      border-radius: var(--radius-lg) var(--radius-lg) 0 0;
      position: fixed;
      bottom: 0;
      top: auto;
      margin: 0;
    }
  }

  .szm-live {
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

  .szm-steps {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    margin-left: auto;
  }
  /* Local pip override — app.css `.pip` exists but the modal uses a
     small variant. Ensure the dot rendering matches the rest of the
     app. */
  :global(.szm-steps .pip) {
    width: 8px;
    height: 8px;
  }

  .szm-caps {
    display: block;
    font-size: 0.7rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    margin-bottom: 0.35rem;
  }

  /* Step 0 — prior push */
  .szm-prior {
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    padding: 0.8rem 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .szm-prior-row {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .szm-prior-name {
    color: var(--bone-0);
    font-weight: 500;
    font-size: 0.95rem;
  }
  .szm-prior-meta {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
    font-size: 0.78rem;
  }
  .szm-prior-when {
    font-family: var(--font-mono, monospace);
    color: var(--bone-3);
  }
  .szm-prior-by {
    color: var(--bone-2);
  }

  /* Step 1 — record type */
  .szm-rtype {
    border: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .szm-rtype legend {
    padding: 0;
    margin: 0 0 0.4rem;
    font-size: 0.75rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .szm-rtype-opt {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.5rem;
    align-items: center;
    padding: 0.55rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    cursor: pointer;
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .szm-rtype-opt:hover {
    border-color: var(--hairline-hi);
    background: var(--ink-2);
  }
  .szm-rtype-opt:has(input:checked) {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .szm-rtype-text {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .szm-rtype-name {
    color: var(--bone-0);
    font-size: 0.9rem;
    font-weight: 500;
  }
  .szm-rtype-kicker {
    color: var(--bone-3);
    font-size: 0.75rem;
  }

  /* Step 2 — search */
  .szm-search {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .szm-q {
    width: 100%;
    padding: 0.55rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.88rem;
    box-sizing: border-box;
  }
  .szm-q:focus {
    outline: none;
    border-color: var(--accent);
  }
  .szm-results {
    margin-top: 0.4rem;
    max-height: 220px;
    overflow-y: auto;
  }
  .szm-results-hint {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.82rem;
  }
  .szm-results-err {
    margin: 0;
    color: var(--live);
    font-size: 0.82rem;
  }
  .szm-results-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .szm-result {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    padding: 0.55rem 0.75rem;
    border-radius: 6px;
    cursor: pointer;
    color: var(--bone-0);
    transition: background 0.15s;
  }
  .szm-result:hover {
    background: var(--ink-2);
  }
  .szm-result-selected {
    border-left: 2px solid var(--accent);
    padding-left: calc(0.75rem - 2px);
    background: var(--accent-soft);
  }
  .szm-result-name {
    display: block;
    font-size: 0.88rem;
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .szm-result-secondary {
    display: block;
    color: var(--bone-3);
    font-family: var(--font-mono, monospace);
    font-size: 0.75rem;
    margin-top: 0.1rem;
  }

  /* Step 3 — review */
  .szm-review {
    display: flex;
    flex-direction: column;
    gap: 0.9rem;
  }
  .szm-record-card {
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    padding: 0.7rem 0.9rem;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
  }
  .szm-record-name {
    color: var(--bone-0);
    font-size: 0.95rem;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .szm-record-secondary {
    color: var(--bone-3);
    font-family: var(--font-mono, monospace);
    font-size: 0.78rem;
  }
  .szm-tags-empty {
    color: var(--bone-4);
    font-size: 0.82rem;
    margin-bottom: 0.4rem;
    display: block;
  }
  .szm-tags-row {
    display: flex;
    gap: 0.4rem;
    flex-wrap: wrap;
    align-items: center;
  }
  .szm-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    color: var(--bone-1);
    padding: 0.2rem 0.55rem;
    border-radius: 999px;
    font-size: 0.78rem;
  }
  .szm-chip-x {
    background: transparent;
    border: none;
    color: var(--bone-3);
    cursor: pointer;
    padding: 0;
    font-size: 0.95rem;
    line-height: 1;
  }
  .szm-chip-x:hover {
    color: var(--live);
  }
  .szm-tag-input {
    background: transparent;
    border: 1px dashed var(--hairline);
    border-radius: 999px;
    color: var(--bone-1);
    padding: 0.2rem 0.55rem;
    font: inherit;
    font-size: 0.78rem;
    min-width: 8ch;
  }
  .szm-tag-input:focus {
    outline: none;
    border-color: var(--accent);
    border-style: solid;
  }
  .szm-attr {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    color: var(--bone-2);
    font-size: 0.8rem;
  }

  /* Step 4 — pushing */
  .szm-pushing {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.8rem;
    padding: 1.5rem 0;
  }
  .szm-pushing p {
    margin: 0;
    color: var(--bone-2);
    font-size: 0.9rem;
  }

  /* Step 5 — success */
  .szm-success {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.6rem;
    padding: 1rem 0;
  }
  .szm-success-title {
    margin: 0;
    color: var(--bone-0);
    font-size: 1rem;
    font-weight: 600;
  }
  .szm-success-sub {
    margin: 0;
    color: var(--bone-2);
    font-size: 0.85rem;
    text-align: center;
  }
  .szm-link-btn {
    background: transparent;
    border: none;
    color: var(--accent);
    text-decoration: underline;
    cursor: pointer;
    font: inherit;
    font-size: 0.85rem;
    padding: 0.3rem 0.6rem;
  }
  .szm-link-btn:hover {
    color: var(--accent-hi, var(--accent));
  }

  /* Step 5-alt — error */
  .szm-error {
    background: var(--live-soft, rgba(255, 80, 50, 0.12));
    border: 1px solid var(--live);
    border-radius: var(--radius);
    padding: 0.65rem 0.9rem;
    color: var(--live);
    font-size: 0.85rem;
  }
  .szm-error-hint {
    margin: 0.5rem 0 0;
    color: var(--bone-3);
    font-size: 0.8rem;
  }

  /* Local fallback for the .pip "working" / "done" / "pending"
     classes when app.css doesn't define them inline. The modal must
     render correctly even on a stripped style fallback. */
  :global(.szm .pip) {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    display: inline-block;
    background: var(--bone-4);
  }
  :global(.szm .pip.working) {
    background: var(--accent);
    animation: szm-blink 1.2s ease-in-out infinite;
  }
  :global(.szm .pip.done) {
    background: var(--olive);
  }
  :global(.szm .pip.pending) {
    background: var(--bone-4);
  }
  @keyframes szm-blink {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.4;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    :global(.szm .pip.working) {
      animation: none;
    }
  }
</style>
