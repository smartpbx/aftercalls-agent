<script lang="ts" module>
  // Mirror-pair component — byte-identical between
  // portal/src/lib/ActionsList.svelte and
  // agent/src/lib/ActionsList.svelte. Fifth mirror pair (after
  // Avatar, SpeakerRenamePicker, SummaryText, ActionItem). Shipped
  // as part of v0.4.0 Phase 4 (#105) — see ui-phase-4 §J.
  //
  // Pure presentational shell: the page wrapper owns all network
  // calls, filter URL persistence, and optimistic-update logic.
  // This component just renders what it's given and emits events
  // on filter / toggle / load-more / retry. That split keeps the
  // diff-check viable — no invoke() on agent vs fetch() on portal
  // living inside the mirrored file.

  import type { ActionItemUser } from "./ActionItem.svelte";

  // Row as served by the me-scoped backend endpoint. The page
  // wrapper's fetch / invoke decodes into this shape. Fields mirror
  // `MeActionItem` in `backend/src/routes/action_items.rs`.
  export type MeActionItem = {
    id: string;
    call_id: string;
    description: string;
    assignee_user_id: string | null;
    status: "open" | "done";
    completed_at: string | null;
    completed_by_user_id: string | null;
    source: "llm" | "manual";
    order_index: number;
    created_at: string;
    created_by_user_id: string | null;
    call_title: string | null;
    call_recorded_at: string;
  };

  // Segmented filter values. Whitelisted at the page wrapper before
  // this prop lands, so malformed URL params never reach the
  // component.
  export type ActionsStatusFilter = "open" | "done" | "all";

  // Envelope shape the backend's `GET /v1/me/action-items` returns.
  // Re-exported here so both surfaces (portal fetch, agent Tauri
  // invoke) can decode directly into this type without importing
  // from `$lib/api.ts` on the agent (which has no `api.ts`).
  export type MeActionItemsResponse = {
    items: MeActionItem[];
    next_cursor: string | null;
    total_open: number;
    total_done: number;
    total_all: number;
  };

  // Per-filter row counts used to label the segmented control.
  // `all` is the union of open + done; page wrapper computes it so
  // the component doesn't have to re-derive.
  export type ActionsTotals = {
    open: number;
    done: number;
    all: number;
  };

  // Event payload from a row check-off. Page wrapper runs the PATCH
  // and handles the optimistic + rollback logic — this component
  // just emits.
  export type ActionsToggleEvent = {
    itemId: string;
    callId: string;
    nextStatus: "open" | "done";
  };

  // #126 (v0.4.2): the actions list also surfaces click-to-edit
  // for description + owner, same UX as the call-detail page. The
  // list component forwards ActionItem's granular callbacks; the
  // page wrapper owns the PATCH. Mutual-exclusion state (`activeRowEdit`)
  // lives in the page wrapper so row-across-page coordination is
  // the wrapper's responsibility.
  export type ActionsActiveRowEdit =
    | { kind: "none" }
    | { kind: "description"; itemId: string }
    | { kind: "owner"; itemId: string };

  export type ActionsDescriptionSave = {
    itemId: string;
    callId: string;
    description: string;
  };
  export type ActionsOwnerSave = {
    itemId: string;
    callId: string;
    assigneeUserId: string | null;
  };
</script>

<script lang="ts">
  import ActionItem from "./ActionItem.svelte";
  import { fade } from "svelte/transition";

  type Props = {
    // Rows to render. Page wrapper applies filter + merges pages;
    // this component treats the list as the source of truth for
    // what's currently visible.
    items: MeActionItem[];
    // Current filter. Mirrors what the segmented control paints as
    // active + what the subhead / aria-live announce.
    status: ActionsStatusFilter;
    // First-load spinner. Distinct from `loadingMore` so the list
    // doesn't jitter when paginating.
    loading: boolean;
    // Pagination button state. `loadingMore=true` → button label
    // flips to "Loading…" + disables.
    loadingMore: boolean;
    // Null on success, string on error. Covers the initial-load
    // error state — load-more errors surface via `loadMoreError`.
    error: string | null;
    // Non-null when the previous cursor request failed. Flips the
    // Load more button label to "Retry" per ui-phase-4 §G.
    loadMoreError: string | null;
    // Backend's pagination cursor. `null` hides the Load more
    // button (final page).
    nextCursor: string | null;
    // Totals for the segmented control. Unknown counts render "—".
    totals?: ActionsTotals | null;
    // Org roster for assignee Avatar resolution. In practice every
    // row's assignee === the caller (the endpoint is me-scoped), but
    // resolving through the roster keeps the render path identical
    // to the call-detail ActionItem and catches "assignee was
    // reassigned after the page loaded" cleanly.
    orgMembers: ActionItemUser[];
    // Transient check-off error. Fixed-position 3s inline note at
    // top of the list per ui-phase-4 §G (no toast system in-repo).
    // Page wrapper sets + clears via setTimeout.
    transientError?: string | null;
    // Optional per-row in-flight flag keyed on item id. When true
    // the ActionItem's checkbox shows a disabled state to prevent
    // a double-PATCH race.
    togglingIds?: Set<string>;
    // Color lookup reused from the call-detail page (maps a speaker
    // display name to the shared palette). Absent → default accent.
    colorFor?: (name: string) => string;

    // Events — page wrapper owns all side effects.
    onfilterchange?: (next: ActionsStatusFilter) => void;
    ontoggle?: (e: ActionsToggleEvent) => void;
    onloadmore?: () => void;
    onretry?: () => void;

    // #126 (v0.4.2): click-to-edit machinery, parallel to what the
    // call-detail page wires on each ActionItem. `canEdit` gates
    // entry into edit mode; when unset/false, rows render read-only
    // (preserves prior /actions-page behaviour for surfaces that
    // don't want in-place edit).
    canEdit?: boolean;
    activeRowEdit?: ActionsActiveRowEdit;
    patchingItemIds?: Set<string>;
    actionItemErrors?: Record<string, string>;
    onDescriptionEditRequest?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onOwnerEditRequest?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onDescriptionSave?: (payload: ActionsDescriptionSave) => void;
    onOwnerSave?: (payload: ActionsOwnerSave) => void;
    onDescriptionCancel?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onOwnerCancel?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onEditErrorClear?: (payload: {
      item: { id: string; call_id: string };
    }) => void;

    // #147 (v0.4.7) — chip-edit forwarding onto the /actions page.
    // Mirrors the call-detail page's ChipMenu wiring: the nested
    // SummaryText inside each read-only action-item description
    // emits chip clicks via ActionItem; ActionsList forwards them
    // up to the page wrapper, which mounts a single <ChipMenu> and
    // keeps all PATCH bookkeeping in one place. Optional — when
    // unset, chips stay non-interactive (read-only surfaces).
    onactionitemchipaction?: (detail: {
      inner: string;
      occurrenceIndex: number;
      anchor: HTMLElement;
      itemId: string;
    }) => void;
    // Page-level active-chip indicator so the matching
    // `.name-chip-active` outline lands on the right row.
    activeChipItemId?: string | null;
    activeChipOccurrenceIndex?: number | null;
  };

  let {
    items,
    status,
    loading,
    loadingMore,
    error,
    loadMoreError,
    nextCursor,
    totals = null,
    orgMembers,
    transientError = null,
    togglingIds = new Set<string>(),
    colorFor,
    onfilterchange,
    ontoggle,
    onloadmore,
    onretry,
    canEdit = false,
    activeRowEdit = { kind: "none" } as ActionsActiveRowEdit,
    patchingItemIds = new Set<string>(),
    actionItemErrors = {},
    onDescriptionEditRequest,
    onOwnerEditRequest,
    onDescriptionSave: onDescSaveProp,
    onOwnerSave: onOwnerSaveProp,
    onDescriptionCancel,
    onOwnerCancel,
    onEditErrorClear,
    onactionitemchipaction,
    activeChipItemId = null,
    activeChipOccurrenceIndex = null,
  }: Props = $props();

  // Adapters: ActionItem fires with {itemId, description} /
  // {itemId, assigneeUserId}; the /actions page wants to know the
  // call_id too. We look up the row by id in the currently-visible
  // items list (it must be present — the fire came from one of the
  // rendered rows) to recover call_id.
  function findCallId(itemId: string): string | null {
    return items.find((it) => it.id === itemId)?.call_id ?? null;
  }
  function onDescriptionSaveForward(payload: {
    itemId: string;
    description: string;
  }) {
    const callId = findCallId(payload.itemId);
    if (!callId) return;
    onDescSaveProp?.({
      itemId: payload.itemId,
      callId,
      description: payload.description,
    });
  }
  function onOwnerSaveForward(payload: {
    itemId: string;
    assigneeUserId: string | null;
  }) {
    const callId = findCallId(payload.itemId);
    if (!callId) return;
    onOwnerSaveProp?.({
      itemId: payload.itemId,
      callId,
      assigneeUserId: payload.assigneeUserId,
    });
  }

  // Subhead copy mirrors ui-phase-4 §Copy verbatim. The three
  // branches reinforce the "across all calls" framing new users
  // need; architect rejected a single "your action items" line per
  // §Open Q 4.
  let subhead = $derived.by(() => {
    switch (status) {
      case "open":
        return "Your open items across all calls.";
      case "done":
        return "Your completed items across all calls.";
      case "all":
        return "Every action item from your calls.";
    }
  });

  // aria-live announcement on filter change. `{n} open action items`
  // / `{n} done action items` / `{n} action items`. Uses the
  // passed-through `items.length` which the page wrapper merges
  // across pages — exactly the number of rendered rows, not a
  // potentially-stale backend total.
  let countLabel = $derived.by(() => {
    const n = items.length;
    switch (status) {
      case "open":
        return `${n} open action items`;
      case "done":
        return `${n} done action items`;
      case "all":
        return `${n} action items`;
    }
  });

  // Segmented filter options. Layout + class names mirror admin's
  // `.segmented` pattern per ui-phase-4 §D — namespaced under
  // `.actions-filter` so the two surfaces don't cross-couple.
  const filterOptions: Array<{ v: ActionsStatusFilter; label: string }> = [
    { v: "open", label: "Open" },
    { v: "done", label: "Done" },
    { v: "all", label: "All" },
  ];

  function setFilter(next: ActionsStatusFilter) {
    if (next === status) return;
    onfilterchange?.(next);
  }

  // The child ActionItem's `ontoggle` fires with `item: ActionItem`
  // — a structural subset of MeActionItem. We already know the row
  // came from our `items` array, so we look it up on the way back
  // out to recover the wider shape (primarily `call_id`, which
  // ActionItem's row shape already carries, but keeping the lookup
  // keeps the types straightforward and picks up any future
  // MeActionItem-only fields without a churn).
  function handleRowToggle(payload: {
    item: { id: string; call_id: string };
    nextStatus: "open" | "done";
  }) {
    ontoggle?.({
      itemId: payload.item.id,
      callId: payload.item.call_id,
      nextStatus: payload.nextStatus,
    });
  }

  function handleLoadMore() {
    if (loadingMore) return;
    onloadmore?.();
  }

  function handleRetry() {
    onretry?.();
  }

  // Empty-state copy per ui-phase-4 §F. Three branches so the
  // wording matches what the user was looking for ("all caught up"
  // reads differently on Done than on Open).
  let emptyTitle = $derived.by(() => {
    switch (status) {
      case "open":
        return "All caught up.";
      case "done":
        return "Nothing done yet.";
      case "all":
        return "No action items yet.";
    }
  });
  let emptySub = $derived.by(() => {
    switch (status) {
      case "open":
        return "New action items from your calls will appear here.";
      case "done":
        return "Check off an open item and it will show up here.";
      case "all":
        return "Record a call to generate action items, or add one manually on a call's detail page.";
    }
  });
</script>

<!-- #131 — align to /calls shell: <main> landmark + horizontal
     centering + reserved right-side actions lane in the header + a
     filter-row wrapper so future filters can join the segmented
     control without reshuffling the tree. Verified both portal and
     agent layouts have no competing <main> (D4). -->
<main class="actions-page">
  <header class="actions-head">
    <div class="actions-head-main">
      <h1>Action items</h1>
      <p class="actions-subhead">{subhead}</p>
    </div>
    <!-- Reserved right-side lane (ui-spec D3). Empty in v0.4.4;
         future filters / scope toggles mount here. `aria-hidden`
         comes off when content lands. -->
    <div class="actions-head-actions" aria-hidden="true"></div>
  </header>

  <div class="actions-filter-row">
    <div
      class="actions-filter"
      role="radiogroup"
      aria-label="Action item status"
    >
      {#each filterOptions as opt (opt.v)}
        <label
          class="actions-filter-opt"
          class:active={status === opt.v}
        >
          <input
            type="radio"
            name="actions-status"
            value={opt.v}
            checked={status === opt.v}
            onchange={() => setFilter(opt.v)}
          />
          <span class="actions-filter-label">{opt.label}</span>
          {#if totals}
            <span class="actions-filter-count" aria-hidden="true">
              {totals[opt.v]}
            </span>
          {/if}
        </label>
      {/each}
    </div>
  </div>

  {#if transientError}
    <!-- ui-phase-4 §G: 3s inline note at top of list on PATCH
         failure. Not a toast system — just a fixed inline element
         the page wrapper auto-clears via setTimeout. `role="alert"`
         so AT announces the failure. -->
    <p
      class="actions-flash"
      role="alert"
      transition:fade={{ duration: 180 }}
    >
      {transientError}
    </p>
  {/if}

  <p class="actions-sr-count" aria-live="polite">{countLabel}</p>

  {#if loading}
    <p class="actions-state">Loading…</p>
  {:else if error}
    <p class="actions-state actions-state-err" role="alert">
      Couldn't load your action items.
      <button
        type="button"
        class="actions-retry"
        onclick={handleRetry}
      >
        Retry
      </button>
    </p>
  {:else if items.length === 0}
    <div class="actions-empty">
      <p class="actions-empty-title">{emptyTitle}</p>
      <p class="actions-empty-sub">{emptySub}</p>
    </div>
  {:else}
    <ul class="actions-list">
      {#each items as item, i (item.id)}
        <ActionItem
          {item}
          users={orgMembers}
          callId={item.call_id}
          index={i}
          totalInList={items.length}
          variant="actions-page"
          callContext={{
            id: item.call_id,
            title: item.call_title,
            recordedAt: item.call_recorded_at,
          }}
          {colorFor}
          {canEdit}
          editingDescription={activeRowEdit.kind === "description" &&
            activeRowEdit.itemId === item.id}
          editingOwner={activeRowEdit.kind === "owner" &&
            activeRowEdit.itemId === item.id}
          saving={patchingItemIds.has(item.id)}
          editError={actionItemErrors[item.id] ?? ""}
          onDescriptionEditRequest={(p) =>
            onDescriptionEditRequest?.({ item: p.item })}
          onOwnerEditRequest={(p) => onOwnerEditRequest?.({ item: p.item })}
          onDescriptionSave={onDescriptionSaveForward}
          onOwnerSave={onOwnerSaveForward}
          onDescriptionCancel={(p) => onDescriptionCancel?.({ item: p.item })}
          onOwnerCancel={(p) => onOwnerCancel?.({ item: p.item })}
          onEditErrorClear={(p) => onEditErrorClear?.({ item: p.item })}
          ontoggle={(payload) =>
            !togglingIds.has(item.id) && handleRowToggle(payload)}
          onchipaction={onactionitemchipaction}
          activeChipOccurrenceIndex={activeChipItemId === item.id
            ? activeChipOccurrenceIndex
            : null}
        />
      {/each}
    </ul>
    {#if nextCursor}
      <div class="actions-more">
        <button
          type="button"
          class="actions-more-btn"
          class:actions-more-retry={loadMoreError !== null}
          disabled={loadingMore}
          onclick={handleLoadMore}
        >
          {#if loadingMore}
            Loading…
          {:else if loadMoreError}
            Retry
          {:else}
            Load more
          {/if}
        </button>
      </div>
    {/if}
  {/if}
</main>

<style>
  /* All styles are component-scoped — Phase 4's acceptance bars
     `portal/src/app.css` and `agent/src/app.css` edits. Mirror-pair
     invariant kept intact. */

  /* #131 — match /calls .page shape: horizontally centred, 900px
     max-width, 2rem padding. `.actions-page` namespace preserves
     the mirror-pair invariant (app.css untouched) and avoids
     accidentally coupling to any future app-wide `.page` rule. */
  .actions-page {
    max-width: 900px;
    margin: 0 auto;
    padding: 2rem;
    position: relative;
    z-index: 2;
  }

  /* #131 — flex head: title + subhead left, reserved actions lane
     right. Mirrors /calls .head verbatim so the three list pages
     (/calls, /actions, /admin) share one visual rhythm. */
  .actions-head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 1.5rem;
    margin-bottom: 1.2rem;
  }
  .actions-head-main {
    min-width: 0;
  }
  .actions-head h1 {
    margin: 0 0 0.2rem;
    font-weight: 600;
    color: var(--bone-0);
  }
  .actions-subhead {
    margin: 0;
    font-size: 0.82rem;
    color: var(--bone-3);
  }
  .actions-head-actions {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    /* Empty in v0.4.4 — reserves layout so the header doesn't
       reshape when future filters/toggles land. Remove
       aria-hidden from the markup when content arrives. */
  }

  /* #131 — filter-row wrapper mirrors /calls .filter-bar rhythm.
     One child in v0.4.4; future filters (e.g. pending review)
     slot in here without restructuring. */
  .actions-filter-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem 0.7rem;
    margin-bottom: 0;
  }

  /* Segmented filter — visual shape mirrors `admin/+page.svelte`'s
     `.segmented` pattern. Namespaced under `.actions-filter` so the
     two surfaces don't cross-couple. */
  .actions-filter {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.2rem;
    padding: 0.2rem;
    margin-bottom: 1.6rem;
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    border-radius: 8px;
  }
  .actions-filter-opt {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.35rem 0.7rem;
    border-radius: 6px;
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--bone-2);
    cursor: pointer;
    transition: all 0.15s;
  }
  .actions-filter-opt input {
    /* Native radio kept in the DOM for AT and keyboard; visually
       hidden behind the label swatch. */
    position: absolute;
    opacity: 0;
    inset: 0;
    margin: 0;
    cursor: pointer;
  }
  .actions-filter-opt:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
  .actions-filter-opt.active {
    color: var(--bone-0);
    background: var(--ink-2);
    box-shadow: inset 0 0 0 1px var(--hairline-hi);
  }
  .actions-filter-opt input:focus-visible + .actions-filter-label {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 3px;
  }
  @media (prefers-reduced-motion: reduce) {
    .actions-filter-opt {
      transition: none;
    }
  }
  .actions-filter-count {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-3);
  }
  .actions-filter-opt.active .actions-filter-count {
    color: var(--accent);
  }

  /* Transient check-off error — fixed inline at top of list. Auto-
     cleared by the page wrapper after 3s. `--live-soft` background
     + `--live` text matches the ai-edit-err / ai-confirm-err pair
     so error surfaces read consistently across the app. */
  .actions-flash {
    margin: 0 0 0.8rem;
    padding: 0.5rem 0.75rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.85rem;
  }

  /* aria-live announcement. Visually hidden so the count announces
     on filter change without visible chrome. */
  .actions-sr-count {
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

  .actions-state {
    margin: 1rem 0;
    color: var(--bone-3);
    font-size: 0.88rem;
  }
  .actions-state-err {
    color: var(--live);
  }
  .actions-retry {
    margin-left: 0.5rem;
    padding: 0.22rem 0.6rem;
    border-radius: 6px;
    border: 1px solid var(--live);
    background: var(--live-soft);
    color: var(--live);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition: all 150ms linear;
  }
  .actions-retry:hover {
    background: var(--live);
    color: var(--ink-0);
  }

  /* #131 — empty state matches /calls .empty exactly (hairline
     token, radius-lg, title weight/color). Gives the three list
     pages a single empty-state visual. */
  .actions-empty {
    border: 1px dashed var(--hairline);
    border-radius: var(--radius-lg);
    padding: 3rem 2rem;
    text-align: center;
    background: var(--ink-1);
    margin-top: 0.5rem;
  }
  .actions-empty-title {
    margin: 0 0 0.35rem;
    font-size: 1.1rem;
    font-weight: 600;
    color: var(--bone-0);
  }
  .actions-empty-sub {
    margin: 0;
    font-size: 0.88rem;
    color: var(--bone-3);
  }

  /* #131 — drop the top-border: .ai-row already carries a
     bottom-border, so a top-border here doubles up when the list
     starts immediately after the filter row. */
  .actions-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }

  .actions-more {
    display: flex;
    justify-content: center;
    margin-top: 1.2rem;
  }
  .actions-more-btn {
    padding: 0.48rem 1.1rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 150ms linear;
  }
  .actions-more-btn:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .actions-more-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .actions-more-retry {
    border-color: var(--live);
    background: var(--live-soft);
    color: var(--live);
  }
  .actions-more-retry:hover:not(:disabled) {
    background: var(--live);
    color: var(--ink-0);
    border-color: var(--live);
  }
  @media (prefers-reduced-motion: reduce) {
    .actions-retry,
    .actions-more-btn {
      transition: none;
    }
  }

  /* Narrow-viewport wrap — segmented filter gets `flex-wrap: wrap`
     for free via its root declaration. Explicit media rule kept
     here for the wrap-point documentation (ui-phase-4 §Responsive
     520px). */
  @media (max-width: 520px) {
    .actions-page {
      padding: 1.25rem 1rem 2rem;
    }
  }
</style>
