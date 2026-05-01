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

  import { onMount, onDestroy } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { page } from "$app/state";
  import { replaceState, afterNavigate } from "$app/navigation";
  import { registerShortcuts } from "@aftercalls/shared/shortcuts";
  import ActionsList, {
    type ActionsActiveRowEdit,
    type ActionsDescriptionSave,
    type ActionsOwnerSave,
    type ActionsDueSave,
  } from "@aftercalls/shared/ui/ActionsList.svelte";
  import ActionItem, { type ActionItemUser } from "@aftercalls/shared/ui/ActionItem.svelte";
  import {
    rewriteChipOccurrence,
    firstLastInitial,
  } from "@aftercalls/shared/ui/SummaryText.svelte";
  import ChipMenu from "@aftercalls/shared/ui/ChipMenu.svelte";
  import type { SpeakerPick } from "@aftercalls/shared/ui/SpeakerRenamePicker.svelte";
  import { isPortalError } from "$lib/portalError";
  // #254 — action-item save / check-off failures route through the
  // shared toast store. Replaces the inline `transientError` flash
  // and the per-row `actionItemErrors` Record.
  import { toast } from "@aftercalls/shared/stores/toast.svelte";
  // #107 — local save-queue. Same shape as the portal sibling but the
  // queued ops replay through Tauri `invoke` rather than fetch.
  import {
    saveQueue,
    isTransientNetworkError,
  } from "$lib/stores/saveQueue.svelte";
  import type {
    ActionsDueFilter,
    ActionsStatusFilter,
    MeActionItem,
    MeActionItemsResponse,
    OrgMember,
  } from "@aftercalls/shared/types";

  const PAGE_SIZE = 50;

  // #390 — assignee scope (Mine / All). In-memory filter on the
  // already-fetched items; no backend changes. The backend endpoint is
  // `me`-scoped so "All" shows every item (unfiltered); "Mine" narrows
  // to items where assignee_user_id === current user's id. Persisted
  // in localStorage so it survives navigation.
  const ASSIGNEE_FILTER_KEY = "aftercalls.actions.assigneeFilter";
  type AssigneeFilter = "mine" | "all";
  let assigneeFilter = $state<AssigneeFilter>("mine");
  // The current user's id — resolved on mount from current_user to
  // drive the "Mine" filter. Null until resolved; "Mine" renders all
  // items as a safe fallback when the id isn't known yet.
  let myUserId = $state<string | null>(null);

  // #390 — group-by-call toggle. When on, items are rendered in
  // per-call sections instead of a flat list. Persisted in localStorage.
  const GROUP_BY_CALL_KEY = "aftercalls.actions.groupByCall";
  let groupByCall = $state(false);

  let status = $state<ActionsStatusFilter>("open");
  // #173 — Due filter, persisted in `?due=...`. Defaults to "week"
  // (#390 changes the default from "all" to "Due soon" = this week).
  let dueFilter = $state<ActionsDueFilter>("week");
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
    if (typeof window === "undefined") return "week";
    const raw = new URL(window.location.href).searchParams.get("due");
    if (
      raw === "overdue" ||
      raw === "today" ||
      raw === "week" ||
      raw === "none" ||
      raw === "all"
    ) {
      return raw;
    }
    // #390 — default is "week" (Due soon) when no URL param is set.
    return "week";
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
    // #390 — "week" is the new default; elide the param so URLs stay
    // clean on the common path. Explicit "all" gets a param so back/
    // forward navigation can distinguish "week default" from "all".
    if (next === "week") {
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

  // #390 — load current user id for the "Mine" assignee filter.
  // LoginResult exposes `user_id` (not `id`).
  async function loadMyUserId() {
    try {
      const me = await invoke<{ user_id: string; email: string } | null>(
        "current_user",
      );
      if (me) myUserId = me.user_id;
    } catch {
      // Non-fatal — "Mine" falls back to showing all items.
    }
  }

  onMount(() => {
    status = readStatusFromUrl();
    dueFilter = readDueFromUrl();
    // #390 — restore assignee filter + group-by-call from localStorage.
    try {
      const saved = localStorage.getItem(ASSIGNEE_FILTER_KEY);
      if (saved === "mine" || saved === "all") assigneeFilter = saved;
    } catch {}
    try {
      const saved = localStorage.getItem(GROUP_BY_CALL_KEY);
      if (saved === "true") groupByCall = true;
    } catch {}
    void loadMyUserId();
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

  // #390 — in-memory assignee filter handler. No backend roundtrip —
  // filters the already-loaded items array on the client.
  function onAssigneeFilterChange(next: AssigneeFilter) {
    if (next === assigneeFilter) return;
    assigneeFilter = next;
    try { localStorage.setItem(ASSIGNEE_FILTER_KEY, next); } catch {}
  }

  // #390 — group-by-call toggle.
  function toggleGroupByCall() {
    groupByCall = !groupByCall;
    try { localStorage.setItem(GROUP_BY_CALL_KEY, String(groupByCall)); } catch {}
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
      // #107 — transient network: keep the optimistic flip + counts
      // and queue the patch. Hard rejects roll back as before.
      if (isTransientNetworkError(e)) {
        saveQueue.enqueue({
          kind: "patchActionItem",
          callId: ev.callId,
          itemId: ev.itemId,
          body: { status: ev.nextStatus },
          at: Date.now(),
        });
      } else {
        items = prevItems;
        totalOpen = prevTotalOpen;
        totalDone = prevTotalDone;
        toast.error("Couldn't save. Try again.");
        void e;
      }
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

  // #390 — in-memory assignee filter. "Mine" narrows to items where
  // assignee_user_id matches the current user's id. Falls back to
  // showing all items if myUserId is not yet resolved (avoids a flash
  // of empty state on first load). "All" passes items through unchanged.
  const visibleItems = $derived.by(() => {
    if (assigneeFilter === "all" || !myUserId) return items;
    return items.filter(
      (it) => it.assignee_user_id === myUserId,
    );
  });

  // #390 — group items by parent call. Each entry is a call header +
  // its action items, sorted by the call's recorded_at desc so the
  // most recent call is first.
  type GroupedActionSection = {
    callId: string;
    callTitle: string | null;
    callRecordedAt: string;
    items: MeActionItem[];
  };
  const callGroups = $derived.by((): GroupedActionSection[] => {
    const map = new Map<string, GroupedActionSection>();
    for (const it of visibleItems) {
      let g = map.get(it.call_id);
      if (!g) {
        g = {
          callId: it.call_id,
          callTitle: it.call_title,
          callRecordedAt: it.call_recorded_at,
          items: [],
        };
        map.set(it.call_id, g);
      }
      g.items.push(it);
    }
    return [...map.values()].sort(
      (a, b) =>
        new Date(b.callRecordedAt).getTime() -
        new Date(a.callRecordedAt).getTime(),
    );
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
      // #107 — queue + optimistic apply on transient network failure.
      if (isTransientNetworkError(e)) {
        saveQueue.enqueue({
          kind: "patchActionItem",
          callId: payload.callId,
          itemId: payload.itemId,
          body: { description: payload.description },
          at: Date.now(),
        });
        items = items.map((it) =>
          it.id === payload.itemId
            ? { ...it, description: payload.description }
            : it,
        );
        if (
          activeRowEdit.kind === "description" &&
          activeRowEdit.itemId === payload.itemId
        ) {
          activeRowEdit = { kind: "none" };
        }
      } else {
        // #124: bad_request from the backend means the assignee isn't
        // in the org. Anything else is "save failed" with a generic
        // try-again message.
        const msg =
          isPortalError(e) && e.kind === "bad_request"
            ? "That teammate isn't in your workspace. Pick someone from your team."
            : "Save failed. Check your connection and try again.";
        toast.error(msg);
      }
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
      // #107 — queue + optimistic apply on transient network failure.
      if (isTransientNetworkError(e)) {
        saveQueue.enqueue({
          kind: "patchActionItem",
          callId: payload.callId,
          itemId: payload.itemId,
          body: { assignee_user_id: payload.assigneeUserId },
          at: Date.now(),
        });
        items = items.map((it) =>
          it.id === payload.itemId
            ? { ...it, assignee_user_id: payload.assigneeUserId }
            : it,
        );
        if (
          activeRowEdit.kind === "owner" &&
          activeRowEdit.itemId === payload.itemId
        ) {
          activeRowEdit = { kind: "none" };
        }
      } else {
        // #124: structured-error matching — see onDescriptionSave above.
        const msg =
          isPortalError(e) && e.kind === "bad_request"
            ? "That teammate isn't in your workspace. Pick someone from your team."
            : "Save failed. Check your connection and try again.";
        toast.error(msg);
      }
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
      // #107 — queue + optimistic apply on transient network failure.
      if (isTransientNetworkError(e)) {
        const body =
          payload.kind === "dated"
            ? { due_kind: "dated" as const, due_at: payload.dueAt }
            : payload.kind === "asap"
              ? { due_kind: "asap" as const, due_at: null }
              : { due_kind: "none" as const, due_at: null };
        saveQueue.enqueue({
          kind: "patchActionItem",
          callId: payload.callId,
          itemId: payload.itemId,
          body,
          at: Date.now(),
        });
        items = items.map((it) =>
          it.id === payload.itemId
            ? { ...it, due_kind: body.due_kind, due_at: body.due_at }
            : it,
        );
        if (
          activeRowEdit.kind === "due" &&
          activeRowEdit.itemId === payload.itemId
        ) {
          activeRowEdit = { kind: "none" };
        }
      } else {
        // #254 — already on toast (today's #173 hotfix). No double-up.
        toast.error("Save failed. Check your connection and try again.");
        void e;
      }
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

  // ── #282 · Keyboard shortcuts on /actions ─────────────────────────
  //
  // Mirrors the portal /actions wiring (j/k highlight, Space toggle,
  // e edit description). Same shape so a future shared wrapper could
  // lift this into a hook.
  let highlightedItemId = $state<string | null>(null);

  $effect(() => {
    if (visibleItems.length === 0) {
      highlightedItemId = null;
      return;
    }
    if (
      !highlightedItemId ||
      !visibleItems.some((it) => it.id === highlightedItemId)
    ) {
      highlightedItemId = visibleItems[0].id;
    }
  });

  function moveHighlight(delta: 1 | -1) {
    if (visibleItems.length === 0) return;
    const cur = highlightedItemId
      ? visibleItems.findIndex((it) => it.id === highlightedItemId)
      : -1;
    const next = Math.max(
      0,
      Math.min(visibleItems.length - 1, (cur < 0 ? 0 : cur) + delta),
    );
    highlightedItemId = visibleItems[next].id;
    requestAnimationFrame(() => {
      const el = document.querySelector<HTMLElement>(
        '[data-shortcut-row="active"]',
      );
      el?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    });
  }

  function toggleHighlighted() {
    const it = visibleItems.find((x) => x.id === highlightedItemId);
    if (!it) return;
    const nextStatus: "open" | "done" =
      it.status === "done" ? "open" : "done";
    void onToggle({ itemId: it.id, callId: it.call_id, nextStatus });
  }

  function editHighlighted() {
    const it = visibleItems.find((x) => x.id === highlightedItemId);
    if (!it) return;
    onDescriptionEditRequest({ item: { id: it.id, call_id: it.call_id } });
  }

  // #608 — Phase-3 row-level delete on /actions. Mirror of the
  // call-detail page's pattern (calls/[id]/+page.svelte:1155). Without
  // this wiring the trash button on every row was a silent no-op.
  let confirmingDeleteId = $state<string | null>(null);
  let deletingId = $state<string | null>(null);

  function onActionItemDeleteRequest(payload: {
    item: { id: string; call_id: string };
  }) {
    if (deletingId) return;
    confirmingDeleteId = payload.item.id;
  }
  function onActionItemDeleteCancel(payload: {
    item: { id: string; call_id: string };
  }) {
    if (deletingId === payload.item.id) return;
    if (confirmingDeleteId === payload.item.id) confirmingDeleteId = null;
  }
  async function onActionItemDeleteConfirm(payload: {
    item: { id: string; call_id: string };
  }) {
    if (deletingId) return;
    deletingId = payload.item.id;
    const prevItems = items;
    items = items.filter((it) => it.id !== payload.item.id);
    try {
      await invoke("delete_action_item", {
        callId: payload.item.call_id,
        itemId: payload.item.id,
      });
      confirmingDeleteId = null;
    } catch (e: any) {
      items = prevItems;
      toast.error("Delete failed. Try again.");
      console.warn("action item delete failed", e);
    } finally {
      deletingId = null;
    }
  }

  let teardownActionShortcuts: (() => void) | null = null;
  onMount(() => {
    teardownActionShortcuts = registerShortcuts(
      "actions",
      "Action items",
      {
        j: () => moveHighlight(1),
        k: () => moveHighlight(-1),
        space: () => toggleHighlighted(),
        e: () => editHighlighted(),
      },
      [
        { keys: "j k", label: "Next / previous action item" },
        { keys: "space", label: "Toggle done on the highlighted item" },
        { keys: "e", label: "Edit the highlighted item's description" },
      ],
    );
  });
  onDestroy(() => {
    teardownActionShortcuts?.();
    teardownActionShortcuts = null;
  });
</script>

<svelte:head>
  <title>Action items · aftercalls</title>
</svelte:head>

<!-- #390 — Mine / All assignee filter + Group-by-call toggle.
     Rendered above the ActionsList flat view; also shown in the grouped
     view header. These are agent-specific controls (portal handles scope
     backend-side via a separate endpoint); kept outside ActionsList so
     the mirror-pair invariant is preserved. -->
<div class="actions-extra-bar">
  <div class="assignee-toggle" role="group" aria-label="Assignee scope">
    <button
      type="button"
      class="scope-opt"
      class:active={assigneeFilter === "mine"}
      onclick={() => onAssigneeFilterChange("mine")}
    >Mine</button>
    <button
      type="button"
      class="scope-opt"
      class:active={assigneeFilter === "all"}
      onclick={() => onAssigneeFilterChange("all")}
    >All</button>
  </div>
  <button
    type="button"
    class="group-toggle"
    class:active={groupByCall}
    onclick={toggleGroupByCall}
    title={groupByCall ? "Show flat list" : "Group by call"}
    aria-pressed={groupByCall}
  >
    {groupByCall ? "Grouped" : "Flat"}
  </button>
</div>

{#if groupByCall}
  <!-- #390 — Per-call grouped view. Replaces the flat ActionsList so
       we can insert call-header rows without touching the mirror-pair
       component. Inherits all edit/toggle/save handlers from the page. -->
  <main class="actions-grouped-page">
    <header class="actions-head">
      <div class="actions-head-main">
        <h1>Action items</h1>
        <p class="actions-subhead">
          {assigneeFilter === "mine"
            ? "Your items across all calls, grouped by call."
            : "All items across all calls, grouped by call."}
        </p>
      </div>
    </header>

    {#if loading}
      <p class="actions-state">Loading…</p>
    {:else if error}
      <p class="actions-state actions-state-err" role="alert">
        Couldn't load your action items.
        <button type="button" class="actions-retry" onclick={onRetry}>Retry</button>
      </p>
    {:else if callGroups.length === 0}
      <div class="actions-empty">
        <p class="actions-empty-title">All caught up.</p>
        <p class="actions-empty-sub">No items match the current filters.</p>
      </div>
    {:else}
      {#each callGroups as group (group.callId)}
        <section class="call-group">
          <a
            href="/calls/{group.callId}"
            class="call-group-head"
            aria-label="Go to call: {group.callTitle ?? '(untitled)'}"
          >
            <span class="call-group-title">{group.callTitle ?? "(untitled)"}</span>
            <span class="call-group-meta">
              {new Date(group.callRecordedAt).toLocaleDateString(undefined, {
                month: "short",
                day: "numeric",
                year: "numeric",
              })}
              · {group.items.length} item{group.items.length === 1 ? "" : "s"}
            </span>
          </a>
          <ul class="actions-list call-group-list">
            {#each group.items as item, i (item.id)}
              <ActionItem
                {item}
                users={orgMembers}
                callId={item.call_id}
                index={i}
                totalInList={group.items.length}
                variant="actions-page"
                callContext={{
                  id: item.call_id,
                  title: item.call_title,
                  recordedAt: item.call_recorded_at,
                }}
                canEdit={true}
                editingDescription={activeRowEdit.kind === "description" &&
                  activeRowEdit.itemId === item.id}
                editingOwner={activeRowEdit.kind === "owner" &&
                  activeRowEdit.itemId === item.id}
                editingDue={activeRowEdit.kind === "due" &&
                  activeRowEdit.itemId === item.id}
                saving={patchingItemIds.has(item.id)}
                onDescriptionEditRequest={(p) =>
                  onDescriptionEditRequest({ item: p.item })}
                onOwnerEditRequest={(p) =>
                  onOwnerEditRequest({ item: p.item })}
                onDueEditRequest={(p) =>
                  onDueEditRequest({ item: p.item })}
                onDescriptionSave={(p) =>
                  onDescriptionSave({
                    itemId: p.itemId,
                    callId: item.call_id,
                    description: p.description,
                  })}
                onOwnerSave={(p) =>
                  onOwnerSave({
                    itemId: p.itemId,
                    callId: item.call_id,
                    assigneeUserId: p.assigneeUserId,
                  })}
                onDueSave={(p) => {
                  if (p.kind === "dated") {
                    onDueSave({ itemId: p.itemId, callId: item.call_id, kind: "dated", dueAt: p.dueAt });
                  } else {
                    onDueSave({ itemId: p.itemId, callId: item.call_id, kind: p.kind });
                  }
                }}
                onDescriptionCancel={(p) =>
                  onDescriptionCancel({ item: p.item })}
                onOwnerCancel={(p) =>
                  onOwnerCancel({ item: p.item })}
                onDueCancel={(p) =>
                  onDueCancel({ item: p.item })}
                ontoggle={(payload) =>
                  !togglingIds.has(item.id) &&
                  onToggle({
                    itemId: payload.item.id,
                    callId: payload.item.call_id,
                    nextStatus: payload.nextStatus,
                  })}
                onchipaction={openActionItemChip}
                activeChipOccurrenceIndex={activeChip?.itemId === item.id
                  ? activeChip.occurrenceIndex
                  : null}
                highlighted={highlightedItemId === item.id}
              />
            {/each}
          </ul>
        </section>
      {/each}
    {/if}
  </main>
{:else}
  <ActionsList
    items={visibleItems}
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
    {highlightedItemId}
    {confirmingDeleteId}
    {deletingId}
    onDeleteRequest={onActionItemDeleteRequest}
    onDeleteConfirm={onActionItemDeleteConfirm}
    onDeleteCancel={onActionItemDeleteCancel}
  />
{/if}

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

<style>
  /* #390 — Mine/All assignee filter + Group-by-call toggle bar.
     Floats above the ActionsList so it doesn't interfere with the
     mirror-pair component's own filter row. Aligns flush with the
     ActionsList's max-width so the two rows form one visual unit. */
  .actions-extra-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    max-width: 900px;
    margin: 0 auto;
    padding: 0.75rem 2rem 0;
  }
  .assignee-toggle {
    display: inline-flex;
    gap: 0.2rem;
    padding: 0.2rem;
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    border-radius: 8px;
  }
  .scope-opt {
    padding: 0.3rem 0.7rem;
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--bone-2);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    cursor: pointer;
    transition: all 0.15s;
    font-family: inherit;
  }
  .scope-opt:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
  .scope-opt.active {
    color: var(--bone-0);
    background: var(--ink-2);
    box-shadow: inset 0 0 0 1px var(--hairline-hi);
  }
  .group-toggle {
    padding: 0.3rem 0.7rem;
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--bone-2);
    background: var(--ink-1);
    border: 1px solid var(--hairline);
    border-radius: 8px;
    cursor: pointer;
    transition: all 0.15s;
    font-family: inherit;
  }
  .group-toggle:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .group-toggle.active {
    color: var(--accent-hi);
    background: var(--accent-soft);
    border-color: var(--accent);
  }

  /* #390 — grouped view. Reuses the ActionsList page shape. */
  .actions-grouped-page {
    max-width: 900px;
    margin: 0 auto;
    padding: 1rem 2rem 2rem;
    position: relative;
    z-index: 2;
  }
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
  .actions-state {
    color: var(--bone-3);
  }
  .actions-state-err {
    color: var(--live);
  }
  .actions-retry {
    background: transparent;
    border: none;
    color: var(--accent);
    font-size: inherit;
    font-family: inherit;
    cursor: pointer;
    text-decoration: underline;
    padding: 0;
  }
  .actions-empty {
    padding: 2.5rem 2rem;
    text-align: center;
    border: 1px dashed var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
  }
  .actions-empty-title {
    font-size: 1rem;
    font-weight: 500;
    color: var(--bone-1);
    margin: 0 0 0.3rem;
  }
  .actions-empty-sub {
    color: var(--bone-3);
    font-size: 0.85rem;
    margin: 0;
  }
  .call-group {
    margin-bottom: 1.5rem;
  }
  .call-group-head {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding: 0.45rem 0;
    margin-bottom: 0.2rem;
    border-bottom: 1px solid var(--hairline);
    text-decoration: none;
    color: inherit;
  }
  .call-group-head:hover .call-group-title {
    color: var(--accent-hi);
  }
  .call-group-title {
    font-size: 0.9rem;
    font-weight: 600;
    color: var(--bone-0);
    transition: color 0.12s;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .call-group-meta {
    font-size: 0.75rem;
    color: var(--bone-3);
    font-family: var(--font-mono);
    white-space: nowrap;
    flex-shrink: 0;
  }
  .call-group-list {
    list-style: none;
    padding: 0;
    margin: 0;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
  }
  .actions-list {
    list-style: none;
    padding: 0;
    margin: 0;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
  }
</style>
