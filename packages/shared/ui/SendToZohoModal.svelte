<script lang="ts" module>
  // Public types — exported so call-sites on either surface can
  // type the props they pass in. Mirrors the portal's
  // `api.ZohoSearchResult` / `ZohoPushResponse` shapes; agent passes
  // Tauri-bridge results that satisfy these.
  export type SzmSearchResult = {
    id: string;
    name: string;
    secondary?: string | null;
    /** #635 — Deals search: which Zoho-side path produced this hit.
     *  Possible values: "deal" (Deal_Name match — legacy, only-value
     *  for non-Deals modules and the happy path for Deals), "account"
     *  (matched via linked Account_Name), "contact_name" /
     *  "contact_email" / "contact_phone" (matched via linked Contact).
     *  Multiple values accumulate when the same deal surfaces through
     *  more than one path. Absent / `["deal"]` → no second-line render. */
    matched_via?: string[];
    /** #635 — linked Account_Name for the matched deal, when known.
     *  Drives the second-line match path. Absent / null when not
     *  applicable (non-Deals modules, deal-only matches). */
    account_name?: string | null;
    /** #635 — linked Contact display name for the matched deal. */
    contact_name?: string | null;
    /** #635 — Contact's stored phone or email (whichever matched, if
     *  available). The backend forwards the Zoho value untouched so
     *  the user sees the contact's data as their org has it. */
    contact_secondary?: string | null;
  };
  export type SzmTag = { kind: string; value: string };
  export type SzmPushResponse = {
    push_id: string;
    zoho_call_record_id: string;
    zoho_url: string;
    tags_added: SzmTag[];
  };
  export type SzmPriorPush = {
    /** #639 — Optional for unlinked prior pushes. Backend omits these
     *  three fields from the wire when the prior push had no parent
     *  record; render the Step-0 prior-push card with an "Unlinked
     *  call" label in that case. */
    module?: string;
    record_id?: string;
    record_name?: string;
    pushed_at: string;
    pushed_by_display_name?: string | null;
    zoho_url?: string | null;
  };
  /** Record-type entry as returned by /v1/zoho/record-types (#197).
   *  The picker treats `singular_label` as the human label,
   *  `plural_label` as the search-page heading. Custom modules carry
   *  resolved `search_field` / `name_field`; standards leave them
   *  empty (the backend uses a hard-coded map). */
  export type SzmRecordType = {
    api_name: string;
    plural_label: string;
    singular_label: string;
    search_field?: string;
    name_field?: string;
  };

  /** /v1/zoho/record-types response shape — standards + customs +
   *  last-refresh hint. Custom-module discovery (#197) populates the
   *  `custom` slice on connect or via the admin Refresh button. */
  export type SzmRecordTypes = {
    standard: SzmRecordType[];
    custom: SzmRecordType[];
    custom_refreshed_at?: string | null;
  };

  export type SzmApi = {
    /** Fetch the org's record-type allowlist — standards + customs.
     *  Surface-specific transport per the mirror-pair invariant. */
    recordTypes: () => Promise<SzmRecordTypes>;
    searchRecords: (
      module: string,
      q: string,
    ) => Promise<SzmSearchResult[]>;
    pushCall: (
      callId: string,
      body: {
        /** #639 — unlinked push (no parent record). When `true`, the
         *  three relation fields below are ignored by the backend; the
         *  modal omits them entirely (not empty strings) so the wire
         *  shape stays clean. */
        unlinked?: boolean;
        module?: string;
        record_id?: string;
        record_name?: string;
        extra_tags?: string[];
      },
    ) => Promise<SzmPushResponse>;
  };
</script>

<script lang="ts">
  // Send-to-CRM (Zoho) modal — canonical shared component (#186).
  //
  // Two surfaces consume this one source file. Surface-specific
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

  import { onMount, tick, untrack } from "svelte";

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
     * invoke("open_url"). The modal never owns the divergent code
     * path; the call-site does.
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
  let step = $state<Step>((untrack(() => priorPush) ? 0 : 1) as Step);
  let modalEl = $state<HTMLDivElement | null>(null);
  const titleId = `szm-title-${Math.random().toString(36).slice(2, 9)}`;
  const liveId = `szm-live-${Math.random().toString(36).slice(2, 9)}`;
  let liveAnnouncement = $state("");

  // Step-1 record type picker. v2 (#197) fetches the list at mount —
  // standards always present, customs cached server-side from the
  // org's `/settings/modules?type=Custom` discovery.
  type RecordType = {
    api_name: string;
    label: string;
    kicker: string;
    /** True for entries from the cached custom-modules list. Drives
     *  the divider + sub-label copy; never sent to the backend. */
    is_custom: boolean;
  };
  // #639 — sentinel api_name for the "Unlinked Call" pinned entry.
  // UI-only: NEVER sent to the backend as a module name. The doPush
  // builder branches on this sentinel to send `{ unlinked: true }`
  // instead of `{ module, record_id, record_name }`.
  const UNLINKED_API_NAME = "UNLINKED";
  const UNLINKED_TYPE: RecordType = {
    api_name: UNLINKED_API_NAME,
    label: "Unlinked Call",
    kicker: "No parent record",
    is_custom: false,
  };
  // Hard-coded standard kickers — keep the v1 copy verbatim for the
  // 5 standards so the visible copy doesn't regress on a partial
  // record-types fetch.
  const STANDARD_KICKERS: Record<string, string> = {
    Contacts: "Person",
    Deals: "Opportunity",
    Accounts: "Company",
    Leads: "Unqualified lead",
    Quotes: "Quote document",
  };
  // Fallback used when fetching record types fails — keeps the v1 push
  // path alive even if the network drops the discovery hop.
  const STANDARD_FALLBACK: RecordType[] = [
    { api_name: "Contacts", label: "Contact", kicker: "Person", is_custom: false },
    { api_name: "Deals", label: "Deal", kicker: "Opportunity", is_custom: false },
    { api_name: "Accounts", label: "Account", kicker: "Company", is_custom: false },
    { api_name: "Leads", label: "Lead", kicker: "Unqualified lead", is_custom: false },
    { api_name: "Quotes", label: "Quote", kicker: "Quote document", is_custom: false },
  ];
  let standardTypes = $state<RecordType[]>(STANDARD_FALLBACK);
  let customTypes = $state<RecordType[]>([]);
  let recordTypesLoading = $state(true);
  let recordTypesError = $state<string | null>(null);
  let chosenType = $state<RecordType>(STANDARD_FALLBACK[0]);

  // #639 — true when the user has picked the pinned Unlinked Call entry.
  // Drives Step-1 → Step-3 skip, Step-3 review-card branch, push-body
  // shape, pip-2 dimming, and Step-5 success copy.
  const isUnlinked = $derived(chosenType.api_name === UNLINKED_API_NAME);
  // #639 — pip 2 must render as "skipped" (not "done") once we've
  // advanced past Step 1 via the unlinked path. Drives `pipClass(2)`
  // override + the `.szm-steps` aria-label suffix + the live-region
  // announcement at Step 3.
  const skippedStep2 = $derived(isUnlinked && step >= 3);

  // Step-1 record-type filter (#221). Modal-session only — closing
  // and reopening the modal resets to empty, since the whole
  // component re-mounts under {#if zohoModalOpen}.
  let typeFilter = $state("");
  function rtypeMatches(rt: RecordType, q: string): boolean {
    if (q.length === 0) return true;
    const needle = q.toLowerCase();
    return (
      rt.label.toLowerCase().includes(needle) ||
      rt.api_name.toLowerCase().includes(needle)
    );
  }
  const filteredStandard = $derived(
    standardTypes.filter((rt) => rtypeMatches(rt, typeFilter.trim())),
  );
  const filteredCustom = $derived(
    customTypes.filter((rt) => rtypeMatches(rt, typeFilter.trim())),
  );
  const filteredEmpty = $derived(
    filteredStandard.length === 0 && filteredCustom.length === 0,
  );

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
  let extraTags = $state<string[]>(
    seedExtraTags(untrack(() => existingTags)),
  );
  let newTagInput = $state("");

  function seedExtraTags(seed: SzmTag[]): string[] {
    return seed.filter((t) => t.kind === "custom").map((t) => t.value);
  }

  // Step-4 + Step-5 push state.
  let pushInFlight = $state(false);
  let pushResponse = $state<SzmPushResponse | null>(null);
  let pushErrorMsg = $state<string | null>(null);
  let pushRetryBlocked = $state(false);
  let pushResumeOnly = $state(false);
  let pushErrorHint = $state(
    "The push did not complete. Zoho was not modified.",
  );

  // Minimum chars before firing a Zoho search. Mirrors backend
  // `SEARCH_MIN_CHARS` (#219); Zoho rejects shorter values from
  // `(Field:contains:X)` with a 400 that would surface as the
  // misleading "Search unavailable" banner.
  const SEARCH_MIN_CHARS = 2;

  $effect(() => {
    // Reactive search effect. Re-runs on query change with a 250ms
    // debounce; older in-flight requests get superseded by checking
    // `searchToken` after await.
    const token = ++searchToken;
    if (step !== 2) return;
    const q = query.trim();
    if (q.length === 0) {
      results = [];
      searchError = null;
      searchInFlight = false;
      return;
    }
    if (q.length < SEARCH_MIN_CHARS) {
      // 1-char queries: empty results + helper hint, no network hop.
      results = [];
      searchError = null;
      searchInFlight = false;
      return;
    }
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
        // #639 — announce the skipped step for screen readers on the
        // unlinked path so listeners aren't surprised by the pip-row's
        // dimmed dot.
        msg = isUnlinked
          ? "Step 3 of 3: review and push (step 2 skipped)"
          : "Step 3 of 3: review and push";
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
    // Fetch record types in parallel with the focus sequence (#197).
    // We deliberately don't block focus on this — Step 1 renders with
    // the v1 fallback list immediately, then upgrades to the
    // discovered list when it arrives. On fetch failure, the fallback
    // keeps the push path alive.
    void loadRecordTypes();
  });

  async function loadRecordTypes() {
    recordTypesLoading = true;
    recordTypesError = null;
    try {
      const out = await api.recordTypes();
      // Standards: prefer backend labels but use the local kicker map
      // so we don't show "Contacts/Contact/Person" inconsistently if
      // the backend label drift.
      const standards: RecordType[] = (out.standard ?? []).map((m) => ({
        api_name: m.api_name,
        label: m.singular_label || m.api_name,
        kicker: STANDARD_KICKERS[m.api_name] ?? m.plural_label ?? "",
        is_custom: false,
      }));
      // Customs: alphabetic by singular_label.
      const customs: RecordType[] = (out.custom ?? [])
        .map((m) => ({
          api_name: m.api_name,
          label: m.singular_label || m.api_name,
          kicker: "Custom module",
          is_custom: true,
        }))
        .sort((a, b) => a.label.localeCompare(b.label));
      if (standards.length > 0) {
        standardTypes = standards;
      }
      customTypes = customs;
      // Re-bind the selection to the freshly-loaded standards entry
      // (so identity-equality on the radio group works) — fall back to
      // the first standard if the previous chosen entry is no longer
      // in either list.
      //
      // #639 — the Unlinked Call sentinel is UI-only and not present in
      // `standardTypes`/`customTypes`. If the user picked it before
      // this fetch resolved, leave the selection untouched; clobbering
      // back to Contact would silently undo their click.
      if (chosenType.api_name !== UNLINKED_API_NAME) {
        const all = [...standardTypes, ...customTypes];
        const match = all.find((t) => t.api_name === chosenType.api_name);
        chosenType = match ?? standardTypes[0];
      }
    } catch (e: any) {
      // Keep the fallback standards list; surface a small error
      // banner so the user knows custom modules aren't available.
      recordTypesError =
        "Couldn't load custom modules. Standard records still work.";
    } finally {
      recordTypesLoading = false;
    }
  }

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
    // #639 — unlinked push needs no chosenRecord; linked push still
    // requires one. Guard accordingly.
    if (!isUnlinked && !chosenRecord) return;
    pushInFlight = true;
    pushErrorMsg = null;
    pushRetryBlocked = false;
    pushResumeOnly = false;
    pushErrorHint = "The push did not complete. Zoho was not modified.";
    pushResponse = null;
    await gotoStep(4);

    try {
      // #639 — for unlinked, omit module/record_id/record_name entirely
      // (not empty strings — the backend's wire contract treats absence
      // as "client honored the unlinked discriminator").
      const pushBody = isUnlinked
        ? { unlinked: true, extra_tags: extraTags }
        : {
            module: chosenType.api_name,
            record_id: chosenRecord!.id,
            record_name: chosenRecord!.name,
            extra_tags: extraTags,
          };
      const out = await api.pushCall(callId, pushBody);
      pushResponse = out;
      onPushed?.(out);
      await gotoStep(5);
    } catch (e: any) {
      const status = e?.status as number | undefined;
      const code = e?.code as string | undefined;
      const retryGuidance = e?.retryGuidance as string | undefined;
      if (retryGuidance === "new_request_after_correction") {
        pushErrorMsg = "The push failed before completion.";
        pushErrorHint =
          "Correct the integration or record details before trying again.";
      } else if (retryGuidance === "new_request") {
        pushErrorMsg = "The push was cancelled.";
        pushErrorHint =
          "You can start a new push if you still want to send this call.";
      } else if (code === "external_push_uncertain") {
        pushRetryBlocked = true;
        pushErrorMsg =
          "The server could not confirm whether the push completed.";
        pushErrorHint =
          "Do not start another push yet. Ask an admin to reconcile the call first.";
      } else if (code === "external_push_pending") {
        pushResumeOnly = true;
        pushErrorMsg =
          "The push is still processing or its response could not be confirmed.";
        pushErrorHint =
          "Retrying here safely reuses the same request; it will not start a second push.";
      } else if (code === "external_push_protocol") {
        pushRetryBlocked = true;
        pushErrorMsg =
          "The server returned an invalid push status.";
        pushErrorHint =
          "Do not start another push. Contact support so the attempt can be reconciled.";
      } else if (status === 429) {
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
    // #639 — pip 2 on the unlinked path renders as "skipped" instead of
    // "done" once the user has advanced to Step 3 — the user did not
    // complete Step 2 (they jumped past it), so showing an olive "done"
    // dot would misrepresent what they did.
    if (idx === 2 && skippedStep2) return "pip skipped";
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

  // #635 / #648 — match-path second line for the multi-path search.
  //
  // Deals (#635): suppress when only the deal-name path matched
  // (`["deal"]`); otherwise join account / contact / contact-secondary
  // with thin-space + middle dot.
  //
  // Leads (#648): suppress when only the last-name path matched
  // (`["last_name"]`, the legacy single-path behaviour); otherwise
  // render the path tags directly as "Matched via …" since Leads are
  // leaf entities with no account/contact join provenance to surface.
  // Multiple matched paths join with commas (e.g. "Matched via email,
  // company") which reads better than the middle-dot separator used
  // for the Deal provenance triple.
  function matchPathFor(r: SzmSearchResult): string {
    const via = r.matched_via;
    if (!via || via.length === 0) return "";
    // Happy paths — suppress the line entirely.
    if (
      via.length === 1 &&
      (via[0] === "deal" || via[0] === "last_name")
    )
      return "";
    // Lead path tags don't carry account/contact join provenance —
    // render a "Matched via <labels>" string from the tags directly.
    const leadTagLabels: Record<string, string> = {
      last_name: "last name",
      first_name: "first name",
      email: "email",
      phone: "phone",
      company: "company",
    };
    if (via.every((t) => t in leadTagLabels)) {
      const named = via.map((t) => leadTagLabels[t]).join(", ");
      return `Matched via ${named}`;
    }
    // Existing Deal path-line (account · contact · secondary).
    const parts: string[] = [];
    if (r.account_name) parts.push(r.account_name);
    if (r.contact_name) parts.push(r.contact_name);
    if (r.contact_secondary) parts.push(r.contact_secondary);
    if (parts.length === 0) return "";
    return parts.join(" · ");
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
          aria-label={skippedStep2
            ? `Step ${step} of 3 (step 2 skipped)`
            : `Step ${step} of 3`}
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
        <!-- #639 — when the prior push was an unlinked Call, the
             backend omits module + record_name from the response;
             render a literal "Unlinked call" label instead of running
             the module-singular transform on `undefined`. -->
        <div class="szm-prior">
          <div class="szm-prior-row">
            {#if priorPush.module}
              <span class="szm-caps">{priorPush.module.replace(/s$/, "")}</span>
              <span class="szm-prior-name">{priorPush.record_name}</span>
            {:else}
              <span class="szm-caps">Call</span>
              <span class="szm-prior-name">Unlinked call</span>
            {/if}
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
          <!-- Filter input (#221). Empty value = original list.
               Live substring match against label + api_name; both
               standards and customs filter through the same gate.
               Modal-session only: closing/reopening resets it. -->
          <input
            type="search"
            class="szm-rtype-filter"
            bind:value={typeFilter}
            placeholder="Filter record types"
            aria-label="Filter record types"
          />
          <!-- #639 — pinned "Unlinked Call" entry. Always rendered above
               the standards; not filtered by the search input. Selecting
               it skips Step 2 entirely on Next. -->
          <label class="szm-rtype-opt szm-rtype-opt-unlinked">
            <input
              type="radio"
              name="szm-rtype"
              value={UNLINKED_TYPE}
              bind:group={chosenType}
            />
            <span class="szm-rtype-text">
              <span class="szm-rtype-name">{UNLINKED_TYPE.label}</span>
              <span class="szm-rtype-kicker">{UNLINKED_TYPE.kicker}</span>
            </span>
          </label>
          <!-- Separator below the pinned entry. Renders unconditionally
               so the pinned row is always visually distinct from the
               filterable list below (even when the filter empties all
               standards). -->
          <div
            class="szm-rtype-divider"
            role="presentation"
            aria-hidden="true"
          ></div>
          {#each filteredStandard as rt (rt.api_name)}
            <label class="szm-rtype-opt">
              <input
                type="radio"
                name="szm-rtype"
                value={rt}
                bind:group={chosenType}
              />
              <span class="szm-rtype-text">
                <span class="szm-rtype-name">{rt.label}</span>
                <span class="szm-rtype-kicker">{rt.kicker}</span>
              </span>
            </label>
          {/each}
          {#if filteredCustom.length > 0}
            <!-- Divider only renders when this org has custom modules
                 with at least one matching the active filter (#197 +
                 #221). Sort order is alphabetic by singular_label,
                 already applied in loadRecordTypes(). -->
            <div
              class="szm-rtype-divider"
              role="presentation"
              aria-hidden="true"
            ></div>
            {#each filteredCustom as rt (rt.api_name)}
              <label class="szm-rtype-opt">
                <input
                  type="radio"
                  name="szm-rtype"
                  value={rt}
                  bind:group={chosenType}
                />
                <span class="szm-rtype-text">
                  <span class="szm-rtype-name">{rt.label}</span>
                  <span class="szm-rtype-kicker">{rt.kicker}</span>
                </span>
              </label>
            {/each}
          {/if}
          {#if filteredEmpty}
            <!-- Empty state when no record type matches the filter. -->
            <p class="szm-rtype-empty" role="status">
              No record types match "{typeFilter.trim()}".
            </p>
          {/if}
          {#if recordTypesError}
            <!-- Non-blocking — the picker still works for standards. -->
            <p class="szm-rtype-err" role="status">
              {recordTypesError}
            </p>
          {/if}
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
            {:else if query.trim().length < SEARCH_MIN_CHARS}
              <!-- Below min — no network hop yet (#219). -->
              <p class="szm-results-hint">
                Keep typing to search {chosenType.label}.
              </p>
            {:else if results.length === 0}
              {#if chosenType.api_name === "Deals"}
                <!-- #635 — Deals empty state doubles as the
                     discoverability hint for the multi-path search.
                     Other modules keep their single-line hint. -->
                <p class="szm-results-hint">
                  No deals found for "{query}".
                </p>
                <p class="szm-results-hint">
                  Search by deal name, account, contact, email, or phone.
                </p>
              {:else}
                <p class="szm-results-hint">
                  No {chosenType.label} found for "{query}".
                </p>
              {/if}
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
                      <span class="szm-result-row">
                        <span class="szm-result-name">{r.name}</span>
                        {#if secondaryFor(r)}
                          <span class="szm-result-secondary"
                            >{secondaryFor(r)}</span
                          >
                        {/if}
                      </span>
                      {#if matchPathFor(r)}
                        <span class="szm-result-path">{matchPathFor(r)}</span>
                      {/if}
                    </button>
                  </li>
                {/each}
              </ul>
            {/if}
          </div>
        </div>
      {:else if step === 3 && isUnlinked}
        <!-- #639 — unlinked review card. No chosenRecord; the user is
             pushing a standalone Call activity with no Zoho parent. -->
        <div class="szm-review">
          <div class="szm-record-card szm-record-card-unlinked">
            <span class="szm-caps">CALL</span>
            <span class="szm-record-name">Not linked to any Zoho record</span>
            <span class="szm-record-secondary"
              >You can link it inside Zoho after pushing.</span
            >
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
            {#if isUnlinked}
              Pushed as an unlinked call activity.
            {:else}
              Linked to {chosenRecord?.name ?? "the record"} as a call
              activity.
            {/if}
          </p>
          {#if pushResponse.zoho_url}
            <button
              type="button"
              class="szm-link-btn"
              onclick={() => zohoUrlHandler(pushResponse!.zoho_url)}
            >
              View in Zoho ↗
            </button>
          {/if}
        </div>
      {:else if step === 51}
        <div class="szm-error" role="alert">
          {pushErrorMsg ?? "The push did not complete. Zoho was not modified."}
        </div>
        <p class="szm-error-hint">{pushErrorHint}</p>
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
          onclick={() => gotoStep(isUnlinked ? 3 : 2)}
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
          onclick={() => gotoStep(isUnlinked ? 1 : 2)}
          disabled={pushInFlight}
        >Back</button>
        <button
          type="button"
          class="rn-dismiss"
          data-step3-push
          onclick={doPush}
          disabled={pushInFlight || (!isUnlinked && !chosenRecord)}
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
        {#if pushRetryBlocked}
          <span></span>
        {:else if pushResumeOnly}
          <button
            type="button"
            class="rn-link"
            onclick={doPush}
          >Check status</button>
        {:else}
          <button
            type="button"
            class="rn-link"
            onclick={() => gotoStep(3)}
          >Retry</button>
        {/if}
        <button
          type="button"
          class="rn-dismiss"
          data-step5-close
          onclick={onClose}
        >Close</button>
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
  /* #639 — skipped pip variant for the Unlinked-Call path. Dimmed to
     signal "this step was bypassed by your choice," not olive (would
     read as "you completed Step 2"). Component-scoped: never propagated
     to app.css. */
  :global(.szm-steps .pip.skipped) {
    background: var(--bone-4);
    opacity: 0.45;
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
    /* Make the whole row a single click target (#220). The label
       wraps both the radio + this text block, but inner span layout
       can absorb the click before the browser's "click label →
       toggle child input" semantics fire. Forcing pointer-events
       to skip these spans keeps the click target as the label. */
    pointer-events: none;
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
  /* Divider between standard + custom modules (#197). Hairline
     keeps the rest of the surface coherent; 0.4rem vertical breathing
     room mirrors the standard pip-row separation in app.css. */
  .szm-rtype-divider {
    height: 1px;
    background: var(--hairline);
    margin: 0.3rem 0.2rem;
  }
  .szm-rtype-err {
    margin: 0.3rem 0 0;
    color: var(--bone-3);
    font-size: 0.78rem;
  }
  /* Filter input above the radio list (#221). Sits flush with the
     legend so the picker still reads as one block; matches the
     Step-2 search input's tone but tightened a notch. */
  .szm-rtype-filter {
    width: 100%;
    padding: 0.45rem 0.65rem;
    margin: 0 0 0.15rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.85rem;
    box-sizing: border-box;
  }
  .szm-rtype-filter:focus {
    outline: none;
    border-color: var(--accent);
  }
  .szm-rtype-empty {
    margin: 0.3rem 0 0;
    color: var(--bone-3);
    font-size: 0.82rem;
    font-style: italic;
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
    /* #635 — column flex so the optional match-path line stacks
       under the name+secondary row without disturbing the existing
       row's geometry. */
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
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
  /* #635 — first row inside .szm-result: name (left, flex-grows) +
     optional secondary (right). Replaces the legacy block-stack
     layout; the second-line match path now lives below this row. */
  .szm-result-row {
    display: flex;
    width: 100%;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
    min-width: 0;
  }
  .szm-result-name {
    display: block;
    font-size: 0.88rem;
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
    flex: 1 1 auto;
  }
  .szm-result-secondary {
    display: block;
    color: var(--bone-3);
    font-family: var(--font-mono, monospace);
    font-size: 0.75rem;
    flex: 0 0 auto;
  }
  /* #635 — match-path second line for Deals multi-path hits. Plain
     mid-grey treatment, single-line ellipsis, no chips/icons/colour-
     coding by `matched_via` kind (issue body explicitly asks for a
     plain visual treatment). Width-constrained to the row so a long
     account+contact+email string truncates instead of wrapping. */
  .szm-result-path {
    display: block;
    width: 100%;
    color: var(--bone-3);
    font-size: 0.85em;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
