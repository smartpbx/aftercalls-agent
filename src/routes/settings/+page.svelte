<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";

  // AssemblyAI shape: [{"from": ["ee we","E-wee"], "to": "Ewee"}]
  // The editor flattens into rows so the form is easy to reason about; we
  // rehydrate the nested shape on save.
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

<main class="page reveal">
  <header class="head" style="--i: 0">
    <h1>Settings</h1>
    <p class="sub">
      Transcription hints for your organization. Applied to every call the
      agent processes.
    </p>
  </header>

  {#if loading}
    <p class="state" style="--i: 1">Loading…</p>
  {:else}
    <section class="card" style="--i: 1">
      <div class="card-head">
        <div>
          <h2>Spelling corrections</h2>
          <p class="hint">
            Rewrites the transcript after AssemblyAI returns. Each row maps
            misheard forms (comma-separated) to the canonical spelling.
            Avoid short common words like "we" or "to" — they will rewrite
            every occurrence in normal speech.
          </p>
        </div>
        <button type="button" class="add" onclick={addRow}>
          + Add row
        </button>
      </div>

      {#if rows.length === 0}
        <p class="empty">No corrections yet.</p>
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
              <button
                type="button"
                class="remove"
                aria-label="Remove row"
                onclick={() => removeRow(idx)}
              >
                <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
                  <path d="M4 4 L12 12 M12 4 L4 12" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
                </svg>
              </button>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <section class="card" style="--i: 2">
      <div class="card-head">
        <div>
          <h2>Word boost</h2>
          <p class="hint">
            Soft nudges during recognition. Good for proper nouns and
            product names — less aggressive than spelling corrections.
          </p>
        </div>
      </div>
      <textarea
        class="boost"
        rows="3"
        placeholder="Ewee, Zoho, Callscribe"
        bind:value={boostText}
      ></textarea>
    </section>

    <div class="actions" style="--i: 3">
      <button type="button" class="save" disabled={saving} onclick={save}>
        {saving ? "Saving…" : "Save changes"}
      </button>
      {#if savedRecently}<span class="saved">Saved</span>{/if}
      {#if error}<span class="error">{error}</span>{/if}
    </div>
  {/if}
</main>

<style>
  .page {
    max-width: 820px;
    margin: 0 auto;
    padding: 2.2rem 2rem 4rem;
    position: relative;
    z-index: 2;
  }

  .head {
    margin-bottom: 1.6rem;
  }

  .sub {
    margin: 0.3rem 0 0;
    color: var(--bone-2);
    font-size: 0.9rem;
    max-width: 54ch;
  }

  .state {
    color: var(--bone-3);
  }

  .card {
    padding: 1.2rem 1.3rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    margin-bottom: 1.2rem;
  }

  .card-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
    margin-bottom: 0.9rem;
  }

  .hint {
    margin: 0.3rem 0 0;
    color: var(--bone-3);
    font-size: 0.8rem;
    line-height: 1.55;
    max-width: 58ch;
  }

  .add {
    flex-shrink: 0;
    padding: 0.38rem 0.85rem;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    transition: all 0.15s;
  }
  .add:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .empty {
    margin: 0;
    padding: 0.8rem 0.2rem;
    color: var(--bone-3);
    font-size: 0.85rem;
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }

  .row {
    display: grid;
    grid-template-columns: 12rem 1fr auto;
    gap: 0.5rem;
    align-items: center;
  }

  .row-head {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--bone-3);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    padding: 0 0.2rem;
    margin-bottom: 0.15rem;
  }

  .input,
  .boost {
    padding: 0.5rem 0.7rem;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: inherit;
    font: inherit;
    font-size: 0.88rem;
    width: 100%;
    box-sizing: border-box;
    transition: border-color 0.15s;
  }

  .input::placeholder,
  .boost::placeholder {
    color: var(--bone-4);
  }

  .input:focus,
  .boost:focus {
    outline: none;
    border-color: var(--accent);
  }

  .boost {
    resize: vertical;
    line-height: 1.55;
  }

  .remove {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--bone-3);
    transition: all 0.15s;
  }
  .remove:hover {
    color: var(--live);
    border-color: var(--live);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 0.8rem;
    padding-top: 0.5rem;
  }

  .save {
    padding: 0.55rem 1.2rem;
    border-radius: 8px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.88rem;
    font-weight: 600;
    transition: all 0.15s;
  }
  .save:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .save:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .saved {
    color: var(--olive);
    font-size: 0.83rem;
    font-weight: 500;
  }

  .error {
    color: var(--live);
    font-size: 0.83rem;
  }
</style>
