<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";

  // AssemblyAI shape: [{"from": ["ee we","E-wee"], "to": "Ewee"}]
  // The editor normalizes the nested-array shape into flat rows so the form
  // is easy to reason about; we rehydrate on save.
  type SpellRow = { to: string; from: string };

  type Vocab = {
    custom_spelling: { from: string[]; to: string }[];
    word_boost: string[];
  };

  let rows = $state<SpellRow[]>([]);
  let boostText = $state("");
  let loading = $state(true);
  let saving = $state(false);
  let error = $state("");
  let savedAt = $state(0);

  onMount(async () => {
    try {
      const v = await invoke<Vocab>("get_org_vocab");
      rows = (v.custom_spelling ?? []).map((e) => ({
        to: e.to,
        from: (e.from ?? []).join(", "),
      }));
      boostText = (v.word_boost ?? []).join(", ");
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  });

  function addRow() {
    rows = [...rows, { to: "", from: "" }];
  }

  function removeRow(idx: number) {
    rows = rows.filter((_, i) => i !== idx);
  }

  function parseList(s: string): string[] {
    return s
      .split(/[,\n]/)
      .map((x) => x.trim())
      .filter(Boolean);
  }

  async function save() {
    error = "";
    saving = true;
    try {
      const spelling = rows
        .map((r) => ({ to: r.to.trim(), from: parseList(r.from) }))
        .filter((e) => e.to && e.from.length > 0);
      const boost = parseList(boostText);
      await invoke("set_org_vocab", {
        customSpelling: spelling,
        wordBoost: boost,
      });
      savedAt = Date.now();
      // Re-normalize what we just saved so the UI matches server truth.
      rows = spelling.map((e) => ({ to: e.to, from: e.from.join(", ") }));
      boostText = boost.join(", ");
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  let savedRecently = $derived(savedAt > 0 && Date.now() - savedAt < 3000);
</script>

<main class="container">
  <h1>Org settings</h1>

  {#if loading}
    <p class="muted">Loading…</p>
  {:else}
    <section>
      <header class="section-head">
        <div>
          <h2>Spelling corrections</h2>
          <p class="hint">
            Rewrites the transcript after AssemblyAI returns. Each row maps
            misheard forms (comma-separated) to the canonical spelling.
            Avoid short common words like "we" or "to" — they will rewrite
            every occurrence in normal speech.
          </p>
        </div>
        <button type="button" class="add" onclick={addRow}>+ Add row</button>
      </header>

      {#if rows.length === 0}
        <p class="muted empty">No corrections yet.</p>
      {:else}
        <div class="rows">
          <div class="row row-head">
            <span>Canonical</span>
            <span>Misheard forms (comma-separated)</span>
            <span></span>
          </div>
          {#each rows as row, idx (idx)}
            <div class="row">
              <input
                class="input"
                placeholder="Ewee"
                bind:value={row.to}
              />
              <input
                class="input"
                placeholder="ee we, E-wee"
                bind:value={row.from}
              />
              <button type="button" class="remove" onclick={() => removeRow(idx)}>
                ✕
              </button>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <section>
      <header class="section-head">
        <div>
          <h2>Word boost</h2>
          <p class="hint">
            Soft nudges during recognition. Good for proper nouns and
            product names — less aggressive than spelling corrections.
          </p>
        </div>
      </header>
      <textarea
        class="boost"
        rows="3"
        placeholder="Ewee, Zoho, Callscribe"
        bind:value={boostText}
      ></textarea>
    </section>

    <div class="actions">
      <button type="button" class="save" disabled={saving} onclick={save}>
        {saving ? "Saving…" : "Save"}
      </button>
      {#if savedRecently}<span class="saved">Saved</span>{/if}
      {#if error}<span class="error">{error}</span>{/if}
    </div>
  {/if}
</main>

<style>
  .container {
    max-width: 820px;
    margin: 0 auto;
    padding: 1.5rem 1.5rem 4rem;
  }

  h1 {
    margin: 0 0 1.5rem;
    font-weight: 600;
    letter-spacing: -0.02em;
    font-size: 1.5rem;
  }

  section {
    margin-bottom: 2rem;
  }

  .section-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
    margin-bottom: 0.9rem;
  }

  h2 {
    margin: 0 0 0.35rem;
    font-weight: 500;
    font-size: 1rem;
    color: #c0c0c0;
  }

  .hint {
    margin: 0;
    color: #808080;
    font-size: 0.82rem;
    line-height: 1.5;
    max-width: 58ch;
  }

  .add {
    flex-shrink: 0;
    padding: 0.4rem 0.9rem;
    font-size: 0.8rem;
    border-radius: 999px;
    border: 1px solid #3a3a3a;
    background-color: #252525;
    color: #c0c0c0;
    cursor: pointer;
  }
  .add:hover {
    border-color: #24c8db;
    color: #f6f6f6;
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }

  .row {
    display: grid;
    grid-template-columns: 12rem 1fr auto;
    gap: 0.5rem;
    align-items: center;
  }

  .row-head {
    font-size: 0.75rem;
    color: #808080;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 0 0.2rem;
  }

  .input,
  .boost {
    padding: 0.5rem 0.7rem;
    border-radius: 8px;
    border: 1px solid #3a3a3a;
    background-color: #1c1c1c;
    color: inherit;
    font: inherit;
    font-size: 0.9rem;
    width: 100%;
    box-sizing: border-box;
  }

  .input:focus,
  .boost:focus {
    outline: none;
    border-color: #24c8db;
  }

  .boost {
    resize: vertical;
    font-family: inherit;
    line-height: 1.5;
  }

  .remove {
    padding: 0.35rem 0.6rem;
    font-size: 0.85rem;
    border-radius: 6px;
    border: 1px solid #3a3a3a;
    background-color: transparent;
    color: #a0a0a0;
    cursor: pointer;
  }
  .remove:hover {
    color: #ff6b6b;
    border-color: #552525;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 0.8rem;
    padding-top: 0.5rem;
  }

  .save {
    padding: 0.55rem 1.4rem;
    border-radius: 999px;
    border: 1px solid #24c8db;
    background-color: #24c8db22;
    color: #f6f6f6;
    font-size: 0.9rem;
    font-weight: 500;
    cursor: pointer;
  }
  .save:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .saved {
    color: #7abf7a;
    font-size: 0.85rem;
  }

  .error {
    color: #ff6b6b;
    font-size: 0.85rem;
  }

  .muted {
    color: #a0a0a0;
  }

  .empty {
    margin: 0;
    padding: 0.6rem 0.2rem;
  }
</style>
