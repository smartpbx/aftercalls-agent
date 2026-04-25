<script lang="ts">
  // Toast host (#119). Mounted once per surface in `+layout.svelte`.
  // Renders the visible slice of `toast.items` in the bottom-right
  // corner.
  //
  // Stack order: `flex-direction: column-reverse` puts the most-
  // recently pushed toast at the visual top of the bottom-right
  // stack. Older toasts fall toward the screen edge and dismiss
  // first as their timers fire. Cap of MAX_VISIBLE (3) is enforced
  // here — extras stay queued in the store and surface as the visible
  // set drains.
  //
  // Keyboard: a window-level Escape listener dismisses the topmost
  // visible toast. We don't trap focus — toasts are non-modal — and
  // we don't focus the toast on mount (would steal focus from the
  // user's current task, which violates the non-blocking contract).
  //
  // Mirrored byte-identical at `agent/src/lib/ToastHost.svelte`.
  import { onDestroy, onMount } from "svelte";
  import Toast from "$lib/Toast.svelte";
  import { toast } from "$lib/stores/toast.svelte";
  import { MAX_VISIBLE } from "$lib/stores/toast.helpers";

  // Visible slice — newest last in the array, but `column-reverse`
  // flips it visually so the newest sits on top.
  let visible = $derived(toast.items.slice(0, MAX_VISIBLE));

  function onKeyDown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    if (toast.items.length === 0) return;
    toast.dismissTop();
  }

  onMount(() => {
    if (typeof window === "undefined") return;
    window.addEventListener("keydown", onKeyDown);
  });
  onDestroy(() => {
    if (typeof window === "undefined") return;
    window.removeEventListener("keydown", onKeyDown);
  });
</script>

{#if visible.length > 0}
  <div class="toast-host" aria-label="Notifications">
    {#each visible as item (item.id)}
      <Toast {item} />
    {/each}
  </div>
{/if}
