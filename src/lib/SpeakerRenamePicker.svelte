<script lang="ts" module>
  // Shape of a member entry the picker expects. Parent maps its
  // source-specific roster (portal api vs agent Tauri invoke) onto
  // this minimal triple before passing.
  export type OrgMemberLite = {
    id: string;
    display_name: string;
    email: string;
  };

  // One of the shared "recently used" entries. Just the name — we
  // resolve it back to a roster member (to get the id) at Enter-time.
  export type RecentRow = { name: string };

  // Emitted on pick. `user` is the matched roster member (FK path) or
  // null when the typed text doesn't resolve to a teammate (free-form
  // path). `freeText` mirrors the typed value for the free-form path;
  // null when a user was picked. Exactly one of the two is non-null.
  export type SpeakerPick =
    | { user: OrgMemberLite; freeText: null }
    | { user: null; freeText: string };
</script>

<script lang="ts">
  // Mirror-pair component — byte-identical between
  // portal/src/lib/SpeakerRenamePicker.svelte and
  // agent/src/lib/SpeakerRenamePicker.svelte (same discipline as
  // Avatar.svelte; see learnings.md #5). Reviewer diffs the two on
  // every touch.
  //
  // Styles are component-scoped deliberately — none of these classes
  // live in app.css. The `diff portal/src/app.css agent/src/app.css`
  // invariant is preserved without any CSS-mirror work.
  //
  // Used by both the transcript per-utterance rename and the
  // Participants chip rename on calls/[id]. Dual outcome:
  //   • clicked a roster row (or typed string exactly matches a
  //     roster display_name) → `onpick({ user, freeText: null })`.
  //   • typed text with no roster match → `onpick({ user: null,
  //     freeText })`.
  // The parent decides what to do with either — this component does
  // NOT call the API itself.

  import Avatar from "./Avatar.svelte";

  type Props = {
    value: string;
    roster: OrgMemberLite[];
    rosterLoaded: boolean;
    rosterError?: boolean;
    recents?: RecentRow[];
    savingLabel?: string;
    saving?: boolean;
    onpick: (pick: SpeakerPick) => void;
    oncancel: () => void;
    // Some surfaces (participants chip) render the picker inline so
    // the dropdown anchors flush left; transcript utt-editor gives
    // the picker its own surface. The chrome only changes where the
    // dropdown positions relative to the input — variant is cosmetic.
    variant?: "chip" | "stack";
    autofocus?: boolean;
    // v0.4.0 Phase 2 (#19): action-item assignee picker reuses this
    // component with different empty-state vocabulary. Default copy
    // stays verbatim from the speaker-rename surface; call sites that
    // want different wording pass `placeholder` + `noMatchHint`.
    // Added via "add a prop, don't fork the component" per the
    // ui-phase-2 spec — keeps the mirror-pair count at one.
    placeholder?: string;
    noMatchHint?: string;
  };

  let {
    value = $bindable(""),
    roster,
    rosterLoaded,
    rosterError = false,
    recents = [],
    savingLabel = "Save",
    saving = false,
    onpick,
    oncancel,
    variant = "stack",
    autofocus = true,
    placeholder = "Name or teammate…",
    noMatchHint = "No match — press Enter to save as free-form name.",
  }: Props = $props();

  let inputEl = $state<HTMLInputElement | null>(null);
  let activeIdx = $state(-1);
  // v0.4.1 (#122 C.2): dropdown collapses after `pickRow` / `commit`
  // and reopens on input focus/click/typing or arrow keys. Default
  // `true` preserves the "open-on-mount" behaviour the transcript +
  // participants-chip call-sites rely on (they unmount the picker
  // immediately after pick so this state is moot there). For the
  // action-item editor + add-composer call-sites, the picker stays
  // mounted across picks — this flag is the actual UX knob.
  let dropdownOpen = $state(true);
  // Stable per-instance listbox id so aria-controls matches the
  // rendered `role="listbox"` element. $props.id() would be nicer but
  // isn't available on Svelte 5 stable without a generator — a
  // random-enough suffix keeps two pickers on the same page (e.g. one
  // participant chip + one transcript editor open at once) distinct.
  const listboxId = `srp-list-${Math.random().toString(36).slice(2, 10)}`;

  // Dropdown rows: recents filtered to names still in the current
  // roster (so deactivated teammates drop out), then roster filtered
  // by case-insensitive substring on display_name + email.
  type Row =
    | { kind: "recent"; name: string }
    | { kind: "member"; member: OrgMemberLite };

  let rows = $derived.by<Row[]>(() => {
    const q = value.trim().toLowerCase();
    const rosterByName = new Map(
      roster.map((m) => [m.display_name.toLowerCase(), m]),
    );
    const recentRows: Row[] = q
      ? []
      : recents
          .filter((r) => rosterByName.has(r.name.toLowerCase()))
          .slice(0, 3)
          .map((r) => ({ kind: "recent" as const, name: r.name }));
    const memberRows: Row[] = roster
      .filter((m) => {
        if (!q) return true;
        return (
          m.display_name.toLowerCase().includes(q) ||
          m.email.toLowerCase().includes(q)
        );
      })
      .map((m) => ({ kind: "member" as const, member: m }));
    return [...recentRows, ...memberRows];
  });

  // Keep activeIdx in-range as the list filters live; -1 means "no
  // row is active" — Enter then takes the free-form path.
  $effect(() => {
    if (activeIdx >= rows.length) activeIdx = rows.length - 1;
  });

  $effect(() => {
    if (autofocus && inputEl) {
      inputEl.focus();
      // Select-all so a retype immediately replaces; matches the
      // implicit bind-value autofocus behaviour the bare <input> had.
      inputEl.select();
    }
  });

  function resolveMemberByName(name: string): OrgMemberLite | null {
    const needle = name.trim().toLowerCase();
    if (!needle) return null;
    return roster.find((m) => m.display_name.toLowerCase() === needle) ?? null;
  }

  function pickRow(row: Row) {
    if (row.kind === "member") {
      onpick({ user: row.member, freeText: null });
    } else {
      // Recents row — resolve back to a roster member by name. If it
      // doesn't resolve (teammate deactivated between reads), fall
      // through as free-form so the Save isn't lost.
      const m = resolveMemberByName(row.name);
      if (m) {
        onpick({ user: m, freeText: null });
      } else {
        onpick({ user: null, freeText: row.name });
      }
    }
    // v0.4.1 (#122 C.2): collapse after firing `onpick`. Setting
    // this last so a parent that re-seeds `value` in response to
    // the pick can't inadvertently re-open the list via a
    // dependent effect.
    dropdownOpen = false;
  }

  function commit() {
    if (activeIdx >= 0 && activeIdx < rows.length) {
      pickRow(rows[activeIdx]);
      return;
    }
    // No row highlighted. If typed text matches a roster name
    // exactly (case-insensitive), treat it as a pick; otherwise
    // free-form.
    const member = resolveMemberByName(value);
    if (member) {
      onpick({ user: member, freeText: null });
      dropdownOpen = false;
      return;
    }
    const trimmed = value.trim();
    if (!trimmed) {
      oncancel();
      return;
    }
    onpick({ user: null, freeText: trimmed });
    dropdownOpen = false;
  }

  function handleKeydown(e: KeyboardEvent) {
    // Isolate picker key handling from the parent row (.utt) and any
    // ancestor keydown handlers. Without this the transcript row's
    // Space/Enter seek handler fires on every typed space.
    e.stopPropagation();
    if (e.key === "ArrowDown") {
      e.preventDefault();
      // v0.4.1 (#122 C.2): arrow keys reopen the list — keyboard
      // users expect arrows to surface choices after a pick.
      dropdownOpen = true;
      if (rows.length === 0) return;
      activeIdx = activeIdx + 1 >= rows.length ? 0 : activeIdx + 1;
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      dropdownOpen = true;
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
</script>

<div class="srp" class:srp-chip={variant === "chip"} onkeydown={handleKeydown} role="presentation">
  <div class="srp-row">
    <input
      class="srp-input"
      type="text"
      {placeholder}
      autocomplete="off"
      aria-label="Rename speaker — start typing or choose a teammate"
      role="combobox"
      aria-expanded={dropdownOpen}
      aria-autocomplete="list"
      aria-controls={listboxId}
      bind:this={inputEl}
      bind:value
      onfocus={() => (dropdownOpen = true)}
      onclick={() => (dropdownOpen = true)}
      oninput={() => (dropdownOpen = true)}
    />
    <button
      type="button"
      class="srp-save"
      disabled={saving}
      onclick={commit}
    >
      {saving ? "…" : savingLabel}
    </button>
    <button type="button" class="srp-cancel" onclick={oncancel}>
      Cancel
    </button>
  </div>
  {#if dropdownOpen}
    <div class="srp-list" role="listbox" id={listboxId}>
      {#if !rosterLoaded}
        <div class="srp-hint">Loading teammates…</div>
      {:else if rosterError}
        <div class="srp-hint srp-hint-err">
          Couldn't load teammates. Free-form save still works.
        </div>
      {:else if roster.length === 0}
        <div class="srp-hint">No teammates in your org yet.</div>
      {:else if rows.length === 0}
        <div class="srp-hint">
          {noMatchHint}
        </div>
      {:else}
        {#each rows as row, i (row.kind === "member" ? "m:" + row.member.id : "r:" + row.name)}
          {#if row.kind === "recent" && (i === 0 || rows[i - 1].kind !== "recent")}
            <div class="srp-hdr">Recently used</div>
          {/if}
          {#if row.kind === "member" && (i === 0 || rows[i - 1].kind !== "member")}
            <div class="srp-hdr">All members</div>
          {/if}
          <button
            type="button"
            class="srp-row-btn"
            class:active={i === activeIdx}
            role="option"
            aria-selected={i === activeIdx}
            {...{ "aria-description": "Linked teammate" }}
            onmouseenter={() => (activeIdx = i)}
            onclick={() => pickRow(row)}
          >
            {#if row.kind === "recent"}
              <Avatar name={row.name} size={20} />
              <span class="srp-stack">
                <span class="srp-name">{row.name}</span>
              </span>
            {:else}
              <Avatar name={row.member.display_name} size={20} />
              <span class="srp-stack">
                <span class="srp-name">{row.member.display_name}</span>
                <span class="srp-email">{row.member.email}</span>
              </span>
            {/if}
          </button>
        {/each}
      {/if}
    </div>
  {/if}
</div>

<style>
  .srp {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    width: 100%;
  }

  .srp-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
  }

  .srp-input {
    flex: 1 1 12rem;
    min-width: 10rem;
    max-width: 18rem;
    padding: 0.45rem 0.65rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.9rem;
  }
  .srp-input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .srp-chip .srp-input {
    flex-basis: 10rem;
    font-size: 0.85rem;
    padding: 0.35rem 0.55rem;
  }

  .srp-save,
  .srp-cancel {
    padding: 0.35rem 0.9rem;
    font-size: 0.8rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    cursor: pointer;
    font: inherit;
  }
  .srp-save {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 500;
  }
  .srp-save:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .srp-list {
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    margin-top: 0.35rem;
    min-width: min(18rem, 100%);
    max-height: 16rem;
    overflow-y: auto;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-1);
    box-shadow: 0 10px 24px -8px rgba(0, 0, 0, 0.45);
    z-index: 20;
    padding: 0.3rem 0;
  }

  .srp-hdr {
    padding: 0.35rem 0.7rem 0.2rem;
    font-size: 0.68rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
  }

  .srp-hint {
    padding: 0.5rem 0.7rem;
    font-size: 0.78rem;
    color: var(--bone-3);
  }
  .srp-hint-err {
    color: var(--bone-2);
  }

  .srp-row-btn {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 0.6rem;
    width: 100%;
    padding: 0.35rem 0.7rem;
    border: none;
    background: transparent;
    color: var(--bone-1);
    text-align: left;
    cursor: pointer;
    font: inherit;
    transition: background 0.15s, color 0.15s;
  }
  .srp-row-btn:hover,
  .srp-row-btn.active {
    background: var(--accent-soft);
    color: var(--bone-0);
  }

  .srp-stack {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 0;
  }

  .srp-name {
    font-size: 0.85rem;
  }
  .srp-email {
    font-size: 0.72rem;
    color: var(--bone-3);
  }
</style>
