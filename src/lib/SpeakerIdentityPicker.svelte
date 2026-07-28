<!--
  SpeakerIdentityPicker — #646 (Phase 2) agent-local three-source speaker
  identity picker for the in-call co-pilot reference-rail roster (CrmContextLane).

  DISTINCT from the shared `SpeakerRenamePicker` (that one is the post-call
  transcript/participant rename against the org roster + free-form). This one
  assigns a LIVE diarized speaker to one of three identity sources:
    (a) Zoho contact  — typeahead via `invoke("zoho_search_records",
        { module: "Contacts", q })` → kind "zoho_contact" (+ contact_id);
    (b) internal user — the org roster via `invoke("org_members")` → kind
        "internal_user" (+ user_id);
    (c) adhoc         — a free-form typed name → kind "adhoc".

  It emits the chosen identity via `onpick` and does NOT call the assign endpoint
  itself (the parent owns the `live_speaker_identity` invoke + store write). The
  interaction shape (source combobox + listbox + free-form CTA + keyboard nav +
  outside-click dismiss) mirrors SpeakerRenamePicker; the styles are
  COMPONENT-SCOPED (`.sip-*`, none in app.css — HR#1 untouched). "Zoho" is a
  sanctioned integration name (HR#2); no other vendor names, no `{@html}`.
-->
<script lang="ts" module>
  import type { SpeakerIdentityKind } from "@aftercalls/shared/types";

  /** Emitted on commit. Exactly the identity the parent needs to build the
   *  `live_speaker_identity` assign args (channel/speakerLabel come from the
   *  roster row the picker was opened on). */
  export type PickedIdentity = {
    kind: SpeakerIdentityKind;
    display_name: string;
    contact_id?: string;
    user_id?: string;
  };
</script>

<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import Avatar from "@aftercalls/shared/ui/Avatar.svelte";
  import type { OrgMember } from "@aftercalls/shared/types";

  type Source = "contact" | "team";
  type ContactHit = { id: string; name: string; secondary?: string | null };

  let {
    speakerLabel = "",
    onpick,
    oncancel,
  }: {
    // The diarization label of the speaker being assigned — for the aria label
    // + the free-form CTA context only.
    speakerLabel?: string;
    onpick: (identity: PickedIdentity) => void;
    oncancel: () => void;
  } = $props();

  // ── Source toggle (Zoho contact ↔ teammate) ─────────────────────────────
  let source = $state<Source>("contact");
  let query = $state("");

  let containerEl = $state<HTMLDivElement | null>(null);
  let inputEl = $state<HTMLInputElement | null>(null);
  let activeIdx = $state(-1);
  const listboxId = `sip-list-${Math.random().toString(36).slice(2, 10)}`;

  // Snapshot the opener so every dismiss path returns focus to the trigger
  // (mirrors SpeakerRenamePicker's focus discipline).
  const openerEl: HTMLElement | null =
    typeof document !== "undefined" &&
    document.activeElement instanceof HTMLElement &&
    document.activeElement !== document.body
      ? document.activeElement
      : null;

  // ── Zoho contact search ─────────────────────────────────────────────────
  let contactHits = $state<ContactHit[]>([]);
  let searching = $state(false);
  let searchError = $state(false);
  let searchTimer = 0;
  // Guards against an out-of-order search landing (a slow early request can't
  // clobber a fast later one).
  let searchSeq = 0;

  function scheduleSearch() {
    clearTimeout(searchTimer);
    searchError = false;
    activeIdx = -1;
    const q = query.trim();
    if (q.length < 2) {
      contactHits = [];
      searching = false;
      return;
    }
    searchTimer = window.setTimeout(runSearch, 150);
  }

  async function runSearch() {
    const q = query.trim();
    if (q.length < 2) return;
    const seq = ++searchSeq;
    searching = true;
    searchError = false;
    try {
      const out = (await invoke("zoho_search_records", {
        module: "Contacts",
        q,
      })) as { results?: ContactHit[] };
      if (seq !== searchSeq) return;
      contactHits = out.results ?? [];
      activeIdx = -1;
    } catch {
      if (seq !== searchSeq) return;
      // Soft degrade — a bone error row, never a red panel. Teammate + adhoc
      // sources still work.
      searchError = true;
      contactHits = [];
      activeIdx = -1;
    } finally {
      if (seq === searchSeq) searching = false;
    }
  }

  function retrySearch() {
    searchError = false;
    void runSearch();
  }

  // ── Teammate roster (loaded once, filtered locally) ─────────────────────
  let roster = $state<OrgMember[]>([]);
  let rosterLoaded = $state(false);
  let rosterError = $state(false);
  async function loadRoster() {
    if (rosterLoaded || roster.length > 0) return;
    try {
      roster = (await invoke<OrgMember[]>("org_members")) ?? [];
      rosterError = false;
    } catch {
      rosterError = true;
    } finally {
      rosterLoaded = true;
    }
  }

  function setSource(next: Source) {
    if (source === next) return;
    source = next;
    activeIdx = -1;
    if (next === "team") void loadRoster();
    else if (query.trim().length >= 2) void runSearch();
    queueMicrotask(() => inputEl?.focus());
  }

  // ── Rows (source-dependent) + the free-form CTA ─────────────────────────
  type Row =
    | { kind: "contact"; hit: ContactHit }
    | { kind: "member"; member: OrgMember }
    | { kind: "cta"; typed: string };

  let memberRows = $derived.by<Row[]>(() => {
    const q = query.trim().toLowerCase();
    return roster
      .filter(
        (m) =>
          !q ||
          m.display_name.toLowerCase().includes(q) ||
          m.email.toLowerCase().includes(q),
      )
      .map((m) => ({ kind: "member" as const, member: m }));
  });

  let rows = $derived.by<Row[]>(() => {
    const base: Row[] =
      source === "contact"
        ? contactHits.map((hit) => ({ kind: "contact" as const, hit }))
        : memberRows;
    const trimmed = query.trim();
    const q = trimmed.toLowerCase();
    // Free-form ("adhoc") CTA — offered when there's typed text that doesn't
    // exactly match a row above. Reachable from either source, so a name that
    // isn't a Zoho contact or a teammate can still label the speaker.
    const exact = base.some(
      (r) =>
        (r.kind === "contact" && r.hit.name.toLowerCase() === q) ||
        (r.kind === "member" && r.member.display_name.toLowerCase() === q),
    );
    const out = [...base];
    if (trimmed.length > 0 && !exact) {
      out.push({ kind: "cta", typed: trimmed });
    }
    return out;
  });

  // Keep activeIdx in range as rows filter live.
  $effect(() => {
    if (activeIdx >= rows.length) activeIdx = rows.length - 1;
  });

  // ── Commit ──────────────────────────────────────────────────────────────
  function emit(row: Row) {
    if (row.kind === "contact") {
      onpick({
        kind: "zoho_contact",
        display_name: row.hit.name,
        contact_id: row.hit.id,
      });
    } else if (row.kind === "member") {
      onpick({
        kind: "internal_user",
        display_name: row.member.display_name,
        user_id: row.member.id,
      });
    } else {
      onpick({ kind: "adhoc", display_name: row.typed });
    }
  }

  function commit() {
    if (activeIdx >= 0 && activeIdx < rows.length) {
      emit(rows[activeIdx]);
      return;
    }
    const trimmed = query.trim();
    if (!trimmed) {
      oncancel();
      return;
    }
    // No highlighted row — a typed teammate/contact name that exactly matches a
    // row commits that row; otherwise fall through to a free-form adhoc name.
    const q = trimmed.toLowerCase();
    const match = rows.find(
      (r) =>
        (r.kind === "contact" && r.hit.name.toLowerCase() === q) ||
        (r.kind === "member" && r.member.display_name.toLowerCase() === q),
    );
    if (match) emit(match);
    else onpick({ kind: "adhoc", display_name: trimmed });
  }

  function handleKeydown(e: KeyboardEvent) {
    e.stopPropagation();
    if (e.key === "ArrowDown") {
      e.preventDefault();
      if (rows.length === 0) return;
      activeIdx = activeIdx + 1 >= rows.length ? 0 : activeIdx + 1;
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      if (rows.length === 0) return;
      activeIdx = activeIdx <= 0 ? rows.length - 1 : activeIdx - 1;
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      commit();
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      oncancel();
      return;
    }
  }

  // Capture-phase outside-click dismissal (mirrors SpeakerRenamePicker).
  function onOutsidePointerDown(e: PointerEvent) {
    const t = e.target as Node | null;
    if (!t) return;
    if (containerEl && containerEl.contains(t)) return;
    oncancel();
  }
  $effect(() => {
    document.addEventListener("pointerdown", onOutsidePointerDown, true);
    return () =>
      document.removeEventListener("pointerdown", onOutsidePointerDown, true);
  });

  // Autofocus the input on mount; restore focus to the opener on unmount.
  $effect(() => {
    inputEl?.focus();
  });
  $effect(() => {
    return () => {
      if (openerEl && document.contains(openerEl)) {
        try {
          openerEl.focus();
        } catch {
          /* opener detached / non-focusable — no-op */
        }
      }
    };
  });

  let placeholder = $derived(
    source === "contact" ? "Search Zoho contacts…" : "Search teammates…",
  );
</script>

<div
  bind:this={containerEl}
  class="sip"
  onkeydown={handleKeydown}
  role="presentation"
>
  <div class="sip-sources" role="tablist" aria-label="Identity source">
    <button
      type="button"
      role="tab"
      aria-selected={source === "contact"}
      class="sip-source"
      class:active={source === "contact"}
      onclick={() => setSource("contact")}
    >
      Contact
    </button>
    <button
      type="button"
      role="tab"
      aria-selected={source === "team"}
      class="sip-source"
      class:active={source === "team"}
      onclick={() => setSource("team")}
    >
      Teammate
    </button>
  </div>

  <div class="sip-row">
    <input
      bind:this={inputEl}
      class="sip-input"
      type="text"
      role="combobox"
      {placeholder}
      autocomplete="off"
      aria-label={`Assign ${speakerLabel || "speaker"} — search a contact, teammate, or type a name`}
      aria-expanded="true"
      aria-controls={listboxId}
      aria-autocomplete="list"
      aria-activedescendant={activeIdx >= 0 ? `sip-opt-${activeIdx}` : undefined}
      bind:value={query}
      oninput={() => {
        if (source === "contact") scheduleSearch();
        else activeIdx = -1;
      }}
    />
    <button type="button" class="sip-cancel" onclick={oncancel}>Cancel</button>
  </div>

  <div class="sip-list" role="listbox" id={listboxId} aria-busy={searching}>
    {#if source === "contact" && searching}
      <div class="sip-hint">Searching…</div>
    {:else if source === "contact" && searchError}
      <div class="sip-error-row">
        <span>Couldn't reach Zoho just now.</span>
        <button type="button" class="sip-retry" onclick={retrySearch}>
          Retry
        </button>
      </div>
    {:else if source === "team" && !rosterLoaded}
      <div class="sip-hint">Loading teammates…</div>
    {:else if source === "team" && rosterError}
      <div class="sip-hint sip-hint-err">Couldn't load teammates.</div>
    {:else if rows.length === 0}
      <div class="sip-hint">
        {#if source === "contact" && query.trim().length < 2}
          Type at least 2 letters to search.
        {:else if source === "contact"}
          No contacts match — type a name to use it as-is.
        {:else}
          No teammates match — type a name to use it as-is.
        {/if}
      </div>
    {:else}
      {#each rows as row, i (row.kind === "contact"
        ? "c:" + row.hit.id
        : row.kind === "member"
          ? "m:" + row.member.id
          : "cta")}
        {#if row.kind === "cta"}
          <button
            type="button"
            id="sip-opt-{i}"
            class="sip-opt sip-cta"
            class:active={i === activeIdx}
            role="option"
            aria-selected={i === activeIdx}
            onmouseenter={() => (activeIdx = i)}
            onclick={() => emit(row)}
          >
            <span class="sip-cta-glyph" aria-hidden="true">+</span>
            <span class="sip-cta-label">Use "{row.typed}" as a name</span>
          </button>
        {:else}
          <button
            type="button"
            id="sip-opt-{i}"
            class="sip-opt"
            class:active={i === activeIdx}
            role="option"
            aria-selected={i === activeIdx}
            onmouseenter={() => (activeIdx = i)}
            onclick={() => emit(row)}
          >
            {#if row.kind === "contact"}
              <Avatar name={row.hit.name} size={20} />
              <span class="sip-opt-body">
                <span class="sip-opt-name">{row.hit.name}</span>
                {#if row.hit.secondary}
                  <span class="sip-opt-sec">{row.hit.secondary}</span>
                {/if}
              </span>
            {:else}
              <Avatar name={row.member.display_name} size={20} />
              <span class="sip-opt-body">
                <span class="sip-opt-name">{row.member.display_name}</span>
                <span class="sip-opt-sec">{row.member.email}</span>
              </span>
            {/if}
          </button>
        {/if}
      {/each}
    {/if}
  </div>
</div>

<style>
  .sip {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    width: 100%;
    min-width: 0;
  }

  /* ── Source toggle (Contact ↔ Teammate) — the scope-pill idiom, scoped ── */
  .sip-sources {
    display: flex;
    gap: 0.3rem;
  }
  .sip-source {
    padding: 0.22rem 0.6rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.74rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .sip-source:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .sip-source.active {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .sip-source:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* ── Input row ── */
  .sip-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .sip-input {
    flex: 1 1 auto;
    min-width: 0;
    padding: 0.45rem 0.6rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.85rem;
  }
  .sip-input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-color: var(--accent);
  }
  .sip-cancel {
    flex-shrink: 0;
    padding: 0.35rem 0.6rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.74rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
  }
  .sip-cancel:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .sip-cancel:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* ── Listbox ── */
  .sip-list {
    display: flex;
    flex-direction: column;
    max-height: 12rem;
    overflow-y: auto;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    background: var(--ink-0);
    padding: 0.2rem 0;
  }
  .sip-hint {
    padding: 0.45rem 0.6rem;
    font-size: 0.78rem;
    color: var(--bone-3);
    line-height: 1.4;
  }
  .sip-hint-err {
    color: var(--bone-2);
  }
  .sip-error-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    padding: 0.4rem 0.6rem;
    font-size: 0.78rem;
    color: var(--bone-2);
  }
  .sip-retry {
    padding: 0.25rem 0.6rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.74rem;
    cursor: pointer;
  }
  .sip-retry:hover {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }
  .sip-retry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .sip-opt {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 0.55rem;
    width: 100%;
    padding: 0.38rem 0.6rem;
    border: none;
    background: transparent;
    color: var(--bone-1);
    text-align: left;
    cursor: pointer;
    font: inherit;
    transition: background 0.15s, color 0.15s;
  }
  .sip-opt:hover,
  .sip-opt.active {
    background: var(--accent-soft);
    color: var(--bone-0);
  }
  .sip-opt-body {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 0;
  }
  .sip-opt-name {
    font-size: 0.84rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .sip-opt-sec {
    font-size: 0.72rem;
    color: var(--bone-3);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  /* Free-form "use this name" CTA — accent "+" glyph, matches the tag-popover
     add idiom (design.md §Tag chip). */
  .sip-cta {
    gap: 0.5rem;
  }
  .sip-cta-glyph {
    display: inline-flex;
    justify-content: center;
    width: 1.25rem;
    flex-shrink: 0;
    color: var(--accent);
    font-size: 1rem;
    font-weight: 600;
  }
  .sip-cta-label {
    font-size: 0.82rem;
    overflow-wrap: anywhere;
  }
</style>
