<script lang="ts">
  // #282 — Keyboard-shortcuts help overlay.
  //
  // Shared package component consumed by both portal and agent.
  //
  // Mounted once in the layout. Reads `shortcutsState` from the
  // shared shortcut module and renders a centered modal listing the
  // shortcuts active in every currently-registered context. Closes
  // via Escape, click-outside, or pressing `?` again (the layout's
  // global binding handles the toggle).

  import { shortcutsState, setHelpVisible } from "../shortcuts.svelte";

  function close() {
    setHelpVisible(false);
  }

  // Pretty-print the normalized key id for display. Splits on `+`,
  // capitalises modifier names, swaps `arrowleft` → `←` etc.
  function fmtKeys(raw: string): string[] {
    return raw.split(" ").map((seq) => seq.trim()).filter(Boolean);
  }
  function fmtPart(part: string): string {
    const lower = part.toLowerCase();
    switch (lower) {
      case "shift":
        return "Shift";
      case "ctrl":
        return "Ctrl";
      case "alt":
        return "Alt";
      case "meta":
      case "super":
        return "Super";
      case "arrowleft":
        return "←";
      case "arrowright":
        return "→";
      case "arrowup":
        return "↑";
      case "arrowdown":
        return "↓";
      case "space":
        return "Space";
      case "enter":
        return "Enter";
      case "escape":
        return "Esc";
      case "?":
        return "?";
      default:
        return part.length === 1 ? part.toUpperCase() : part;
    }
  }
</script>

{#if shortcutsState.helpOpen}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="shortcuts-backdrop"
    onclick={close}
    role="presentation"
  ></div>
  <div
    class="shortcuts-modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby="shortcuts-modal-title"
  >
    <header class="shortcuts-head">
      <h2 id="shortcuts-modal-title">Keyboard shortcuts</h2>
      <button
        type="button"
        class="shortcuts-close"
        aria-label="Close"
        onclick={close}
      >×</button>
    </header>
    <div class="shortcuts-body">
      {#each shortcutsState.contexts as group (group.context)}
        {#if group.items.length > 0}
          <section class="shortcuts-group">
            <h3>{group.title}</h3>
            <dl>
              {#each group.items as row (row.keys + ":" + row.label)}
                <div class="shortcuts-row">
                  <dt>
                    {#each fmtKeys(row.keys) as seq, sIdx}
                      {#if sIdx > 0}
                        <span class="shortcuts-or">or</span>
                      {/if}
                      <span class="shortcuts-combo">
                        {#each seq.split("+") as part, pIdx}
                          {#if pIdx > 0}<span class="shortcuts-plus">+</span>{/if}
                          <kbd>{fmtPart(part)}</kbd>
                        {/each}
                      </span>
                    {/each}
                  </dt>
                  <dd>{row.label}</dd>
                </div>
              {/each}
            </dl>
          </section>
        {/if}
      {/each}
    </div>
    <footer class="shortcuts-foot">
      <span>Press <kbd>?</kbd> to toggle this panel · <kbd>Esc</kbd> to close</span>
    </footer>
  </div>
{/if}

<style>
  /* Component-scoped only — no entries leak into app.css (mirror-pair
     invariant). Modal sits above everything else; backdrop is a soft
     dim with a slight blur to keep the focused panel obvious. */
  .shortcuts-backdrop {
    position: fixed;
    inset: 0;
    z-index: 70;
    background: rgba(0, 0, 0, 0.45);
    backdrop-filter: blur(2px);
    -webkit-backdrop-filter: blur(2px);
  }
  .shortcuts-modal {
    position: fixed;
    z-index: 71;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(560px, calc(100vw - 2rem));
    max-height: calc(100vh - 4rem);
    display: flex;
    flex-direction: column;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-lg, 14px);
    box-shadow: 0 20px 50px -10px rgba(0, 0, 0, 0.6);
    color: var(--bone-0);
  }
  .shortcuts-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 1rem 1.2rem;
    border-bottom: 1px solid var(--hairline);
  }
  .shortcuts-head h2 {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
    letter-spacing: -0.005em;
  }
  .shortcuts-close {
    appearance: none;
    background: transparent;
    border: none;
    color: var(--bone-3);
    font-size: 1.4rem;
    line-height: 1;
    cursor: pointer;
    padding: 0 0.4rem;
    border-radius: 6px;
  }
  .shortcuts-close:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
  .shortcuts-body {
    padding: 1rem 1.2rem;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 1.4rem;
  }
  .shortcuts-group h3 {
    margin: 0 0 0.6rem;
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--bone-3);
    font-weight: 600;
  }
  .shortcuts-group dl {
    margin: 0;
    display: grid;
    grid-template-columns: minmax(0, auto) 1fr;
    gap: 0.45rem 1rem;
    align-items: baseline;
  }
  .shortcuts-row {
    display: contents;
  }
  .shortcuts-row dt {
    margin: 0;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-wrap: wrap;
  }
  .shortcuts-row dd {
    margin: 0;
    color: var(--bone-1);
    font-size: 0.88rem;
    line-height: 1.4;
  }
  .shortcuts-combo {
    display: inline-flex;
    align-items: center;
    gap: 0.15rem;
  }
  .shortcuts-plus {
    color: var(--bone-3);
    font-size: 0.78rem;
    margin: 0 0.1rem;
  }
  .shortcuts-or {
    color: var(--bone-3);
    font-size: 0.74rem;
    margin: 0 0.15rem;
    font-style: italic;
  }
  .shortcuts-modal kbd {
    font-family: var(--font-mono);
    font-size: 0.74rem;
    padding: 0.1rem 0.4rem;
    border: 1px solid var(--hairline-hi);
    border-bottom-width: 2px;
    border-radius: 4px;
    background: var(--ink-2);
    color: var(--bone-0);
    min-width: 1.5rem;
    text-align: center;
    display: inline-block;
  }
  .shortcuts-foot {
    padding: 0.7rem 1.2rem;
    border-top: 1px solid var(--hairline);
    color: var(--bone-3);
    font-size: 0.78rem;
    text-align: center;
  }
  .shortcuts-foot kbd {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    padding: 0.05rem 0.35rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 4px;
    background: var(--ink-2);
    color: var(--bone-1);
  }

  @media (max-width: 640px) {
    .shortcuts-modal {
      width: calc(100vw - 1rem);
      max-height: calc(100vh - 2rem);
    }
    .shortcuts-body {
      padding: 0.9rem 1rem;
    }
    .shortcuts-head {
      padding: 0.85rem 1rem;
    }
    .shortcuts-row dd {
      font-size: 0.92rem;
    }
  }
</style>
