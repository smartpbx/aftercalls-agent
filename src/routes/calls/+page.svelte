<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";

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
  };

  // Tidy a raw app binary or application-name into something human. Unknown
  // apps fall through unchanged so we don't hide information.
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
      default:
        return "";
    }
  }

  let calls = $state<Call[]>([]);
  let error = $state("");
  let loading = $state(true);
  let query = $state("");

  onMount(async () => {
    try {
      calls = await invoke<Call[]>("list_calls");
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  });

  let filtered = $derived.by(() => {
    if (!query.trim()) return calls;
    const q = query.trim().toLowerCase();
    return calls.filter(
      (c) =>
        (c.title ?? "").toLowerCase().includes(q) ||
        (c.matched_client ?? "").toLowerCase().includes(q),
    );
  });

  // Group by the date's ISO yyyy-mm-dd so day headings scan quickly.
  let groups = $derived.by(() => {
    const map = new Map<string, Call[]>();
    for (const c of filtered) {
      const key = new Date(c.recorded_at).toISOString().slice(0, 10);
      const arr = map.get(key) ?? [];
      arr.push(c);
      map.set(key, arr);
    }
    return [...map.entries()].sort((a, b) => (a[0] < b[0] ? 1 : -1));
  });

  function fmtDay(iso: string) {
    const d = new Date(iso);
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
</script>

<main class="page reveal">
  <header class="head" style="--i: 0">
    <div>
      <h1>Calls</h1>
      <p class="sub">
        {calls.length} {calls.length === 1 ? "call" : "calls"} in your archive
      </p>
    </div>

    <div class="search">
      <span class="search-glyph" aria-hidden="true">
        <svg viewBox="0 0 16 16" width="13" height="13">
          <circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" />
          <path d="M10.5 10.5 L14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
        </svg>
      </span>
      <input
        type="text"
        placeholder="Filter by title or client"
        bind:value={query}
      />
    </div>
  </header>

  {#if loading}
    <p class="state" style="--i: 1">Loading…</p>
  {:else if error}
    <p class="state err" style="--i: 1">{error}</p>
  {:else if calls.length === 0}
    <div class="empty" style="--i: 1">
      <p class="empty-title">No calls yet</p>
      <p class="empty-sub">
        Go to Record to capture your first call.
      </p>
      <a href="/" class="empty-cta">Go to Record →</a>
    </div>
  {:else if filtered.length === 0}
    <p class="state" style="--i: 1">No calls match "{query}".</p>
  {:else}
    <div class="groups">
      {#each groups as [day, items], idx (day)}
        <section class="group" style="--i: {idx + 1}">
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
                      {call.title ?? "(untitled)"}
                    </h3>
                    <div class="entry-meta">
                      {#if call.matched_client}
                        <span class="chip chip-accent">{call.matched_client}</span>
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
    z-index: 2;
  }

  .head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 1.5rem;
    margin-bottom: 1.6rem;
  }

  .head h1 {
    margin-bottom: 0.2rem;
  }

  .sub {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.82rem;
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

  /* ── States ────────────────────────────────────────────────────────── */
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

  /* ── Groups ────────────────────────────────────────────────────────── */
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
    gap: 0.4rem;
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

  .chip-accent {
    border-color: rgba(58, 155, 146, 0.32);
    background: var(--accent-soft);
    color: var(--accent-hi);
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
