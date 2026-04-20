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
  };

  let calls = $state<Call[]>([]);
  let error = $state("");
  let loading = $state(true);

  onMount(async () => {
    try {
      calls = await invoke<Call[]>("list_calls");
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  });

  function fmtDate(iso: string) {
    const d = new Date(iso);
    return d.toLocaleString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function fmtDuration(ms: number) {
    const s = Math.round(ms / 1000);
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}:${String(r).padStart(2, "0")}`;
  }
</script>

<main class="container">
  <h1>Calls</h1>

  {#if loading}
    <p class="muted">Loading…</p>
  {:else if error}
    <p class="error">{error}</p>
  {:else if calls.length === 0}
    <p class="muted">No calls yet. Record one from the Record tab.</p>
  {:else}
    <ul class="call-list">
      {#each calls as call (call.id)}
        <li>
          <a href="/calls/{call.id}">
            <div class="row">
              <span class="title">{call.title ?? "(untitled)"}</span>
              <span class="duration">{fmtDuration(call.duration_ms)}</span>
            </div>
            <div class="meta">
              <span>{fmtDate(call.recorded_at)}</span>
              {#if call.matched_client}
                <span class="client">{call.matched_client}</span>
              {/if}
              {#if call.status !== "complete"}
                <span class="status">{call.status}</span>
              {/if}
            </div>
          </a>
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  .container {
    max-width: 900px;
    margin: 0 auto;
    padding: 2rem 1.5rem 4rem;
  }

  h1 {
    margin: 0 0 1.5rem;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .muted {
    color: #a0a0a0;
  }

  .error {
    color: #ff6b6b;
  }

  .call-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .call-list a {
    display: block;
    padding: 0.9rem 1.1rem;
    border: 1px solid #2a2a2a;
    border-radius: 10px;
    background-color: #1e1e1e;
    color: inherit;
    text-decoration: none;
    transition: border-color 0.15s, background-color 0.15s;
  }

  .call-list a:hover {
    border-color: #24c8db;
    background-color: #222;
  }

  .row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 1rem;
  }

  .title {
    font-weight: 500;
    font-size: 1rem;
  }

  .duration {
    font-family: ui-monospace, Menlo, monospace;
    color: #a0a0a0;
    font-size: 0.85rem;
  }

  .meta {
    display: flex;
    gap: 0.75rem;
    margin-top: 0.3rem;
    font-size: 0.8rem;
    color: #808080;
  }

  .client {
    color: #24c8db;
  }

  .status {
    color: #e0b050;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
</style>
