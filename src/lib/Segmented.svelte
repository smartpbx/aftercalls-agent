<script lang="ts" module>
  // #477 — shared segmented-control primitive. Mirror pair:
  //   portal/src/lib/Segmented.svelte ≡ agent/src/lib/Segmented.svelte
  // The two must stay byte-identical. Reviewer diffs on every touch.
  //
  // Visual reference: the `.segmented` / `.seg.active` pattern that
  // shipped on `/admin` (recording notification mode) and `/staff/support`
  // (source filter). One container with a hairline border and `--ink-0`
  // fill; pill-shaped options that flip to `--accent-soft` /
  // `--accent-hi` on the active state.
  //
  // Documented in `design.md` §Pattern library "Segmented control".
  // Specialized variants — the `.actions-filter` count-bearing pills
  // in `ActionsList.svelte` and the `.ai-due-segmented` due-type pills
  // in `ActionItem.svelte` — keep their own visual treatments and
  // stay outside this component's scope (they would bloat the API).
  export type SegmentedOption<V extends string = string> = {
    value: V;
    label: string;
    disabled?: boolean;
  };
</script>

<script lang="ts" generics="V extends string = string">
  type Props = {
    options: SegmentedOption<V>[];
    value: V;
    // Optional `name` plumbs through to the rendered `<input type=radio>`
    // elements when present, letting the control participate in a real
    // form submission. When absent the radios still render (for AT) but
    // share the auto-generated name and the parent reads `value` via
    // `bind:value`.
    name?: string;
    // ARIA label for the radiogroup. Defaults to undefined — pass one
    // unless an enclosing `aria-label`/`aria-labelledby` already names
    // the group.
    ariaLabel?: string;
    // Whole-control disable (e.g. while saving). Per-option disable is
    // also supported via `option.disabled`.
    disabled?: boolean;
    // Optional change callback. Fires after `value` has been updated to
    // the newly-selected option. Useful for callers that need to kick
    // off a side-effect (URL update, fetch) on user pick — they can
    // still `bind:value` for two-way state and add `onchange` for the
    // side-effect, instead of plumbing a $effect.
    onchange?: (v: V) => void;
  };

  let {
    options,
    value = $bindable(),
    name,
    ariaLabel,
    disabled = false,
    onchange,
  }: Props = $props();

  // Stable radiogroup name. Auto-generated when caller doesn't pass one
  // so multiple <Segmented> instances on the same page don't collide.
  let autoId = $props.id();
  let groupName = $derived(name ?? `seg-${autoId}`);

  function pick(v: V, optDisabled: boolean | undefined) {
    if (disabled || optDisabled) return;
    if (value === v) return;
    value = v;
    onchange?.(v);
  }
</script>

<div class="segmented" role="radiogroup" aria-label={ariaLabel}>
  {#each options as opt (opt.value)}
    <label
      class="seg"
      class:active={value === opt.value}
      class:disabled={disabled || opt.disabled}
    >
      <input
        type="radio"
        name={groupName}
        value={opt.value}
        checked={value === opt.value}
        disabled={disabled || opt.disabled}
        onchange={() => pick(opt.value, opt.disabled)}
      />
      <span>{opt.label}</span>
    </label>
  {/each}
</div>

<style>
  /* #477 — segmented-control visual. Mirrors the `.segmented` pattern
     that shipped first on `/admin` (recording mode) and `/staff/support`
     (source filter). Component-scoped so it never collides with legacy
     `.segmented` / `.seg` selectors elsewhere on the page during the
     migration. Hard rule #1: app.css stays byte-identical — all of
     this lives here. */
  .segmented {
    display: inline-flex;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    padding: 3px;
    gap: 2px;
    flex-wrap: wrap;
  }
  .seg {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0.4rem 0.8rem;
    font-size: 0.82rem;
    color: var(--bone-2);
    border-radius: 6px;
    cursor: pointer;
    user-select: none;
    transition: background 0.15s, color 0.15s;
  }
  .seg:hover {
    color: var(--bone-0);
  }
  .seg input {
    /* Native radio kept in the DOM for AT + keyboard; visually hidden
       behind the label. Focus rings still land via :focus-visible
       below. */
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  .seg.active {
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .seg.disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .seg input:focus-visible + span {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 3px;
  }
  @media (prefers-reduced-motion: reduce) {
    .seg {
      transition: none;
    }
  }

  /* Mobile pass — wrap to a vertical stack so each option is a ≥44px
     tap target on narrow viewports. Matches the @media block that
     used to live in `/admin/+page.svelte`. */
  @media (max-width: 640px) {
    .segmented {
      display: flex;
      flex-direction: column;
      align-items: stretch;
      gap: 4px;
    }
    .seg {
      min-height: 44px;
      padding: 0.55rem 0.75rem;
    }
  }
</style>
