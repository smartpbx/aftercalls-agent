<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";

  type Theme = "dark" | "light" | "system";
  let theme = $state<Theme>("dark");

  type Me = {
    email: string;
    display_name: string;
    role: string;
    org_display_name: string;
  };
  let me = $state<Me | null>(null);
  let signingOut = $state(false);

  async function signOut() {
    signingOut = true;
    try {
      await invoke("logout");
      goto("/login");
    } catch (e) {
      error = String(e);
    } finally {
      signingOut = false;
    }
  }

  function setTheme(t: Theme) {
    theme = t;
    document.documentElement.setAttribute("data-theme", t);
    try {
      localStorage.setItem("aftercalls:theme", t);
    } catch (_) {}
  }

  let error = $state("");

  onMount(async () => {
    // Read the theme that the bootstrap script already applied so the UI
    // starts matching what the page is rendering.
    const applied = document.documentElement.getAttribute("data-theme");
    if (applied === "light" || applied === "system") theme = applied;
    else theme = "dark";

    try {
      me = await invoke<Me | null>("current_user");
    } catch (e) {
      console.warn("current_user failed", e);
    }
  });

  // Org vocab management now lives in the portal — changes affect every
  // teammate's calls. The agent still reads vocab via get_org_vocab at
  // pipeline time; it's just no longer editable from here.
  async function openPortalVocab() {
    try {
      await openUrl("https://app.aftercalls.io/admin/vocab");
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }
</script>

<main class="page reveal">
  <header class="head" style="--i: 0">
    <h1>Settings</h1>
    <p class="sub">
      Transcription hints for your organization. Applied to every call the
      agent processes.
    </p>
  </header>

  {#if me}
    <section class="card" style="--i: 0.5">
      <div class="card-head">
        <div>
          <h2>Account</h2>
          <p class="hint">
            Signed in as <strong>{me.email}</strong> ({me.role}) on
            <strong>{me.org_display_name}</strong>.
          </p>
        </div>
        <button
          type="button"
          class="add"
          disabled={signingOut}
          onclick={signOut}
        >
          {signingOut ? "Signing out…" : "Sign out"}
        </button>
      </div>
    </section>
  {/if}

  <section class="card" style="--i: 1">
    <div class="card-head">
      <div>
        <h2>Appearance</h2>
        <p class="hint">
          System mode follows your OS light/dark preference.
        </p>
      </div>
    </div>
    <div class="theme-row" role="group" aria-label="Theme">
      {#each [
        { v: "dark", label: "Dark" },
        { v: "light", label: "Light" },
        { v: "system", label: "System" },
      ] as opt (opt.v)}
        <button
          type="button"
          class="theme-opt"
          class:active={theme === opt.v}
          aria-pressed={theme === opt.v}
          onclick={() => setTheme(opt.v as Theme)}
        >
          <span class="theme-swatch {opt.v}"></span>
          <span>{opt.label}</span>
        </button>
      {/each}
    </div>
  </section>

  <section class="card" style="--i: 2">
    <div class="card-head">
      <div>
        <h2>Org vocab</h2>
        <p class="hint">
          Spelling corrections and word-boost hints are managed by your
          org's admin in the web portal — changes take effect for every
          teammate's calls.
        </p>
      </div>
      <a
        class="add"
        href="https://app.aftercalls.io/admin/vocab"
        target="_blank"
        rel="noopener"
        onclick={(e) => {
          e.preventDefault();
          openPortalVocab();
        }}
      >
        Open portal ↗
      </a>
    </div>
  </section>

  {#if error}<p class="error" style="--i: 3">{error}</p>{/if}
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

  .theme-row {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 0.5rem;
  }

  .theme-opt {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.7rem 0.9rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-0);
    color: var(--bone-1);
    font-size: 0.9rem;
    font-weight: 500;
    transition:
      border-color 0.15s,
      color 0.15s,
      background 0.15s;
  }

  .theme-opt:hover {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }

  .theme-opt.active {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--bone-0);
  }

  .theme-swatch {
    width: 22px;
    height: 22px;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    flex-shrink: 0;
    position: relative;
    overflow: hidden;
  }
  /* Little previews that stay true to each theme regardless of current mode
   * so the user can see what they're picking. */
  .theme-swatch.dark {
    background: linear-gradient(135deg, #0e0d0c 0%, #24211d 100%);
    border-color: #3a9b92;
  }
  .theme-swatch.light {
    background: linear-gradient(135deg, #faf6ec 0%, #e1d8c3 100%);
    border-color: #237e76;
  }
  .theme-swatch.system {
    background: linear-gradient(135deg, #0e0d0c 0%, #0e0d0c 50%, #faf6ec 50%, #faf6ec 100%);
    border-color: var(--accent);
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
