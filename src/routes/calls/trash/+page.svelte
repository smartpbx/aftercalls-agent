<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";

  // Mirrors the portal's trash view. Deleted calls live here for
  // 30 days before the backend's nightly purge drops them for good.
  // Users see their own bin; admins could flip to "all team" via
  // a future toggle (portal has this — agent keeps it simple).

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
    deleted_at?: string;
  };

  let rows = $state<Call[]>([]);
  let loading = $state(true);
  let error = $state("");
  let busy = $state<Record<string, boolean>>({});

  const PURGE_DAYS = 30;

  async function load() {
    loading = true;
    error = "";
    try {
      rows = await invoke<Call[]>("list_trashed");
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  onMount(load);

  async function restore(row: Call) {
    busy[row.id] = true;
    try {
      await invoke("restore_call", { id: row.id });
      rows = rows.filter((r) => r.id !== row.id);
    } catch (e) {
      error = String(e);
    } finally {
      delete busy[row.id];
    }
  }

  async function permadelete(row: Call) {
    const ok = window.confirm(
      `Permanently delete "${row.title ?? "(untitled)"}"? The audio is removed from storage and cannot be undone.`,
    );
    if (!ok) return;
    busy[row.id] = true;
    try {
      await invoke("permadelete_call", { id: row.id });
      rows = rows.filter((r) => r.id !== row.id);
    } catch (e) {
      error = String(e);
    } finally {
      delete busy[row.id];
    }
  }

  function daysLeft(iso: string | undefined): string {
    if (!iso) return "";
    const purgeAt = new Date(iso).getTime() + PURGE_DAYS * 86400 * 1000;
    const ms = purgeAt - Date.now();
    if (ms <= 0) return "purging soon";
    const d = Math.ceil(ms / (86400 * 1000));
    return d === 1 ? "1 day left" : `${d} days left`;
  }
  function fmtTs(iso: string) {
    return new Date(iso).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
  }
  function fmtDur(ms: number) {
    const s = Math.round(ms / 1000);
    const m = Math.floor(s / 60);
    const r = s % 60;
    return m === 0 ? `${r}s` : `${m}:${String(r).padStart(2, "0")}`;
  }
</script>

<main class="page">
  <header class="head">
    <div>
      <h1>Trash</h1>
      <p class="sub">
        Deleted calls stay here for {PURGE_DAYS} days before auto-purge. Restore
        or delete forever from this list.
      </p>
    </div>
    <a class="back" href="/calls">← Back to Calls</a>
  </header>

  {#if error}<p class="error">{error}</p>{/if}

  {#if loading}
    <p class="state">Loading…</p>
  {:else if rows.length === 0}
    <div class="empty">
      <p class="empty-title">Trash is empty</p>
      <p class="empty-sub">
        Deleted calls appear here for 30 days before auto-purge.
      </p>
    </div>
  {:else}
    <ul class="list">
      {#each rows as r (r.id)}
        <li class="row">
          <div class="meta">
            <span class="title">{r.title ?? "(untitled)"}</span>
            <span class="sub-meta">
              deleted {fmtTs(r.deleted_at!)} · {fmtDur(r.duration_ms)} ·
              {daysLeft(r.deleted_at)}
            </span>
          </div>
          <div class="actions">
            <button
              class="row-btn"
              disabled={busy[r.id]}
              onclick={() => restore(r)}
            >
              Restore
            </button>
            <button
              class="row-btn row-btn-danger"
              disabled={busy[r.id]}
              onclick={() => permadelete(r)}
            >
              Delete forever
            </button>
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  .page {
    max-width: 720px;
    margin: 0 auto;
    padding: 2rem;
  }
  .head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 1rem;
    margin-bottom: 1.3rem;
  }
  .head h1 {
    margin: 0 0 0.3rem;
  }
  .sub {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.88rem;
    max-width: 58ch;
  }
  .back {
    font-size: 0.88rem;
    color: var(--bone-3);
    text-decoration: none;
  }
  .back:hover {
    color: var(--bone-0);
  }
  .state {
    color: var(--bone-3);
  }
  .error {
    padding: 0.6rem 0.8rem;
    background: var(--live-soft);
    color: var(--live);
    border-radius: var(--radius);
    font-size: 0.85rem;
    margin-bottom: 1rem;
  }
  .empty {
    padding: 2.5rem 2rem;
    text-align: center;
    border: 1px dashed var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
  }
  .empty-title {
    font-size: 1rem;
    color: var(--bone-1);
    margin: 0 0 0.3rem;
    font-weight: 500;
  }
  .empty-sub {
    color: var(--bone-3);
    font-size: 0.85rem;
    margin: 0;
  }
  .list {
    list-style: none;
    padding: 0;
    margin: 0;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
  }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 1rem;
    padding: 0.85rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .row:last-child {
    border-bottom: none;
  }
  .meta {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    min-width: 0;
    flex: 1;
  }
  .title {
    font-size: 0.92rem;
    color: var(--bone-0);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub-meta {
    font-size: 0.78rem;
    color: var(--bone-3);
    font-family: var(--font-mono);
  }
  .actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .row-btn {
    padding: 0.35rem 0.75rem;
    font-size: 0.8rem;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    border-radius: 6px;
    cursor: pointer;
  }
  .row-btn:hover:not(:disabled) {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }
  .row-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .row-btn-danger {
    color: var(--live);
    border-color: rgba(255, 80, 50, 0.3);
  }
  .row-btn-danger:hover:not(:disabled) {
    color: #fff;
    background: var(--live);
    border-color: var(--live);
  }
</style>
