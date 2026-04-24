<script lang="ts">
  // Mirror-pair component — byte-identical between
  // portal/src/lib/DateInput.svelte and agent/src/lib/DateInput.svelte
  // (same discipline as Avatar, SpeakerRenamePicker, ChipMenu). Reviewer
  // diffs the two on every touch. Styles are component-scoped — none of
  // these classes live in app.css, so the `diff portal/src/app.css
  // agent/src/app.css` invariant holds by construction.
  //
  // Custom date-picker that replaces <input type="date"> on /calls +
  // /calls/trash. Built for #189 — webkit2gtk's native date input paints
  // today as the placeholder AND won't dismiss on outside click; both
  // symptoms are unfixable in-place. Trigger is a text-shaped button
  // (NOT a typable input — avoids the locale-vs-ISO ambiguity and the
  // webkit typing-into-date-field bug we're leaving behind). Popup is
  // a role=dialog with a 7x{5,6} day grid.
  //
  // Contract matches the native input it replaces — `value` is ISO
  // YYYY-MM-DD (or "" for unset); onchange emits a bare string. Parents
  // swap `bind:value={fromDate} onchange={onDateChange}` for
  // `value={fromDate} onchange={(v) => { fromDate = v; onDateChange(); }}`
  // and keep their existing debounce + URL-sync logic untouched.

  interface Props {
    value: string;                       // ISO YYYY-MM-DD; "" = unset
    onchange: (value: string) => void;
    min?: string;                        // ISO YYYY-MM-DD; optional floor
    max?: string;                        // ISO YYYY-MM-DD; optional ceiling
    placeholder?: string;
    id?: string;
    disabled?: boolean;
    ariaLabel?: string;                  // passthrough for the trigger
    ariaDescribedby?: string;            // passthrough for the trigger
  }

  let {
    value,
    onchange,
    min = "",
    max = "",
    placeholder = "yyyy-mm-dd",
    id = "",
    disabled = false,
    ariaLabel = "",
    ariaDescribedby = "",
  }: Props = $props();

  // Per-instance ids so multiple DateInputs on the same page (From + To)
  // don't collide on aria-labelledby / month-header references.
  const instanceId = `di-${Math.random().toString(36).slice(2, 10)}`;
  const monthHeaderId = `${instanceId}-month`;

  // ── ISO + date helpers ────────────────────────────────────────────
  // Keep all arithmetic in local date-parts, NOT via new Date(iso)
  // which parses ISO as UTC midnight and shifts day depending on TZ.
  // Everything here treats a YYYY-MM-DD as a calendar date, period.

  function pad2(n: number): string {
    return n < 10 ? `0${n}` : `${n}`;
  }

  function isoFromParts(y: number, m: number, d: number): string {
    // m is 1-12 on input; Date normalises day overflow (e.g. Feb 30 → Mar 2).
    const dt = new Date(y, m - 1, d);
    return `${dt.getFullYear()}-${pad2(dt.getMonth() + 1)}-${pad2(dt.getDate())}`;
  }

  function parseIso(iso: string): { y: number; m: number; d: number } | null {
    if (!iso || !/^\d{4}-\d{2}-\d{2}$/.test(iso)) return null;
    const [y, m, d] = iso.split("-").map(Number);
    // Round-trip validation: Feb 30 → Mar 2 would mask as valid, reject.
    const rt = isoFromParts(y, m, d);
    if (rt !== iso) return null;
    return { y, m, d };
  }

  function todayIso(): string {
    const t = new Date();
    return `${t.getFullYear()}-${pad2(t.getMonth() + 1)}-${pad2(t.getDate())}`;
  }

  function addDays(iso: string, delta: number): string {
    const p = parseIso(iso);
    if (!p) return iso;
    return isoFromParts(p.y, p.m, p.d + delta);
  }

  function addMonths(iso: string, delta: number): string {
    const p = parseIso(iso);
    if (!p) return iso;
    return isoFromParts(p.y, p.m + delta, p.d);
  }

  function compareIso(a: string, b: string): number {
    // Lexical compare is correct for YYYY-MM-DD.
    return a < b ? -1 : a > b ? 1 : 0;
  }

  function clampToRange(iso: string): string {
    if (min && compareIso(iso, min) < 0) return min;
    if (max && compareIso(iso, max) > 0) return max;
    return iso;
  }

  function isInRange(iso: string): boolean {
    if (min && compareIso(iso, min) < 0) return false;
    if (max && compareIso(iso, max) > 0) return false;
    return true;
  }

  // Long-form aria label for day buttons: "24 April 2026".
  const LONG_MONTHS = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
  ];

  function longDate(iso: string): string {
    const p = parseIso(iso);
    if (!p) return iso;
    return `${p.d} ${LONG_MONTHS[p.m - 1]} ${p.y}`;
  }

  // ── State ─────────────────────────────────────────────────────────

  let open = $state(false);
  let triggerEl = $state<HTMLButtonElement | null>(null);
  let popEl = $state<HTMLDivElement | null>(null);
  let gridEl = $state<HTMLDivElement | null>(null);
  let flipAbove = $state(false);

  // viewMonth drives which month the grid paints. Updated on open and
  // on arrow-key cross-month navigation. Stored as first-of-month ISO
  // for cheap `addMonths` arithmetic. Seeded lazily in the $effect
  // below — initializing with `value` here would capture the prop by
  // reference and Svelte 5 warns (state_referenced_locally).
  let viewMonth = $state<string>("");
  // Focused day ISO — independent of committed `value`. Arrow keys move
  // this; Enter commits it. null = no focus (mouse-only flow).
  let focusDay = $state<string>("");

  function firstOfMonth(iso: string): string {
    const p = parseIso(iso);
    const seed = p ?? parseIso(todayIso())!;
    return `${seed.y}-${pad2(seed.m)}-01`;
  }

  // Seed the grid view off `value` while closed. When `value` changes
  // externally (URL back/forward, Clear, parent reset) the effect re-
  // seeds so a reopen shows the right month. Not re-seeded while open
  // so the user's in-flight nav isn't interrupted.
  $effect(() => {
    if (!open) {
      viewMonth = firstOfMonth(value || todayIso());
    }
  });

  // ── Grid cells ────────────────────────────────────────────────────

  type Cell = {
    iso: string;       // YYYY-MM-DD
    day: number;       // 1-31
    inMonth: boolean;  // false = prev/next spillover
    isToday: boolean;
    isSelected: boolean;
    disabled: boolean; // outside min/max
  };

  let cells = $derived.by<Cell[]>(() => {
    const v = parseIso(viewMonth)!;
    const today = todayIso();
    // First cell = Sunday on or before the 1st of the month.
    const first = new Date(v.y, v.m - 1, 1);
    const leading = first.getDay(); // 0 = Sunday
    const gridStart = new Date(v.y, v.m - 1, 1 - leading);
    // 6 rows × 7 = 42 cells. Some months only need 5; we still paint 6
    // so the popup height is stable and doesn't jitter as the user
    // navigates months. Empty-looking trailing cells are spillover.
    const out: Cell[] = [];
    for (let i = 0; i < 42; i++) {
      const dt = new Date(gridStart);
      dt.setDate(gridStart.getDate() + i);
      const iso = `${dt.getFullYear()}-${pad2(dt.getMonth() + 1)}-${pad2(dt.getDate())}`;
      out.push({
        iso,
        day: dt.getDate(),
        inMonth: dt.getMonth() === v.m - 1,
        isToday: iso === today,
        isSelected: iso === value,
        disabled: !isInRange(iso),
      });
    }
    return out;
  });

  // WAI-ARIA 1.2 ownership: role="grid" must own role="row" which must
  // own role="gridcell". Flat 42-cell array → 6 rows of 7 for the markup
  // wrappers. Rows and gridcells use `display: contents` in CSS so the
  // existing 7-column grid layout on `.di-grid` still lays out the cells;
  // the wrappers exist only to satisfy the ARIA ownership contract.
  // (Reviewer RC-1.)
  let weeks = $derived.by<Cell[][]>(() => {
    const rows: Cell[][] = [];
    for (let r = 0; r < 6; r++) {
      rows.push(cells.slice(r * 7, r * 7 + 7));
    }
    return rows;
  });

  // ── Open / close ──────────────────────────────────────────────────

  function openPop() {
    if (disabled) return;
    // Seed view + focus on the selected value if any, else today clamped.
    const seedIso = value || clampToRange(todayIso());
    viewMonth = firstOfMonth(seedIso);
    focusDay = value || seedIso;
    open = true;
    // Flip-above decision on next tick once the popup has a bounding rect.
    queueMicrotask(decideFlip);
    // Move focus into the grid so arrow keys work immediately.
    queueMicrotask(() => {
      focusGridCell(focusDay);
    });
  }

  function closePop(opts: { returnFocus: boolean } = { returnFocus: true }) {
    if (!open) return;
    open = false;
    if (opts.returnFocus) {
      queueMicrotask(() => {
        try {
          triggerEl?.focus();
        } catch {
          /* trigger detached; no-op */
        }
      });
    }
  }

  function toggle() {
    if (open) closePop();
    else openPop();
  }

  function decideFlip() {
    if (!triggerEl || typeof window === "undefined") return;
    const rect = triggerEl.getBoundingClientRect();
    // Conservative 300px estimate — matches ChipMenu's cardHeightGuess + 20.
    flipAbove = rect.bottom + 300 > window.innerHeight - 8;
  }

  function focusGridCell(iso: string) {
    if (!gridEl) return;
    const sel = gridEl.querySelector<HTMLButtonElement>(
      `button[data-iso="${iso}"]`,
    );
    if (sel && !sel.disabled) {
      try {
        sel.focus();
      } catch {
        /* cell detached; no-op */
      }
    }
  }

  // ── Commit paths ──────────────────────────────────────────────────

  function commit(iso: string) {
    if (!isInRange(iso)) return;
    onchange(iso);
    closePop();
  }

  function pickToday() {
    const t = clampToRange(todayIso());
    if (!isInRange(t)) return;
    viewMonth = firstOfMonth(t);
    onchange(t);
    closePop();
  }

  function pickClear() {
    onchange("");
    closePop();
  }

  // ── Keyboard handling ─────────────────────────────────────────────

  function onTriggerKey(e: KeyboardEvent) {
    if (e.key === "Enter" || e.key === " " || e.key === "ArrowDown") {
      e.preventDefault();
      openPop();
    }
  }

  function onGridKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      closePop();
      return;
    }
    // Commit on Enter / Space when a day cell has focus.
    if (e.key === "Enter" || e.key === " ") {
      if (focusDay && isInRange(focusDay)) {
        e.preventDefault();
        commit(focusDay);
      }
      return;
    }
    let next = focusDay;
    if (e.key === "ArrowLeft") next = addDays(focusDay, -1);
    else if (e.key === "ArrowRight") next = addDays(focusDay, 1);
    else if (e.key === "ArrowUp") next = addDays(focusDay, -7);
    else if (e.key === "ArrowDown") next = addDays(focusDay, 7);
    else if (e.key === "PageUp") {
      next = e.shiftKey
        ? addMonths(focusDay, -12)
        : addMonths(focusDay, -1);
    } else if (e.key === "PageDown") {
      next = e.shiftKey
        ? addMonths(focusDay, 12)
        : addMonths(focusDay, 1);
    } else if (e.key === "Home") {
      const p = parseIso(focusDay);
      if (p) {
        const d = new Date(p.y, p.m - 1, p.d);
        next = addDays(focusDay, -d.getDay());
      }
    } else if (e.key === "End") {
      const p = parseIso(focusDay);
      if (p) {
        const d = new Date(p.y, p.m - 1, p.d);
        next = addDays(focusDay, 6 - d.getDay());
      }
    } else {
      return;
    }
    e.preventDefault();
    next = clampToRange(next);
    focusDay = next;
    viewMonth = firstOfMonth(next);
    // Wait for the grid to repaint before snapping focus to the new cell.
    queueMicrotask(() => focusGridCell(next));
  }

  // ── Outside-click dismissal ───────────────────────────────────────
  // Capture-phase pointerdown, same pattern as SpeakerRenamePicker
  // (L270-291) and ChipMenu. pointerdown (not click) fires before
  // focus/selection on the new target so dismiss lands on the first
  // press. Guarded with contains(target) on both trigger and popup
  // because the popup is a sibling of the trigger in the DOM (not a
  // child) and the listener needs to ignore presses inside either.

  function onOutsidePointerDown(e: PointerEvent) {
    if (!open) return;
    const t = e.target as Node | null;
    if (!t) return;
    if (triggerEl && triggerEl.contains(t)) return;
    if (popEl && popEl.contains(t)) return;
    // Outside press → dismiss without commit, don't steal focus back to
    // the trigger (the user is heading elsewhere).
    closePop({ returnFocus: false });
  }

  $effect(() => {
    if (typeof document === "undefined") return;
    document.addEventListener("pointerdown", onOutsidePointerDown, true);
    return () => {
      document.removeEventListener(
        "pointerdown",
        onOutsidePointerDown,
        true,
      );
    };
  });

  // Re-decide flip on window resize / scroll while open so the popup
  // doesn't clip on a dragged webview.
  $effect(() => {
    if (!open || typeof window === "undefined") return;
    const h = () => decideFlip();
    window.addEventListener("resize", h);
    window.addEventListener("scroll", h, true);
    return () => {
      window.removeEventListener("resize", h);
      window.removeEventListener("scroll", h, true);
    };
  });

  // ── Derived labels ────────────────────────────────────────────────

  let monthLabel = $derived.by(() => {
    const p = parseIso(viewMonth);
    if (!p) return "";
    return `${LONG_MONTHS[p.m - 1]} ${p.y}`;
  });

  let triggerText = $derived(value || placeholder);
  let triggerHasValue = $derived(!!value);

  // Compose aria-label. Default "Date: 2026-04-24" or "Date: not set";
  // if caller passed ariaLabel we prefix onto the current value so SR
  // users hear "From: 2026-04-24".
  let triggerAriaLabel = $derived.by(() => {
    const prefix = ariaLabel || "Date";
    return value ? `${prefix}: ${value}` : `${prefix}: not set`;
  });

  // Prev/next month disabled gating when min/max would clip the month.
  let prevDisabled = $derived.by(() => {
    if (!min) return false;
    const v = parseIso(viewMonth)!;
    const prev = isoFromParts(v.y, v.m - 1, 1);
    const p = parseIso(prev)!;
    // last day of prev month
    const lastDayPrev = isoFromParts(p.y, p.m + 1, 0);
    return compareIso(lastDayPrev, min) < 0;
  });
  let nextDisabled = $derived.by(() => {
    if (!max) return false;
    const v = parseIso(viewMonth)!;
    const next = isoFromParts(v.y, v.m + 1, 1);
    return compareIso(next, max) > 0;
  });

  // Hide Today button entirely when today is outside min/max — a
  // disabled "Today" affordance is a confusing no-op (ui.md §Footer).
  let todayAvailable = $derived(isInRange(todayIso()));
</script>

<div class="di" class:di-open={open}>
  <button
    type="button"
    bind:this={triggerEl}
    class="di-trigger"
    class:di-trigger-empty={!triggerHasValue}
    {id}
    {disabled}
    aria-haspopup="dialog"
    aria-expanded={open}
    aria-label={triggerAriaLabel}
    aria-describedby={ariaDescribedby || undefined}
    onclick={toggle}
    onkeydown={onTriggerKey}
  >
    {triggerText}
  </button>

  {#if open}
    <div
      bind:this={popEl}
      class="di-pop"
      class:di-pop-above={flipAbove}
      role="dialog"
      aria-modal="false"
      aria-label="Choose date"
      aria-labelledby={monthHeaderId}
      onkeydown={onGridKey}
      tabindex="-1"
    >
      <div class="di-head">
        <button
          type="button"
          class="di-nav-btn"
          aria-label="Previous month"
          disabled={prevDisabled}
          onclick={() => (viewMonth = addMonths(viewMonth, -1))}
        >
          ‹
        </button>
        <span
          id={monthHeaderId}
          class="di-month"
          aria-live="polite"
          aria-atomic="true"
        >
          {monthLabel}
        </span>
        <button
          type="button"
          class="di-nav-btn"
          aria-label="Next month"
          disabled={nextDisabled}
          onclick={() => (viewMonth = addMonths(viewMonth, 1))}
        >
          ›
        </button>
      </div>

      <div class="di-wday" aria-hidden="true">
        <span>Su</span><span>Mo</span><span>Tu</span><span>We</span>
        <span>Th</span><span>Fr</span><span>Sa</span>
      </div>

      <div bind:this={gridEl} class="di-grid" role="grid">
        {#each weeks as week, wi (wi)}
          <div class="di-row" role="row">
            {#each week as c (c.iso)}
              <div class="di-gridcell" role="gridcell">
                <button
                  type="button"
                  class="di-cell"
                  class:di-out={!c.inMonth}
                  class:di-today={c.isToday && !c.isSelected}
                  class:di-selected={c.isSelected}
                  class:di-disabled={c.disabled}
                  data-iso={c.iso}
                  aria-label={longDate(c.iso)}
                  aria-current={c.isToday ? "date" : undefined}
                  aria-pressed={c.isSelected}
                  aria-disabled={c.disabled}
                  disabled={c.disabled}
                  tabindex={c.iso === focusDay ? 0 : -1}
                  onclick={() => commit(c.iso)}
                  onfocus={() => (focusDay = c.iso)}
                >
                  <span aria-hidden="true">{c.day}</span>
                </button>
              </div>
            {/each}
          </div>
        {/each}
      </div>

      <div class="di-foot">
        {#if todayAvailable}
          <button
            type="button"
            class="di-foot-btn di-today-btn"
            onclick={pickToday}
          >
            Today
          </button>
        {:else}
          <span></span>
        {/if}
        <button
          type="button"
          class="di-foot-btn di-clear-btn"
          onclick={pickClear}
        >
          Clear
        </button>
      </div>
    </div>
  {/if}
</div>

<style>
  /* Mirror-pair component styles — byte-identical between portal + agent.
     None of these classes leak into app.css; the `diff portal/src/app.css
     agent/src/app.css` invariant is preserved by construction. */

  .di {
    position: relative;
    display: inline-flex;
  }

  /* ── Trigger ─────────────────────────────────────────────────────── */

  .di-trigger {
    appearance: none;
    padding: 0.35rem 0.55rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: var(--ink-1);
    color: var(--bone-0);
    font-family: var(--font-mono);
    font-size: 0.8rem;
    line-height: 1.2;
    cursor: pointer;
    min-width: 9rem;
    text-align: left;
    transition: border-color 150ms;
  }
  .di-trigger:hover {
    border-color: var(--hairline-hi);
  }
  /* webkit2gtk has historically flaked on :focus-visible — keep :focus
     as a belt-and-braces fallback so keyboard users always see the ring
     on the Linux agent. (architect risk §webkit2gtk :focus-visible) */
  .di-trigger:focus {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 4px;
  }
  .di-trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 4px;
  }
  .di-trigger-empty {
    color: var(--bone-3);
    font-family: var(--font-sans);
  }
  .di-trigger:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  /* Open state — active-field border cue matches the srp-input pattern. */
  .di-open .di-trigger {
    border-color: var(--accent);
  }

  /* ── Popover ─────────────────────────────────────────────────────── */

  .di-pop {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    width: 280px;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    box-shadow: 0 14px 30px -10px rgba(0, 0, 0, 0.55);
    padding: 0.75rem 0.75rem 0.6rem;
    z-index: 25;
    animation: di-pop-in 100ms ease-out both;
  }
  .di-pop-above {
    top: auto;
    bottom: calc(100% + 6px);
  }
  @keyframes di-pop-in {
    from {
      opacity: 0;
      transform: translateY(-4px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .di-pop {
      animation: di-pop-in-reduced 100ms ease-out both;
    }
    @keyframes di-pop-in-reduced {
      from {
        opacity: 0;
      }
      to {
        opacity: 1;
      }
    }
  }

  /* ── Header ──────────────────────────────────────────────────────── */

  .di-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 0.5rem;
  }
  .di-nav-btn {
    appearance: none;
    width: 1.75rem;
    height: 1.75rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    border-radius: 4px;
    color: var(--bone-2);
    font-size: 1rem;
    line-height: 1;
    cursor: pointer;
    font-family: inherit;
    transition: background 120ms, color 120ms;
  }
  .di-nav-btn:hover:not(:disabled) {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .di-nav-btn:focus {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .di-nav-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .di-nav-btn:disabled {
    opacity: 0.3;
    cursor: not-allowed;
  }
  .di-month {
    font-family: var(--font-sans);
    font-size: 0.75rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }

  /* ── Weekday header row ──────────────────────────────────────────── */

  .di-wday {
    display: grid;
    grid-template-columns: repeat(7, 1fr);
    gap: 2px;
    margin-bottom: 0.35rem;
  }
  .di-wday span {
    font-size: 0.65rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    text-align: center;
    font-family: var(--font-sans);
  }

  /* ── Day grid ────────────────────────────────────────────────────── */

  .di-grid {
    display: grid;
    grid-template-columns: repeat(7, 1fr);
    gap: 2px;
    justify-items: center;
  }
  /* WAI-ARIA 1.2 ownership wrappers — `.di-row` (role="row") and
     `.di-gridcell` (role="gridcell") exist purely to satisfy the
     grid/row/gridcell contract. `display: contents` pulls their
     children up into `.di-grid`'s 7-column track so the visual
     layout is identical to a flat 42-cell grid. (RC-1 fix.) */
  .di-row,
  .di-gridcell {
    display: contents;
  }
  .di-cell {
    appearance: none;
    width: 32px;
    height: 32px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    border-radius: var(--radius-sm);
    color: var(--bone-1);
    font-family: var(--font-mono);
    font-size: 0.8rem;
    cursor: pointer;
    padding: 0;
    transition: background 120ms, color 120ms;
  }
  .di-cell:hover:not(:disabled):not(.di-selected) {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .di-cell:focus {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 4px;
  }
  .di-cell:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 4px;
  }
  .di-cell.di-out {
    color: var(--bone-4);
  }
  .di-cell.di-out:hover:not(:disabled):not(.di-selected) {
    color: var(--bone-3);
  }
  /* Today affordance — box-shadow inset (not border) so the 1px ring
     doesn't shift the 32px cell's content vs borderless cells, which
     would jitter the grid as the user navigates. ui.md §Today detail. */
  .di-cell.di-today {
    box-shadow: inset 0 0 0 1px var(--accent);
    color: var(--bone-0);
  }
  /* Selected cell — accent fill wins over the today ring when both
     apply (see ui.md state matrix: "Selected + today"). */
  .di-cell.di-selected {
    background: var(--accent);
    color: var(--ink-0);
    box-shadow: none;
  }
  .di-cell.di-selected:hover:not(:disabled) {
    background: var(--accent-hi);
  }
  .di-cell.di-selected:focus {
    outline: 2px solid var(--accent-hi);
    outline-offset: 2px;
  }
  .di-cell.di-selected:focus-visible {
    outline: 2px solid var(--accent-hi);
    outline-offset: 2px;
  }
  .di-cell.di-disabled,
  .di-cell:disabled {
    color: var(--bone-4);
    cursor: not-allowed;
  }
  .di-cell.di-disabled:hover,
  .di-cell:disabled:hover {
    background: transparent;
  }

  /* ── Footer row ──────────────────────────────────────────────────── */

  .di-foot {
    display: flex;
    justify-content: space-between;
    align-items: center;
    border-top: 1px solid var(--hairline);
    margin-top: 0.5rem;
    padding-top: 0.5rem;
  }
  .di-foot-btn {
    appearance: none;
    font-family: inherit;
    font-size: 0.78rem;
    color: var(--bone-2);
    background: transparent;
    border: none;
    cursor: pointer;
    padding: 0.25rem 0.5rem;
    border-radius: 4px;
    transition: background 120ms, color 120ms;
  }
  .di-foot-btn:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
  .di-foot-btn:focus {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .di-foot-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
