<script lang="ts">
  // v0.4.0 Phase 4 (#105) — agent /actions page.
  //
  // Thin wrapper: owns Tauri invokes + URL state; defers rendering
  // to the byte-identical `ActionsList.svelte` mirror pair. The
  // portal sibling at `portal/src/routes/actions/+page.svelte` runs
  // the same shape with fetch() — the mirror lives on the list
  // component, not the page wrapper (same justified divergence as
  // the call-detail pages).
  //
  // Filter URL persistence: `?status=open|done|all`. Default `open`;
  // malformed values fall back silently. URL writes use
  // `replaceState` (not `pushState`) so the back button stays a
  // coarse navigation primitive — per ui-phase-4 §D.
  //
  // Check-off wiring: optimistic local flip + `patch_action_item`
  // invoke. Filter-aware per ui-phase-4 §E (same semantics as
  // portal). On failure: rollback + `toast.error(...)` via the shared
  // store (#254 — dropped the inline `transientError` slot and the
  // per-row `actionItemErrors` Record in the same pass).

  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { page } from "$app/state";
  import { replaceState, afterNavigate } from "$app/navigation";
  import ActionsList, {
    type MeActionItem,
    type ActionsStatusFilter,
    type ActionsDueFilter,
    type MeActionItemsResponse,
    type ActionsActiveRowEdit,
    type ActionsDescriptionSave,
    type ActionsOwnerSave,
    type ActionsDueSave,
  } from "$lib/ActionsList.svelte";
  import type { ActionItemUser } from "$lib/ActionItem.svelte";
  import {
    rewriteChipOccurrence,
    firstLastInitial,
  } from "$lib/SummaryText.svelte";
  import ChipMenu from "$lib/ChipMenu.svelte";
  import type { SpeakerPick } from "$lib/SpeakerRenamePicker.svelte";
  import { isPortalError } from "$lib/portalError";
  // #254 — action-item save / check-off failures route through the
  // shared toast store. Replaces the inline `transientError` flash
  // and the per-row `actionItemErrors` Record.
  import { toast } from "$lib/stores/toast.svelte";

  // Roster shape the Tauri invoke returns. Superset of ActionItemUser
  // so we just map over the relevant fields.
  type OrgMember = {
    id: string;
    first_name?: string;
    last_name?: string;
    display_name: string;
    email: string;
  };

  const PAGE_SIZE = 50;

  let status = $state<ActionsStatusFilter>("open");
  // #173 — Due filter, persisted in `?due=...`. Defaults to "all".
  let dueFilter = $state<ActionsDueFilter>("all");
  let items = $state<MeActionItem[]>([]);
  let nextCursor = $state<string | null>(null);
  // #130b — three filter-stable counts sourced from the backend
  // envelope on every `loadFirst`. `loadMore` keeps them as-is
  // (paging doesn't change the unfiltered totals).
  let totalOpen = $state(0);
  let totalDone = $state(0);
  let totalAll = $state(0);
  let loading = $state(true);
  let loadingMore = $state(false);
  let error = $state<string | null>(null);
  let loadMoreError = $state<string | null>(null);
  let orgMembers = $state<ActionItemUser[]>([]);
  let togglingIds = $state<Set<string>>(new Set());

  function readStatusFromUrl(): ActionsStatusFilter {
    if (typeof window === "undefined") return "open";
    const raw = new URL(window.location.href).searchParams.get("status");
    if (raw === "done" || raw === "all") return raw;
    return "open";
  }

  function readDueFromUrl(): ActionsDueFilter {
    if (typeof window === "undefined") return "all";
    const raw = new URL(window.location.href).searchParams.get("due");
    if (
      raw === "overdue" ||
      raw === "today" ||
      raw === "week" ||
      raw === "none"
    ) {
      return raw;
    }
    return "all";
  }

  function writeStatusToUrl(next: ActionsStatusFilter) {
    if (typeof window === "undefined") return;
    const u = new URL(window.location.href);
    if (next === "open") {
      u.searchParams.delete("status");
    } else {
      u.searchParams.set("status", next);
    }
    // #130a — use SvelteKit's `replaceState` so the browser history
    // stays in sync with the filter pill without a push-navigation.
    // NOTE (#139B): `replaceState` from `$app/navigation` updates
    // `history` + `page.state` but does NOT update `page.url` — so
    // there is intentionally no URL-observing `$effect` on this
    // page; filter-click writes `status` + URL synchronously above,
    // and browser back/forward is handled by `afterNavigate` below.
    replaceState(u.pathname + u.search, page.state);
  }

  function writeDueToUrl(next: ActionsDueFilter) {
    if (typeof window === "undefined") return;
    const u = new URL(window.location.href);
    if (next === "all") {
      u.searchParams.delete("due");
    } else {
      u.searchParams.set("due", next);
    }
    replaceState(u.pathname + u.search, page.state);
  }

  async function loadFirst() {
    loading = true;
    error = null;
    loadMoreError = null;
    try {
      const resp = await invoke<MeActionItemsResponse>(
        "list_me_action_items",
        { status, cursor: null, limit: PAGE_SIZE, due: dueFilter },
      );
      items = resp.items;
      nextCursor = resp.next_cursor;
      totalOpen = resp.total_open;
      totalDone = resp.total_done;
      totalAll = resp.total_all;
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      loading = false;
    }
  }

  async function loadMore() {
    if (!nextCursor || loadingMore) return;
    loadingMore = true;
    loadMoreError = null;
    try {
      const resp = await invoke<MeActionItemsResponse>(
        "list_me_action_items",
        { status, cursor: nextCursor, limit: PAGE_SIZE, due: dueFilter },
      );
      items = [...items, ...resp.items];
      nextCursor = resp.next_cursor;
    } catch (e: any) {
      loadMoreError = String(e?.message ?? e);
    } finally {
      loadingMore = false;
    }
  }

  async function loadOrgMembers() {
    try {
      const members = await invoke<OrgMember[]>("org_members");
      orgMembers = members.map((m) => ({
        id: m.id,
        first_name: m.first_name ?? "",
        last_name: m.last_name ?? "",
        display_name: m.display_name,
        email: m.email,
      }));
    } catch {
      // Nice-to-have; the list still renders without a roster (no
      // avatar chip on assignee column). Don't escalate.
    }
  }

  onMount(() => {
    status = readStatusFromUrl();
    dueFilter = readDueFromUrl();
    void loadOrgMembers();
    void loadFirst();
  });

  async function onFilterChange(next: ActionsStatusFilter) {
    if (next === status) return;
    status = next;
    writeStatusToUrl(next);
    nextCursor = null;
    items = [];
    await loadFirst();
  }

  async function onDueFilterChange(next: ActionsDueFilter) {
    if (next === dueFilter) return;
    dueFilter = next;
    writeDueToUrl(next);
    nextCursor = null;
    items = [];
    await loadFirst();
  }

  async function onToggle(ev: {
    itemId: string;
    callId: string;
    nextStatus: "open" | "done";
  }) {
    if (togglingIds.has(ev.itemId)) return;
    togglingIds = new Set([...togglingIds, ev.itemId]);

    const prevItems = items;
    // #130b — snapshot the two counts that move on a status flip so
    // rollback can restore them together with `items`. `totalAll`
    // stays stable on a status flip (no row is added or removed from
    // the backlog).
    const prevTotalOpen = totalOpen;
    const prevTotalDone = totalDone;
    const patchedAt = new Date().toISOString();
    const nextItems = prevItems.map((it) => {
      if (it.id !== ev.itemId) return it;
      return {
        ...it,
        status: ev.nextStatus,
        completed_at: ev.nextStatus === "done" ? patchedAt : null,
      } satisfies MeActionItem;
    });
    const filteredItems =
      status === "all"
        ? nextItems
        : nextItems.filter((it) => it.status === status);
    items = filteredItems;
    // Optimistic badge update. Paired with the rollback below so a
    // failed invoke snaps the pills back to their pre-click values.
    if (ev.nextStatus === "done") {
      totalOpen = Math.max(0, totalOpen - 1);
      totalDone = totalDone + 1;
    } else {
      totalOpen = totalOpen + 1;
      totalDone = Math.max(0, totalDone - 1);
    }

    try {
      await invoke("patch_action_item", {
        callId: ev.callId,
        itemId: ev.itemId,
        body: { status: ev.nextStatus },
      });
    } catch (e: any) {
      items = prevItems;
      totalOpen = prevTotalOpen;
      totalDone = prevTotalDone;
      toast.error("Couldn't save. Try again.");
      void e;
    } finally {
      togglingIds = new Set(
        [...togglingIds].filter((id) => id !== ev.itemId),
      );
    }
  }

  // #130b — segmented-control counts. All three come straight from
  // the backend envelope (one FILTER-aggregate scan), so the pills
  // show distinct stable values on every filter and the counts
  // don't drift with the visible page slice.
  const totals = $derived({
    open: totalOpen,
    done: totalDone,
    all: totalAll,
  });

  // Back/forward navigation: SvelteKit's actual navigation flow
  // DOES update `page.url` (via `update_url` inside `_navigate`),
  // so `afterNavigate` fires with the fresh URL and we can resync
  // `status` on popstate. `replaceState` (used for filter-pill
  // clicks) does NOT update `page.url` — so we deliberately do NOT
  // watch `page.url` in a `$effect` there. The v0.4.3 `$effect`
  // trapped on `status` as a tracked read, re-fired on every
  // filter click, and clobbered `status` back with the stale URL
  // value (#139B). Filter-click syncs synchronously via
  // `onFilterChange`; no URL observer needed outside popstate.
  afterNavigate(({ type }) => {
    // `type: "enter"` fires once on initial hydration — no-op so
    // onMount owns the first load. Only real back/forward
    // navigation (popstate) should trigger a resync.
    if (type !== "popstate") return;
    const urlStatus = readStatusFromUrl();
    const urlDue = readDueFromUrl();
    const statusChanged = urlStatus !== status;
    const dueChanged = urlDue !== dueFilter;
    if (!statusChanged && !dueChanged) return;
    status = urlStatus;
    dueFilter = urlDue;
    nextCursor = null;
    items = [];
    void loadFirst();
  });

  function onRetry() {
    void loadFirst();
  }

  // ── Click-to-edit machinery (#126 / v0.4.2) ───────────────────────
  // Mirrors the portal /actions wrapper; invoke replaces fetch.
  let activeRowEdit = $state<ActionsActiveRowEdit>({ kind: "none" });
  let patchingItemIds = $state<Set<string>>(new Set());

  function markPatching(itemId: string, on: boolean) {
    if (on) {
      patchingItemIds = new Set([...patchingItemIds, itemId]);
    } else {
      patchingItemIds = new Set(
        [...patchingItemIds].filter((id) => id !== itemId),
      );
    }
  }

  function onDescriptionEditRequest(payload: {
    item: { id: string; call_id: string };
  }) {
    activeRowEdit = { kind: "description", itemId: payload.item.id };
  }
  function onOwnerEditRequest(payload: {
    item: { id: string; call_id: string };
  }) {
    activeRowEdit = { kind: "owner", itemId: payload.item.id };
  }

  async function onDescriptionSave(payload: ActionsDescriptionSave) {
    if (patchingItemIds.has(payload.itemId)) return;
    markPatching(payload.itemId, true);
    try {
      const updated = (await invoke("patch_action_item", {
        callId: payload.callId,
        itemId: payload.itemId,
        body: { description: payload.description },
      })) as MeActionItem;
      items = items.map((it) =>
        it.id === payload.itemId
          ? { ...it, description: updated.description }
          : it,
      );
      if (
        activeRowEdit.kind === "description" &&
        activeRowEdit.itemId === payload.itemId
      ) {
        activeRowEdit = { kind: "none" };
      }
    } catch (e: unknown) {
      // #124: bad_request from the backend means the assignee isn't
      // in the org. Anything else is "save failed" with a generic
      // try-again message.
      const msg =
        isPortalError(e) && e.kind === "bad_request"
          ? "That teammate isn't in your workspace. Pick someone from your team."
          : "Save failed. Check your connection and try again.";
      toast.error(msg);
    } finally {
      markPatching(payload.itemId, false);
    }
  }

  async function onOwnerSave(payload: ActionsOwnerSave) {
    if (patchingItemIds.has(payload.itemId)) return;
    markPatching(payload.itemId, true);
    try {
      const updated = (await invoke("patch_action_item", {
        callId: payload.callId,
        itemId: payload.itemId,
        body: { assignee_user_id: payload.assigneeUserId },
      })) as MeActionItem;
      items = items.map((it) =>
        it.id === payload.itemId
          ? { ...it, assignee_user_id: updated.assignee_user_id }
          : it,
      );
      if (
        activeRowEdit.kind === "owner" &&
        activeRowEdit.itemId === payload.itemId
      ) {
        activeRowEdit = { kind: "none" };
      }
    } catch (e: unknown) {
      // #124: structured-error matching — see onDescriptionSave above.
      const msg =
        isPortalError(e) && e.kind === "bad_request"
          ? "That teammate isn't in your workspace. Pick someone from your team."
          : "Save failed. Check your connection and try again.";
      toast.error(msg);
    } finally {
      markPatching(payload.itemId, false);
    }
  }

  function onDescriptionCancel(payload: {
    item: { id: string; call_id: string };
  }) {
    if (
      activeRowEdit.kind === "description" &&
      activeRowEdit.itemId === payload.item.id
    ) {
      activeRowEdit = { kind: "none" };
    }
  }
  function onOwnerCancel(payload: {
    item: { id: string; call_id: string };
  }) {
    if (
      activeRowEdit.kind === "owner" &&
      activeRowEdit.itemId === payload.item.id
    ) {
      activeRowEdit = { kind: "none" };
    }
  }

  // ── #173: due-date edit handlers ────────────────────────────────
  function onDueEditRequest(payload: {
    item: { id: string; call_id: string };
  }) {
    activeRowEdit = { kind: "due", itemId: payload.item.id };
  }
  function onDueCancel(payload: {
    item: { id: string; call_id: string };
  }) {
    if (
      activeRowEdit.kind === "due" &&
      activeRowEdit.itemId === payload.item.id
    ) {
      activeRowEdit = { kind: "none" };
    }
    void payload;
  }
  async function onDueSave(payload: ActionsDueSave) {
    if (patchingItemIds.has(payload.itemId)) return;
    markPatching(payload.itemId, true);
    try {
      const body =
        payload.kind === "dated"
          ? { due_kind: "dated" as const, due_at: payload.dueAt }
          : payload.kind === "asap"
            ? { due_kind: "asap" as const, due_at: null }
            : { due_kind: "none" as const, due_at: null };
      const updated = (await invoke("patch_action_item", {
        callId: payload.callId,
        itemId: payload.itemId,
        body,
      })) as MeActionItem;
      items = items.map((it) =>
        it.id === payload.itemId
          ? {
              ...it,
              due_kind: updated.due_kind,
              due_at: updated.due_at,
            }
          : it,
      );
      if (
        activeRowEdit.kind === "due" &&
        activeRowEdit.itemId === payload.itemId
      ) {
        activeRowEdit = { kind: "none" };
      }
    } catch (e: unknown) {
      // #254 — already on toast (today's #173 hotfix). No double-up.
      toast.error("Save failed. Check your connection and try again.");
      void e;
    } finally {
      markPatching(payload.itemId, false);
    }
  }

  // ── Chip-edit wiring (#147 / v0.4.7) ─────────────────────────────
  //
  // Agent twin of the portal handler — see portal/src/routes/actions
  // for the longer narrative. Divergence is confined to the PATCH
  // path (invoke vs fetch); shape + occurrence bookkeeping is
  // identical.
  let activeChip = $state<{
    itemId: string;
    anchor: HTMLElement;
    inner: string;
    occurrenceIndex: number;
    isExternal: boolean;
  } | null>(null);
  const memberRosterLoaded = $derived(orgMembers.length > 0);
  const memberRosterError = $state(false);

  function openActionItemChip(detail: {
    inner: string;
    occurrenceIndex: number;
    anchor: HTMLElement;
    itemId: string;
    isExternal: boolean;
  }) {
    activeChip = {
      itemId: detail.itemId,
      anchor: detail.anchor,
      inner: detail.inner,
      occurrenceIndex: detail.occurrenceIndex,
      isExternal: detail.isExternal,
    };
  }
  function closeChipMenu() {
    activeChip = null;
  }

  async function onChipMenuSelect(
    action: "rename" | "unlink",
    pick?: SpeakerPick,
  ) {
    if (!activeChip) return;
    const ac = activeChip;
    // #195 — external-mode unlink auto-populates the persistent
    // client-allowlist. Fire-and-forget via the Tauri command.
    const shouldRememberClient = ac.isExternal && action === "unlink";
    const rememberName = ac.inner;
    let replacement: string | undefined;
    if (action === "rename") {
      const user = pick?.user;
      if (!user) {
        action = "unlink";
      } else {
        const full = orgMembers.find((m) => m.id === user.id);
        replacement = full
          ? firstLastInitial({
              id: full.id,
              first_name: full.first_name,
              last_name: full.last_name,
              display_name: full.display_name,
            })
          : "";
        if (!replacement) {
          const parts = (user.display_name ?? "").trim().split(/\s+/);
          const first = parts[0] ?? "";
          const lastInitial = parts[1] ? parts[1][0] : "";
          replacement = lastInitial
            ? `${first} ${lastInitial.toUpperCase()}.`
            : first;
        }
      }
    }
    try {
      const item = items.find((it) => it.id === ac.itemId);
      if (!item) {
        closeChipMenu();
        return;
      }
      const current = item.description ?? "";
      const rewritten = rewriteChipOccurrence(
        current,
        ac.occurrenceIndex,
        action,
        replacement,
      );
      if (rewritten === current) {
        closeChipMenu();
        return;
      }
      const updated = (await invoke("patch_action_item", {
        callId: item.call_id,
        itemId: ac.itemId,
        body: { description: rewritten },
      })) as MeActionItem;
      items = items.map((it) =>
        it.id === ac.itemId
          ? { ...it, description: updated.description }
          : it,
      );
    } catch (e) {
      console.warn("chip action failed", e);
    } finally {
      closeChipMenu();
    }
    // #195 fire-and-forget allowlist add.
    if (shouldRememberClient && rememberName) {
      invoke("add_client_allowlist_entry", {
        name: rememberName,
        source: "unlink",
      }).catch((e) => console.debug("allowlist add failed", e));
    }
  }

  // ChipMenu wants OrgMemberLite[] (id + display_name + email); the
  // /actions roster already carries those fields via ActionItemUser,
  // so we just narrow the view rather than re-mapping.
  const chipRoster = $derived(
    orgMembers.map((m) => ({
      id: m.id,
      display_name: m.display_name,
      email: m.email,
    })),
  );
</script>

<svelte:head>
  <title>Action items · aftercalls</title>
</svelte:head>

<ActionsList
  {items}
  {status}
  {loading}
  {loadingMore}
  {error}
  {loadMoreError}
  {nextCursor}
  {totals}
  {orgMembers}
  {togglingIds}
  canEdit={true}
  {activeRowEdit}
  {patchingItemIds}
  due={dueFilter}
  onfilterchange={onFilterChange}
  onduefilterchange={onDueFilterChange}
  ontoggle={onToggle}
  onloadmore={loadMore}
  onretry={onRetry}
  onDescriptionEditRequest={onDescriptionEditRequest}
  onOwnerEditRequest={onOwnerEditRequest}
  onDueEditRequest={onDueEditRequest}
  onDescriptionSave={onDescriptionSave}
  onOwnerSave={onOwnerSave}
  onDueSave={onDueSave}
  onDescriptionCancel={onDescriptionCancel}
  onOwnerCancel={onOwnerCancel}
  onDueCancel={onDueCancel}
  onactionitemchipaction={openActionItemChip}
  activeChipItemId={activeChip?.itemId ?? null}
  activeChipOccurrenceIndex={activeChip?.occurrenceIndex ?? null}
/>

{#if activeChip}
  <ChipMenu
    anchor={activeChip.anchor}
    name={activeChip.inner}
    users={chipRoster}
    rosterLoaded={memberRosterLoaded}
    rosterError={memberRosterError}
    isExternal={activeChip.isExternal}
    onselect={onChipMenuSelect}
    onclose={closeChipMenu}
  />
{/if}
