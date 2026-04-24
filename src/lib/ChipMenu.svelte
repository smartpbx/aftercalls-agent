<script lang="ts" module>
  // #140 · v0.4.5 — floating two-mode popover anchored to a clicked
  // `<name>` chip inside `SummaryText`. Mirror pair:
  //   portal/src/lib/ChipMenu.svelte ≡ agent/src/lib/ChipMenu.svelte
  // The two must stay byte-identical. Reviewer diffs on every touch.
  //
  // Two states (driven by `mode`):
  //   • "menu"   — two buttons: Rename teammate / Unlink.
  //   • "picker" — swap to an inline SpeakerRenamePicker (variant
  //                "stack", placeholder tuned for chip-edit vocab).
  //                Esc returns to the menu state; a second Esc
  //                closes the popover via the parent's onclose.
  //
  // Parent owns:
  //   • anchor positioning (this component is a fixed-position card
  //     absolutely-placed via the anchor's getBoundingClientRect)
  //   • roster wiring (user passes `users`; we don't fetch)
  //   • close semantics (click-outside + after-select)
  //
  // Z-index: 30 — above the call-detail tag popover (20), below the
  // agent note-to-self overlay (50). Documented in plan.md §6.1 #Q10.

  export type ChipMenuAction = "rename" | "unlink";
</script>

<script lang="ts">
  import SpeakerRenamePicker, {
    type OrgMemberLite,
    type SpeakerPick,
  } from "./SpeakerRenamePicker.svelte";

  type Props = {
    anchor: HTMLElement;
    name: string;
    users: OrgMemberLite[];
    rosterLoaded?: boolean;
    rosterError?: boolean;
    // #150 · v0.4.6 — swap the two-item menu into its "external"
    // shape: "Link to teammate" / "Leave as text". Unlink doesn't
    // apply when the chip wasn't linked in the first place, so
    // "Leave as text" just dismisses the popover without emitting
    // onselect. Picker behaviour is unchanged — picking a teammate
    // still fires `onselect("rename", pick)`, which the parent's
    // rewrite helper handles identically to a linked rename.
    isExternal?: boolean;
    onselect: (action: ChipMenuAction, pick?: SpeakerPick) => void;
    onclose: () => void;
  };

  let {
    anchor,
    name,
    users,
    rosterLoaded = true,
    rosterError = false,
    isExternal = false,
    onselect,
    onclose,
  }: Props = $props();

  type Mode = "menu" | "picker";
  let mode = $state<Mode>("menu");
  let focusIdx = $state(0);
  let pickerValue = $state("");

  // Anchor positioning — fixed-position card, bottom-left of the
  // anchor, with a 6px gap. Flips above if the chip sits within
  // `cardHeightGuess + 20px` of the viewport bottom.
  let pos = $state({ top: 0, left: 0, placeBelow: true });
  let cardEl = $state<HTMLDivElement | null>(null);

  function recomputePosition() {
    if (!anchor) return;
    const rect = anchor.getBoundingClientRect();
    const cardHeightGuess = cardEl?.offsetHeight ?? 120;
    const cardWidthGuess = cardEl?.offsetWidth ?? 240;
    const vh = window.innerHeight;
    const vw = window.innerWidth;
    const placeBelow = rect.bottom + cardHeightGuess + 20 < vh;
    const top = placeBelow ? rect.bottom + 6 : rect.top - cardHeightGuess - 6;
    // Left edge flips inward if the card would overflow right.
    let left = rect.left;
    if (left + cardWidthGuess + 8 > vw) {
      left = Math.max(8, vw - cardWidthGuess - 8);
    }
    pos = { top, left, placeBelow };
  }

  $effect(() => {
    recomputePosition();
    // Re-position on scroll / resize while the popover is live.
    const onRelayout = () => recomputePosition();
    window.addEventListener("scroll", onRelayout, true);
    window.addEventListener("resize", onRelayout);
    return () => {
      window.removeEventListener("scroll", onRelayout, true);
      window.removeEventListener("resize", onRelayout);
    };
  });

  // Click-outside close. `pointerdown` (not `click`) so the dismiss
  // fires before focus / selection events on the new target.
  function onOutsidePointerDown(e: PointerEvent) {
    const t = e.target as Node | null;
    if (!t) return;
    if (cardEl && cardEl.contains(t)) return;
    if (anchor && anchor.contains(t)) return;
    onclose();
  }

  $effect(() => {
    document.addEventListener("pointerdown", onOutsidePointerDown, true);
    return () => {
      document.removeEventListener(
        "pointerdown",
        onOutsidePointerDown,
        true,
      );
    };
  });

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      if (mode === "picker") {
        mode = "menu";
        focusIdx = 0;
        return;
      }
      onclose();
      return;
    }
    if (mode !== "menu") return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      focusIdx = (focusIdx + 1) % 2;
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      focusIdx = (focusIdx + 1) % 2; // only 2 items; wraps either way
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (focusIdx === 0) {
        mode = "picker";
      } else if (isExternal) {
        // #150 — "Leave as text" is dismiss-only; no mutation.
        onclose();
      } else {
        onselect("unlink");
      }
    }
  }

  // Focus the card on mount so keyboard flow lands here.
  $effect(() => {
    cardEl?.focus();
  });

  function openRename() {
    mode = "picker";
    pickerValue = "";
  }

  function doUnlink() {
    onselect("unlink");
  }

  // #150 — external mode's item-1 action. Dismiss without emitting
  // a mutation — the mention keeps its italic rendering.
  function doLeaveAsText() {
    onclose();
  }

  function onPickerPick(pick: SpeakerPick) {
    // Chip-edit intentionally constrains to FK-only picks — the
    // picker's free-form fallback is folded back into "unlink",
    // matching the D14/D15 rationale: if the user wants a
    // non-teammate rendering, Unlink is the path.
    if (pick.user) {
      onselect("rename", pick);
    } else {
      onselect("unlink");
    }
  }

  function onPickerCancel() {
    mode = "menu";
    focusIdx = 0;
  }
</script>

<div
  class="chip-menu"
  role="menu"
  aria-label="Mention actions"
  tabindex="-1"
  bind:this={cardEl}
  style="top: {pos.top}px; left: {pos.left}px;"
  onkeydown={onKeydown}
>
  {#if mode === "menu"}
    <button
      type="button"
      role="menuitem"
      class="chip-menu-item"
      class:chip-menu-item-active={focusIdx === 0}
      onmouseenter={() => (focusIdx = 0)}
      onclick={openRename}
    >
      <span class="chip-menu-glyph" aria-hidden="true">
        <svg viewBox="0 0 12 12" width="12" height="12">
          <path
            d="M2 10l1-3 5-5 2 2-5 5-3 1z"
            fill="none"
            stroke="currentColor"
            stroke-width="1.2"
            stroke-linejoin="round"
          />
        </svg>
      </span>
      {isExternal ? "Link to teammate" : "Rename teammate"}
    </button>
    {#if isExternal}
      <!-- #150 — external mode: item 1 is dismiss-only. No Unlink
           glyph (the chip was never linked); no hint text. -->
      <button
        type="button"
        role="menuitem"
        class="chip-menu-item"
        class:chip-menu-item-active={focusIdx === 1}
        onmouseenter={() => (focusIdx = 1)}
        onclick={doLeaveAsText}
      >
        <span class="chip-menu-glyph" aria-hidden="true">
          <svg viewBox="0 0 12 12" width="12" height="12">
            <path
              d="M3 9 L9 3 M3 3 L9 9"
              stroke="currentColor"
              stroke-width="1.2"
              stroke-linecap="round"
            />
          </svg>
        </span>
        Leave as text
      </button>
    {:else}
      <button
        type="button"
        role="menuitem"
        class="chip-menu-item"
        class:chip-menu-item-active={focusIdx === 1}
        onmouseenter={() => (focusIdx = 1)}
        onclick={doUnlink}
      >
        <span class="chip-menu-glyph" aria-hidden="true">
          <svg viewBox="0 0 12 12" width="12" height="12">
            <path
              d="M4 8L2 10M8 4l2-2"
              stroke="currentColor"
              stroke-width="1.2"
              stroke-linecap="round"
            />
            <path
              d="M5 9a2 2 0 0 1-3-3l1-1M7 3a2 2 0 0 1 3 3l-1 1"
              fill="none"
              stroke="currentColor"
              stroke-width="1.2"
              stroke-linecap="round"
            />
          </svg>
        </span>
        Unlink <span class="chip-menu-hint">(just show as text)</span>
      </button>
    {/if}
  {:else}
    <div class="chip-menu-picker">
      <SpeakerRenamePicker
        bind:value={pickerValue}
        roster={users}
        {rosterLoaded}
        {rosterError}
        variant="stack"
        placeholder="Pick a teammate"
        noMatchHint="Pick a teammate from the list to re-link this mention."
        savingLabel="Link"
        onpick={onPickerPick}
        oncancel={onPickerCancel}
      />
    </div>
  {/if}
  <span class="chip-menu-sr-name" aria-hidden="true">{name}</span>
</div>

<style>
  .chip-menu {
    position: fixed;
    z-index: 30;
    min-width: 220px;
    padding: 0.3rem;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    box-shadow: 0 14px 30px -10px rgba(0, 0, 0, 0.55);
    font-size: 0.85rem;
    color: var(--bone-1);
    animation: chip-menu-in 100ms ease-out both;
    outline: none;
  }
  @media (prefers-reduced-motion: reduce) {
    .chip-menu {
      animation: none;
    }
  }
  @keyframes chip-menu-in {
    from {
      opacity: 0;
      transform: translateY(-2px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }

  .chip-menu-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.45rem 0.6rem;
    border: none;
    background: transparent;
    color: var(--bone-1);
    font: inherit;
    font-size: 0.85rem;
    text-align: left;
    border-radius: 6px;
    cursor: pointer;
    transition: background 120ms linear, color 120ms linear;
  }
  .chip-menu-item:hover,
  .chip-menu-item-active {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .chip-menu-item:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .chip-menu-glyph {
    color: var(--bone-3);
    flex-shrink: 0;
  }
  .chip-menu-item:hover .chip-menu-glyph,
  .chip-menu-item-active .chip-menu-glyph {
    color: var(--accent);
  }
  .chip-menu-hint {
    color: var(--bone-3);
    font-size: 0.78rem;
    margin-left: 0.2rem;
  }
  .chip-menu-item:hover .chip-menu-hint,
  .chip-menu-item-active .chip-menu-hint {
    color: var(--bone-2);
  }
  .chip-menu-picker {
    padding: 0.2rem;
    min-width: 260px;
  }

  .chip-menu-sr-name {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>
