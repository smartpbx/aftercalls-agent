<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount, onDestroy } from "svelte";
  import { page } from "$app/state";
  import { goto, replaceState, afterNavigate } from "$app/navigation";
  import * as api from "$lib/api";
  import DateInput from "@aftercalls/shared/ui/DateInput.svelte";
  import SpeakerRenamePicker from "@aftercalls/shared/ui/SpeakerRenamePicker.svelte";
  import { portalErrorToText } from "$lib/portalError";
  import {
    isProcessing,
    isDelayed,
    PILL_PROCESSING,
    PILL_STILL_WORKING,
  } from "@aftercalls/shared/processing-thresholds";
  import { registerShortcuts } from "@aftercalls/shared/shortcuts";
  import { toast } from "@aftercalls/shared/stores/toast.svelte";
  import type {
    CallListItem as Call,
    CallsListResponse,
    Me,
    OrgMember,
    Tag,
    TagSuggestion,
  } from "@aftercalls/shared/types";

  // #57 Tag-aware filter bar.
  //
  // The bar is two parts: a text search input and a tag-chip row.
  // Tag filters live on the URL as repeated `?tag=kind:value` params
  // (the backend accepts this shape natively), so refresh + share-link
  // preserve the filter set. We re-fetch whenever the tag set changes;
  // the title input filters the already-fetched rows client-side.
  //
  // TODO(refactor): the Add-filter popover duplicates markup with the
  // call-detail Add-tag flow. When the parallel agent extracts a shared
  // TagChip / TagAutocomplete component, switch to importing that.

  // Tidy a raw app binary or application-name into something human.
  function prettyApp(raw: string | null): string | null {
    if (!raw) return null;
    const key = raw.toLowerCase();
    const map: Record<string, string> = {
      zoom: "Zoom",
      "zoom.us": "Zoom",
      teams: "Teams",
      "teams-for-linux": "Teams",
      "microsoft teams": "Teams",
      slack: "Slack",
      discord: "Discord",
      "discord-canary": "Discord",
      firefox: "Firefox",
      "google chrome": "Chrome",
      chrome: "Chrome",
      chromium: "Chromium",
      "zen-browser": "Zen",
      zen: "Zen",
      brave: "Brave",
      "obs-studio": "OBS",
      obs: "OBS",
      smartpbx: "SmartPBX",
      ringotel: "Ringotel",
      signal: "Signal",
      telegram: "Telegram",
    };
    return map[key] ?? raw;
  }

  function sourceKindLabel(kind: string | null): string {
    switch (kind) {
      case "auto_detected":
        return "Auto";
      case "imported":
        return "Imported";
      case "manual":
        return "Manual";
      case "self_note":
        return "Note to self";
      case "zoho_meeting":
        return "Zoho Meeting";
      case "zoho_cliq":
        return "Zoho Cliq";
      case "smartpbx":
        return "SmartPBX";
      default:
        return "";
    }
  }

  let calls = $state<Call[]>([]);
  let error = $state("");
  let loading = $state(true);
  let query = $state("");
  let me = $state<Me | null>(null);
  // #634 — `unread` joins the existing list-view state machine. The
  // backend's `/v1/calls?view=unread` filter narrows the row set to
  // calls where `EXISTS (call_reads ...)` is false for the caller;
  // the row-level `is_read` flag drives the visual indicator on rows
  // shown by every other view too.
  type ListView = "active" | "pinned" | "snoozed" | "unread";
  let listView = $state<ListView>("active");

  // #386 — keyset pagination. `nextCursor` mirrors the backend
  // envelope; `null` on the final page hides the Load-more button.
  // PAGE_SIZE matches the portal so both surfaces request the same
  // chunk size and feel consistent end-to-end.
  const PAGE_SIZE = 50;
  let nextCursor = $state<string | null>(null);
  let loadingMore = $state(false);
  let loadMoreError = $state<string | null>(null);

  // #595 — Importable filter pill. The same /calls page renders calls +
  // candidates either independently or interleaved by date.
  //   - "all"        → both fetches fire; rows merged by recorded_at
  //                    / discovered_at.
  //   - "importable" → only the candidates fetch fires.
  //   - "hide"       → only the calls fetch fires.
  // The pill state lives on the URL as `?filter=importable|hide` so
  // refresh + share-link preserve it. Default ("all") drops the param.
  // Mirror of the portal `/calls` filter — wire shapes (candidates,
  // ImportCandidate fields, etc.) match the portal's TS types byte-for-
  // byte through `agent/src/lib/api.ts`.
  type ImportableFilter = "all" | "importable" | "hide";
  let importableFilter = $state<ImportableFilter>("all");
  let candidates = $state<api.ImportCandidate[]>([]);
  // Per-candidate busy flag — disables Import/Dismiss buttons while a
  // request is in flight so a double-click can't fire two competing
  // promotes. The optimistic-replacement bookkeeping below mirrors the
  // portal's `promotedCandidateIds` so a stale poll result that briefly
  // re-includes a just-imported candidate doesn't re-paint the row.
  let candidateBusy = $state<Record<string, boolean>>({});
  let promotedCandidateIds = $state<Set<string>>(new Set());

  // #386 — backend now returns `{ calls, next_cursor }`. Older agents
  // talking to a backend that still returns a bare array would break,
  // but the two ship together — see issue #386 / context-map for the
  // hybrid PR rule. Helper centralises the shape parse so both
  // load + loadMore reuse it.
  function parseListResponse(raw: unknown): CallsListResponse {
    const r = raw as { calls?: Call[]; next_cursor?: string | null } | null;
    return {
      calls: r?.calls ?? [],
      next_cursor: r?.next_cursor ?? null,
    };
  }
  // #403 — persist scope between sessions via localStorage. The key is
  // namespaced so a future multi-surface storage clear doesn't
  // accidentally wipe unrelated prefs. We read the value synchronously
  // at module evaluation (before onMount) so the first `load()` uses
  // the restored scope instead of always defaulting to "mine".
  const SCOPE_KEY = "aftercalls:calls_scope";
  function readPersistedScope(): "mine" | "all" {
    try {
      const raw = typeof localStorage !== "undefined"
        ? localStorage.getItem(SCOPE_KEY)
        : null;
      return raw === "all" ? "all" : "mine";
    } catch {
      return "mine";
    }
  }
  let scope = $state<"mine" | "all">(readPersistedScope());

  let canSeeAll = $derived(
    !!me && (me.role === "admin" || me.role === "owner"),
  );

  // Active tag filters, each as "kind:value". Kept in sync with the URL.
  let tagFilters = $state<string[]>([]);

  // #146 — optional date range. Empty string = unset; mirrors the
  // value shape that `<input type="date">` emits so there's no extra
  // conversion on every change. Both ends live on the URL as
  // `?from=YYYY-MM-DD&to=YYYY-MM-DD` for share-links + back/forward.
  let fromDate = $state("");
  let toDate = $state("");
  let dateTimer: ReturnType<typeof setTimeout> | null = null;
  let searchTimer: ReturnType<typeof setTimeout> | null = null;
  let callActionBusy = $state<Record<string, boolean>>({});
  let snoozePickerFor = $state<string | null>(null);
  let snoozeDraft = $state("");

  // Admin-only: filter the All-team list to a single member.
  let userFilter = $state<{ id: string; name: string } | null>(null);
  let memberRoster = $state<OrgMember[]>([]);
  let memberRosterLoaded = $state(false);
  // #332 · person-filter popover state. Roster + filter picking is
  // now driven by `SpeakerRenamePicker` (filterMode) so the picker's
  // internal `value` state replaces the old `userPopoverQuery`. We
  // keep the typed-value mirror here only because the picker accepts
  // a `bind:value` and we want to reset on close.
  let userPopoverOpen = $state(false);
  let userPopoverValue = $state("");

  async function ensureMemberRoster() {
    if (memberRosterLoaded) return;
    memberRosterLoaded = true;
    try {
      memberRoster = await invoke<OrgMember[]>("org_members");
    } catch {
      memberRoster = [];
    }
  }

  // Add-filter popover state.
  let popoverOpen = $state(false);
  let popoverKind = $state<string>("");
  let popoverQuery = $state("");
  let suggestions = $state<TagSuggestion[]>([]);
  let suggestLoading = $state(false);

  function parseTagParam(raw: string): { kind: string; value: string } | null {
    const i = raw.indexOf(":");
    if (i <= 0 || i === raw.length - 1) return null;
    return { kind: raw.slice(0, i), value: raw.slice(i + 1) };
  }

  function tagLabel(t: string) {
    const p = parseTagParam(t);
    return p ? `${p.kind}: ${p.value}` : t;
  }

  function tagKind(t: string): string {
    return parseTagParam(t)?.kind ?? "custom";
  }

  function readTagsFromUrl(): string[] {
    if (typeof window === "undefined") return [];
    const params = new URL(window.location.href).searchParams;
    return params.getAll("tag").filter((t) => !!parseTagParam(t));
  }

  // #146 — read + validate `?from=` / `?to=` from the URL. Strict
  // `YYYY-MM-DD` shape; anything else is silently dropped so a
  // pasted malformed URL can't wedge the input.
  function readDateFromUrl(key: string): string {
    if (typeof window === "undefined") return "";
    const raw = new URL(window.location.href).searchParams.get(key) ?? "";
    return /^\d{4}-\d{2}-\d{2}$/.test(raw) ? raw : "";
  }

  // #595 — read the importable-filter pill state from the URL. Only
  // accepts the two non-default values; anything else (including "all"
  // explicitly) collapses to "all" so a stray query param can't wedge
  // the page into a strange mode. Mirror of the portal helper.
  function readImportableFilterFromUrl(): ImportableFilter {
    if (typeof window === "undefined") return "all";
    const raw = new URL(window.location.href).searchParams.get("filter");
    if (raw === "importable" || raw === "hide") return raw;
    return "all";
  }

  function readListViewFromUrl(): ListView {
    if (typeof window === "undefined") return "active";
    const raw = new URL(window.location.href).searchParams.get("view");
    if (
      raw === "pinned" ||
      raw === "snoozed" ||
      // #634 — `unread` joins the allowlist; anything else still
      // collapses to the default "active" view so a stray query
      // param can't wedge the page into a strange state.
      raw === "unread"
    ) {
      return raw;
    }
    return "active";
  }

  function syncUrl() {
    if (typeof window === "undefined") return;
    const u = new URL(window.location.href);
    u.searchParams.delete("tag");
    for (const t of tagFilters) u.searchParams.append("tag", t);
    // #146 — date range on the URL. Delete + set (instead of mutate)
    // so clearing an input drops the param cleanly.
    u.searchParams.delete("from");
    u.searchParams.delete("to");
    if (fromDate) u.searchParams.set("from", fromDate);
    if (toDate) u.searchParams.set("to", toDate);
    // #595 — `?filter=importable|hide`. Default ("all") drops the
    // param so the URL stays clean for users with no candidates.
    u.searchParams.delete("filter");
    if (importableFilter !== "all") {
      u.searchParams.set("filter", importableFilter);
    }
    u.searchParams.delete("view");
    if (listView !== "active") u.searchParams.set("view", listView);
    // #135 — use SvelteKit's `replaceState` so the browser history
    // stays in sync with the filter pill without a push-navigation.
    // `replaceState` from `$app/navigation` updates `history` +
    // `page.state` but does NOT update `page.url` — so there is
    // intentionally no URL-observing `$effect` here; filter-click
    // writes `tagFilters` + URL synchronously, and browser
    // back/forward is handled by `afterNavigate` below.
    replaceState(u.pathname + u.search, page.state);
  }

  async function load() {
    loading = true;
    error = "";
    loadMoreError = null;
    // #595 — parallel fetch. The pill state determines which side
    // fires; "all" fires both, "importable" only candidates, "hide"
    // only calls. Each fetch's failure is isolated so a 500 on the
    // less-load-bearing side doesn't blank the whole page (mirror of
    // portal `/calls`).
    const wantCalls = importableFilter !== "importable";
    const wantCandidates = importableFilter !== "hide";
    const callsPromise: Promise<unknown> = wantCalls
      ? invoke<unknown>("list_calls", {
          scope,
          user: scope === "all" ? (userFilter?.id ?? null) : null,
          tags: tagFilters,
          // #146 — pack `YYYY-MM-DD` into full RFC3339 on this side so
          // the Tauri command + backend only ever see parsed timestamps.
          fromDate: fromDate ? `${fromDate}T00:00:00Z` : null,
          toDate: toDate ? `${toDate}T23:59:59Z` : null,
          // #386 — first page → no cursor; PAGE_SIZE is the chunk size.
          cursor: null,
          limit: PAGE_SIZE,
          q: query,
          view: listView,
        }).catch((e) => ({ _err: e }))
      : Promise.resolve(null);
    const candidatesPromise: Promise<api.ImportCandidatesResponse | { _err: unknown } | null> =
      wantCandidates
        ? api.importCandidates
            .list()
            .catch((e: unknown) => ({ _err: e }))
        : Promise.resolve(null);
    try {
      const [callsRaw, candidatesResp] = await Promise.all([
        callsPromise,
        candidatesPromise,
      ]);
      if (wantCalls) {
        if (callsRaw && (callsRaw as { _err?: unknown })._err !== undefined) {
          throw (callsRaw as { _err: unknown })._err;
        }
        const resp = parseListResponse(callsRaw);
        calls = resp.calls;
        nextCursor = resp.next_cursor;
      } else {
        calls = [];
        nextCursor = null;
      }
      if (wantCandidates) {
        if (
          candidatesResp &&
          (candidatesResp as { _err?: unknown })._err !== undefined
        ) {
          // Candidates failure is isolated — toast the error rather
          // than blanking the page. Calls list (the dominant surface)
          // still paints whatever the calls fetch returned.
          toast.error("Couldn't load importable recordings — try refreshing.");
          candidates = [];
        } else {
          const r = candidatesResp as api.ImportCandidatesResponse | null;
          candidates = (r?.items ?? []).filter(
            (c) => !promotedCandidateIds.has(c.id),
          );
        }
      } else {
        candidates = [];
      }
    } catch (e) {
      error = portalErrorToText(e);
    } finally {
      loading = false;
    }
  }

  // #386 — keyset Load-more, mirror of the portal page. Appends rather
  // than replacing so the day-grouped list accumulates.
  async function loadMore() {
    if (!nextCursor || loadingMore) return;
    loadingMore = true;
    loadMoreError = null;
    try {
      const raw = await invoke<unknown>("list_calls", {
        scope,
        user: scope === "all" ? (userFilter?.id ?? null) : null,
        tags: tagFilters,
        fromDate: fromDate ? `${fromDate}T00:00:00Z` : null,
        toDate: toDate ? `${toDate}T23:59:59Z` : null,
        cursor: nextCursor,
        limit: PAGE_SIZE,
        q: query,
        view: listView,
      });
      const resp = parseListResponse(raw);
      calls = [...calls, ...resp.calls];
      nextCursor = resp.next_cursor;
    } catch (e) {
      loadMoreError = portalErrorToText(e);
    } finally {
      loadingMore = false;
    }
  }

  // #146 — live-on-change with 250ms debounce. Typing a date picks one
  // digit at a time in some browsers (`on:input` fires per keystroke),
  // so we don't want to refetch on every tick. Clear is immediate via
  // `clearDates()`.
  function onDateChange() {
    if (dateTimer) clearTimeout(dateTimer);
    dateTimer = setTimeout(() => {
      syncUrl();
      void load();
    }, 250);
  }

  function onSearchChange() {
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      void load();
    }, 250);
  }

  async function setListView(next: ListView) {
    if (listView === next) return;
    listView = next;
    syncUrl();
    await load();
  }

  async function clearDates() {
    if (dateTimer) {
      clearTimeout(dateTimer);
      dateTimer = null;
    }
    if (searchTimer) {
      clearTimeout(searchTimer);
      searchTimer = null;
    }
    fromDate = "";
    toDate = "";
    syncUrl();
    await load();
  }

  async function setScope(next: "mine" | "all") {
    if (scope === next) return;
    scope = next;
    // #403 — persist selection so the next session opens the same scope.
    try {
      if (typeof localStorage !== "undefined") {
        localStorage.setItem(SCOPE_KEY, scope);
      }
    } catch {}
    if (scope !== "all") userFilter = null;
    if (scope === "all") void ensureMemberRoster();
    await load();
  }

  async function setUserFilter(
    m: { id: string; display_name: string } | null,
  ) {
    userFilter = m ? { id: m.id, name: m.display_name } : null;
    userPopoverOpen = false;
    userPopoverValue = "";
    await load();
  }

  // #332 · open the person-filter picker. Always seeds an empty
  // input so the user starts a fresh search; matches the prior
  // popover behaviour where opening cleared the typed query.
  async function openUserPicker() {
    await ensureMemberRoster();
    userPopoverValue = "";
    userPopoverOpen = true;
  }

  async function addTagFilter(kind: string, value: string) {
    const k = kind.trim();
    const v = value.trim();
    if (!k || !v) return;
    const entry = `${k}:${v}`;
    if (tagFilters.includes(entry)) {
      popoverOpen = false;
      return;
    }
    tagFilters = [...tagFilters, entry];
    syncUrl();
    popoverOpen = false;
    popoverQuery = "";
    popoverKind = "";
    await load();
  }

  async function removeTagFilter(entry: string) {
    tagFilters = tagFilters.filter((t) => t !== entry);
    syncUrl();
    await load();
  }

  async function clearFilters() {
    tagFilters = [];
    query = "";
    userFilter = null;
    fromDate = "";
    toDate = "";
    listView = "active";
    if (dateTimer) {
      clearTimeout(dateTimer);
      dateTimer = null;
    }
    if (searchTimer) {
      clearTimeout(searchTimer);
      searchTimer = null;
    }
    syncUrl();
    await load();
  }

  // #595 — flip the importable filter pill. The URL is synced before
  // the load fires so refresh during the in-flight request still
  // reflects the user's intent. Mirror of portal `setImportableFilter`.
  async function setImportableFilter(next: ImportableFilter) {
    if (importableFilter === next) return;
    importableFilter = next;
    syncUrl();
    await load();
  }

  async function refreshSuggestions() {
    suggestLoading = true;
    try {
      suggestions = await invoke<TagSuggestion[]>("tag_suggestions", {
        kind: popoverKind || null,
        q: popoverQuery.trim() || null,
      });
    } catch {
      suggestions = [];
    } finally {
      suggestLoading = false;
    }
  }

  async function openPopover() {
    popoverOpen = true;
    popoverQuery = "";
    popoverKind = "";
    await refreshSuggestions();
  }

  function onPopoverKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      popoverOpen = false;
    } else if (e.key === "Enter") {
      const v = popoverQuery.trim();
      if (v && popoverKind) {
        e.preventDefault();
        void addTagFilter(popoverKind, v);
      } else if (suggestions.length > 0) {
        e.preventDefault();
        const s = suggestions[0];
        void addTagFilter(s.kind, s.value);
      }
    }
  }

  // Focus the popover's search input when it opens. Using a Svelte action
  // keeps us off of raw `autofocus`, which is an a11y smell — we only
  // pull focus inside a user-opened popover, not on page load.
  function focusOnMount(node: HTMLInputElement) {
    node.focus();
  }

  let suggestTimer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    if (!popoverOpen) return;
    popoverKind;
    popoverQuery;
    if (suggestTimer) clearTimeout(suggestTimer);
    suggestTimer = setTimeout(() => {
      void refreshSuggestions();
    }, 120);
  });

  // #142 follow-up — the note-to-self entry point moved to the Record
  // page (Call/Note tab toggle). The dedicated button + recording-state
  // listener that lived here in v0.4.5 are gone; the global hotkey and
  // tray menu still work and are unchanged.

  // #286 — derived "still working" pill. Re-evaluated every 30s so a
  // call that crosses its threshold while the page is open changes
  // pill state without a refresh. Cleared on unmount so the timer
  // doesn't leak across navigations.
  let nowMs = $state(Date.now());
  let nowTimer: ReturnType<typeof setInterval> | undefined;

  // #282 — keyboard shortcuts. Mirrors the portal calls list (j/k +
  // Enter/o + `/`). Shape kept identical so a future shared
  // calls-list component can lift this verbatim.
  let searchInput: HTMLInputElement | undefined = $state();
  let highlightedCallId = $state<string | null>(null);
  // #595 — only call rows are navigable via j/k + Enter/o; candidates
  // sit outside the keyboard-row cycle since their action is the
  // Import / Dismiss buttons rather than open-detail.
  let visibleIds = $derived.by(() => {
    const ids: string[] = [];
    for (const [, items] of groups) {
      for (const row of items) {
        if (row.kind === "call") ids.push(row.call.id);
      }
    }
    return ids;
  });

  $effect(() => {
    if (visibleIds.length === 0) {
      highlightedCallId = null;
      return;
    }
    if (!highlightedCallId || !visibleIds.includes(highlightedCallId)) {
      highlightedCallId = visibleIds[0];
    }
  });

  function moveHighlight(delta: 1 | -1) {
    if (visibleIds.length === 0) return;
    const cur = highlightedCallId
      ? visibleIds.indexOf(highlightedCallId)
      : -1;
    const next = Math.max(
      0,
      Math.min(visibleIds.length - 1, (cur < 0 ? 0 : cur) + delta),
    );
    highlightedCallId = visibleIds[next];
    requestAnimationFrame(() => {
      const el = document.querySelector<HTMLElement>(
        '[data-shortcut-row="active"]',
      );
      el?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    });
  }

  function openHighlighted() {
    if (highlightedCallId) void goto(`/calls/${highlightedCallId}`);
  }

  // #634 — keyboard shortcut handler: `m` marks the highlighted row
  // read. No-op when the highlighted row is already read or no row
  // is highlighted. Builds a synthetic MouseEvent so `markRead` can
  // share the same `e.preventDefault() / stopPropagation()` posture
  // as the click path (the shared shortcuts module already guards
  // text-input targets so no extra guard is needed here).
  function markHighlightedRead() {
    if (!highlightedCallId) return;
    const call = calls.find((c) => c.id === highlightedCallId);
    if (!call || call.is_read !== false) return;
    void markRead(new MouseEvent("click"), call);
  }

  function focusSearch(e: KeyboardEvent) {
    e.preventDefault();
    searchInput?.focus();
    searchInput?.select();
  }

  let teardownShortcuts: (() => void) | null = null;

  onMount(async () => {
    try {
      me = await invoke<Me | null>("current_user");
    } catch {}
    tagFilters = readTagsFromUrl();
    fromDate = readDateFromUrl("from");
    toDate = readDateFromUrl("to");
    importableFilter = readImportableFilterFromUrl();
    listView = readListViewFromUrl();
    if (scope === "all" && canSeeAll) void ensureMemberRoster();
    nowTimer = setInterval(() => {
      nowMs = Date.now();
    }, 30_000);
    teardownShortcuts = registerShortcuts(
      "calls-list",
      "Calls list",
      {
        j: () => moveHighlight(1),
        k: () => moveHighlight(-1),
        enter: () => openHighlighted(),
        o: () => openHighlighted(),
        "/": (e) => focusSearch(e),
        // #634 — `m` marks the highlighted row as read. The shared
        // shortcut module already guards against text-input targets
        // (same guard j/k rely on), so a user typing `m` in the
        // search box doesn't accidentally fire the action.
        m: () => markHighlightedRead(),
      },
      [
        { keys: "j k", label: "Next / previous call" },
        { keys: "enter o", label: "Open the highlighted call" },
        { keys: "/", label: "Focus search" },
        { keys: "m", label: "Mark the highlighted call as read" },
      ],
    );
    await load();
  });

  onDestroy(() => {
    if (nowTimer !== undefined) {
      clearInterval(nowTimer);
      nowTimer = undefined;
    }
    if (searchTimer) {
      clearTimeout(searchTimer);
      searchTimer = null;
    }
    teardownShortcuts?.();
    teardownShortcuts = null;
  });

  // Back/forward navigation: `replaceState` (used for filter-pill
  // clicks) does NOT update `page.url`, so a `$effect` on
  // `page.url.searchParams` never re-evaluates. Real navigation
  // (popstate) DOES update `page.url`, so `afterNavigate` with a
  // `type === "popstate"` guard re-seeds filters and reloads.
  afterNavigate(({ type }) => {
    if (type !== "popstate") return;
    tagFilters = readTagsFromUrl();
    fromDate = readDateFromUrl("from");
    toDate = readDateFromUrl("to");
    importableFilter = readImportableFilterFromUrl();
    listView = readListViewFromUrl();
    void load();
  });

  let filtered = $derived.by(() => {
    return calls;
  });

  // #595 — title-search applies to candidates too so the search box
  // filters across both row kinds.
  let filteredCandidates = $derived.by(() => {
    if (!query.trim()) return candidates;
    const q = query.trim().toLowerCase();
    return candidates.filter((c) =>
      candidateTitle(c).toLowerCase().includes(q),
    );
  });

  // #527 — tags discoverability tip. True when the user has calls loaded
  // but none of them carry any tags, and no filters are active. The tip
  // is shown in the chip-row so the "+ Add filter" button is adjacent.
  // Dismissed via localStorage so it only appears once. Defer the check
  // until the first load completes so we don't flash during loading.
  const TAG_TIP_KEY = "aftercalls:tags-tip-dismissed";
  let tagTipDismissed = $state(
    typeof localStorage !== "undefined" && !!localStorage.getItem(TAG_TIP_KEY),
  );
  let anyCallHasTags = $derived(calls.some((c) => c.tags && c.tags.length > 0));
  let showTagTip = $derived(
    !loading &&
      !tagTipDismissed &&
      !anyCallHasTags &&
      calls.length > 0 &&
      tagFilters.length === 0 &&
      !userFilter,
  );
  function dismissTagTip() {
    tagTipDismissed = true;
    try {
      localStorage.setItem(TAG_TIP_KEY, "1");
    } catch {}
  }

  // Group by LOCAL yyyy-mm-dd. Using toISOString here would bucket a call
  // recorded at 11pm local on Monday under Tuesday (UTC).
  function localDayKey(iso: string): string {
    const d = new Date(iso);
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    return `${y}-${m}-${day}`;
  }

  // #595 — unified row shape for the grouped list. Calls and
  // candidates render side-by-side when the pill is "all"; the
  // discriminator drives the markup branch in the each-block below.
  type ListRow =
    | { kind: "call"; call: Call; recordedAt: string }
    | { kind: "candidate"; candidate: api.ImportCandidate; recordedAt: string };

  let groups = $derived.by(() => {
    const map = new Map<string, ListRow[]>();
    for (const c of filtered) {
      const key = localDayKey(c.recorded_at);
      const arr = map.get(key) ?? [];
      arr.push({ kind: "call", call: c, recordedAt: c.recorded_at });
      map.set(key, arr);
    }
    for (const c of filteredCandidates) {
      const at = candidateRecordedAt(c);
      const key = localDayKey(at);
      const arr = map.get(key) ?? [];
      arr.push({ kind: "candidate", candidate: c, recordedAt: at });
      map.set(key, arr);
    }
    // Within a day, sort newest first by the row's recorded /
    // discovered timestamp so candidates and calls interleave
    // naturally.
    for (const arr of map.values()) {
      arr.sort((a, b) => (a.recordedAt < b.recordedAt ? 1 : -1));
    }
    return [...map.entries()].sort((a, b) => (a[0] < b[0] ? 1 : -1));
  });

  function fmtDay(key: string) {
    const [y, m, dd] = key.split("-").map(Number);
    const d = new Date(y, m - 1, dd);
    const today = new Date();
    const yest = new Date();
    yest.setDate(today.getDate() - 1);
    const sameDay = (a: Date, b: Date) =>
      a.getFullYear() === b.getFullYear() &&
      a.getMonth() === b.getMonth() &&
      a.getDate() === b.getDate();
    if (sameDay(d, today)) return "Today";
    if (sameDay(d, yest)) return "Yesterday";
    return d.toLocaleDateString(undefined, {
      weekday: "short",
      month: "short",
      day: "numeric",
      year:
        d.getFullYear() !== today.getFullYear() ? "numeric" : undefined,
    });
  }

  function fmtTime(iso: string) {
    return new Date(iso).toLocaleTimeString(undefined, {
      hour: "numeric",
      minute: "2-digit",
    });
  }

  function fmtDuration(ms: number) {
    const s = Math.round(ms / 1000);
    const m = Math.floor(s / 60);
    const r = s % 60;
    if (m === 0) return `${r}s`;
    return `${m}:${String(r).padStart(2, "0")}`;
  }

  async function promoteRowTag(e: MouseEvent, t: Tag) {
    e.preventDefault();
    e.stopPropagation();
    await addTagFilter(t.kind, t.value);
  }

  // #303 — placeholder external recordings. Mirror of the portal's
  // importPlaceholder; uses the new Tauri `hydrate_call` command
  // which proxies to POST /v1/calls/{id}/hydrate. Optimistic
  // local-state flip so the row reacts immediately.
  let hydrating = $state<Record<string, boolean>>({});
  async function importPlaceholder(e: MouseEvent, callId: string) {
    e.preventDefault();
    e.stopPropagation();
    if (hydrating[callId] || dismissingPlaceholder[callId]) return;
    hydrating = { ...hydrating, [callId]: true };
    try {
      await invoke("hydrate_call", { id: callId });
      calls = calls.map(c =>
        c.id === callId ? { ...c, status: "transcribing" } : c
      );
    } catch (e: any) {
      console.warn("hydrate failed", portalErrorToText(e));
    } finally {
      hydrating = { ...hydrating, [callId]: false };
    }
  }

  // Mirror of the portal's dismissPlaceholder. Soft-deletes a
  // `status='available'` placeholder so legacy `auto_import = TRUE`
  // integrations (Zoho Meeting today, SmartPBX next) get the same
  // Import + Dismiss pair the new #595 candidate flow ships with.
  // Routes through the existing `delete_call` Tauri command (calls
  // DELETE /v1/calls/{id}) — placeholders carry no audio yet, and
  // the nightly purge sweeps soft-deleted rows after 30 days.
  let dismissingPlaceholder = $state<Record<string, boolean>>({});
  async function dismissPlaceholder(e: MouseEvent, callId: string) {
    e.preventDefault();
    e.stopPropagation();
    if (hydrating[callId] || dismissingPlaceholder[callId]) return;
    dismissingPlaceholder = { ...dismissingPlaceholder, [callId]: true };
    const prev = calls;
    calls = calls.filter((c) => c.id !== callId);
    try {
      await invoke("delete_call", { id: callId });
    } catch (err: any) {
      calls = prev;
      toast.error(`Couldn't dismiss: ${portalErrorToText(err)}`);
    } finally {
      dismissingPlaceholder = { ...dismissingPlaceholder, [callId]: false };
    }
  }

  // #595 — candidate metadata + render helpers. Candidates carry
  // source-specific JSONB so we read defensively — a malformed payload
  // shouldn't blank the row.
  function candidateTitle(c: api.ImportCandidate): string {
    const m = c.metadata ?? {};
    const title = (m as any).title;
    if (typeof title === "string" && title.trim()) return title;
    const topic = (m as any).topic;
    if (typeof topic === "string" && topic.trim()) return topic;
    if (c.ingest_source === "smartpbx") {
      const caller = (m as any).caller_extension;
      const callee = (m as any).callee_extension;
      if (caller && callee) return `${caller} → ${callee}`;
      if (caller) return `From ${caller}`;
    }
    return "(untitled)";
  }
  function candidateRecordedAt(c: api.ImportCandidate): string {
    const m = c.metadata ?? {};
    const start = (m as any).started_at ?? (m as any).start_time;
    if (typeof start === "string" && start) return start;
    return c.discovered_at;
  }
  function candidateDurationMs(c: api.ImportCandidate): number {
    const m = c.metadata ?? {};
    const secs = (m as any).duration_secs;
    if (typeof secs === "number" && isFinite(secs)) {
      return Math.max(0, Math.round(secs * 1000));
    }
    return 0;
  }

  // #595 — Source label for the candidate row's small chip. Admin-
  // context labels — naming the upstream product is fine here per
  // the vendor-opaque rule (the rule applies to end-user copy on the
  // public site and in-app marketing surfaces, not to source chips
  // that signal the integration the candidate came from).
  function candidateSourceLabel(src: "smartpbx" | "zoho_meeting"): string {
    return src === "smartpbx" ? "FusionPBX" : "Zoho Meeting";
  }

  // #595 — Import a candidate. Optimistic shape: replace the candidate
  // row in place with a synthetic Call that paints as `transcribing`
  // (same status the legacy hydrate flow uses), so the row stays put
  // without a layout jump. The server returns the real call_id; we
  // stamp `promotedCandidateIds` so a stale poll result doesn't
  // re-paint the candidate after the next refresh.
  async function importCandidate(e: MouseEvent, c: api.ImportCandidate) {
    e.preventDefault();
    e.stopPropagation();
    if (candidateBusy[c.id]) return;
    candidateBusy = { ...candidateBusy, [c.id]: true };
    const prevCandidates = candidates;
    const prevCalls = calls;
    try {
      const resp = await api.importCandidates.import(c.id);
      const optimistic: Call = {
        id: resp.call_id,
        session_id: resp.call_id,
        recorded_at: candidateRecordedAt(c),
        duration_ms: candidateDurationMs(c),
        title: candidateTitle(c),
        matched_client: null,
        status: "transcribing",
        source_app: null,
        source_kind: null,
        ingest_source: c.ingest_source,
        tags: [],
      };
      candidates = candidates.filter((x) => x.id !== c.id);
      promotedCandidateIds = new Set([...promotedCandidateIds, c.id]);
      // Only insert into the calls list if calls are visible — when
      // the filter is "importable", the calls array is empty by
      // design and we shouldn't pollute it.
      if (importableFilter !== "importable") {
        calls = [optimistic, ...calls];
      }
      if (!resp.was_new) {
        toast.info("This recording is already importing.");
      }
    } catch (err: any) {
      candidates = prevCandidates;
      calls = prevCalls;
      toast.error(`Couldn't import: ${portalErrorToText(err)}`);
    } finally {
      candidateBusy = { ...candidateBusy, [c.id]: false };
    }
  }

  // #595 — Dismiss a candidate. Optimistic removal — server is
  // idempotent; cross-org / unknown ids return 404 which surfaces as a
  // revert + toast.
  async function dismissCandidate(e: MouseEvent, c: api.ImportCandidate) {
    e.preventDefault();
    e.stopPropagation();
    if (candidateBusy[c.id]) return;
    candidateBusy = { ...candidateBusy, [c.id]: true };
    const prev = candidates;
    candidates = candidates.filter((x) => x.id !== c.id);
    try {
      await api.importCandidates.dismiss(c.id);
    } catch (err: any) {
      candidates = prev;
      toast.error(`Couldn't dismiss: ${portalErrorToText(err)}`);
    } finally {
      candidateBusy = { ...candidateBusy, [c.id]: false };
    }
  }

  function localDateValue(d: Date): string {
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    return `${y}-${m}-${day}`;
  }

  function tomorrowValue(): string {
    const d = new Date();
    d.setDate(d.getDate() + 1);
    return localDateValue(d);
  }

  function nextMondayValue(): string {
    const d = new Date();
    const days = (8 - d.getDay()) % 7 || 7;
    d.setDate(d.getDate() + days);
    return localDateValue(d);
  }

  function snoozeWireValue(day: string): string {
    return `${day}T00:00:00Z`;
  }

  function snoozeLabel(iso: string): string {
    return new Date(iso).toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
    });
  }

  async function togglePin(e: MouseEvent, call: Call) {
    e.preventDefault();
    e.stopPropagation();
    if (callActionBusy[call.id]) return;
    const nextPinned = !call.pinned_at;
    callActionBusy = { ...callActionBusy, [call.id]: true };
    const prev = calls;
    calls = calls.map((c) =>
      c.id === call.id
        ? { ...c, pinned_at: nextPinned ? new Date().toISOString() : null }
        : c,
    );
    try {
      await invoke("patch_call", { id: call.id, body: { pinned: nextPinned } });
      await load();
    } catch (err: any) {
      calls = prev;
      toast.error(`Couldn't update pin: ${portalErrorToText(err)}`);
    } finally {
      callActionBusy = { ...callActionBusy, [call.id]: false };
    }
  }

  function openSnoozePicker(e: MouseEvent, call: Call) {
    e.preventDefault();
    e.stopPropagation();
    snoozePickerFor = snoozePickerFor === call.id ? null : call.id;
    snoozeDraft = call.snoozed_until
      ? localDateValue(new Date(call.snoozed_until))
      : tomorrowValue();
  }

  async function applySnooze(e: MouseEvent, call: Call, day = snoozeDraft) {
    e.preventDefault();
    e.stopPropagation();
    if (!day || callActionBusy[call.id]) return;
    callActionBusy = { ...callActionBusy, [call.id]: true };
    const prev = calls;
    const wire = snoozeWireValue(day);
    calls = calls.map((c) =>
      c.id === call.id ? { ...c, snoozed_until: wire } : c,
    );
    try {
      await invoke("patch_call", { id: call.id, body: { snoozed_until: wire } });
      snoozePickerFor = null;
      await load();
    } catch (err: any) {
      calls = prev;
      toast.error(`Couldn't snooze: ${portalErrorToText(err)}`);
    } finally {
      callActionBusy = { ...callActionBusy, [call.id]: false };
    }
  }

  async function clearSnooze(e: MouseEvent, call: Call) {
    e.preventDefault();
    e.stopPropagation();
    if (callActionBusy[call.id]) return;
    callActionBusy = { ...callActionBusy, [call.id]: true };
    const prev = calls;
    calls = calls.map((c) =>
      c.id === call.id ? { ...c, snoozed_until: null } : c,
    );
    try {
      await invoke("patch_call", { id: call.id, body: { snoozed_until: null } });
      snoozePickerFor = null;
      await load();
    } catch (err: any) {
      calls = prev;
      toast.error(`Couldn't clear snooze: ${portalErrorToText(err)}`);
    } finally {
      callActionBusy = { ...callActionBusy, [call.id]: false };
    }
  }

  // #634 — mark-as-read flows. Optimistic update + rollback on error,
  // matching the existing pin / snooze posture above. Each fires a
  // window-level `unread-count-changed` event with a delta so the
  // layout's sidebar pill ticks down on the same animation frame
  // without waiting for the next 60s poll. Rollback restores both
  // the row state AND the pill (using the inverse delta).
  async function markRead(e: MouseEvent, call: Call) {
    e.preventDefault();
    e.stopPropagation();
    if (callActionBusy[call.id]) return;
    if (call.is_read) return;
    callActionBusy = { ...callActionBusy, [call.id]: true };
    const prev = calls;
    calls = calls.map((c) => (c.id === call.id ? { ...c, is_read: true } : c));
    window.dispatchEvent(
      new CustomEvent("unread-count-changed", { detail: { delta: -1 } }),
    );
    try {
      await api.calls.markRead(call.id);
    } catch (err: any) {
      calls = prev;
      // Inverse delta so the sidebar chip restores its previous
      // value rather than ticking another step in the wrong
      // direction.
      window.dispatchEvent(
        new CustomEvent("unread-count-changed", { detail: { delta: 1 } }),
      );
      toast.error(`Couldn't mark read: ${portalErrorToText(err)}`);
    } finally {
      callActionBusy = { ...callActionBusy, [call.id]: false };
    }
  }

  let markAllBusy = $state(false);
  async function markAllRead() {
    if (markAllBusy) return;
    // Snapshot the unread set we know about locally for optimistic
    // UI + a precise rollback on error. The backend marks every
    // unread call in the org for the caller; the local rows we know
    // about may be a subset. The pill goes to 0 either way.
    const prevCalls = calls;
    const prevUnreadIds = calls.filter((c) => !c.is_read).map((c) => c.id);
    if (prevUnreadIds.length === 0) return;
    markAllBusy = true;
    calls = calls.map((c) => ({ ...c, is_read: true }));
    window.dispatchEvent(
      new CustomEvent("unread-count-changed", { detail: { absolute: 0 } }),
    );
    try {
      await api.calls.markAllRead();
      // Re-tick the layout poll in case the server-side count is
      // ahead of our optimistic 0 (e.g. another browser tab marked
      // a call read between paint and click). No payload → layout
      // refetches `/auth/me`.
      window.dispatchEvent(new CustomEvent("unread-count-changed"));
    } catch (err: any) {
      calls = prevCalls;
      window.dispatchEvent(new CustomEvent("unread-count-changed"));
      toast.error(`Couldn't mark all read: ${portalErrorToText(err)}`);
    } finally {
      markAllBusy = false;
    }
  }

  // #634 — derived count of unread rows currently rendered. Drives
  // the conditional render of the "Mark all read" header button +
  // the keyboard shortcut handler's enable gate. Stays local to the
  // page so the sidebar pill (which reflects the org-wide count
  // from /auth/me) and the page-local visible count don't drift.
  let visibleUnreadCount = $derived(
    calls.filter((c) => !c.is_read && c.status === "complete").length,
  );
</script>

<main class="page">
  <header class="head">
    <div>
      <h1>Calls</h1>
      <p class="sub">
        {calls.length} {calls.length === 1 ? "call" : "calls"}
        {scope === "all" ? "across the team" : "in your archive"}
        {#if candidates.length > 0}
          · {candidates.length} importable
        {/if}
      </p>
    </div>
    <div class="head-actions">
      {#if canSeeAll}
        <div class="scope-toggle" role="group" aria-label="Scope">
          <button
            type="button"
            class="scope-opt"
            class:active={scope === "mine"}
            onclick={() => setScope("mine")}
          >
            My calls
          </button>
          <button
            type="button"
            class="scope-opt"
            class:active={scope === "all"}
            onclick={() => setScope("all")}
          >
            All team
          </button>
        </div>
      {/if}
      {#if visibleUnreadCount > 0}
        <!-- #634 — Mark all read. Renders only when there's at
             least one unread row in the current view. Optimistic
             flip + `unread-count-changed` event so the sidebar
             chip ticks down on the same frame. Same hairline-
             border quiet-action shape as the adjacent Trash link
             so the cluster reads as a single right-aligned group. -->
        <button
          type="button"
          class="mark-all-read-btn"
          disabled={markAllBusy}
          onclick={markAllRead}
        >
          Mark all read
        </button>
      {/if}
      <a class="trash-link" href="/calls/trash" title="Recycle bin">Trash</a>
    </div>
  </header>

  <!-- #595 — Importable filter pills. Hidden when the user has zero
       candidates AND the filter is in its default state — keeps the
       /calls UX byte-identical for users without an integration. The
       pill row also stays visible when the user has switched to
       "Importable only" so they retain a way to switch back. Mirrors
       the portal `/calls` page. -->
  {#if candidates.length > 0 || importableFilter !== "all"}
    <div class="importable-pills" role="group" aria-label="Importable filter">
      <button
        type="button"
        class="importable-pill"
        class:active={importableFilter === "all"}
        onclick={() => setImportableFilter("all")}
      >
        All
      </button>
      <button
        type="button"
        class="importable-pill"
        class:active={importableFilter === "importable"}
        onclick={() => setImportableFilter("importable")}
      >
        Importable only
      </button>
      <button
        type="button"
        class="importable-pill"
        class:active={importableFilter === "hide"}
        onclick={() => setImportableFilter("hide")}
      >
        Hide importable
      </button>
    </div>
  {/if}

  <div class="list-view-pills" role="group" aria-label="Call list view">
    <button
      type="button"
      class="list-view-pill"
      class:active={listView === "active"}
      onclick={() => setListView("active")}
    >
      Active
    </button>
    <button
      type="button"
      class="list-view-pill"
      class:active={listView === "pinned"}
      onclick={() => setListView("pinned")}
    >
      Pinned
    </button>
    <button
      type="button"
      class="list-view-pill"
      class:active={listView === "snoozed"}
      onclick={() => setListView("snoozed")}
    >
      Snoozed
    </button>
    <!-- #634 — Unread is the narrowest filter (cuts across all of
         Active/Pinned/Snoozed) so it sits last in the row per
         ui.md. No count baked into the label — the sidebar chip
         carries that, this pill's job is navigation. -->
    <button
      type="button"
      class="list-view-pill"
      class:active={listView === "unread"}
      onclick={() => setListView("unread")}
    >
      Unread
    </button>
  </div>

  <div class="filter-bar">
    <div class="search">
      <span class="search-glyph" aria-hidden="true">
        <svg viewBox="0 0 16 16" width="13" height="13">
          <circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" />
          <path d="M10.5 10.5 L14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
        </svg>
      </span>
      <input
        type="text"
        placeholder="Search titles and transcripts"
        bind:value={query}
        bind:this={searchInput}
        oninput={onSearchChange}
      />
    </div>
    <div class="chip-row">
      {#each tagFilters as t (t)}
        <span class="tag-chip tag-{tagKind(t)}">
          <span class="tag-chip-label">{tagLabel(t)}</span>
          <button
            type="button"
            class="tag-chip-x"
            aria-label="Remove filter {tagLabel(t)}"
            onclick={() => removeTagFilter(t)}
          >×</button>
        </span>
      {/each}
      {#if userFilter}
        <span class="tag-chip tag-person">
          <span class="tag-chip-label">by: {userFilter.name}</span>
          <button
            type="button"
            class="tag-chip-x"
            aria-label="Clear person filter"
            onclick={() => setUserFilter(null)}
          >×</button>
        </span>
      {/if}
      {#if scope === "all" && canSeeAll}
        <div class="add-wrap by-person-wrap">
          {#if userPopoverOpen}
            <!-- #332 · person-filter is the SpeakerRenamePicker
                 primitive in filterMode. The picker self-handles
                 outside-click + Escape dismissal, so the trigger
                 button hides while it's mounted (matches the
                 call-detail Participants chip swap pattern at
                 calls/[id]/+page.svelte L3543–3580). -->
            <div class="by-person-picker" role="dialog" aria-label="Filter by person">
              <SpeakerRenamePicker
                bind:value={userPopoverValue}
                roster={memberRoster}
                rosterLoaded={memberRosterLoaded}
                placeholder="Search team…"
                noMatchHint="No matching teammates"
                filterMode
                onpick={(p) => {
                  // Free-form picks are suppressed by `filterMode`,
                  // so `p.user` is always present here. Guard anyway
                  // to keep the type narrowing clean.
                  if (p.user) void setUserFilter(p.user);
                }}
                oncancel={() => {
                  userPopoverOpen = false;
                  userPopoverValue = "";
                }}
              />
            </div>
          {:else}
            <button
              type="button"
              class="add-filter"
              onclick={() => void openUserPicker()}
              aria-haspopup="dialog"
              aria-expanded={userPopoverOpen}
            >
              + By person
            </button>
          {/if}
        </div>
      {/if}
      <div class="add-wrap">
        <button
          type="button"
          class="add-filter"
          onclick={() => (popoverOpen ? (popoverOpen = false) : openPopover())}
          aria-haspopup="dialog"
          aria-expanded={popoverOpen}
        >
          + Add filter
        </button>
        {#if popoverOpen}
          <div
            class="pop-backdrop"
            role="button"
            tabindex="-1"
            aria-label="Close filter picker"
            onclick={() => (popoverOpen = false)}
            onkeydown={(e) => e.key === "Escape" && (popoverOpen = false)}
          ></div>
          <div class="pop" role="dialog" aria-label="Add tag filter">
            <div class="pop-kinds" role="radiogroup" aria-label="Tag kind">
              {#each [
                { k: "", label: "Any" },
                { k: "client", label: "Client" },
                { k: "purpose", label: "Purpose" },
                { k: "topic", label: "Topic" },
                { k: "custom", label: "Custom" },
              ] as opt (opt.k)}
                <button
                  type="button"
                  class="pop-kind"
                  class:active={popoverKind === opt.k}
                  role="radio"
                  aria-checked={popoverKind === opt.k}
                  onclick={() => (popoverKind = opt.k)}
                >{opt.label}</button>
              {/each}
            </div>
            <input
              class="pop-search"
              type="text"
              placeholder={popoverKind ? `Search ${popoverKind}…` : "Search tags…"}
              bind:value={popoverQuery}
              onkeydown={onPopoverKeydown}
              use:focusOnMount
            />
            <ul class="pop-list">
              {#if suggestLoading}
                <li class="pop-empty">
                  <span class="pop-empty-spinner" aria-hidden="true"></span>
                  Loading…
                </li>
              {:else if suggestions.length === 0}
                {#if popoverQuery.trim() && popoverKind}
                  <li>
                    <button
                      type="button"
                      class="pop-item pop-item-create"
                      onclick={() => addTagFilter(popoverKind, popoverQuery)}
                    >
                      Filter by <b>{popoverKind}: {popoverQuery.trim()}</b>
                    </button>
                  </li>
                {:else}
                  <li class="pop-empty">No matching tags</li>
                {/if}
              {:else}
                {#each suggestions as s (s.kind + ":" + s.value)}
                  <li>
                    <button
                      type="button"
                      class="pop-item"
                      onclick={() => addTagFilter(s.kind, s.value)}
                    >
                      <span class="tag-chip tag-{s.kind} tag-chip-tight">
                        <span class="tag-chip-label">{s.kind}: {s.value}</span>
                      </span>
                      <span class="pop-count">{s.count}</span>
                    </button>
                  </li>
                {/each}
              {/if}
            </ul>
          </div>
        {/if}
      </div>
    </div>
  </div>

  <!-- #527 — Tags discoverability tip. Shown once (localStorage-dismissed)
       when the user has calls but hasn't tagged any of them. Sits between
       the filter-bar and the date-bar so the "Add filter" button is nearby
       and contextually adjacent. -->
  {#if showTagTip}
    <div class="tag-tip" role="note">
      <p class="tag-tip-text">
        <span class="tag-tip-icon" aria-hidden="true">🏷</span>
        Add tags to calls to filter and find them faster — client names,
        topics, or custom labels. Open a call and tap "Add tag" to start.
      </p>
      <button
        type="button"
        class="tag-tip-dismiss"
        onclick={dismissTagTip}
        aria-label="Dismiss tags tip"
        title="Dismiss"
      >×</button>
    </div>
  {/if}

  <!-- #146 · Date range. Sits in its own row so the filter-bar above
       stays legible at narrow agent widths. #189 swapped the native
       `<input type="date">` for the custom DateInput mirror-pair
       because webkit2gtk paints today as the placeholder AND doesn't
       dismiss on outside-click. DateInput emits the same YYYY-MM-DD
       strings so the 250ms debounce + URL-sync below is unchanged. -->
  <div class="date-bar">
    <label class="date-label">
      <span class="date-label-text">From</span>
      <DateInput
        value={fromDate}
        onchange={(v) => {
          fromDate = v;
          onDateChange();
        }}
        max={toDate}
        ariaLabel="From"
        ariaDescribedby="date-hint"
      />
    </label>
    <label class="date-label">
      <span class="date-label-text">To</span>
      <DateInput
        value={toDate}
        onchange={(v) => {
          toDate = v;
          onDateChange();
        }}
        min={fromDate}
        ariaLabel="To"
        ariaDescribedby="date-hint"
      />
    </label>
    {#if fromDate || toDate}
      <button type="button" class="date-clear" onclick={clearDates}>
        All dates
      </button>
    {/if}
    <span id="date-hint" class="sr-only">
      Dates narrow the visible calls to the selected range.
    </span>
  </div>

  {#if loading}
    <p class="state">Loading…</p>
  {:else if error}
    <p class="state err">{error}</p>
  {:else if calls.length === 0 && candidates.length === 0 && tagFilters.length === 0 && !query.trim() && !fromDate && !toDate && importableFilter === "all" && listView === "active"}
    <div class="empty">
      <p class="empty-title">No calls yet</p>
      <p class="empty-sub">
        Go to Record to capture your first call.
      </p>
      <a href="/" class="empty-cta">Go to Record →</a>
    </div>
  {:else if filtered.length === 0 && filteredCandidates.length === 0}
    <div class="empty">
      <p class="empty-title">
        {#if importableFilter === "importable"}
          No importable recordings
        {:else if listView === "pinned"}
          No pinned calls
        {:else if listView === "snoozed"}
          No snoozed calls
        {:else if listView === "unread"}
          You're all caught up.
        {:else if (fromDate || toDate) && tagFilters.length === 0 && !userFilter && !query.trim()}
          No calls in the selected range
        {:else}
          No calls match these filters
        {/if}
      </p>
      {#if listView === "unread"}
        <p class="empty-sub">No unread calls in this view.</p>
      {/if}
      <p class="empty-sub">
        <button type="button" class="empty-clear" onclick={clearFilters}>
          Clear filters
        </button>
      </p>
    </div>
  {:else}
    <div class="groups">
      {#each groups as [day, items] (day)}
        <section class="group">
          <div class="group-head">
            <span class="day">{fmtDay(day)}</span>
            <span class="day-count">
              {items.length} {items.length === 1 ? "item" : "items"}
            </span>
          </div>

          <ul class="entries">
            {#each items as row (row.kind + ":" + (row.kind === "call" ? row.call.id : row.candidate.id))}
              {#if row.kind === "call"}
                {@const call = row.call}
              <li>
                <a
                  href="/calls/{call.id}"
                  class="entry"
                  class:entry-unread={call.is_read === false &&
                    call.status === "complete"}
                  data-shortcut-row={highlightedCallId === call.id
                    ? "active"
                    : null}
                >
                  <span class="entry-time">{fmtTime(call.recorded_at)}</span>
                  <div class="entry-body">
                    <h3 class="entry-title">
                      {#if call.source_kind === "self_note"}
                        <span
                          class="entry-title-glyph"
                          title="Note to self"
                          aria-hidden="true"
                        >
                          <svg viewBox="0 0 16 16" width="12" height="12">
                            <rect
                              x="6"
                              y="2"
                              width="4"
                              height="7"
                              rx="2"
                              fill="none"
                              stroke="currentColor"
                              stroke-width="1.4"
                            />
                            <path
                              d="M4 8.5a4 4 0 0 0 8 0"
                              fill="none"
                              stroke="currentColor"
                              stroke-width="1.4"
                              stroke-linecap="round"
                            />
                            <path
                              d="M8 12.5v1.5M6.2 14h3.6"
                              fill="none"
                              stroke="currentColor"
                              stroke-width="1.4"
                              stroke-linecap="round"
                            />
                          </svg>
                        </span>
                      {/if}
                      {#if !call.title && call.source_kind === "self_note"}
                        Note to self — {fmtTime(call.recorded_at)}
                      {:else}
                        {call.title ?? "(untitled)"}
                      {/if}
                    </h3>
                    <div class="entry-meta">
                      {#if scope === "all" && call.user_display_name}
                        <span
                          class="owner-chip"
                          title="Recorded by {call.user_display_name}"
                        >
                          <svg viewBox="0 0 16 16" width="10" height="10" aria-hidden="true">
                            <circle cx="8" cy="5.5" r="2.8" fill="currentColor"/>
                            <path d="M2.5 14 C3 10.5 5.5 9.5 8 9.5 C10.5 9.5 13 10.5 13.5 14 Z" fill="currentColor"/>
                          </svg>
                          {call.user_display_name}
                        </span>
                      {/if}
                      {#if call.tags && call.tags.length > 0}
                        <!-- Show up to 2 tags inline; overflow goes
                             into a "+N" chip with the hidden tags in
                             a tooltip (full list is on the detail
                             page). Clicking a visible chip filters
                             the list by that tag. -->
                        {#each call.tags.slice(0, 2) as t (t.kind + ":" + t.value)}
                          <button
                            type="button"
                            class="tag-chip tag-{t.kind} tag-chip-tight tag-chip-row"
                            onclick={(e) => promoteRowTag(e, t)}
                            title="Filter by {t.kind}: {t.value}"
                          >
                            <span class="tag-chip-label">{t.kind}: {t.value}</span>
                          </button>
                        {/each}
                        {#if call.tags.length > 2}
                          <span
                            class="tag-chip tag-chip-tight tag-more"
                            title={call.tags
                              .slice(2)
                              .map((t) => `${t.kind}: ${t.value}`)
                              .join("\n")}
                          >+{call.tags.length - 2}</span>
                        {/if}
                      {:else if call.matched_client}
                        <span class="tag-chip tag-client tag-chip-tight">
                          <span class="tag-chip-label">client: {call.matched_client}</span>
                        </span>
                      {/if}
                      {#if call.pinned_at}
                        <span class="status-chip status-chip-pinned">Pinned</span>
                      {/if}
                      {#if call.snoozed_until}
                        <span class="status-chip status-chip-snoozed">
                          Snoozed {snoozeLabel(call.snoozed_until)}
                        </span>
                      {/if}
                      {#if call.ingest_source && call.ingest_source !== "agent"}
                        <!-- #303 — external-source chip drives off
                             ingest_source (the wire-side external
                             classification), NOT source_kind which is
                             the agent-side label. Keeps Zoho Meeting
                             / SmartPBX / etc. recognizable in the
                             list rather than rendering as the
                             generic "Imported" source_kind would.
                             #412 — the redundant `source_app` chip
                             that previously preceded this branch was
                             dropped; ingest_source already conveys
                             the same provenance signal. -->
                        <span class="status-chip">{sourceKindLabel(call.ingest_source)}</span>
                      {:else if call.source_kind}
                        <span class="status-chip">{sourceKindLabel(call.source_kind)}</span>
                        <!-- #412 dropped the standalone source_app chip as
                             redundant with ingest_source. That holds for
                             IMPORTED calls, but an agent-recorded call has
                             ingest_source = 'agent' (which badges nothing),
                             so the row showed only Auto/Manual and you had
                             to open the call to see whether it was Zoom,
                             Teams or Meet. Re-added on this branch only. -->
                        {#if prettyApp(call.source_app)}
                          <span class="status-chip" title="Recorded in {prettyApp(call.source_app)}">
                            {prettyApp(call.source_app)}
                          </span>
                        {/if}
                      {/if}
                      {#if call.status === "available"}
                        <!-- #303 — placeholder external recording.
                             Inline Import button hydrates the row on
                             demand. Dismiss soft-deletes the row so
                             legacy `auto_import = TRUE` integrations
                             (Zoho Meeting, SmartPBX) match the new
                             #595 candidate flow's Import + Dismiss
                             pair. Both stop event propagation so the
                             row's <a> doesn't navigate. -->
                        <button
                          type="button"
                          class="import-btn"
                          disabled={hydrating[call.id] || dismissingPlaceholder[call.id]}
                          onclick={(e) => importPlaceholder(e, call.id)}
                        >
                          {hydrating[call.id] ? "Importing…" : "Import"}
                        </button>
                        <button
                          type="button"
                          class="dismiss-btn"
                          disabled={hydrating[call.id] || dismissingPlaceholder[call.id]}
                          onclick={(e) => dismissPlaceholder(e, call.id)}
                        >
                          {dismissingPlaceholder[call.id] ? "Dismissing…" : "Dismiss"}
                        </button>
                      {:else if isProcessing(call.status)}
                        {#if isDelayed(call.status, call.recorded_at, nowMs)}
                          <span class="status-chip status-chip-still" title="Taking a little longer than usual">
                            {PILL_STILL_WORKING}
                          </span>
                        {:else}
                          <span class="status-chip status-chip-sig">{PILL_PROCESSING}</span>
                        {/if}
                      {:else if call.status === "failed"}
                        <!-- #482 — explicit Failed chip rather than the
                             generic status-chip-sig fallback so users
                             notice on the list. Detail-page retry
                             surface tracked as #488. -->
                        <span class="status-chip status-chip-failed" title="Processing failed — open the call to retry">
                          Failed
                        </span>
                      {:else if call.status !== "complete"}
                        <span class="status-chip status-chip-sig">{call.status}</span>
                      {/if}
                      {#if call.is_read === false && call.status === "complete"}
                        <!-- #634 — Mark read row affordance. Only
                             rendered for unread complete calls (read
                             rows have nothing to mark). Compact
                             row-action shape matches the
                             Pin / Snooze cluster so the four buttons
                             feel uniform; aria-label carries the
                             call title for SR users. -->
                        <button
                          type="button"
                          class="row-action mark-read-btn"
                          disabled={callActionBusy[call.id]}
                          aria-label="Mark {call.title || 'call'} as read"
                          aria-keyshortcuts="m"
                          onclick={(e) => markRead(e, call)}
                        >
                          Mark read
                        </button>
                      {/if}
                      <button
                        type="button"
                        class="row-action"
                        disabled={callActionBusy[call.id]}
                        onclick={(e) => togglePin(e, call)}
                      >
                        {call.pinned_at ? "Unpin" : "Pin"}
                      </button>
                      <button
                        type="button"
                        class="row-action"
                        disabled={callActionBusy[call.id]}
                        onclick={(e) => openSnoozePicker(e, call)}
                      >
                        Snooze
                      </button>
                      {#if call.snoozed_until}
                        <button
                          type="button"
                          class="row-action"
                          disabled={callActionBusy[call.id]}
                          onclick={(e) => clearSnooze(e, call)}
                        >
                          Unsnooze
                        </button>
                      {/if}
                      {#if snoozePickerFor === call.id}
                        <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
                        <span
                          class="snooze-pop"
                          role="group"
                          tabindex="-1"
                          aria-label="Snooze options"
                          onclick={(e) => {
                            e.preventDefault();
                            e.stopPropagation();
                          }}
                          onkeydown={(e) => e.stopPropagation()}
                        >
                          <button
                            type="button"
                            class="row-action"
                            onclick={(e) => applySnooze(e, call, tomorrowValue())}
                          >
                            Tomorrow
                          </button>
                          <button
                            type="button"
                            class="row-action"
                            onclick={(e) => applySnooze(e, call, nextMondayValue())}
                          >
                            Next Mon
                          </button>
                          <DateInput
                            value={snoozeDraft}
                            onchange={(v) => (snoozeDraft = v)}
                            ariaLabel="Snooze until"
                          />
                          <button
                            type="button"
                            class="row-action"
                            onclick={(e) => applySnooze(e, call)}
                          >
                            Set
                          </button>
                        </span>
                      {/if}
                    </div>
                  </div>
                  <span class="entry-dur">{fmtDuration(call.duration_ms)}</span>
                </a>
              </li>
              {:else}
                {@const c = row.candidate}
                {@const recordedAt = candidateRecordedAt(c)}
                {@const durMs = candidateDurationMs(c)}
                <!-- #595 — candidate row. Renders alongside real call
                     rows when the filter is "All" or "Importable
                     only". The "To import" pip + Import / Dismiss
                     button cluster lives in app.css (mirrored from
                     portal/src/app.css) — see design.md
                     §"Candidate row (#595)". -->
              <li>
                <div class="entry candidate-entry">
                  <span class="entry-time">{fmtTime(recordedAt)}</span>
                  <div class="entry-body">
                    <h3 class="entry-title">{candidateTitle(c)}</h3>
                    <div class="entry-meta">
                      <span class="candidate-pip" aria-hidden="true"></span>
                      <span class="candidate-pip-label">To import</span>
                      <span
                        class="source-chip source-{c.ingest_source}"
                        title="From {candidateSourceLabel(c.ingest_source)}"
                      >
                        {candidateSourceLabel(c.ingest_source)}
                      </span>
                    </div>
                  </div>
                  <div class="candidate-actions">
                    <button
                      type="button"
                      class="candidate-btn candidate-btn-import"
                      disabled={candidateBusy[c.id]}
                      onclick={(e) => importCandidate(e, c)}
                    >
                      {candidateBusy[c.id] ? "Importing…" : "Import"}
                    </button>
                    <button
                      type="button"
                      class="candidate-btn candidate-btn-dismiss"
                      disabled={candidateBusy[c.id]}
                      onclick={(e) => dismissCandidate(e, c)}
                    >
                      Dismiss
                    </button>
                  </div>
                  {#if durMs > 0}
                    <span class="entry-dur">{fmtDuration(durMs)}</span>
                  {:else}
                    <span class="entry-dur entry-dur-na">—</span>
                  {/if}
                </div>
              </li>
              {/if}
            {/each}
          </ul>
        </section>
      {/each}
      {#if nextCursor}
        <!-- #386 — keyset Load-more. Hidden on the final page. Retry
             on error stays on the same cursor so a click replays the
             previous request. -->
        <div class="load-more-wrap">
          {#if loadMoreError}
            <p class="state err">{loadMoreError}</p>
          {/if}
          <button
            type="button"
            class="cta-btn load-more"
            disabled={loadingMore}
            onclick={loadMore}
          >
            {loadingMore ? "Loading…" : "Load more"}
          </button>
        </div>
      {/if}
    </div>
  {/if}
</main>

<style>
  .page {
    max-width: 900px;
    margin: 0 auto;
    padding: 2.2rem 2rem 4rem;
    position: relative;
    /* #145 — no `z-index` here. Setting one would form a stacking
       context pinned at the page level, and the filter popover
       (`.pop` at z-index 11) would end up painting UNDER the layout
       topstrip (z-index 5 in app.css) because the popover's local
       stack tops out inside the `.page` context. Keep this bare so
       the popover's own z-index competes at the global level. */
  }

  .head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 1.5rem;
    margin-bottom: 1.2rem;
  }
  .head-actions {
    display: flex;
    align-items: center;
    gap: 0.7rem;
  }
  .scope-toggle {
    display: inline-flex;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    overflow: hidden;
  }
  .scope-opt {
    padding: 0.35rem 0.7rem;
    font-size: 0.78rem;
    color: var(--bone-2);
    background: transparent;
    border: none;
    border-right: 1px solid var(--hairline);
    cursor: pointer;
    transition: background 0.1s, color 0.1s;
  }
  .scope-opt:last-child { border-right: none; }
  .scope-opt:hover { color: var(--bone-0); background: var(--ink-2); }
  .scope-opt.active {
    color: var(--accent-hi);
    background: var(--accent-soft);
  }
  .trash-link {
    font-size: 0.8rem;
    color: var(--bone-3);
    padding: 0.4rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    text-decoration: none;
    transition: all 0.15s;
    align-self: center;
  }
  .trash-link:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  /* #634 — "Mark all read" header button. Same quiet-action shape
     as `.trash-link` so the right-aligned head-actions cluster reads
     uniformly. The button is conditional on `visibleUnreadCount > 0`
     in markup; CSS makes no assumption about presence. */
  .mark-all-read-btn {
    font-size: 0.8rem;
    color: var(--bone-3);
    background: transparent;
    padding: 0.4rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    cursor: pointer;
    transition: all 0.15s;
    align-self: center;
    white-space: nowrap;
  }
  .mark-all-read-btn:hover:not(:disabled) {
    color: var(--accent);
    border-color: var(--accent);
  }
  .mark-all-read-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  /* #634 — `.mark-read-btn` row affordance. Inherits everything
     from `.row-action` (defined below); the dedicated class here
     just gives the row affordance a stable hook for future tweaks
     without disturbing the shared row-action shape used by Pin /
     Snooze. */
  .row-action.mark-read-btn:hover:not(:disabled),
  .row-action.mark-read-btn:focus-visible {
    color: var(--accent);
    border-color: var(--accent);
  }

  /* #142 · v0.4.5 — mic glyph prefix on self-note rows. Decorative
     only (aria-hidden on the span); the title text "Note to self —
     …" already carries the semantic. Sits flush-left of the title
     to give sighted users a quick scan cue. */
  .entry-title-glyph {
    display: inline-flex;
    vertical-align: -2px;
    margin-right: 0.35rem;
    color: var(--bone-3);
  }
  .entry:hover .entry-title-glyph {
    color: var(--bone-1);
  }

  .head h1 {
    margin-bottom: 0.2rem;
  }

  .sub {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.82rem;
  }

  /* ── Filter bar ──────────────────────────────────────────────────── */
  .filter-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem 0.7rem;
    margin-bottom: 1.6rem;
  }

  .search {
    position: relative;
    display: flex;
    align-items: center;
  }

  .search-glyph {
    position: absolute;
    left: 0.7rem;
    color: var(--bone-3);
    display: flex;
    align-items: center;
  }

  .search input {
    width: 260px;
    padding: 0.52rem 0.85rem 0.52rem 2.1rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
    color: var(--bone-0);
    font-size: 0.85rem;
    transition: border-color 0.15s;
  }

  .search input::placeholder {
    color: var(--bone-3);
  }

  .search input:focus {
    outline: none;
    border-color: var(--accent);
  }

  .chip-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.35rem;
  }

  /* ── Tag chips (kind-colored, matches design.md Tag chip pattern) ── */
  .tag-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0.18rem 0.55rem;
    font-size: 0.75rem;
    font-weight: 500;
    letter-spacing: 0.01em;
    border-radius: 999px;
    background: var(--ink-3);
    color: var(--bone-1);
    border: 1px solid transparent;
    line-height: 1.2;
  }
  .tag-chip-tight {
    padding: 0.08rem 0.45rem;
    font-size: 0.7rem;
  }
  .tag-chip-label {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 26ch;
  }
  .tag-client {
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .tag-purpose {
    background: var(--olive-soft);
    color: var(--olive);
  }
  .tag-topic {
    background: rgba(201, 162, 74, 0.14);
    color: var(--sig);
  }
  .tag-custom {
    background: var(--ink-2);
    color: var(--bone-1);
  }
  .tag-person {
    background: rgba(58, 155, 146, 0.14);
    color: var(--accent-hi);
  }

  .owner-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0.1rem 0.5rem;
    font-size: 0.72rem;
    color: var(--bone-2);
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: 999px;
  }
  .owner-chip svg { color: var(--bone-3); }

  /* #332 · person-filter picker surface. The picker swaps in
     where the "By person" trigger sat, so it lays out inline in
     the chip row. SpeakerRenamePicker owns its own input chrome
     and absolutely-positioned dropdown list; this wrapper just
     fixes a stable width so the trigger ↔ picker swap doesn't
     reflow the row. Chrome inside is component-scoped — no row
     paint duplicates here. */
  .by-person-wrap {
    position: relative;
  }
  .by-person-picker {
    width: 240px;
  }

  .tag-chip-x {
    appearance: none;
    background: transparent;
    border: none;
    color: inherit;
    font-size: 0.95rem;
    line-height: 1;
    padding: 0 0.1rem;
    cursor: pointer;
    opacity: 0.7;
    transition: opacity 0.12s;
  }
  .tag-chip-x:hover {
    opacity: 1;
  }
  .tag-chip-row {
    appearance: none;
    border: none;
    cursor: pointer;
    font-family: inherit;
  }
  .tag-chip-row:hover {
    filter: brightness(1.15);
  }
  /* Overflow chip for call-list rows when there are more tags than
     the two that fit inline. Neutral bone styling — not a real tag
     kind, just a "+N more" indicator. Hidden tags live in the title
     attribute for a hover preview; the detail page has the full list. */
  .tag-more {
    background: var(--ink-2);
    color: var(--bone-3);
    font-weight: 500;
    cursor: help;
  }

  /* ── Add-filter button + popover ─────────────────────────────────── */
  .add-wrap {
    position: relative;
  }
  .add-filter {
    appearance: none;
    background: transparent;
    border: 1px dashed var(--hairline-hi);
    color: var(--bone-2);
    font-family: inherit;
    font-size: 0.75rem;
    padding: 0.22rem 0.65rem;
    border-radius: 999px;
    cursor: pointer;
    transition: all 0.12s;
  }
  .add-filter:hover {
    color: var(--bone-0);
    border-color: var(--accent);
  }
  .pop-backdrop {
    position: fixed;
    inset: 0;
    z-index: 10;
    background: transparent;
  }
  .pop {
    position: absolute;
    top: calc(100% + 0.35rem);
    left: 0;
    z-index: 11;
    width: 280px;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    box-shadow: 0 14px 30px -10px rgba(0, 0, 0, 0.55);
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }
  .pop-kinds {
    display: flex;
    flex-wrap: wrap;
    gap: 0.2rem;
  }
  .pop-kind {
    appearance: none;
    background: transparent;
    color: var(--bone-2);
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 0.15rem 0.55rem;
    font-size: 0.72rem;
    font-family: inherit;
    cursor: pointer;
    transition: all 0.12s;
  }
  .pop-kind:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .pop-kind.active {
    background: var(--accent-soft);
    color: var(--accent-hi);
    border-color: transparent;
  }
  .pop-search {
    width: 100%;
    padding: 0.4rem 0.6rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: var(--ink-0);
    color: var(--bone-0);
    font-family: inherit;
    font-size: 0.82rem;
  }
  .pop-search:focus {
    outline: none;
    border-color: var(--accent);
  }
  .pop-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 240px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .pop-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    width: 100%;
    appearance: none;
    background: transparent;
    border: none;
    color: var(--bone-1);
    font-family: inherit;
    font-size: 0.8rem;
    padding: 0.3rem 0.4rem;
    border-radius: 6px;
    cursor: pointer;
    text-align: left;
  }
  .pop-item:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .pop-item-create {
    color: var(--bone-2);
    font-style: italic;
  }
  .pop-count {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--bone-3);
  }
  .pop-empty {
    color: var(--bone-3);
    font-size: 0.8rem;
    padding: 0.4rem;
    text-align: center;
    list-style: none;
  }
  /* #508 — small spinner glyph next to "Loading…" so the affordance is
     consistent with other in-flight states in the agent. */
  .pop-empty-spinner {
    display: inline-block;
    width: 0.7rem;
    height: 0.7rem;
    margin-right: 0.4rem;
    vertical-align: -1px;
    border: 1.5px solid var(--hairline-hi);
    border-top-color: var(--bone-3);
    border-radius: 50%;
    animation: pop-empty-spin 0.7s linear infinite;
  }
  @keyframes pop-empty-spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .pop-empty-spinner {
      animation: none;
    }
  }

  /* ── #146 · Date range bar ───────────────────────────────────────── */
  /* #527 — tags discoverability tip */
  .tag-tip {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.55rem 0.75rem;
    margin-bottom: 0.8rem;
    border: 1px dashed var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-1);
  }
  .tag-tip-text {
    flex: 1;
    margin: 0;
    font-size: 0.8rem;
    line-height: 1.5;
    color: var(--bone-2);
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
  }
  .tag-tip-icon {
    font-size: 0.9rem;
    flex-shrink: 0;
  }
  .tag-tip-dismiss {
    flex-shrink: 0;
    width: 1.4rem;
    height: 1.4rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: 0;
    padding: 0;
    background: transparent;
    color: var(--bone-3);
    font-size: 1.1rem;
    line-height: 1;
    border-radius: 4px;
    cursor: pointer;
    transition: color 0.15s, background 0.15s;
  }
  .tag-tip-dismiss:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }

  .date-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem 0.8rem;
    margin-bottom: 1.4rem;
  }
  .date-label {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--bone-3);
    font-size: 0.78rem;
  }
  .date-label-text {
    letter-spacing: 0.02em;
  }
  /* .date-input + .date-input-empty retired in #189 — DateInput
     component owns its own trigger + popover styling. The old
     `.date-input-empty::-webkit-datetime-edit { color: transparent }`
     workaround from #166 is superseded because the native webkit2gtk
     widget is gone. */
  .date-clear {
    appearance: none;
    background: transparent;
    border: none;
    color: var(--accent);
    font-family: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    text-decoration: underline;
    padding: 0;
  }
  .date-clear:hover {
    color: var(--accent-hi);
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

  /* ── States ──────────────────────────────────────────────────────── */
  .state {
    color: var(--bone-3);
    font-size: 0.9rem;
  }
  .state.err {
    color: var(--live);
  }

  .empty {
    padding: 3rem 2rem;
    text-align: center;
    border: 1px dashed var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
  }
  .empty-title {
    font-size: 1.1rem;
    font-weight: 600;
    color: var(--bone-0);
    margin: 0 0 0.35rem;
  }
  .empty-sub {
    color: var(--bone-3);
    margin: 0 0 1.1rem;
    font-size: 0.88rem;
  }
  .empty-cta {
    display: inline-block;
    padding: 0.5rem 1rem;
    border: 1px solid var(--accent);
    border-radius: 8px;
    color: var(--accent);
    font-size: 0.85rem;
    font-weight: 500;
    transition: all 0.15s;
  }
  .empty-cta:hover {
    background: var(--accent);
    color: var(--ink-0);
  }
  .empty-clear {
    appearance: none;
    background: transparent;
    border: none;
    color: var(--accent);
    font-family: inherit;
    font-size: 0.88rem;
    cursor: pointer;
    text-decoration: underline;
    padding: 0;
  }
  .empty-clear:hover {
    color: var(--accent-hi);
  }

  /* #386 — Load-more affordance, sits at the bottom of the day-grouped
     list when the backend returns a `next_cursor`. Mirror of the
     portal page so the two surfaces feel consistent. */
  .load-more-wrap {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.6rem;
    margin-top: 1.2rem;
    padding-top: 0.6rem;
  }
  .cta-btn {
    appearance: none;
    background: var(--accent);
    border: 1px solid var(--accent);
    color: var(--ink-0);
    font-size: 0.9rem;
    font-weight: 600;
    padding: 0.55rem 1.1rem;
    border-radius: 8px;
    cursor: pointer;
    transition: background 0.15s;
  }
  .cta-btn:hover:not(:disabled) {
    background: var(--accent-hi);
  }
  .cta-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .load-more {
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.48rem 1.1rem;
  }

  /* ── Groups ──────────────────────────────────────────────────────── */
  .groups {
    display: flex;
    flex-direction: column;
    gap: 1.8rem;
  }

  .group-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.8rem;
    margin-bottom: 0.5rem;
    padding-bottom: 0.5rem;
    border-bottom: 1px solid var(--hairline);
  }

  .day {
    font-size: 0.88rem;
    font-weight: 600;
    color: var(--bone-0);
    letter-spacing: -0.005em;
  }

  .day-count {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--bone-3);
    letter-spacing: 0.04em;
  }

  .entries {
    list-style: none;
    padding: 0;
    margin: 0;
  }

  .entry {
    display: grid;
    grid-template-columns: 66px 1fr auto;
    align-items: center;
    gap: 1rem;
    padding: 0.7rem 0.6rem;
    border-radius: var(--radius-sm);
    transition: background 0.12s;
  }

  .entry:hover {
    background: var(--ink-1);
  }

  .entry-time {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--bone-3);
    letter-spacing: 0.02em;
  }

  .entry-body {
    min-width: 0;
  }

  .entry-title {
    font-size: 0.95rem;
    font-weight: 500;
    color: var(--bone-0);
    line-height: 1.3;
    margin: 0 0 0.25rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    transition: color 0.15s;
  }

  .entry:hover .entry-title {
    color: var(--accent);
  }
  /* #282 — keyboard-driven highlight (j/k). A subtle accent rail on
     the left + a faint background tint so the active row is obvious
     without overpowering hover. */
  .entry[data-shortcut-row="active"] {
    background: var(--ink-1);
    box-shadow: inset 2px 0 0 var(--accent);
  }
  .entry[data-shortcut-row="active"] .entry-title {
    color: var(--accent);
  }

  /* #634 — unread call indicator. 3px accent rail on the left edge
     (slightly wider than the 2px keyboard-shortcut rail above so the
     two states are distinguishable when both apply) plus the title
     in a heavier weight. Both signals coexist with hover + the
     keyboard rail; CSS specificity is single-class so any override
     stacks predictably. Per ui.md the indicator is purely visual —
     no extra ARIA decoration on the row. The shortcut-active rule
     above wins on the rail when both are set because it's later in
     the cascade; keep that order intact. */
  .entry-unread {
    box-shadow: inset 3px 0 0 var(--accent);
  }
  .entry-unread .entry-title {
    font-weight: 600;
  }
  .entry[data-shortcut-row="active"].entry-unread {
    /* Both rails would compete; widen the inset so the keyboard
       rail's 2px reads as the inner edge of the 3px unread rail. */
    box-shadow: inset 3px 0 0 var(--accent);
  }

  .entry-meta {
    display: flex;
    gap: 0.35rem;
    flex-wrap: wrap;
  }

  /* #412 — `.status-chip` is the small (4px-radius) state pill family
     used on the calls-list row: Processing / Still working / Failed /
     other-status fallback. Renamed from the generic `.chip` to break
     a class-name collision with `calls/[id]/+page.svelte`'s larger
     8px-radius `.chip` (see design.md §Pattern library — chip
     families on the calls-list row). Tracked for app.css promotion
     in #412b. Mirrors the portal calls-list rule. */
  .status-chip {
    display: inline-flex;
    align-items: center;
    padding: 0.1rem 0.5rem;
    font-size: 0.7rem;
    font-weight: 500;
    letter-spacing: 0.01em;
    border-radius: 4px;
    background: var(--ink-3);
    color: var(--bone-1);
    border: 1px solid var(--hairline);
  }

  /* #303 — Import button on placeholder external rows. Same chip
     family but accent-fill so the call to action reads as actionable
     rather than informational. */
  .import-btn {
    display: inline-flex;
    align-items: center;
    padding: 0.15rem 0.6rem;
    font-size: 0.72rem;
    font-weight: 600;
    color: var(--ink-0);
    background: var(--accent);
    border: 1px solid var(--accent);
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    transition: background 120ms, border-color 120ms;
  }
  .import-btn:hover:not(:disabled),
  .import-btn:focus-visible {
    background: var(--accent-hi, #56b8ae);
    border-color: var(--accent-hi, #56b8ae);
    outline: none;
  }
  .import-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* Dismiss companion to .import-btn for placeholder rows. Same chip-
     family geometry (4px radius) as Import so they line up; ghost
     palette so it reads as the secondary action. Mirrors
     candidate-btn-dismiss in app.css for the new #595 candidate flow,
     so the two import surfaces feel like one control family. */
  .dismiss-btn {
    display: inline-flex;
    align-items: center;
    margin-left: 0.35rem;
    padding: 0.15rem 0.6rem;
    font-size: 0.72rem;
    font-weight: 600;
    color: var(--bone-2);
    background: transparent;
    border: 1px solid var(--hairline);
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    transition: background 120ms, border-color 120ms, color 120ms;
  }
  .dismiss-btn:hover:not(:disabled),
  .dismiss-btn:focus-visible {
    color: var(--bone-0);
    background: var(--ink-2);
    border-color: var(--bone-3);
    outline: none;
  }
  .dismiss-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .status-chip-sig {
    border-color: rgba(201, 162, 74, 0.3);
    color: var(--sig);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.64rem;
  }
  /* #482 — Failed-pipeline chip. Live-red tint so it stands out from
     the chip family without competing with the accent-fill Import
     button. Mirror of portal styling. */
  .status-chip.status-chip-failed {
    color: var(--live, #d94a4a);
    background: var(--live-soft, rgba(217, 74, 74, 0.12));
    border: 1px solid var(--live, #d94a4a);
  }
  /* #286 · Post-threshold "Still working" pill. Slightly warmer
     (soft tint of --sig) + bolder weight so it reads as "we
     noticed and we're on it" rather than as an error. Mirrors the
     portal calls-list treatment. */
  .status-chip-still {
    background: rgba(201, 162, 74, 0.14);
    border-color: rgba(201, 162, 74, 0.45);
    color: var(--sig);
    font-weight: 600;
    letter-spacing: 0.02em;
    font-size: 0.7rem;
    padding: 0.1rem 0.5rem;
  }
  .status-chip-pinned {
    color: var(--accent);
    border-color: rgba(58, 155, 146, 0.45);
    background: rgba(58, 155, 146, 0.12);
  }
  .status-chip-snoozed {
    color: var(--bone-1);
    border-color: var(--hairline-hi);
    background: var(--ink-2);
  }
  .row-action {
    display: inline-flex;
    align-items: center;
    min-height: 1.35rem;
    padding: 0.1rem 0.45rem;
    font-size: 0.7rem;
    font-weight: 500;
    color: var(--bone-2);
    background: transparent;
    border: 1px solid var(--hairline);
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
  }
  .row-action:hover:not(:disabled),
  .row-action:focus-visible {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
    outline: none;
  }
  .row-action:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .snooze-pop {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-wrap: wrap;
    padding: 0.25rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: var(--ink-1);
  }
  .snooze-pop :global(.date-input-wrap) {
    width: 8.5rem;
  }

  .entry-dur {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--bone-2);
    letter-spacing: 0.02em;
  }

  /* #595 — Importable filter pills. Anchor row above the existing
     filter bar. Same segmented-control rhythm as the scope-toggle
     (mine / all) so the page reads as having two parallel filter axes.
     Hidden when the user has no candidates and no non-default filter
     selected — see the conditional in the markup. Mirror of the portal
     /calls treatment. */
  .importable-pills {
    display: inline-flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-2);
    margin-bottom: 0.7rem;
    width: max-content;
    max-width: 100%;
    flex-wrap: wrap;
  }
  .importable-pill {
    padding: 0.4rem 0.85rem;
    font-size: 0.78rem;
    font-weight: 500;
    color: var(--bone-3);
    border-radius: 6px;
    border: none;
    background: transparent;
    cursor: pointer;
    white-space: nowrap;
    transition: color 0.15s, background 0.15s;
  }
  .importable-pill:hover {
    color: var(--bone-0);
  }
  .importable-pill.active {
    background: var(--ink-0);
    color: var(--accent);
    box-shadow: inset 0 0 0 1px var(--hairline-hi);
  }
  .list-view-pills {
    display: inline-flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-2);
    margin: 0 0 0.7rem 0.6rem;
    width: max-content;
    max-width: 100%;
    flex-wrap: wrap;
  }
  .list-view-pill {
    padding: 0.4rem 0.8rem;
    font-size: 0.78rem;
    font-weight: 500;
    color: var(--bone-3);
    border-radius: 6px;
    border: none;
    background: transparent;
    cursor: pointer;
    white-space: nowrap;
    transition: color 0.15s, background 0.15s;
  }
  .list-view-pill:hover {
    color: var(--bone-0);
  }
  .list-view-pill.active {
    background: var(--ink-0);
    color: var(--accent);
    box-shadow: inset 0 0 0 1px var(--hairline-hi);
  }

  /* #595 — Candidate row (the import-not-yet-promoted variant of an
     entry). Re-uses the existing .entry layout so rows align with real
     call rows in the same list; the only structural addition is
     `.candidate-actions`, the right-aligned button cluster replacing
     the absent <a> click target. The pip + button styles themselves
     live in app.css (mirrored from the portal) — see design.md
     §"Candidate row (#595)". */
  .candidate-entry {
    cursor: default;
    /* Add a fourth column for the action-button cluster. The layout
       reads "time | body | actions | duration" so the duration stays
       right-aligned with real call rows in the same list. */
    grid-template-columns: 66px 1fr auto auto;
  }
  .candidate-entry:hover {
    background: transparent;
  }
  .candidate-entry .entry-title {
    color: var(--bone-1);
  }
  .candidate-actions {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
  }
  .entry-dur-na {
    color: var(--bone-3);
    font-style: italic;
  }
  @media (max-width: 540px) {
    .candidate-entry {
      grid-template-columns: 1fr;
    }
    .candidate-actions {
      width: 100%;
      justify-content: flex-start;
    }
  }

  /* #595 — Source chip on candidate rows. Mirrors the portal's
     `.source-chip` shape (rounded pill, accent-tinted) and per-source
     variants (Zoho Meeting picks up the gold `--sig` family; SmartPBX
     stays accent). Distinguishes externally-discovered recordings
     from agent-recorded ones at a glance. */
  .source-chip {
    display: inline-flex;
    align-items: center;
    padding: 0.1rem 0.5rem;
    font-size: 0.72rem;
    color: var(--accent);
    background: var(--accent-soft, rgba(58, 155, 146, 0.12));
    border: 1px solid var(--accent);
    border-radius: 999px;
    font-weight: 500;
  }
  .source-chip.source-zoho_meeting {
    color: var(--sig, #c9a24a);
    background: rgba(201, 162, 74, 0.12);
    border-color: rgba(201, 162, 74, 0.55);
  }
  .source-chip.source-smartpbx {
    color: var(--accent);
    background: var(--accent-soft, rgba(58, 155, 146, 0.12));
    border-color: var(--accent);
  }
</style>
