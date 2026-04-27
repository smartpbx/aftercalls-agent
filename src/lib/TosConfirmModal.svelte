<script lang="ts">
  // Confirmation modal for publish / delete-draft on the
  // /staff/tos page (#86). Reuses the shared `.rn-*` shell from
  // `portal/src/app.css` (release-notes modal, #87) so we don't
  // introduce a third modal skeleton to maintain.
  //
  // Props are intentionally free-form — the caller supplies the
  // title, body content (as snippet), and primary/cancel labels. The
  // component owns the backdrop + Escape dismissal + focus-on-primary
  // behaviour.

  import { onMount, tick } from "svelte";
  import type { Snippet } from "svelte";

  interface Props {
    title: string;
    primaryLabel: string;
    cancelLabel?: string;
    primaryVariant?: "primary" | "danger";
    busy?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
    body: Snippet;
  }

  let {
    title,
    primaryLabel,
    cancelLabel = "Cancel",
    primaryVariant = "primary",
    busy = false,
    onConfirm,
    onCancel,
    body,
  }: Props = $props();

  let modalEl = $state<HTMLDivElement | null>(null);
  const titleId = `tos-confirm-title-${Math.random().toString(36).slice(2, 9)}`;

  onMount(async () => {
    // Focus the primary action so Enter confirms and Space on the
    // button fires the same handler. Matches the regen-modal pattern.
    await tick();
    const primary = modalEl?.querySelector<HTMLButtonElement>(
      ".rn-primary",
    );
    primary?.focus();
  });
</script>

<div
  class="rn-backdrop"
  role="button"
  tabindex="-1"
  onclick={() => {
    if (!busy) onCancel();
  }}
  onkeydown={(e) => {
    if (e.key === "Escape" && !busy) onCancel();
  }}
>
  <div
    class="rn-modal tos-confirm"
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    bind:this={modalEl}
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => {
      if (e.key === "Escape") {
        if (!busy) onCancel();
        return;
      }
      e.stopPropagation();
    }}
    tabindex="-1"
  >
    <div class="rn-head">
      <h2 id={titleId}>{title}</h2>
    </div>
    <div class="rn-body">
      {@render body()}
    </div>
    <div class="rn-actions">
      <button
        type="button"
        class="rn-link"
        onclick={onCancel}
        disabled={busy}
      >
        {cancelLabel}
      </button>
      <button
        type="button"
        class="rn-dismiss rn-primary"
        class:danger={primaryVariant === "danger"}
        onclick={onConfirm}
        disabled={busy}
      >
        {primaryLabel}
      </button>
    </div>
  </div>
</div>

<style>
  /* Scoped variant tweak — `.rn-primary` in app.css is accent-coloured;
     the danger variant swaps to the --live tokens for destructive
     confirms (draft delete). */
  .rn-dismiss.danger {
    background: var(--live);
    border-color: var(--live);
    color: var(--ink-0);
  }
  .rn-dismiss.danger:hover:not(:disabled) {
    background: var(--live-hi, var(--live));
    border-color: var(--live-hi, var(--live));
  }
</style>
