<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount, onDestroy } from "svelte";
  import { page } from "$app/state";
  import { goto, replaceState, afterNavigate } from "$app/navigation";
  import DateInput from "$lib/DateInput.svelte";
  import { portalErrorToText } from "$lib/portalError";
  import {
    isProcessing,
    isDelayed,
    PILL_PROCESSING,
    PILL_STILL_WORKING,
  } from "$lib/processing-thresholds";
  import { registerShortcuts } from "$lib/shortcuts.svelte";

  // #57 Tag-aware filter bar.
  //
  // The bar is two parts: a title-only text input and a tag-chip row.
  // Tag filters live on the URL as repeated `?tag=kind:value` params
  // (the backend accepts this shape natively), so refresh + share-link
  // preserve the filter set. We re-fetch whenever the tag set changes;
  // the title input filters the already-fetched rows client-side.
  //
  // TODO(refactor): the Add-filter popover duplicates markup with the
  // call-detail Add-tag flow. When the parallel agent extracts a shared
  // TagChip / TagAutocomplete component, switch to importing that.

  type Tag = { kind: string; value: string };

  type Call = {
    id: string;
    session_id: string;
    recorded_at: string;
    duration_ms: number;
    title: string | null;
    matched_client: string | null;
    status: string;
    source_app: string | null;
    source_kind: string | null;
    /** External-source classification (#303): 'agent' (default),
     *  'zoho_meeting', 'zoho_cliq', 'smartpbx', 'imported_file'.
     *  Older backends that don't emit this field land as undefined;
     *  treat undefined as 'agent'. */
    ingest_source?: string;
    tags?: Tag[];
    // Owner metadata (populated when scope=all). Optional so legacy
    // responses don't break the type.
    user_id?: string;
    user_display_name?: string;
  };

  type TagSuggestion = { kind: string; value: string; count: number };

  type Me = {
    email: string;
    // #96: structured first/last alongside display_name. Optional
    // because older auth.json files may predate the split.
    first_name?: string;
    last_name?: string;
    display_name: string;
    role: string;
    org_display_name: string;
    user_id?: string;
  };

  // #96: OrgMember now carries first_name + last_name alongside the
  // display_name the picker renders. Unused here; kept in the type so
  // TS doesn't drift from the API shape.
  type OrgMember = {
    id: string;
    first_name: string;
    last_name: string;
    display_name: string;
    email: string;
  };

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

  // Admin-only: filter the All-team list to a single member.
  let userFilter = $state<{ id: string; name: string } | null>(null);
  let memberRoster = $state<OrgMember[]>([]);
  let memberRosterLoaded = $state(false);
  let userPopoverOpen = $state(false);
  let userPopoverQuery = $state("");

  async function ensureMemberRoster() {
    if (memberRosterLoaded) return;
    memberRosterLoaded = true;
    try {
      memberRoster = await invoke<OrgMember[]>("org_members");
    } catch {
      memberRoster = [];
    }
  }

  let filteredMembers = $derived.by(() => {
    const q = userPopoverQuery.trim().toLowerCase();
    if (!q) return memberRoster;
    return memberRoster.filter(
      (m) =>
        m.display_name.toLowerCase().includes(q) ||
        m.email.toLowerCase().includes(q),
    );
  });

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
    try {
      calls = await invoke<Call[]>("list_calls", {
        scope,
        user: scope === "all" ? (userFilter?.id ?? null) : null,
        tags: tagFilters,
        // #146 — pack `YYYY-MM-DD` into full RFC3339 on this side so
        // the Tauri command + backend only ever see parsed timestamps.
        fromDate: fromDate ? `${fromDate}T00:00:00Z` : null,
        toDate: toDate ? `${toDate}T23:59:59Z` : null,
      });
    } catch (e) {
      error = portalErrorToText(e);
    } finally {
      loading = false;
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

  async function clearDates() {
    if (dateTimer) {
      clearTimeout(dateTimer);
      dateTimer = null;
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

  async function setUserFilter(m: OrgMember | null) {
    userFilter = m ? { id: m.id, name: m.display_name } : null;
    userPopoverOpen = false;
    userPopoverQuery = "";
    await load();
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
    if (dateTimer) {
      clearTimeout(dateTimer);
      dateTimer = null;
    }
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
  let visibleIds = $derived.by(() => {
    const ids: string[] = [];
    for (const [, items] of groups) for (const c of items) ids.push(c.id);
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
      },
      [
        { keys: "j k", label: "Next / previous call" },
        { keys: "enter o", label: "Open the highlighted call" },
        { keys: "/", label: "Focus the title filter" },
      ],
    );
    await load();
  });

  onDestroy(() => {
    if (nowTimer !== undefined) {
      clearInterval(nowTimer);
      nowTimer = undefined;
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
    void load();
  });

  let filtered = $derived.by(() => {
    if (!query.trim()) return calls;
    const q = query.trim().toLowerCase();
    // Title-only now — client lives in the tag-filter bar above.
    return calls.filter((c) => (c.title ?? "").toLowerCase().includes(q));
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

  let groups = $derived.by(() => {
    const map = new Map<string, Call[]>();
    for (const c of filtered) {
      const key = localDayKey(c.recorded_at);
      const arr = map.get(key) ?? [];
      arr.push(c);
      map.set(key, arr);
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
    if (hydrating[callId]) return;
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
</script>

<main class="page">
  <header class="head">
    <div>
      <h1>Calls</h1>
      <p class="sub">
        {calls.length} {calls.length === 1 ? "call" : "calls"}
        {scope === "all" ? "across the team" : "in your archive"}
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
      <a class="trash-link" href="/calls/trash" title="Recycle bin">Trash</a>
    </div>
  </header>

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
        placeholder="Filter by title"
        bind:value={query}
        bind:this={searchInput}
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
        <div class="add-wrap">
          <button
            type="button"
            class="add-filter"
            onclick={async () => {
              if (userPopoverOpen) {
                userPopoverOpen = false;
              } else {
                await ensureMemberRoster();
                userPopoverQuery = "";
                userPopoverOpen = true;
              }
            }}
            aria-haspopup="dialog"
            aria-expanded={userPopoverOpen}
          >
            + By person
          </button>
          {#if userPopoverOpen}
            <div
              class="pop-backdrop"
              role="button"
              tabindex="-1"
              aria-label="Close person picker"
              onclick={() => (userPopoverOpen = false)}
              onkeydown={(e) => e.key === "Escape" && (userPopoverOpen = false)}
            ></div>
            <div class="pop" role="dialog" aria-label="Filter by person">
              <input
                class="pop-search"
                type="text"
                placeholder="Search team…"
                bind:value={userPopoverQuery}
                use:focusOnMount
                onkeydown={(e) => {
                  if (e.key === "Escape") userPopoverOpen = false;
                  else if (e.key === "Enter" && filteredMembers.length > 0) {
                    e.preventDefault();
                    void setUserFilter(filteredMembers[0]);
                  }
                }}
              />
              <ul class="pop-list">
                {#if filteredMembers.length === 0}
                  <li class="pop-empty">
                    {memberRoster.length === 0 ? "Loading…" : "No matching teammates"}
                  </li>
                {:else}
                  {#each filteredMembers as m (m.id)}
                    <li>
                      <button
                        type="button"
                        class="pop-item"
                        onclick={() => setUserFilter(m)}
                      >
                        <span class="person-name">{m.display_name}</span>
                        <span class="person-email">{m.email}</span>
                      </button>
                    </li>
                  {/each}
                {/if}
              </ul>
            </div>
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
  {:else if calls.length === 0 && tagFilters.length === 0 && !query.trim() && !fromDate && !toDate}
    <div class="empty">
      <p class="empty-title">No calls yet</p>
      <p class="empty-sub">
        Go to Record to capture your first call.
      </p>
      <a href="/" class="empty-cta">Go to Record →</a>
    </div>
  {:else if filtered.length === 0}
    <div class="empty">
      <p class="empty-title">
        {#if (fromDate || toDate) && tagFilters.length === 0 && !userFilter && !query.trim()}
          No calls in the selected range
        {:else}
          No calls match these filters
        {/if}
      </p>
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
              {items.length} {items.length === 1 ? "call" : "calls"}
            </span>
          </div>

          <ul class="entries">
            {#each items as call (call.id)}
              <li>
                <a
                  href="/calls/{call.id}"
                  class="entry"
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
                      {#if prettyApp(call.source_app)}
                        <span class="chip" title={sourceKindLabel(call.source_kind)}>
                          {prettyApp(call.source_app)}
                        </span>
                      {:else if call.ingest_source && call.ingest_source !== "agent"}
                        <!-- #303 — external-source chip drives off
                             ingest_source (the wire-side external
                             classification), NOT source_kind which is
                             the agent-side label. Keeps Zoho Meeting
                             / SmartPBX / etc. recognizable in the
                             list rather than rendering as the
                             generic "Imported" source_kind would. -->
                        <span class="chip">{sourceKindLabel(call.ingest_source)}</span>
                      {:else if call.source_kind}
                        <span class="chip">{sourceKindLabel(call.source_kind)}</span>
                      {/if}
                      {#if call.status === "available"}
                        <!-- #303 — placeholder external recording.
                             Inline Import button hydrates the row on
                             demand. Stops event propagation so the
                             row's <a> doesn't navigate; optimistic
                             local-state flip to 'transcribing' so the
                             UI reacts before the server roundtrip
                             completes. -->
                        <button
                          type="button"
                          class="import-btn"
                          disabled={hydrating[call.id]}
                          onclick={(e) => importPlaceholder(e, call.id)}
                        >
                          {hydrating[call.id] ? "Importing…" : "Import"}
                        </button>
                      {:else if isProcessing(call.status)}
                        {#if isDelayed(call.status, call.recorded_at, nowMs)}
                          <span class="chip chip-still" title="Taking a little longer than usual">
                            {PILL_STILL_WORKING}
                          </span>
                        {:else}
                          <span class="chip chip-sig">{PILL_PROCESSING}</span>
                        {/if}
                      {:else if call.status === "failed"}
                        <!-- #482 — explicit Failed chip rather than the
                             generic chip-sig fallback so users notice on
                             the list. Detail-page retry surface tracked
                             as #488. -->
                        <span class="chip chip-failed" title="Processing failed — open the call to retry">
                          Failed
                        </span>
                      {:else if call.status !== "complete"}
                        <span class="chip chip-sig">{call.status}</span>
                      {/if}
                    </div>
                  </div>
                  <span class="entry-dur">{fmtDuration(call.duration_ms)}</span>
                </a>
              </li>
            {/each}
          </ul>
        </section>
      {/each}
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

  .person-name {
    display: block;
    font-size: 0.9rem;
    color: var(--bone-1);
  }
  .person-email {
    display: block;
    font-size: 0.72rem;
    color: var(--bone-3);
    margin-top: 0.1rem;
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

  .entry-meta {
    display: flex;
    gap: 0.35rem;
    flex-wrap: wrap;
  }

  .chip {
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

  .chip-sig {
    border-color: rgba(201, 162, 74, 0.3);
    color: var(--sig);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.64rem;
  }
  /* #482 — Failed-pipeline chip. Live-red tint so it stands out from
     the chip family without competing with the accent-fill Import
     button. Mirror of portal styling. */
  .chip.chip-failed {
    color: var(--live, #d94a4a);
    background: var(--live-soft, rgba(217, 74, 74, 0.12));
    border: 1px solid var(--live, #d94a4a);
  }
  /* #286 · Post-threshold "Still working" pill. Slightly warmer
     (soft tint of --sig) + bolder weight so it reads as "we
     noticed and we're on it" rather than as an error. Mirrors the
     portal calls-list treatment. */
  .chip-still {
    background: rgba(201, 162, 74, 0.14);
    border-color: rgba(201, 162, 74, 0.45);
    color: var(--sig);
    font-weight: 600;
    letter-spacing: 0.02em;
    font-size: 0.7rem;
    padding: 0.1rem 0.5rem;
  }

  .entry-dur {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--bone-2);
    letter-spacing: 0.02em;
  }
</style>
