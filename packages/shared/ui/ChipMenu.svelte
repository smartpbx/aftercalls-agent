<script lang="ts" module>
  // #140 · v0.4.5 — floating two-mode popover anchored to a clicked
  // `<name>` chip inside `SummaryText`. Shared package component
  // consumed by both portal and agent.
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
    // shape: "Link to teammate" / "Leave as text". The external
    // branch is for italic name chips that never matched the org
    // roster (often clients or counterparties the LLM wrapped
    // anyway). #178 · v0.6.x — "Leave as text" now emits
    // `onselect("unlink")` instead of dismissing silently, so the
    // parent's rewrite path strips the `<name>` wrapper from the
    // stored text; otherwise the italic chip reappears on reload.
    // Picker behaviour is unchanged — picking a teammate still
    // fires `onselect("rename", pick)`, which the parent's rewrite
    // helper handles identically to a linked rename.
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
  // #541 — first-item-focus + focus trap for the menu items.
  let menuItemEls = $state<HTMLButtonElement[]>([]);

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
    const itemCount = menuItemEls.filter(Boolean).length || 2;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      focusIdx = (focusIdx + 1) % itemCount;
      menuItemEls[focusIdx]?.focus();
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      focusIdx = (focusIdx - 1 + itemCount) % itemCount;
      menuItemEls[focusIdx]?.focus();
      return;
    }
    // #541 — Tab / Shift+Tab traps focus within the menu items.
    if (e.key === "Tab") {
      e.preventDefault();
      if (e.shiftKey) {
        focusIdx = (focusIdx - 1 + itemCount) % itemCount;
      } else {
        focusIdx = (focusIdx + 1) % itemCount;
      }
      menuItemEls[focusIdx]?.focus();
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (focusIdx === 0) {
        mode = "picker";
      } else {
        // #178 — both modes now persist the unlink. In external
        // mode ("Leave as text") this was previously dismiss-only,
        // so the italic chip reappeared on reload because the
        // `<name>` wrapper stayed in the stored text. Routing
        // through `onselect("unlink")` reuses the page-level
        // rewrite-and-persist path for both linked and external
        // chips.
        onselect("unlink");
      }
    }
  }

  // #541 — Focus the first menu item (not just the card) on mount and
  // whenever we return from "picker" mode back to "menu".
  $effect(() => {
    if (mode === "menu") {
      // After the reactive update, buttons are rendered; index into
      // menuItemEls[0] which is bound via `bind:this` on each item.
      menuItemEls[0]?.focus();
    }
  });

  function openRename() {
    mode = "picker";
    pickerValue = "";
  }

  function doUnlink() {
    onselect("unlink");
  }

  // #178 — external mode's item-2 action. Persist the unlink by
  // stripping the `<name>` wrapper via the parent's rewrite path,
  // so the italic chip doesn't reappear on reload. Previously this
  // was `onclose()` (dismiss-only) per #150, but dismissing without
  // mutating left the wrapper in `summary_text` / action-item
  // `description`, defeating the user's intent.
  function doLeaveAsText() {
    onselect("unlink");
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
      bind:this={menuItemEls[0]}
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
      <!-- #150 / #178 — external mode's item 2. No Unlink glyph
           (the chip was never linked to a teammate) and no hint
           text, but the action still strips the `<name>` wrapper
           so the chip stays plain text on subsequent reloads. -->
      <button
        bind:this={menuItemEls[1]}
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
        bind:this={menuItemEls[1]}
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
