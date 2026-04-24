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

  // #171 bulk-pick payload. Parent calls renameSpeaker(from, to,
  // toUserId) with this. `from` is the context speaker the user opened
  // the picker on; exactly one of `user` / `freeText` is non-null,
  // same contract as SpeakerPick.
  export type SpeakerBulkPick = {
    from: string;
    user: OrgMemberLite | null;
    freeText: string | null;
  };
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
    // #170 · externals seen in the current call (speakers with no
    // linked teammate FK). Defaults to []; participants-chip
    // call-sites may leave this empty.
    externals?: string[];
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
    // #171 · context for the "Replace every X with…" bulk-rename row.
    // `contextSpeaker` is the speaker label the picker opened on
    // (usually `u.speaker` at startUttEdit time); `contextCount` is
    // the number of utterances with that speaker in the call. The
    // bulk row renders only when contextCount > 1 AND variant ===
    // "stack" (transcript editor). Participants-chip variant already
    // bulk-renames by definition; bulk row would be redundant there.
    contextSpeaker?: string;
    contextCount?: number;
    // #171 · emitted from the secondary picker commit. Parent calls
    // renameSpeaker(call.id, from, to, toUserId). Distinct from
    // `onpick` so the parent can branch (onpick = per-utterance or
    // applyToAll via the checkbox; onbulkpick = direct rename-all
    // from the picker without the utterance-editor ceremony).
    onbulkpick?: (pick: SpeakerBulkPick) => void;
  };

  let {
    value = $bindable(""),
    roster,
    rosterLoaded,
    rosterError = false,
    recents = [],
    externals = [],
    savingLabel = "Save",
    saving = false,
    onpick,
    oncancel,
    variant = "stack",
    autofocus = true,
    placeholder = "Name or teammate…",
    noMatchHint = "No match — press Enter to save as free-form name.",
    contextSpeaker = "",
    contextCount = 0,
    onbulkpick,
  }: Props = $props();

  // #169 · snapshot the opener so Escape / outside-click / commit
  // paths all return focus to the element that opened the picker
  // (e.g. the `.utt-speaker` or `.speaker-chip` trigger). Captured
  // at <script> body time — runs before the picker's DOM mounts
  // and before the autofocus $effect steals focus to `.srp-input`,
  // so whatever is focused here is the trigger (on browsers that
  // focus buttons on click). Self-contained per ui.md §1 — no
  // opener-prop plumbing required.
  const openerEl: HTMLElement | null =
    typeof document !== "undefined" &&
    document.activeElement instanceof HTMLElement &&
    document.activeElement !== document.body
      ? document.activeElement
      : null;

  let containerEl = $state<HTMLDivElement | null>(null);
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

  // #171 · picker mode. `primary` = the normal rename flow; `secondary`
  // = the bulk "replace every X with…" chooser, engaged by activating
  // the bulk row. The secondary picker re-uses the same .srp-list
  // surface, so no portal / overlay is introduced.
  let mode = $state<"primary" | "secondary">("primary");
  // Stash the primary typed value when entering secondary so Back
  // restores what the user was typing pre-detour.
  let stashedValue = $state("");

  // Dropdown rows: recents filtered to names still in the current
  // roster (so deactivated teammates drop out), roster filtered
  // by case-insensitive substring on display_name + email, plus
  // #170 externals filtered identically and de-duplicated against
  // the roster (a teammate row wins over a free-form duplicate).
  type Row =
    | { kind: "recent"; name: string }
    | { kind: "member"; member: OrgMemberLite }
    | { kind: "external"; name: string }
    | { kind: "bulk" }
    | { kind: "cta"; typed: string };

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
    // #170 · externals from this call. Dedup against roster
    // case-insensitively (teammate row always wins — linking the FK
    // is strictly more useful than re-typing free-form). Hide the
    // context speaker itself in secondary mode only; primary mode
    // doesn't carry a "rename target is self" ambiguity (the picker
    // is renaming the context speaker).
    const hideName = mode === "secondary"
      ? contextSpeaker.trim().toLowerCase()
      : "";
    const seen = new Set<string>();
    const externalRows: Row[] = [];
    for (const raw of externals) {
      const name = (raw ?? "").trim();
      if (!name) continue;
      const key = name.toLowerCase();
      if (rosterByName.has(key)) continue;
      if (key === hideName) continue;
      if (seen.has(key)) continue;
      if (q && !name.toLowerCase().includes(q)) continue;
      seen.add(key);
      externalRows.push({ kind: "external" as const, name });
      if (externalRows.length >= 12) break;
    }
    const memberRows: Row[] = roster
      .filter((m) => {
        if (m.display_name.toLowerCase() === hideName) return false;
        if (!q) return true;
        return (
          m.display_name.toLowerCase().includes(q) ||
          m.email.toLowerCase().includes(q)
        );
      })
      .map((m) => ({ kind: "member" as const, member: m }));

    const base: Row[] = [...recentRows, ...externalRows, ...memberRows];

    // #171 · bulk row. Gated on variant=stack + contextCount > 1 +
    // contextSpeaker present + primary mode (secondary picker has no
    // recursion). Rendered as second-to-last row (just before the
    // "+ Add a new external speaker" CTA if that CTA paints;
    // otherwise at the tail).
    const showBulk =
      mode === "primary" &&
      variant === "stack" &&
      contextCount > 1 &&
      contextSpeaker.trim().length > 0 &&
      !!onbulkpick;

    // CTA row — shown when input has non-empty text that doesn't
    // match any row above (case-insensitive). In secondary mode the
    // CTA is still valid: picking an external-by-free-form is a
    // legitimate bulk target (a new external participant).
    const trimmed = value.trim();
    const showCta =
      trimmed.length > 0 &&
      !base.some(
        (r) =>
          (r.kind === "recent" && r.name.toLowerCase() === q) ||
          (r.kind === "external" && r.name.toLowerCase() === q) ||
          (r.kind === "member" &&
            r.member.display_name.toLowerCase() === q),
      );

    const out = [...base];
    if (showBulk) out.push({ kind: "bulk" as const });
    if (showCta) out.push({ kind: "cta" as const, typed: trimmed });
    return out;
  });

  // Keep activeIdx in-range as the list filters live; -1 means "no
  // row is active" — Enter then takes the free-form path.
  $effect(() => {
    if (activeIdx >= rows.length) activeIdx = rows.length - 1;
  });

  $effect(() => {
    if (autofocus && inputEl && mode === "primary") {
      inputEl.focus();
      // Select-all so a retype immediately replaces; matches the
      // implicit bind-value autofocus behaviour the bare <input> had.
      inputEl.select();
    }
  });

  // #169 · outside-click dismissal. Same capture-phase pointerdown
  // pattern as ChipMenu (L101–120) + ActionItem: pointerdown (not
  // click) fires before focus / selection on the new target, so the
  // "Apply to all" checkbox behind the open list receives its click
  // on the first press. Capture phase so inner stopPropagation calls
  // cannot swallow the dismiss.
  function onOutsidePointerDown(e: PointerEvent) {
    const t = e.target as Node | null;
    if (!t) return;
    if (containerEl && containerEl.contains(t)) return;
    oncancel();
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

  // #169 · restore focus to the opener on unmount. All dismiss paths
  // (Escape, outside-click pointerdown → oncancel, commit → parent
  // tears down the editor) unmount this component via the parent, so
  // one cleanup handler covers every path. Guard on
  // `document.contains(openerEl)` in case the rename replaced the
  // opener DOM between open and unmount.
  $effect(() => {
    return () => {
      if (openerEl && document.contains(openerEl)) {
        try {
          openerEl.focus();
        } catch {
          /* opener detached or non-focusable; no-op */
        }
      }
    };
  });

  function resolveMemberByName(name: string): OrgMemberLite | null {
    const needle = name.trim().toLowerCase();
    if (!needle) return null;
    return roster.find((m) => m.display_name.toLowerCase() === needle) ?? null;
  }

  function enterSecondary() {
    if (!onbulkpick) return;
    stashedValue = value;
    value = "";
    mode = "secondary";
    activeIdx = -1;
    dropdownOpen = true;
    // Re-focus the input — the placeholder swaps to the bulk prompt.
    queueMicrotask(() => inputEl?.focus());
  }

  function backToPrimary() {
    mode = "primary";
    value = stashedValue;
    stashedValue = "";
    activeIdx = -1;
    dropdownOpen = true;
    queueMicrotask(() => {
      inputEl?.focus();
      inputEl?.select();
    });
  }

  function emitBulk(user: OrgMemberLite | null, freeText: string | null) {
    if (!onbulkpick) return;
    const from = contextSpeaker.trim();
    if (!from) return;
    // Short-circuit: same name + no FK change = no-op.
    const toLabel = user ? user.display_name : (freeText ?? "").trim();
    if (!toLabel) return;
    if (toLabel.toLowerCase() === from.toLowerCase() && !user) {
      // Typed identical free-form name — pointless rewrite.
      backToPrimary();
      return;
    }
    onbulkpick({ from, user, freeText });
    // Parent unmounts on success; no dropdownOpen toggle here.
  }

  function pickRow(row: Row) {
    if (row.kind === "bulk") {
      enterSecondary();
      return;
    }
    if (mode === "secondary") {
      if (row.kind === "member") {
        emitBulk(row.member, null);
      } else if (row.kind === "external") {
        emitBulk(null, row.name);
      } else if (row.kind === "recent") {
        const m = resolveMemberByName(row.name);
        if (m) emitBulk(m, null);
        else emitBulk(null, row.name);
      } else if (row.kind === "cta") {
        emitBulk(null, row.typed);
      }
      return;
    }
    // primary mode
    if (row.kind === "member") {
      onpick({ user: row.member, freeText: null });
    } else if (row.kind === "external") {
      onpick({ user: null, freeText: row.name });
    } else if (row.kind === "recent") {
      // Recents row — resolve back to a roster member by name. If it
      // doesn't resolve (teammate deactivated between reads), fall
      // through as free-form so the Save isn't lost.
      const m = resolveMemberByName(row.name);
      if (m) {
        onpick({ user: m, freeText: null });
      } else {
        onpick({ user: null, freeText: row.name });
      }
    } else if (row.kind === "cta") {
      onpick({ user: null, freeText: row.typed });
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
    // free-form. In secondary mode, free-form commits a bulk rename
    // to an external target.
    const trimmed = value.trim();
    const member = resolveMemberByName(value);
    if (mode === "secondary") {
      if (!trimmed) {
        backToPrimary();
        return;
      }
      if (member) emitBulk(member, null);
      else emitBulk(null, trimmed);
      return;
    }
    if (member) {
      onpick({ user: member, freeText: null });
      dropdownOpen = false;
      return;
    }
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
      // #171 · two-step Escape in secondary mode: first Escape drops
      // back to primary; second Escape dismisses the editor.
      if (mode === "secondary") {
        backToPrimary();
        return;
      }
      oncancel();
      return;
    }
  }

  // Render-time helpers for section headers. A row starts a new
  // section when the preceding visible row has a different kind (or
  // it's the first row). Recent / external / member all carry their
  // own .srp-hdr; bulk + cta are standalone (no header).
  function showHeaderFor(i: number, kind: Row["kind"]): boolean {
    if (kind !== "recent" && kind !== "external" && kind !== "member")
      return false;
    if (i === 0) return true;
    return rows[i - 1].kind !== kind;
  }
  function headerLabel(kind: Row["kind"]): string {
    if (kind === "recent") return "Recently used";
    if (kind === "external") return "In this call";
    if (kind === "member") return "Teammates";
    return "";
  }

  // Input placeholder / aria-label swap in secondary mode.
  let currentPlaceholder = $derived(
    mode === "secondary"
      ? `Pick who "${contextSpeaker}" should be…`
      : placeholder,
  );
  let currentAriaLabel = $derived(
    mode === "secondary"
      ? `Pick who "${contextSpeaker}" should be`
      : "Rename speaker — start typing or choose a teammate",
  );
  let saveLabel = $derived(
    mode === "secondary" ? (saving ? "…" : "Go") : saving ? "…" : savingLabel,
  );
</script>

<div
  bind:this={containerEl}
  class="srp"
  class:srp-chip={variant === "chip"}
  onkeydown={handleKeydown}
  role="presentation"
>
  <div class="srp-row">
    {#if mode === "secondary"}
      <button
        type="button"
        class="srp-back"
        aria-label="Back to rename this line"
        onclick={backToPrimary}
      >
        ‹
      </button>
    {/if}
    <input
      class="srp-input"
      type="text"
      placeholder={currentPlaceholder}
      autocomplete="off"
      aria-label={currentAriaLabel}
      role="combobox"
      aria-expanded={dropdownOpen}
      aria-autocomplete="list"
      aria-controls={listboxId}
      disabled={saving}
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
      {saveLabel}
    </button>
    <button type="button" class="srp-cancel" onclick={oncancel}>
      Cancel
    </button>
  </div>
  {#if dropdownOpen}
    <div class="srp-list" role="listbox" id={listboxId}>
      <!-- #171 · primary↔secondary cross-fade. `{#key mode}` replays
           the fade-in animation each time `mode` flips; the keyframe
           is a 120ms opacity 0.6→1 lift (spec ui.md §3 motion table).
           `prefers-reduced-motion: reduce` collapses to an instant
           swap via the media-query gate on the animation property —
           same discipline as ChipMenu.svelte L312–316. -->
      {#key mode}
        <div class="srp-list-body">
          {#if mode === "secondary"}
            <div class="srp-bulk-head">
              Replace every "{contextSpeaker}" with:
            </div>
          {/if}
          {#if !rosterLoaded}
            <div class="srp-hint">Loading teammates…</div>
          {:else if rosterError}
            <div class="srp-hint srp-hint-err">
              Couldn't load teammates. Free-form save still works.
            </div>
          {:else if roster.length === 0 && rows.length === 0}
            <div class="srp-hint">No teammates in your org yet.</div>
          {:else if rows.length === 0}
            <div class="srp-hint">
              {noMatchHint}
            </div>
          {:else}
            {#each rows as row, i (row.kind === "member"
              ? "m:" + row.member.id
              : row.kind === "bulk"
                ? "b:bulk"
                : row.kind === "cta"
                  ? "c:cta"
                  : row.kind + ":" + row.name)}
              {#if showHeaderFor(i, row.kind)}
                <div class="srp-hdr">{headerLabel(row.kind)}</div>
              {/if}
              {#if row.kind === "bulk"}
                <div class="srp-sep" aria-hidden="true"></div>
                <button
                  type="button"
                  class="srp-row-btn srp-bulk-row"
                  class:active={i === activeIdx}
                  role="option"
                  aria-selected={i === activeIdx}
                  {...{ "aria-description": "Open bulk-replace chooser" }}
                  onmouseenter={() => (activeIdx = i)}
                  onclick={() => pickRow(row)}
                >
                  <span class="srp-bulk-glyph" aria-hidden="true">⟲</span>
                  <span class="srp-bulk-label">
                    Replace every "{contextSpeaker}" in this transcript with…
                  </span>
                </button>
              {:else if row.kind === "cta"}
                <div class="srp-sep" aria-hidden="true"></div>
                <button
                  type="button"
                  class="srp-row-btn srp-cta-row"
                  class:active={i === activeIdx}
                  role="option"
                  aria-selected={i === activeIdx}
                  {...{ "aria-description": "Save as a free-form external speaker" }}
                  onmouseenter={() => (activeIdx = i)}
                  onclick={() => pickRow(row)}
                >
                  <span class="srp-cta-glyph" aria-hidden="true">+</span>
                  <span class="srp-cta-label">
                    Save "{row.typed}" as an external speaker
                  </span>
                </button>
              {:else}
                <button
                  type="button"
                  class="srp-row-btn"
                  class:active={i === activeIdx}
                  class:srp-row-external={row.kind === "external"}
                  role="option"
                  aria-selected={i === activeIdx}
                  {...{ "aria-description": row.kind === "external"
                    ? "External speaker from this call"
                    : "Linked teammate" }}
                  onmouseenter={() => (activeIdx = i)}
                  onclick={() => pickRow(row)}
                >
                  {#if row.kind === "recent"}
                    <Avatar name={row.name} size={20} />
                    <span class="srp-stack">
                      <span class="srp-name">{row.name}</span>
                    </span>
                  {:else if row.kind === "external"}
                    <span class="srp-ext-avatar">
                      <Avatar name={row.name} size={20} />
                    </span>
                    <span class="srp-stack">
                      <span class="srp-name srp-name-external">{row.name}</span>
                      <span class="srp-email">External · in this call</span>
                    </span>
                  {:else}
                    <Avatar name={row.member.display_name} size={20} />
                    <span class="srp-stack">
                      <span class="srp-name">{row.member.display_name}</span>
                      <span class="srp-email">{row.member.email}</span>
                    </span>
                  {/if}
                </button>
              {/if}
            {/each}
          {/if}
        </div>
      {/key}
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
  .srp-input:disabled {
    opacity: 0.7;
    cursor: not-allowed;
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

  /* #171 · back chevron in secondary picker. Ghost button — hover
     tint to bone-0 on ink-2, matches the ChipMenu dismiss chrome. */
  .srp-back {
    appearance: none;
    background: transparent;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    padding: 0.2rem 0.5rem;
    color: var(--bone-2);
    font-size: 1rem;
    line-height: 1;
    cursor: pointer;
    font: inherit;
  }
  .srp-back:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .srp-back:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
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

  /* #171 · primary↔secondary cross-fade. `{#key mode}` remounts
     this wrapper each time `mode` flips, replaying the animation.
     Spec: ui.md §3 motion table — 120ms opacity 0.6→1 on the new
     content. `prefers-reduced-motion: reduce` collapses to an
     instant swap, mirroring ChipMenu.svelte's animation-gate. */
  .srp-list-body {
    animation: srp-mode-swap 120ms ease-out both;
  }
  @media (prefers-reduced-motion: reduce) {
    .srp-list-body {
      animation: none;
    }
  }
  @keyframes srp-mode-swap {
    from {
      opacity: 0.6;
    }
    to {
      opacity: 1;
    }
  }

  .srp-bulk-head {
    padding: 0.45rem 0.7rem 0.35rem;
    font-size: 0.7rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
    border-bottom: 1px solid var(--hairline);
    margin-bottom: 0.25rem;
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

  .srp-sep {
    height: 1px;
    background: var(--hairline);
    margin: 0.3rem 0.5rem;
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

  /* #170 · external row vocabulary. Reuse the "unlinked speaker"
     idiom: reduced-opacity avatar + italic name + hairline-hi
     dashed border hinting the free-form origin. No new accent — the
     palette stays at four semantic colors per design.md §Never-do-
     this #2. */
  .srp-row-external .srp-ext-avatar {
    display: inline-flex;
    border-radius: 50%;
    padding: 1px;
    border: 1px dashed var(--hairline-hi);
    opacity: 0.9;
  }
  .srp-name-external {
    font-style: italic;
  }

  /* #171 · bulk-replace row. Counterclockwise-arrow glyph in
     bone-2 at rest; label uses bone-1 at rest and promotes to
     bone-0 + accent-soft on hover/active (shared .srp-row-btn
     chrome). Label wraps on narrow viewports — users need to see
     the full copy before committing to a global rewrite. */
  .srp-bulk-row {
    gap: 0.5rem;
  }
  .srp-bulk-glyph {
    display: inline-block;
    width: 1rem;
    text-align: center;
    color: var(--bone-2);
    font-size: 0.95rem;
  }
  .srp-bulk-row:hover .srp-bulk-glyph,
  .srp-bulk-row.active .srp-bulk-glyph {
    color: var(--bone-0);
  }
  .srp-bulk-label {
    font-size: 0.82rem;
    white-space: normal;
  }

  /* #170 · "+ Save as external" CTA row. 12px accent-tinted glyph
     matches the tag-popover "+" idiom (design.md §Tag chip). */
  .srp-cta-row {
    gap: 0.5rem;
  }
  .srp-cta-glyph {
    display: inline-block;
    width: 1rem;
    text-align: center;
    color: var(--accent);
    font-size: 1rem;
    font-weight: 600;
  }
  .srp-cta-label {
    font-size: 0.82rem;
  }
</style>
