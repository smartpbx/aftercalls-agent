<script lang="ts">
  import { tick } from "svelte";
  import {
    filterCommandPaletteItems,
    type CommandPaletteItem,
    type CommandPaletteSection,
    type CommandPaletteSectionId,
    type CommandPaletteVisibleItem,
  } from "../commandPalette";
  import {
    closeCommandPalette,
    shortcutsState,
  } from "../shortcuts.svelte";

  let {
    sections = [],
    disabled = false,
  } = $props<{
    sections?: CommandPaletteSection[];
    disabled?: boolean;
  }>();

  let query = $state("");
  let activeIndex = $state(0);
  let inputEl = $state<HTMLInputElement | null>(null);
  let lastFocusBeforeOpen: HTMLElement | null = null;
  let providerRows = $state<Record<string, CommandPaletteItem[]>>({});
  let loading = $state<Record<string, boolean>>({});
  let errors = $state<Record<string, boolean>>({});
  let loadSeq = 0;
  let loadedProviderKey = "";

  const open = $derived(shortcutsState.commandOpen && !disabled);
  const providerKey = $derived(
    sections
      .filter((section) => section.provider)
      .map((section) => `${section.id}:${section.providerKey ?? section.title}`)
      .join("|"),
  );

  const visibleRows = $derived.by<CommandPaletteVisibleItem[]>(() => {
    const rows: CommandPaletteVisibleItem[] = [];
    for (const section of sections) {
      const staticItems = section.items ?? [];
      const asyncItems = providerRows[section.id] ?? [];
      const items = filterCommandPaletteItems(
        [...staticItems, ...asyncItems],
        query,
        section.limit ?? 8,
      );
      if (items.length > 0) {
        for (const item of items) {
          rows.push({ kind: "item", item, sectionId: section.id });
        }
        continue;
      }
      if (loading[section.id]) {
        rows.push({
          kind: "loading",
          id: `${section.id}:loading`,
          label: section.loadingLabel ?? `Loading ${section.title.toLocaleLowerCase()}...`,
          sectionId: section.id,
        });
      } else if (errors[section.id]) {
        rows.push({
          kind: "error",
          id: `${section.id}:error`,
          label: section.errorLabel ?? `${section.title} could not load`,
          sectionId: section.id,
        });
      }
    }
    if (rows.length === 0) rows.push({ kind: "empty", id: "empty" });
    return rows;
  });

  const itemRows = $derived(visibleRows.filter((row) => row.kind === "item"));
  const activeItem = $derived(itemRows[activeIndex]?.item ?? null);
  const activeId = $derived(activeItem ? rowId(activeItem) : undefined);
  const resultCount = $derived(itemRows.length);

  function rowId(item: CommandPaletteItem): string {
    return `command-palette-row-${item.section}-${item.id}`.replace(/[^a-zA-Z0-9_-]/g, "-");
  }

  function sectionTitle(id: CommandPaletteSectionId): string {
    return sections.find((section) => section.id === id)?.title ?? id;
  }

  function rowSectionId(row: CommandPaletteVisibleItem): CommandPaletteSectionId | null {
    return row.kind === "empty" ? null : row.sectionId;
  }

  function showSectionHeader(row: CommandPaletteVisibleItem, idx: number): boolean {
    const id = rowSectionId(row);
    if (!id) return false;
    return idx === 0 || rowSectionId(visibleRows[idx - 1]) !== id;
  }

  function close() {
    closeCommandPalette();
    query = "";
    const target = lastFocusBeforeOpen;
    lastFocusBeforeOpen = null;
    if (target && typeof target.focus === "function") {
      queueMicrotask(() => target.focus());
    }
  }

  async function run(item: CommandPaletteItem | null) {
    if (!item) return;
    closeCommandPalette();
    query = "";
    try {
      await item.run();
    } finally {
      lastFocusBeforeOpen = null;
    }
  }

  function move(delta: number) {
    if (itemRows.length === 0) return;
    activeIndex = (activeIndex + delta + itemRows.length) % itemRows.length;
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      move(1);
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      move(-1);
      return;
    }
    if (e.key === "Home") {
      e.preventDefault();
      activeIndex = 0;
      return;
    }
    if (e.key === "End") {
      e.preventDefault();
      activeIndex = Math.max(0, itemRows.length - 1);
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      void run(activeItem);
    }
  }

  $effect(() => {
    if (disabled && shortcutsState.commandOpen) {
      closeCommandPalette();
    }
  });

  $effect(() => {
    if (providerKey !== loadedProviderKey) {
      loadedProviderKey = providerKey;
      providerRows = {};
      loading = {};
      errors = {};
      loadSeq += 1;
    }
  });

  $effect(() => {
    if (!open) {
      providerRows = {};
      loading = {};
      errors = {};
      loadSeq += 1;
    }
  });

  $effect(() => {
    if (!open) return;
    lastFocusBeforeOpen = (document.activeElement as HTMLElement | null) ?? null;
    void tick().then(() => inputEl?.focus());
  });

  $effect(() => {
    activeIndex = Math.min(activeIndex, Math.max(0, itemRows.length - 1));
  });

  $effect(() => {
    if (!open) return;
    const currentSeq = ++loadSeq;
    const providers = sections.filter((section) => section.provider);
    const nextLoading: Record<string, boolean> = {};
    const nextErrors: Record<string, boolean> = {};
    for (const section of providers) {
      nextLoading[section.id] = true;
      nextErrors[section.id] = false;
    }
    loading = nextLoading;
    errors = nextErrors;
    const timer = window.setTimeout(() => {
      for (const section of providers) {
        const provider = section.provider;
        if (!provider) continue;
        provider(query)
          .then((items) => {
            if (currentSeq !== loadSeq) return;
            providerRows = { ...providerRows, [section.id]: items };
            loading = { ...loading, [section.id]: false };
          })
          .catch(() => {
            if (currentSeq !== loadSeq) return;
            providerRows = { ...providerRows, [section.id]: [] };
            loading = { ...loading, [section.id]: false };
            errors = { ...errors, [section.id]: true };
          });
      }
    }, 120);
    return () => window.clearTimeout(timer);
  });

  function retry(sectionId: CommandPaletteSectionId) {
    const section = sections.find((s) => s.id === sectionId);
    if (!section?.provider) return;
    const currentSeq = ++loadSeq;
    loading = { ...loading, [sectionId]: true };
    errors = { ...errors, [sectionId]: false };
    section
      .provider(query)
      .then((items) => {
        if (currentSeq !== loadSeq) return;
        providerRows = { ...providerRows, [sectionId]: items };
        loading = { ...loading, [sectionId]: false };
      })
      .catch(() => {
        if (currentSeq !== loadSeq) return;
        providerRows = { ...providerRows, [sectionId]: [] };
        loading = { ...loading, [sectionId]: false };
        errors = { ...errors, [sectionId]: true };
      });
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="cp-backdrop" onclick={close} role="presentation"></div>
  <div
    class="cp-modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby="command-palette-title"
    onkeydown={onKeydown}
  >
    <header class="cp-head">
      <div class="cp-title-row">
        <h2 id="command-palette-title">Command palette</h2>
        <button
          type="button"
          class="cp-close"
          aria-label="Close command palette"
          onclick={close}
        >×</button>
      </div>
      <input
        bind:this={inputEl}
        bind:value={query}
        class="cp-search"
        role="combobox"
        aria-label="Search commands"
        aria-expanded="true"
        aria-controls="command-palette-results"
        aria-activedescendant={activeId}
        placeholder="Search calls, actions, pages, or commands"
      />
      <span class="cp-live" aria-live="polite">
        {resultCount === 1 ? "1 result" : `${resultCount} results`}
      </span>
    </header>

    <div id="command-palette-results" class="cp-body" role="listbox">
      {#each visibleRows as row, idx (row.kind === "item" ? rowId(row.item) : row.id)}
        {#if showSectionHeader(row, idx)}
          <div class="cp-section-head" aria-hidden="true">
            {sectionTitle(rowSectionId(row) ?? "calls")}
          </div>
        {/if}
        {#if row.kind === "item"}
          {@const itemIndex = itemRows.findIndex((candidate) => candidate.kind === "item" && candidate.item === row.item)}
          <button
            id={rowId(row.item)}
            type="button"
            class="cp-row"
            class:active={itemIndex === activeIndex}
            role="option"
            aria-selected={itemIndex === activeIndex}
            title={row.item.title}
            onmouseenter={() => (activeIndex = itemIndex)}
            onclick={() => run(row.item)}
          >
            <span class="cp-row-main">
              <span class="cp-row-title">{row.item.title}</span>
              {#if row.item.subtitle}
                <span class="cp-row-subtitle">{row.item.subtitle}</span>
              {/if}
            </span>
            <span class="cp-row-meta">
              <span class="cp-chip">{row.item.kind}</span>
              {#if itemIndex === activeIndex}
                <kbd>Enter</kbd>
              {/if}
            </span>
          </button>
        {:else if row.kind === "loading"}
          <div class="cp-status-row" role="status">
            <span class="pip working" aria-hidden="true"></span>
            <span>{row.label}</span>
          </div>
        {:else if row.kind === "error"}
          <div class="cp-status-row cp-error" role="status">
            <span>{row.label}</span>
            <button type="button" class="cp-retry" onclick={() => retry(row.sectionId)}>
              Retry
            </button>
          </div>
        {:else}
          <div class="cp-empty">
            <strong>No matches</strong>
            <span>Try a call title, action item, page, or command.</span>
          </div>
        {/if}
      {/each}
    </div>
  </div>
{/if}

<style>
  .cp-backdrop {
    position: fixed;
    inset: 0;
    z-index: 80;
    background: rgba(0, 0, 0, 0.5);
    backdrop-filter: blur(2px);
    -webkit-backdrop-filter: blur(2px);
    animation: cp-fade 100ms ease-out;
  }

  .cp-modal {
    position: fixed;
    z-index: 81;
    top: 12vh;
    left: 50%;
    transform: translateX(-50%);
    width: min(640px, calc(100vw - 2rem));
    max-height: min(72vh, 680px);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-lg, 14px);
    box-shadow: 0 24px 60px -14px rgba(0, 0, 0, 0.65);
    color: var(--bone-0);
    animation: cp-pop 100ms ease-out;
  }

  .cp-head {
    flex: 0 0 auto;
    padding: 1rem;
    border-bottom: 1px solid var(--hairline);
  }

  .cp-title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    margin-bottom: 0.75rem;
  }

  .cp-title-row h2 {
    margin: 0;
    color: var(--bone-2);
    font-size: 0.75rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .cp-close {
    appearance: none;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: var(--bone-3);
    cursor: pointer;
    font-size: 1.3rem;
    line-height: 1;
    padding: 0.1rem 0.45rem;
  }

  .cp-close:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }

  .cp-search {
    width: 100%;
    box-sizing: border-box;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.95rem;
    padding: 0.72rem 0.85rem;
    outline: none;
  }

  .cp-search:focus {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }

  .cp-live {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
  }

  .cp-body {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    padding: 0.45rem;
  }

  .cp-row {
    width: 100%;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    border: 1px solid transparent;
    border-radius: var(--radius);
    background: transparent;
    color: inherit;
    text-align: left;
    padding: 0.68rem 0.75rem;
    cursor: pointer;
    transition: background 120ms, border-color 120ms, color 120ms;
  }

  .cp-row:hover,
  .cp-row.active {
    background: var(--ink-2);
    border-color: var(--hairline-hi);
  }

  .cp-row-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.12rem;
  }

  .cp-row-title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--bone-0);
    font-weight: 500;
  }

  .cp-row-subtitle {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--bone-3);
    font-size: 0.8rem;
  }

  .cp-row-meta {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
  }

  .cp-chip,
  .cp-row-meta kbd {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--bone-3);
  }

  .cp-chip {
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 0.12rem 0.42rem;
    background: var(--ink-0);
  }

  .cp-row-meta kbd {
    border: 1px solid var(--hairline-hi);
    border-radius: 4px;
    background: var(--ink-3);
    color: var(--bone-1);
    padding: 0.08rem 0.34rem;
  }

  .cp-status-row,
  .cp-empty {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.8rem 0.9rem;
    color: var(--bone-3);
    font-size: 0.85rem;
  }

  .cp-error {
    color: var(--live);
  }

  .cp-retry {
    appearance: none;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: var(--ink-2);
    color: var(--bone-0);
    cursor: pointer;
    font: inherit;
    font-size: 0.78rem;
    font-weight: 600;
    margin-left: auto;
    padding: 0.22rem 0.55rem;
  }

  .cp-retry:hover {
    border-color: var(--hairline-hi);
    background: var(--ink-3);
  }

  .cp-empty {
    min-height: 8rem;
    justify-content: center;
    flex-direction: column;
    text-align: center;
  }

  .cp-empty strong {
    color: var(--bone-0);
    font-size: 0.95rem;
  }

  .cp-section-head {
    padding: 0.65rem 0.75rem 0.35rem;
    color: var(--bone-3);
    font-size: 0.68rem;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }

  @keyframes cp-fade {
    from { opacity: 0; }
    to { opacity: 1; }
  }

  @keyframes cp-pop {
    from {
      opacity: 0;
      transform: translate(-50%, -4px);
    }
    to {
      opacity: 1;
      transform: translate(-50%, 0);
    }
  }

  @media (max-width: 640px) {
    .cp-modal {
      top: 0.5rem;
      width: calc(100vw - 1rem);
      max-height: calc(100dvh - 1rem);
    }

    .cp-row {
      align-items: flex-start;
    }

    .cp-row-subtitle {
      white-space: normal;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }

    .cp-row-meta kbd {
      display: none;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .cp-backdrop,
    .cp-modal {
      animation: cp-fade 100ms ease-out;
    }
  }
</style>
