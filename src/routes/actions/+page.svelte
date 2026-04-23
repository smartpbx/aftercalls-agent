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
  // portal). On failure: rollback + 3s inline transient note.

  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { page } from "$app/state";
  import ActionsList, {
    type MeActionItem,
    type ActionsStatusFilter,
    type MeActionItemsResponse,
  } from "$lib/ActionsList.svelte";
  import type { ActionItemUser } from "$lib/ActionItem.svelte";

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
  let items = $state<MeActionItem[]>([]);
  let nextCursor = $state<string | null>(null);
  let totalOpen = $state(0);
  let loading = $state(true);
  let loadingMore = $state(false);
  let error = $state<string | null>(null);
  let loadMoreError = $state<string | null>(null);
  let transientError = $state<string | null>(null);
  let transientTimer: ReturnType<typeof setTimeout> | null = null;
  let orgMembers = $state<ActionItemUser[]>([]);
  let togglingIds = $state<Set<string>>(new Set());

  function readStatusFromUrl(): ActionsStatusFilter {
    if (typeof window === "undefined") return "open";
    const raw = new URL(window.location.href).searchParams.get("status");
    if (raw === "done" || raw === "all") return raw;
    return "open";
  }

  function writeStatusToUrl(next: ActionsStatusFilter) {
    if (typeof window === "undefined") return;
    const u = new URL(window.location.href);
    if (next === "open") {
      u.searchParams.delete("status");
    } else {
      u.searchParams.set("status", next);
    }
    window.history.replaceState(window.history.state, "", u.toString());
  }

  async function loadFirst() {
    loading = true;
    error = null;
    loadMoreError = null;
    try {
      const resp = await invoke<MeActionItemsResponse>(
        "list_me_action_items",
        { status, cursor: null, limit: PAGE_SIZE },
      );
      items = resp.items;
      nextCursor = resp.next_cursor;
      totalOpen = resp.total_open;
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
        { status, cursor: nextCursor, limit: PAGE_SIZE },
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

  function clearTransient() {
    if (transientTimer) {
      clearTimeout(transientTimer);
      transientTimer = null;
    }
  }

  function setTransient(msg: string) {
    clearTransient();
    transientError = msg;
    transientTimer = setTimeout(() => {
      transientError = null;
      transientTimer = null;
    }, 3000);
  }

  async function onToggle(ev: {
    itemId: string;
    callId: string;
    nextStatus: "open" | "done";
  }) {
    if (togglingIds.has(ev.itemId)) return;
    togglingIds = new Set([...togglingIds, ev.itemId]);

    const prevItems = items;
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

    try {
      await invoke("patch_action_item", {
        callId: ev.callId,
        itemId: ev.itemId,
        body: { status: ev.nextStatus },
      });
      if (ev.nextStatus === "done") {
        totalOpen = Math.max(0, totalOpen - 1);
      } else {
        totalOpen = totalOpen + 1;
      }
    } catch (e: any) {
      items = prevItems;
      setTransient("Couldn't save. Try again.");
    } finally {
      togglingIds = new Set(
        [...togglingIds].filter((id) => id !== ev.itemId),
      );
    }
  }

  const totals = $derived.by(() => {
    if (status === "open") {
      return {
        open: totalOpen,
        done: items.filter((it) => it.status === "done").length,
        all: totalOpen,
      };
    }
    if (status === "done") {
      return {
        open: totalOpen,
        done: items.length,
        all: totalOpen + items.length,
      };
    }
    const openCount = items.filter((it) => it.status === "open").length;
    const doneCount = items.filter((it) => it.status === "done").length;
    return {
      open: totalOpen || openCount,
      done: doneCount,
      all: items.length,
    };
  });

  $effect(() => {
    const urlStatus = (() => {
      const raw = page.url.searchParams.get("status");
      if (raw === "done" || raw === "all") return raw;
      return "open";
    })();
    if (urlStatus !== status) {
      status = urlStatus;
      nextCursor = null;
      items = [];
      void loadFirst();
    }
  });

  function onRetry() {
    void loadFirst();
  }
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
  {transientError}
  {togglingIds}
  onfilterchange={onFilterChange}
  ontoggle={onToggle}
  onloadmore={loadMore}
  onretry={onRetry}
/>
