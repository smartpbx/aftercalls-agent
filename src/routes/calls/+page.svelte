<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount, onDestroy } from "svelte";
  import { page } from "$app/state";
  import { replaceState, afterNavigate } from "$app/navigation";

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
      default:
        return "";
    }
  }

  let calls = $state<Call[]>([]);
  let error = $state("");
  let loading = $state(true);
  let query = $state("");
  let me = $state<Me | null>(null);
  let scope = $state<"mine" | "all">("mine");

  let canSeeAll = $derived(
    !!me && (me.role === "admin" || me.role === "superadmin"),
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
      error = String(e);
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

  // #142 · v0.4.5 — note-to-self button state. `recordingActive`
  // tracks whether ANY recording (regular or self-note) is in flight;
  // used to disable the ghost button with a title explaining why.
  // Seeded on mount via is_recording + kept current via the
  // `recording-state` event the backend emits on every transition.
  let recordingActive = $state(false);
  let noteStartError = $state("");
  let noteErrorTimer: ReturnType<typeof setTimeout> | null = null;

  async function startNoteToSelf() {
    if (recordingActive) return;
    noteStartError = "";
    try {
      await invoke<string>("start_self_note");
    } catch (e) {
      noteStartError = typeof e === "string" ? e : String(e);
      if (noteErrorTimer) clearTimeout(noteErrorTimer);
      noteErrorTimer = setTimeout(() => (noteStartError = ""), 4000);
    }
  }

  let stopStateListen: (() => void) | null = null;
  onMount(async () => {
    try {
      me = await invoke<Me | null>("current_user");
    } catch {}
    tagFilters = readTagsFromUrl();
    fromDate = readDateFromUrl("from");
    toDate = readDateFromUrl("to");
    if (scope === "all" && canSeeAll) void ensureMemberRoster();
    try {
      const status = await invoke<{ recording: boolean }>("is_recording");
      recordingActive = !!status?.recording;
    } catch {}
    try {
      stopStateListen = await listen<{ recording: boolean }>(
        "recording-state",
        (ev) => {
          recordingActive = !!ev.payload?.recording;
        },
      );
    } catch {}
    await load();
  });

  onDestroy(() => {
    if (stopStateListen) stopStateListen();
    if (noteErrorTimer) clearTimeout(noteErrorTimer);
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
      <button
        type="button"
        class="note-self-btn"
        onclick={startNoteToSelf}
        disabled={recordingActive}
        title={recordingActive
          ? "Recording already in progress"
          : "Record a short dictation that goes through the regular pipeline (Super+Shift+N)"}
        aria-label="Note to self"
      >
        <svg
          class="note-self-glyph"
          viewBox="0 0 16 16"
          width="13"
          height="13"
          aria-hidden="true"
        >
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
        <span>Note to self</span>
      </button>
      <a class="trash-link" href="/calls/trash" title="Recycle bin">Trash</a>
    </div>
    {#if noteStartError}
      <p class="note-start-err" role="alert">{noteStartError}</p>
    {/if}
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
                <li class="pop-empty">Loading…</li>
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

  <!-- #146 · Date range. Sits in its own row so the filter-bar above
       stays legible at narrow agent widths. Both inputs are native
       `<input type="date">` so the OS datepicker covers mobile + desk
       without a custom widget. 250ms debounce on change keeps
       keystroke-per-digit browsers from firing three loads. -->
  <div class="date-bar">
    <label class="date-label">
      <span class="date-label-text">From</span>
      <input
        class="date-input"
        class:date-input-empty={!fromDate}
        type="date"
        bind:value={fromDate}
        onchange={onDateChange}
        aria-describedby="date-hint"
      />
    </label>
    <label class="date-label">
      <span class="date-label-text">To</span>
      <input
        class="date-input"
        class:date-input-empty={!toDate}
        type="date"
        bind:value={toDate}
        onchange={onDateChange}
        aria-describedby="date-hint"
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
                <a href="/calls/{call.id}" class="entry">
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
                      {:else if call.source_kind}
                        <span class="chip">{sourceKindLabel(call.source_kind)}</span>
                      {/if}
                      {#if call.status !== "complete"}
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

  /* #142 · v0.4.5 — note-to-self entry point. Ghost button; disabled
     while any recording is in flight. Sits in the head-actions cluster
     just before the Trash link so the record + archive surface share
     the same row. */
  .note-self-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.42rem 0.75rem;
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.82rem;
    font-weight: 500;
    border-radius: 8px;
    cursor: pointer;
    transition:
      background 150ms linear,
      border-color 150ms linear,
      color 150ms linear;
    align-self: center;
  }
  .note-self-btn:hover {
    border-color: var(--hairline-hi);
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .note-self-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .note-self-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .note-self-glyph {
    color: var(--bone-2);
  }
  .note-self-btn:hover:not(:disabled) .note-self-glyph {
    color: var(--accent);
  }

  .note-start-err {
    margin: 0.5rem 0 0;
    padding: 0.45rem 0.7rem;
    border: 1px solid var(--live);
    background: var(--live-soft);
    color: var(--live);
    border-radius: 6px;
    font-size: 0.82rem;
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

  /* ── #146 · Date range bar ───────────────────────────────────────── */
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
  .date-input {
    padding: 0.35rem 0.55rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: var(--ink-1);
    color: var(--bone-0);
    font-family: inherit;
    font-size: 0.8rem;
    /* Tell the native datepicker chrome to match our dark palette. */
    color-scheme: dark;
  }
  .date-input:focus {
    outline: none;
    border-color: var(--accent);
  }
  /* webkit2gtk paints today's date as the placeholder label when the
     value is empty — makes users think a filter is already active.
     Hide the auto-filled edit until the user actually picks a date.
     The class drops off as soon as fromDate/toDate is set and the
     native rendering returns. */
  .date-input-empty::-webkit-datetime-edit {
    color: transparent;
  }
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

  .chip-sig {
    border-color: rgba(201, 162, 74, 0.3);
    color: var(--sig);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.64rem;
  }

  .entry-dur {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--bone-2);
    letter-spacing: 0.02em;
  }
</style>
