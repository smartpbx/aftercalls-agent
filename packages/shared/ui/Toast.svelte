<script lang="ts">
  // Single toast item (#119). Renders one entry from the toast queue.
  // The host component (`ToastHost.svelte`) maps over `toast.items` and
  // mounts one of these per visible id.
  //
  // ARIA contract:
  //   - info / success / warning  → role="status"  + aria-live="polite"
  //   - error                     → role="alert"   + aria-live="assertive"
  // The error variant grabs the SR's attention; non-error toasts wait
  // their turn so a stream of saves doesn't drown out an in-flight read.
  //
  // Hover-pause: mouseenter / focusin call `toast.pause(id)`; the
  // matching leave/blur calls `toast.resume(id)`. This is the standard
  // toast behaviour — gives the user time to read or click an action
  // when they engage with the toast.
  //
  // Styles are all in `app.css` under `.toast-*` selectors so the
  // mirror invariant (portal/agent app.css byte-identical) is the
  // single place these visuals live. Component is markup-only.
  import { toast } from "../stores/toast.svelte";
  import type { ToastItem } from "../stores/toast.helpers";

  type Props = {
    item: ToastItem;
  };

  let { item }: Props = $props();

  // role + aria-live derived from variant. Keep them in lockstep:
  // role="alert" implies assertive; role="status" implies polite.
  let role = $derived<"alert" | "status">(
    item.variant === "error" ? "alert" : "status",
  );
  let ariaLive = $derived<"assertive" | "polite">(
    item.variant === "error" ? "assertive" : "polite",
  );

  // Variant glyph — small visual + SR cue. Mono so it tracks the
  // technical-readout family (timestamps, version pills); doesn't
  // compete with the Geist sans body text.
  let glyph = $derived.by(() => {
    switch (item.variant) {
      case "success":
        return "✓";
      case "warning":
        return "!";
      case "error":
        return "×";
      default:
        return "i";
    }
  });

  function onAction() {
    if (!item.action) return;
    try {
      item.action.onClick();
    } finally {
      // Action click dismisses the toast — the action either resolves
      // the situation (Undo / Retry) or the user explicitly chose it,
      // so leaving the toast on screen would be noise.
      toast.dismiss(item.id);
    }
  }
</script>

<!-- The wrapper is the live region. Pause on hover/focus per the
     standard toast playbook so users can read longer messages.
     `tabindex="-1"` so the close button + action button are the
     keyboard stops — the wrapper itself isn't tab-targeted. -->
<div
  class="toast toast-{item.variant}"
  {role}
  aria-live={ariaLive}
  aria-atomic="true"
  onmouseenter={() => toast.pause(item.id)}
  onmouseleave={() => toast.resume(item.id)}
  onfocusin={() => toast.pause(item.id)}
  onfocusout={() => toast.resume(item.id)}
>
  <span class="toast-glyph" aria-hidden="true">{glyph}</span>
  <div class="toast-body">
    <p class="toast-msg">{item.message}</p>
    {#if item.action}
      <button type="button" class="toast-action" onclick={onAction}>
        {item.action.label}
      </button>
    {/if}
  </div>
  <button
    type="button"
    class="toast-close"
    aria-label="Dismiss notification"
    onclick={() => toast.dismiss(item.id)}
  >
    ×
  </button>
</div>
