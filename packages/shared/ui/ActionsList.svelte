<script lang="ts" module>
  // Shared package component consumed by both portal and agent.
  // Shipped as part of v0.4.0 Phase 4 (#105) — see ui-phase-4 §J.
  //
  // Pure presentational shell: the page wrapper owns all network
  // calls, filter URL persistence, and optimistic-update logic.
  // This component just renders what it's given and emits events
  // on filter / toggle / load-more / retry. That split keeps the
  // package boundary viable — no invoke() on agent vs fetch() on
  // portal living inside the shared file.

  import type {
    ActionItemUser,
    ActionItemDueKind,
  } from "./ActionItem.svelte";

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
    // #173 — due-date metadata; mirrors the per-call ActionItem
    // shape so the row component renders identically across surfaces.
    due_kind: ActionItemDueKind;
    due_at: string | null;
  };

  // Segmented filter values. Whitelisted at the page wrapper before
  // this prop lands, so malformed URL params never reach the
  // component.
  export type ActionsStatusFilter = "open" | "done" | "all";

  // #173 — Due filter buckets for the /actions page. `all` is the
  // default (no narrow); the others restrict + flip the sort.
  export type ActionsDueFilter =
    | "all"
    | "overdue"
    | "today"
    | "week"
    | "none";

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
  // #173 — `"due"` extends the discriminated union so the row-level
  // due-date editor is mutually exclusive with description / owner.
  export type ActionsActiveRowEdit =
    | { kind: "none" }
    | { kind: "description"; itemId: string }
    | { kind: "owner"; itemId: string }
    | { kind: "due"; itemId: string };

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
  // #173 — due-date save event flowing through ActionsList. Carries
  // call_id alongside the kind/dueAt so the page wrapper can route
  // the PATCH to the right call-scoped endpoint.
  export type ActionsDueSave =
    | { itemId: string; callId: string; kind: "none" }
    | { itemId: string; callId: string; kind: "asap" }
    | { itemId: string; callId: string; kind: "dated"; dueAt: string };
</script>

<script lang="ts">
  import ActionItem from "./ActionItem.svelte";

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
    // Optional per-row in-flight flag keyed on item id. When true
    // the ActionItem's checkbox shows a disabled state to prevent
    // a double-PATCH race.
    togglingIds?: Set<string>;
    // Color lookup reused from the call-detail page (maps a speaker
    // display name to the shared palette). Absent → default accent.
    colorFor?: (name: string) => string;

    // Events — page wrapper owns all side effects.
    onfilterchange?: (next: ActionsStatusFilter) => void;
    // #173 — Due filter parallels the status filter wiring. When set
    // to anything other than "all", the backend flips its sort to
    // `due_at ASC NULLS LAST` so dated rows surface first.
    due?: ActionsDueFilter;
    onduefilterchange?: (next: ActionsDueFilter) => void;
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

    // #173 — due-date edit lifecycle, parallel to description / owner.
    onDueEditRequest?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onDueSave?: (payload: ActionsDueSave) => void;
    onDueCancel?: (payload: {
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
      isExternal: boolean;
    }) => void;
    // Page-level active-chip indicator so the matching
    // `.name-chip-active` outline lands on the right row.
    activeChipItemId?: string | null;
    activeChipOccurrenceIndex?: number | null;

    // #282 — keyboard-driven highlight (j/k navigation on the
    // /actions page wrapper). The page owns the highlight state;
    // ActionsList just forwards it down to the matching ActionItem.
    highlightedItemId?: string | null;

    // #380 — team-scope toggle. When `canSeeAll` is true (admin/owner)
    // the reserved right-side lane renders "Mine / All team" buttons.
    // The page wrapper owns the actual fetch; ActionsList just renders
    // the control and fires `onscopechange` on click.
    scope?: "mine" | "all";
    canSeeAll?: boolean;
    onscopechange?: (next: "mine" | "all") => void;

    // #608 — Phase-3 row-level delete forwarding. The /actions page
    // mounts ActionsList; without these props the trash button on
    // any row would be a silent no-op (no confirm, no API call).
    // Same shape as the call-detail page: the parent owns the
    // confirmingDeleteId / deletingId state and forwards through.
    confirmingDeleteId?: string | null;
    deletingId?: string | null;
    onDeleteRequest?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onDeleteConfirm?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
    onDeleteCancel?: (payload: {
      item: { id: string; call_id: string };
    }) => void;
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
    togglingIds = new Set<string>(),
    colorFor,
    onfilterchange,
    due = "all" as ActionsDueFilter,
    onduefilterchange,
    ontoggle,
    onloadmore,
    onretry,
    canEdit = false,
    activeRowEdit = { kind: "none" } as ActionsActiveRowEdit,
    patchingItemIds = new Set<string>(),
    onDescriptionEditRequest,
    onOwnerEditRequest,
    onDescriptionSave: onDescSaveProp,
    onOwnerSave: onOwnerSaveProp,
    onDescriptionCancel,
    onOwnerCancel,
    onDueEditRequest,
    onDueSave: onDueSaveProp,
    onDueCancel,
    onactionitemchipaction,
    activeChipItemId = null,
    activeChipOccurrenceIndex = null,
    highlightedItemId = null,
    scope = "mine" as "mine" | "all",
    canSeeAll = false,
    onscopechange,
    confirmingDeleteId = null,
    deletingId = null,
    onDeleteRequest,
    onDeleteConfirm,
    onDeleteCancel,
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
  // #173 — same callId-pin pattern for the due-date save event.
  function onDueSaveForward(
    payload:
      | { itemId: string; kind: "none" }
      | { itemId: string; kind: "asap" }
      | { itemId: string; kind: "dated"; dueAt: string },
  ) {
    const callId = findCallId(payload.itemId);
    if (!callId) return;
    if (payload.kind === "dated") {
      onDueSaveProp?.({
        itemId: payload.itemId,
        callId,
        kind: "dated",
        dueAt: payload.dueAt,
      });
    } else {
      onDueSaveProp?.({
        itemId: payload.itemId,
        callId,
        kind: payload.kind,
      });
    }
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

  // #173 — Due filter dropdown options. We keep this as a select
  // (not a segmented control) because there are five options and the
  // segmented pattern would dominate the filter row visually.
  const dueOptions: Array<{ v: ActionsDueFilter; label: string }> = [
    { v: "all", label: "All" },
    { v: "overdue", label: "Overdue" },
    { v: "today", label: "Due today" },
    { v: "week", label: "This week" },
    { v: "none", label: "No date" },
  ];

  function setFilter(next: ActionsStatusFilter) {
    if (next === status) return;
    onfilterchange?.(next);
  }

  function setDueFilter(next: ActionsDueFilter) {
    if (next === due) return;
    onduefilterchange?.(next);
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
    <!-- #380 — reserved right-side lane. Scope toggle lands here for
         admin/owner viewers; hidden (aria-hidden) for members. -->
    <div class="actions-head-actions" aria-hidden={!canSeeAll}>
      {#if canSeeAll}
        <div class="scope-toggle" role="group" aria-label="Scope">
          <button
            type="button"
            class="scope-opt"
            class:active={scope === "mine"}
            onclick={() => onscopechange?.("mine")}
          >
            Mine
          </button>
          <button
            type="button"
            class="scope-opt"
            class:active={scope === "all"}
            onclick={() => onscopechange?.("all")}
          >
            All team
          </button>
        </div>
      {/if}
    </div>
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
    <!-- #173: Due filter dropdown. Native <select> for the same
         reason the date-range filter on /calls uses one — five
         options is past the point where a segmented control reads
         cleanly, and the native picker carries keyboard + AT
         conventions for free. URL persistence via `?due=...` lives
         in the page wrapper. -->
    <label class="actions-due-label">
      <span class="actions-due-label-text">Due</span>
      <select
        class="actions-due-select"
        class:actions-due-select-active={due !== "all"}
        value={due}
        onchange={(e) =>
          setDueFilter((e.currentTarget as HTMLSelectElement).value as ActionsDueFilter)}
      >
        {#each dueOptions as opt (opt.v)}
          <option value={opt.v}>{opt.label}</option>
        {/each}
      </select>
    </label>
  </div>

  <!-- #254 — the inline `.actions-flash` retired. Check-off + edit
       failures now route through the page-level toast store, so the
       wrapper page emits `toast.error(...)` instead of priming this
       slot with a 3s setTimeout-cleared string. -->

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
          editingDue={activeRowEdit.kind === "due" &&
            activeRowEdit.itemId === item.id}
          saving={patchingItemIds.has(item.id)}
          onDescriptionEditRequest={(p) =>
            onDescriptionEditRequest?.({ item: p.item })}
          onOwnerEditRequest={(p) => onOwnerEditRequest?.({ item: p.item })}
          onDueEditRequest={(p) => onDueEditRequest?.({ item: p.item })}
          onDescriptionSave={onDescriptionSaveForward}
          onOwnerSave={onOwnerSaveForward}
          onDueSave={onDueSaveForward}
          onDescriptionCancel={(p) => onDescriptionCancel?.({ item: p.item })}
          onOwnerCancel={(p) => onOwnerCancel?.({ item: p.item })}
          onDueCancel={(p) => onDueCancel?.({ item: p.item })}
          ontoggle={(payload) =>
            !togglingIds.has(item.id) && handleRowToggle(payload)}
          onchipaction={onactionitemchipaction}
          activeChipOccurrenceIndex={activeChipItemId === item.id
            ? activeChipOccurrenceIndex
            : null}
          highlighted={highlightedItemId === item.id}
          confirmingDelete={confirmingDeleteId === item.id}
          deleting={deletingId === item.id}
          ondeleterequest={(p) =>
            onDeleteRequest?.({
              item: { id: p.item.id, call_id: item.call_id },
            })}
          ondeleteconfirm={(p) =>
            onDeleteConfirm?.({
              item: { id: p.item.id, call_id: item.call_id },
            })}
          ondeletecancel={(p) =>
            onDeleteCancel?.({
              item: { id: p.item.id, call_id: item.call_id },
            })}
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
     `portal/src/app.css` and `agent/src/app.css` edits. */

  /* #131 — match /calls .page shape: horizontally centred, 900px
     max-width, 2rem padding. `.actions-page` namespace keeps app.css
     untouched and avoids accidentally coupling to any future
     app-wide `.page` rule. */
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
  }

  /* #380 — scope toggle: "Mine / All team". Matches the /calls page
     .scope-toggle shape so both pages share one visual rhythm. */
  .scope-toggle {
    display: inline-flex;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    overflow: hidden;
  }
  .scope-opt {
    padding: 0.3rem 0.8rem;
    border: none;
    background: transparent;
    color: var(--bone-2);
    font-size: 0.8rem;
    cursor: pointer;
    transition:
      background 0.12s,
      color 0.12s;
  }
  .scope-opt + .scope-opt {
    border-left: 1px solid var(--hairline);
  }
  .scope-opt.active {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .scope-opt:hover:not(.active) {
    background: var(--ink-2);
    color: var(--bone-1);
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

  /* #254 — `.actions-flash` retired. The page wrapper now toasts on
     check-off failure via the shared `toast.error(...)` store
     instead of priming a 3s inline note here. */

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

  /* ── #255 part 5: mobile pass ─────────────────────────────────────
     640px primary breakpoint. Filter pills + due dropdown become
     touch-first (≥44px targets, ≥16px input font). The Due
     `<select>` drops onto its own line so neither control truncates
     when both labels lengthen. Empty-state padding shrinks but stays
     centered. */
  @media (max-width: 640px) {
    .actions-page {
      padding: 1.25rem 1rem 2rem;
    }
    .actions-head {
      gap: 0.75rem;
      margin-bottom: 1rem;
    }
    .actions-filter-row {
      gap: 0.6rem;
      align-items: stretch;
      margin-bottom: 0.4rem;
    }
    .actions-filter {
      flex: 1 1 100%;
      gap: 0.25rem;
      margin-bottom: 0;
    }
    .actions-filter-opt {
      /* ≥44px tap target. Distribute the three pills so they fill
         the row without squeezing labels off their baseline. */
      flex: 1 1 0;
      justify-content: center;
      min-height: 44px;
      padding: 0.55rem 0.7rem;
      font-size: 0.88rem;
    }
    .actions-due-label {
      flex: 1 1 100%;
      gap: 0.5rem;
      margin-bottom: 0;
    }
    .actions-due-select {
      /* iOS Safari zooms inputs whose font is <16px on focus —
         keep the dropdown at the platform baseline. ≥44px tall to
         match the filter pills. */
      flex: 1 1 auto;
      min-height: 44px;
      padding: 0.6rem 2rem 0.6rem 0.75rem;
      font-size: 16px;
      background-position: right 0.75rem center;
    }
    .actions-empty {
      padding: 2rem 1.25rem;
    }
  }

  /* ── #173: Due filter select ─────────────────────────────────── */

  .actions-due-label {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    margin-bottom: 1.6rem;
  }
  .actions-due-label-text {
    font-size: 0.78rem;
    font-weight: 500;
    color: var(--bone-3);
  }
  .actions-due-select {
    appearance: none;
    -webkit-appearance: none;
    padding: 0.4rem 1.8rem 0.4rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: var(--ink-1);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.82rem;
    font-weight: 500;
    cursor: pointer;
    /* Caret glyph baked into the background so the chrome reads
       consistently across platforms. Cream stroke on the inline
       SVG hard-coded since data: URIs can't read CSS vars. */
    background-image: url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='10' height='6' viewBox='0 0 10 6' fill='none' stroke='%23a3a39c' stroke-width='1.5' stroke-linecap='round'><path d='M1 1l4 4 4-4'/></svg>");
    background-repeat: no-repeat;
    background-position: right 0.6rem center;
    transition: border-color 150ms linear, color 150ms linear;
  }
  .actions-due-select:hover {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }
  .actions-due-select-active {
    border-color: var(--accent);
    color: var(--accent);
  }
  .actions-due-select:focus {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .actions-due-select:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .actions-due-select {
      transition: none;
    }
  }
</style>
