<script lang="ts" module>
  // #11 — Highlight-to-correct replace modal. Shared package
  // component consumed by both portal and agent.
  //
  // Lifecycle: opened by the parent when the user clicks the
  // "Replace…" affordance on a `HighlightCorrectMenu` popover. The
  // selection text + an anchor describing where the click landed are
  // passed in as props; the user picks scope, types the replacement,
  // submits → parent's `onsubmit` POSTs to the backend.
  //
  // Surface-agnostic: the parent injects the API call via `onsubmit`
  // so the same component renders identically in the SvelteKit
  // portal (fetch via `api.calls.textReplace`) and the Tauri agent
  // (Tauri `invoke('text_replace', …)`). Same pattern as
  // ShareCallModal / SendToZohoModal.

  export type ReplaceScope = "this_occurrence_only" | "everywhere_in_call";

  /** Identifies which row + occurrence the highlight landed on. The
   *  backend mirrors this shape — see `text_replace::Anchor`. */
  export type ReplaceAnchor = {
    region: "transcript" | "summary" | "action_item";
    utterance_idx?: number;
    action_item_id?: string;
    occurrence_index?: number;
  };

  export type ReplaceResult = {
    replaced: number;
    regions: {
      transcript: number;
      summary: number;
      action_items: number;
    };
  };
</script>

<script lang="ts">
  import { onMount, tick } from "svelte";

  type Props = {
    /** The text the user highlighted — prefilled into the find input.
     *  Read-only on the form (the issue's "Replace 'X' with [input]"
     *  shape — X is the selection, the user only types the
     *  replacement). */
    selection: string;
    /** Where the selection landed; carried verbatim into the
     *  `this_occurrence_only` request. The `everywhere_in_call`
     *  branch ignores anchor on the backend, but we still need it
     *  in props so the same modal handles both scopes. */
    anchor: ReplaceAnchor;
    /** Submit handler — parent does the actual POST + maps the
     *  response. Resolves with the per-region counts so the toast
     *  reads "Replaced N times". Throws on failure; the modal
     *  surfaces the error message inline. */
    onsubmit: (args: {
      find: string;
      replace: string;
      scope: ReplaceScope;
      anchor: ReplaceAnchor;
    }) => Promise<ReplaceResult>;
    onclose: () => void;
  };

  let { selection, anchor, onsubmit, onclose }: Props = $props();

  // Scope default: "everywhere in this call" matches the issue's
  // expected primary use-case (a transcription error usually repeats
  // across the call). The user can downgrade to a single-row swap
  // for surgical edits.
  let scope = $state<ReplaceScope>("everywhere_in_call");
  let replaceText = $state("");
  let submitting = $state(false);
  let errorText = $state("");

  let modalEl = $state<HTMLDivElement | null>(null);
  let replaceInput = $state<HTMLInputElement | null>(null);

  const titleId = `replace-modal-title-${Math.random().toString(36).slice(2, 9)}`;
  const findId = `replace-modal-find-${Math.random().toString(36).slice(2, 9)}`;
  const replaceId = `replace-modal-replace-${Math.random().toString(36).slice(2, 9)}`;

  onMount(async () => {
    await tick();
    // Focus the replace input — `find` is already filled from the
    // selection, so the user's first keystroke types the new text.
    replaceInput?.focus();
  });

  async function submit() {
    if (submitting) return;
    if (!selection.trim()) {
      errorText = "No selection to replace.";
      return;
    }
    errorText = "";
    submitting = true;
    try {
      const result = await onsubmit({
        find: selection,
        // Empty replace is intentional — backend treats it as "delete
        // the matched span", which is the documented edge case from
        // the issue body. No client-side guard.
        replace: replaceText,
        scope,
        anchor,
      });
      // Parent owns the toast; we just close. Returning the result
      // keeps the API symmetric with the other modal patterns.
      onclose();
      return result;
    } catch (e) {
      errorText =
        e instanceof Error
          ? e.message || "Couldn't replace. Try again."
          : "Couldn't replace. Try again.";
    } finally {
      submitting = false;
    }
  }

  function onBackdropClick() {
    if (!submitting) onclose();
  }

  function onModalKeydown(e: KeyboardEvent) {
    if (submitting) return;
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
      return;
    }
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      // Ctrl/Cmd-Enter submits; Enter alone in a text input is
      // ambiguous (radios, etc.) so we require the modifier.
      e.preventDefault();
      submit();
    }
  }
</script>

<div
  class="rn-backdrop"
  role="button"
  tabindex="-1"
  onclick={onBackdropClick}
  onkeydown={(e) => {
    if (e.key === "Escape" && !submitting) onclose();
  }}
>
  <div
    class="rn-modal replace-modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    bind:this={modalEl}
    onclick={(e) => e.stopPropagation()}
    onkeydown={onModalKeydown}
    tabindex="-1"
  >
    <div class="rn-head">
      <h2 id={titleId}>Replace text</h2>
      <p class="head-sub">Fix a transcription error in this call.</p>
    </div>
    <div class="rn-body">
      <div class="field">
        <label for={findId}>Replace</label>
        <input
          id={findId}
          type="text"
          class="find-input"
          value={selection}
          readonly
          aria-readonly="true"
        />
      </div>

      <div class="field">
        <label for={replaceId}>With</label>
        <input
          id={replaceId}
          type="text"
          class="replace-input"
          bind:value={replaceText}
          bind:this={replaceInput}
          placeholder="Leave empty to delete the matched text"
          disabled={submitting}
        />
      </div>

      <fieldset class="scope">
        <legend>Scope</legend>
        <label class="scope-opt">
          <input
            type="radio"
            name="replace-scope"
            value="everywhere_in_call"
            bind:group={scope}
            disabled={submitting}
          />
          <span class="scope-name">Everywhere in this call</span>
          <span class="scope-hint">Transcript, summary, and action items.</span>
        </label>
        <label class="scope-opt">
          <input
            type="radio"
            name="replace-scope"
            value="this_occurrence_only"
            bind:group={scope}
            disabled={submitting}
          />
          <span class="scope-name">Just this occurrence</span>
          <span class="scope-hint">Only the highlighted match.</span>
        </label>
      </fieldset>

      {#if errorText}
        <p class="error" role="alert">{errorText}</p>
      {/if}
    </div>
    <div class="rn-actions">
      <button
        type="button"
        class="rn-dismiss"
        onclick={onclose}
        disabled={submitting}
      >
        Cancel
      </button>
      <button
        type="button"
        class="cta-btn replace-submit"
        onclick={submit}
        disabled={submitting}
      >
        {submitting ? "Replacing…" : "Replace"}
      </button>
    </div>
  </div>
</div>

<style>
  /* Component-local styling — no app.css edits, mirror pair invariant
     intact. Uses the shared `.rn-*` shell from app.css (same as
     ShareCallModal / TosConfirmModal / ImpersonationStartModal). */

  .replace-modal {
    max-width: 520px;
  }

  .head-sub {
    margin: 4px 0 0;
    color: var(--bone-2);
    font-size: 13px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 14px;
  }

  .field label {
    color: var(--bone-2);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .find-input,
  .replace-input {
    width: 100%;
    padding: 8px 10px;
    background: var(--ink-1);
    border: 1px solid var(--hairline);
    border-radius: 6px;
    color: var(--bone-0);
    font-family: inherit;
    font-size: 14px;
  }

  .find-input {
    background: var(--ink-0);
    color: var(--bone-1);
    cursor: default;
  }

  .replace-input:focus {
    outline: none;
    border-color: var(--accent);
  }

  .scope {
    border: 1px solid var(--hairline);
    border-radius: 6px;
    padding: 10px 12px;
    margin: 0 0 8px;
  }

  .scope legend {
    color: var(--bone-2);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    padding: 0 6px;
  }

  .scope-opt {
    display: grid;
    grid-template-columns: auto 1fr;
    grid-template-rows: auto auto;
    column-gap: 8px;
    align-items: center;
    padding: 6px 0;
    cursor: pointer;
  }

  .scope-opt input[type="radio"] {
    grid-row: 1 / 3;
    accent-color: var(--accent);
  }

  .scope-name {
    color: var(--bone-0);
    font-size: 14px;
  }

  .scope-hint {
    grid-column: 2;
    color: var(--bone-2);
    font-size: 12px;
  }

  .error {
    margin: 6px 0 0;
    color: var(--live);
    font-size: 13px;
  }
</style>
