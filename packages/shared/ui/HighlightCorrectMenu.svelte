<script lang="ts" module>
  // #11 — Selection popover that surfaces "Replace…" when the user
  // highlights text inside the summary or transcript region. Shared
  // package component consumed by both portal and agent.
  //
  // Lifecycle:
  //   - Parent owns selection-detection (subscribes to `selectionchange`
  //     on the document and forwards a `Selection` snapshot when the
  //     user highlights inside an opt-in container).
  //   - Parent passes us the anchor `Range`'s bounding rect so we can
  //     position the popover above (or below, on overflow) the
  //     selection.
  //   - We render a small "Replace" pill; clicking it invokes
  //     `onreplace` with the selected text + the parent-resolved
  //     anchor (region + row id + occurrence index). Parent then
  //     mounts `ReplaceTextModal` with the same props.
  //
  // Z-index: 30 — same band as `ChipMenu` (above the call-detail tag
  // popover at 20, below the agent note-to-self overlay at 50).

  export type HighlightAnchor = {
    region: "transcript" | "summary" | "action_item";
    utterance_idx?: number;
    action_item_id?: string;
    occurrence_index?: number;
  };
</script>

<script lang="ts">
  import { onMount, onDestroy, tick } from "svelte";

  type Props = {
    /** Bounding rect (viewport-relative) of the user's selection.
     *  Sourced from `range.getBoundingClientRect()` in the parent. */
    rect: DOMRect;
    /** The selected text — passed straight to the modal as the
     *  read-only `find` input. */
    selection: string;
    /** Where the selection landed; built by the parent from the DOM
     *  (which utterance idx, which action-item id, etc.). */
    anchor: HighlightAnchor;
    /** Click on "Replace" → parent opens the modal. */
    onreplace: (args: { selection: string; anchor: HighlightAnchor }) => void;
    /** Dismissal — parent closes us when the user clicks outside or
     *  the selection collapses. */
    onclose: () => void;
  };

  let { rect, selection, anchor, onreplace, onclose }: Props = $props();

  let cardEl = $state<HTMLDivElement | null>(null);
  // #541 — first-item-focus: focus the Replace button on mount.
  let replaceBtnEl = $state<HTMLButtonElement | null>(null);
  // Position state — fixed-position overlay. `placeBelow` flips when
  // the selection sits within `cardHeightGuess` of the viewport top
  // (so the popover doesn't get clipped above the selection).
  let pos = $state({ top: 0, left: 0, placeBelow: false });

  function recompute() {
    const cardHeight = cardEl?.offsetHeight ?? 36;
    const cardWidth = cardEl?.offsetWidth ?? 96;
    const gap = 6;
    // Centre horizontally on the selection. Clamp to viewport so the
    // popover doesn't extend off-screen on a near-edge selection.
    const centerX = rect.left + rect.width / 2;
    const desiredLeft = Math.max(
      8,
      Math.min(window.innerWidth - cardWidth - 8, centerX - cardWidth / 2),
    );
    // Default: above the selection. Flip below when there isn't
    // room. Distance from the rect edge is `gap`.
    const aboveTop = rect.top - cardHeight - gap;
    const belowTop = rect.bottom + gap;
    if (aboveTop < 8) {
      pos = { top: belowTop, left: desiredLeft, placeBelow: true };
    } else {
      pos = { top: aboveTop, left: desiredLeft, placeBelow: false };
    }
  }

  function onScrollOrResize() {
    // Selection rect doesn't update with scroll, so the parent keeps
    // a fresh `rect` prop; whenever the parent rerenders us with a
    // new rect, recompute. We also recompute on our own viewport
    // changes so the popover stays anchored on resize.
    recompute();
  }

  function onDocPointerDown(e: PointerEvent) {
    // Click outside the card AND outside the user's selection should
    // dismiss. We rely on the parent's `selectionchange` listener to
    // tell us when the selection collapses — we just close on a click
    // outside the popover itself.
    const target = e.target as Node | null;
    if (!cardEl || !target) return;
    if (cardEl.contains(target)) return;
    onclose();
  }

  // Recompute whenever the rect prop changes — parent rerenders us
  // with the latest rect on every selectionchange tick.
  $effect(() => {
    // Touch the rect to register a dependency.
    void rect;
    void selection;
    void tick().then(recompute);
  });

  onMount(() => {
    window.addEventListener("scroll", onScrollOrResize, { passive: true });
    window.addEventListener("resize", onScrollOrResize);
    // Pointer-down (capture) so we beat the parent's bubble-phase
    // handlers.
    document.addEventListener("pointerdown", onDocPointerDown, true);
  });

  onDestroy(() => {
    window.removeEventListener("scroll", onScrollOrResize);
    window.removeEventListener("resize", onScrollOrResize);
    document.removeEventListener("pointerdown", onDocPointerDown, true);
  });

  // #541 — focus trap: Escape dismisses; only one focusable item so
  // Tab does not need trapping (single-item toolbar).
  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }

  // #541 — focus the Replace button after mount so keyboard users
  // can immediately act without a Tab press.
  $effect(() => {
    replaceBtnEl?.focus();
  });

  function onReplaceClick() {
    onreplace({ selection, anchor });
  }
</script>

<div
  bind:this={cardEl}
  class="hc-card"
  class:place-below={pos.placeBelow}
  style="top: {pos.top}px; left: {pos.left}px;"
  role="toolbar"
  tabindex="-1"
  aria-label="Selection actions"
  onkeydown={onKeydown}
>
  <button
    bind:this={replaceBtnEl}
    type="button"
    class="hc-btn"
    onclick={onReplaceClick}
    onmousedown={(e) => {
      // Default mousedown blurs the selection; preventDefault keeps
      // the highlight visible long enough for the click to register.
      e.preventDefault();
    }}
  >
    Replace…
  </button>
</div>

<style>
  /* Component-local styling — no app.css edits, mirror pair invariant
     intact. Lifted from the design.md "pip" + "cta-btn" pattern but
     scoped local because this surface is component-specific. */

  .hc-card {
    position: fixed;
    z-index: 30;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 4px;
    background: var(--ink-0);
    border: 1px solid var(--hairline-hi);
    border-radius: 6px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.28);
    pointer-events: auto;
  }

  .hc-btn {
    appearance: none;
    border: 0;
    background: transparent;
    color: var(--bone-0);
    font: inherit;
    font-size: 13px;
    padding: 4px 10px;
    border-radius: 4px;
    cursor: pointer;
  }

  .hc-btn:hover,
  .hc-btn:focus-visible {
    background: var(--ink-2);
    outline: none;
  }
</style>
