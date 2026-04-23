<script lang="ts" module>
  // Minimal shape this component needs per roster entry. Parent maps
  // its source-specific row (portal OrgMember vs agent-Tauri-invoke
  // shape) onto this triple before passing. Matches the shape of
  // SummaryMember in SummaryText — callers pass the same array to
  // both components.
  export type ActionItemUser = {
    id: string;
    first_name: string;
    last_name: string;
    display_name: string;
    // Email required for the SpeakerRenamePicker the edit mode reuses
    // as the assignee combobox. Parent populates from its roster
    // source (api.OrgMember on portal, org_members Tauri invoke on
    // agent); both shapes have `email` already.
    email: string;
  };

  // Structured action item as served by the backend's GET
  // /v1/calls/{id}/action-items (Phase 1 of the v0.4.0 bundle).
  // Fields mirror the backend `ActionItem` DTO 1:1.
  export type ActionItem = {
    id: string;
    call_id: string;
    description: string;
    assignee_user_id: string | null;
    status: "open" | "done";
    completed_at: string | null;
    completed_by_user_id: string | null;
    source: "llm" | "manual";
    created_at: string;
    order_index: number;
  };

  // Save payload the parent receives when the editor commits.
  // `assignee_user_id` is tri-state from the parent's POV:
  //   `string` → set to this uuid
  //   `null`   → explicit clear
  //   the parent decides whether to include the field in the PATCH
  //   based on whether the user changed it — the component surfaces
  //   the picked value verbatim.
  export type ActionItemEditSave = {
    itemId: string;
    description: string;
    assigneeUserId: string | null;
  };
</script>

<script lang="ts">
  // Mirror-pair component — byte-identical between
  // portal/src/lib/ActionItem.svelte and
  // agent/src/lib/ActionItem.svelte (same discipline as
  // Avatar.svelte, SpeakerRenamePicker.svelte, SummaryText.svelte).
  // Reviewer diffs the two on every touch.
  //
  // Styles are component-scoped deliberately — none of these classes
  // live in app.css. The `diff portal/src/app.css agent/src/app.css`
  // invariant stays intact.
  //
  // Phase 2 of v0.4.0 bundle (closes #19): adds click-to-edit
  // affordance. Pencil appears in a row-actions slot, hover-revealed
  // on desktop (opacity 0.4 → 1) and always-visible on touch.
  // Clicking the pencil flips the row into an inline editor: textarea
  // for the description (raw `<name>...</name>` markers preserved) +
  // compact SpeakerRenamePicker for the assignee + Save / Cancel.
  // The PARENT owns the mutation — this component emits `onedit`
  // when the pencil is clicked (so the parent can dirty-cancel any
  // other row), `onsave` with the edited payload, `oncancel` on
  // dismiss.
  //
  // Phase 3 of v0.4.0 bundle (closes #104): adds
  //   - Trash (.ai-del) next to the pencil, hover-revealed with the
  //     same opacity + reduced-motion rules as .ai-edit.
  //   - Inline delete confirm: clicking trash swaps the row-actions
  //     slot from [pencil|trash] to [Delete|Cancel]. Zero-ms
  //     transition (ui-phase-3 §Motion). Escape / Cancel restore the
  //     pencil+trash; Enter on Delete fires ondelete.
  //   - Unassigned chip: `.ai-assignee-empty` renders when the FK is
  //     null AND the description has no `<name>` markers. Dashed 20px
  //     circle + "Unassigned" label in --bone-3 (ui-phase-3 §D).
  //
  // State matrix (extended from Phase 1's):
  //   default           — read-only row
  //   editing           — textarea + picker + Save/Cancel; driven by `editing` prop
  //   saving            — Save label flips to "Saving…", disabled
  //   error             — inline callout above the buttons; draft preserved
  //   confirmingDelete  — trash/pencil swap to Delete/Cancel inline buttons
  //   deleting          — Delete label flips to "Deleting…", disabled
  //   deleteError       — inline error line replacing the confirm buttons
  //
  // Phase 4 still reserves `ontoggle` — status check-off lands there.

  import Avatar from "./Avatar.svelte";
  import SummaryText from "./SummaryText.svelte";
  import SpeakerRenamePicker, {
    type OrgMemberLite,
    type SpeakerPick,
  } from "./SpeakerRenamePicker.svelte";
  import { formatRelativeTime } from "./time";

  // Phase 4 (#105): /actions page row context. When the row is rendered
  // on the /actions listing (variant="actions-page"), the parent passes
  // this so the row can render a secondary link line
  // `{call-title} · {relative-time}`. Unset on call-detail (variant
  // defaults to "call-detail") — the existing idx pill already anchors
  // the row within the call.
  export type ActionItemCallContext = {
    id: string;
    title: string | null;
    recordedAt: string | Date | null;
  };

  type Props = {
    item: ActionItem;
    users?: ActionItemUser[];
    callId: string;
    index: number;
    variant?: "call-detail" | "actions-page";
    callContext?: ActionItemCallContext;
    colorFor?: (name: string) => string;
    // Phase 2 (#19): inline-edit machinery. The PARENT owns mutation
    // + "only one row edits at a time" state; this component just
    // renders the read-only / edit affordances and emits events.
    //
    //   editing  — true when this row is the one in edit mode; false
    //              otherwise. Parent typically stores an
    //              `editingItemId` and passes `editing={id === editingItemId}`.
    //   saving   — flips the Save button label to "Saving…" and
    //              disables both buttons. Controlled so the parent
    //              can reset it when its PATCH resolves or fails.
    //   editError — inline error line above Save/Cancel. Component
    //              never drops the user's draft on error; parent
    //              controls the message.
    //   canEdit  — permission gate. False (member viewing a call
    //              they can't edit) hides the pencil entirely.
    editing?: boolean;
    saving?: boolean;
    editError?: string;
    canEdit?: boolean;
    // Phase 3 (#104): inline delete confirm. Parent owns state so
    // only one row confirms at a time (same rule as `editing`). When
    // `confirmingDelete` flips true, the row-actions slot swaps from
    // [pencil|trash] to [Delete|Cancel]. `deleting` flips the Delete
    // label to "Deleting…" and disables both buttons during the
    // in-flight DELETE. `deleteError` replaces the buttons with an
    // inline error line until the parent clears it (e.g. after a
    // retry click).
    confirmingDelete?: boolean;
    deleting?: boolean;
    deleteError?: string;
    // Phase 4 reserves `ontoggle` for the status check-off; P3 treats
    // it as a no-op.
    onedit?: (payload: { item: ActionItem }) => void;
    onsave?: (payload: ActionItemEditSave) => void;
    oncancel?: () => void;
    // Phase 3 (#104): three stages. `ondeleterequest` fires when the
    // trash is clicked (parent flips confirmingDelete for this row,
    // clearing any other row's confirm). `ondeleteconfirm` fires on
    // Delete — parent runs the DELETE + removes the row on success
    // / sets deleteError on failure. `ondeletecancel` fires on the
    // inline Cancel or Escape inside the slot; parent clears
    // confirmingDelete without touching the row.
    ondeleterequest?: (payload: { item: ActionItem }) => void;
    ondeleteconfirm?: (payload: { item: ActionItem }) => void;
    ondeletecancel?: (payload: { item: ActionItem }) => void;
    ontoggle?: (payload: {
      item: ActionItem;
      nextStatus: "open" | "done";
    }) => void;
  };

  let {
    item,
    users = [],
    callId: _callId,
    index,
    variant = "call-detail",
    callContext,
    colorFor,
    editing = false,
    saving = false,
    editError = "",
    canEdit = false,
    confirmingDelete = false,
    deleting = false,
    deleteError = "",
    onedit,
    onsave,
    oncancel,
    ondeleterequest,
    ondeleteconfirm,
    ondeletecancel,
    ontoggle,
  }: Props = $props();

  // Phase 4 (#105): row check-off wiring. The checkbox is interactive
  // when the parent wires `ontoggle` AND the row is not currently in
  // the edit / delete / save flight paths (mid-edit toggling would
  // race against the PATCH in-flight from Save). Call-detail rows
  // with only `canEdit=false` (member viewing a teammate's call, or
  // no ontoggle wired yet) stay disabled — same posture Phase 1-3
  // shipped.
  let canToggle = $derived(
    ontoggle !== undefined && !editing && !saving && !deleting,
  );

  // Secondary line on /actions rows — `{call-title} · {relative-time}`
  // linking to /calls/{id}. Suppressed on call-detail (no callContext)
  // and during edit mode (the editor owns the body slot).
  let callContextTitle = $derived(callContext?.title ?? "(untitled)");
  let callContextRelative = $derived.by(() => {
    if (!callContext?.recordedAt) return "";
    return formatRelativeTime(callContext.recordedAt);
  });
  let callContextIso = $derived.by(() => {
    if (!callContext?.recordedAt) return "";
    const d =
      typeof callContext.recordedAt === "string"
        ? new Date(callContext.recordedAt)
        : callContext.recordedAt;
    return d instanceof Date && !Number.isNaN(d.getTime())
      ? d.toISOString()
      : "";
  });

  function handleToggle(e: Event) {
    const el = e.currentTarget as HTMLInputElement;
    if (!ontoggle) {
      // Keep the native checked state in lockstep with the row's
      // `item.status` when the parent hasn't wired a toggle — prevents
      // a visible flip that the store won't honour.
      el.checked = isDone;
      return;
    }
    const nextStatus: "open" | "done" = el.checked ? "done" : "open";
    ontoggle({ item, nextStatus });
  }

  // Resolve the assignee FK against the loaded roster. If the FK is
  // set but the user isn't in `users` (stale roster, cross-org leak
  // defence, cached deletion), render nothing for the chip — don't
  // fall through to rendering the raw UUID.
  let resolvedAssignee = $derived.by(() => {
    if (!item.assignee_user_id) return null;
    return users.find((u) => u.id === item.assignee_user_id) ?? null;
  });

  let isDone = $derived(item.status === "done");
  let trimmedDesc = $derived((item.description ?? "").trim());
  let isEmpty = $derived(trimmedDesc.length === 0);
  let idxLabel = $derived(
    String(index + 1).padStart(2, "0"),
  );

  // Screen-reader label for the checkbox. Strip <name>...</name>
  // markers so the announced text reads naturally (no "less-than name
  // greater-than Clayton M." spoken aloud). Truncated to avoid a
  // paragraph being announced on tab. On the /actions variant the
  // leading "Action item {n}:" prefix is dropped — positional numbering
  // is meaningless across calls (ui-phase-4 §Copy).
  let srLabel = $derived.by(() => {
    const bare = trimmedDesc.replace(/<name>([^<]+)<\/name>/g, "$1");
    const truncated = bare.length > 80 ? bare.slice(0, 77) + "…" : bare;
    const suffix = isDone ? " (completed)" : "";
    if (variant === "actions-page") {
      return `${truncated || "(no description)"}${suffix}`;
    }
    return `Action item ${index + 1}: ${truncated || "(no description)"}${suffix}`;
  });

  let assigneeColor = $derived.by(() => {
    if (!resolvedAssignee) return "var(--accent)";
    if (colorFor) return colorFor(resolvedAssignee.display_name);
    return "var(--accent)";
  });

  // Phase 3 (#104): show the "Unassigned" chip only when the FK is
  // null AND the description has no `<name>` markers. If the prose
  // already surfaces a name via SummaryText's chip render, double-
  // labelling would be noise (ui-phase-3 §D case-#4 suppression).
  // The regex matches the same shape SummaryText tokenizes on —
  // intentionally narrow (`[^<]+` refuses nested tags), identical to
  // the learnings #97+#102 safe-render contract.
  let hasNameMarker = $derived(
    /<name>[^<]+<\/name>/.test(item.description ?? ""),
  );
  let showUnassigned = $derived(
    item.assignee_user_id === null && !hasNameMarker,
  );

  // ── Editor local state ─────────────────────────────────────────
  //
  // Description draft is a plain local string seeded from the row's
  // `description` when edit mode opens. We preserve raw
  // `<name>...</name>` markers verbatim — the user is editing the
  // wire format and the read surface re-parses them into chips on
  // save. Assignee draft tracks the picker's current selection.
  // Both reset on entering edit mode (below) rather than on every
  // prop change, so Save-side re-renders don't clobber the draft.
  let descDraft = $state("");
  let assigneeDraft = $state<string | null>(null);
  let assigneeValue = $state("");
  let pickerRoster = $derived.by<OrgMemberLite[]>(() =>
    users.map((u) => ({
      id: u.id,
      display_name: u.display_name,
      email: u.email,
    })),
  );

  // Seed the drafts when the row flips into edit mode. `editing` is
  // a parent-controlled boolean; entering edit mode without dirtying
  // drafts means Cancel reverts to whatever the row held at entry,
  // not whatever state a half-written draft left behind.
  $effect(() => {
    if (editing) {
      descDraft = item.description ?? "";
      assigneeDraft = item.assignee_user_id ?? null;
      const current = item.assignee_user_id
        ? users.find((u) => u.id === item.assignee_user_id)
        : null;
      assigneeValue = current?.display_name ?? "";
    }
  });

  function handleEditClick() {
    if (!canEdit) return;
    onedit?.({ item });
  }

  function handlePickerPick(pick: SpeakerPick) {
    if (pick.user) {
      assigneeDraft = pick.user.id;
      assigneeValue = pick.user.display_name;
    } else {
      // Free-form text is IGNORED in the action-item context
      // (assignee_user_id is FK-only; see ui-phase-2 §D). We leave
      // the picker input showing the typed text but keep
      // `assigneeDraft` at null so Save writes `assignee_user_id =
      // null`. Clearing the input (freeText === "") is the
      // "unassign" path.
      assigneeDraft = null;
      assigneeValue = pick.freeText;
    }
  }

  function clearAssignee() {
    assigneeDraft = null;
    assigneeValue = "";
  }

  function handleSave() {
    const next = descDraft.trim();
    if (!next) return; // backend rejects empty; don't bother round-tripping
    onsave?.({
      itemId: item.id,
      description: next,
      assigneeUserId: assigneeDraft,
    });
  }

  function handleCancel() {
    oncancel?.();
  }

  function onDescKeydown(e: KeyboardEvent) {
    // Cmd/Ctrl+Enter saves; Escape cancels. Listener is on the
    // textarea only — we never capture these at the page level
    // (ui-phase-2 §G reminder: don't eat Ctrl+S / Cmd+Enter for the
    // whole page).
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      handleCancel();
      return;
    }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      handleSave();
      return;
    }
  }

  // ── Phase 3 (#104): delete affordance + inline confirm ───────────

  function handleDeleteRequest() {
    if (!canEdit || deleting) return;
    ondeleterequest?.({ item });
  }

  function handleDeleteConfirm() {
    if (deleting) return;
    ondeleteconfirm?.({ item });
  }

  function handleDeleteCancel() {
    if (deleting) return;
    ondeletecancel?.({ item });
  }

  function onConfirmKeydown(e: KeyboardEvent) {
    // Escape inside the inline-confirm slot behaves like Cancel.
    // Scoped to the confirm wrapper — page-level keydown isn't eaten.
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      handleDeleteCancel();
    }
  }
</script>

<li
  class="ai-row"
  class:ai-done={isDone}
  class:ai-editing={editing}
  class:ai-actions-page={variant === "actions-page"}
>
  <input
    type="checkbox"
    class="ai-check"
    class:ai-check-live={canToggle}
    disabled={!canToggle}
    checked={isDone}
    aria-label={srLabel}
    onchange={handleToggle}
  />
  {#if variant !== "actions-page"}
    <span class="ai-idx">{idxLabel}</span>
  {:else}
    <span class="ai-idx ai-idx-spacer" aria-hidden="true"></span>
  {/if}
  <span class="ai-body">
    {#if editing}
      <div class="ai-editor">
        <textarea
          class="ai-edit-desc"
          bind:value={descDraft}
          disabled={saving}
          rows="3"
          aria-label="Action item description"
          onkeydown={onDescKeydown}
        ></textarea>
        <div class="ai-edit-picker">
          <SpeakerRenamePicker
            bind:value={assigneeValue}
            roster={pickerRoster}
            rosterLoaded={true}
            saving={false}
            variant="stack"
            autofocus={false}
            placeholder="Assign to a teammate…"
            noMatchHint="No match — leave empty to keep this item unassigned."
            savingLabel="Pick"
            onpick={handlePickerPick}
            oncancel={clearAssignee}
          />
        </div>
        {#if editError}
          <p class="ai-edit-err" role="alert">{editError}</p>
        {/if}
        <div class="ai-edit-actions">
          <button
            type="button"
            class="ai-save"
            disabled={saving || descDraft.trim().length === 0}
            onclick={handleSave}
          >
            {saving ? "Saving…" : "Save"}
          </button>
          <button
            type="button"
            class="ai-cancel"
            disabled={saving}
            onclick={handleCancel}
          >
            Cancel
          </button>
        </div>
      </div>
    {:else if isEmpty}
      <em class="ai-empty">—</em>
    {:else}
      <SummaryText
        text={item.description}
        {users}
        {colorFor}
      />
    {/if}
    {#if callContext && variant === "actions-page" && !editing}
      <!-- Phase 4 (#105): /actions row context. `flex-basis: 100%`
           drops this below the description + assignee chip, so the
           row's top line stays dense (checkbox + description +
           chip) and this line acts as a footer link. Whole line is
           an anchor per ui-phase-4 §I (larger hit target; title +
           time resolve to the same destination so there's no
           ambiguity at the separator). -->
      <a
        class="actx-context"
        href="/calls/{callContext.id}"
        aria-label="Open call: {callContextTitle}"
      >
        <span class="actx-title">{callContextTitle}</span>
        {#if callContextRelative}
          <span class="actx-sep" aria-hidden="true">·</span>
          <time class="actx-time" datetime={callContextIso}>
            {callContextRelative}
          </time>
        {/if}
      </a>
    {/if}
    {#if !editing && resolvedAssignee}
      <span
        class="ai-assignee"
        title="Assigned to {resolvedAssignee.display_name}"
      >
        <Avatar
          name={resolvedAssignee.display_name}
          color={assigneeColor}
          size={20}
        />
        <span class="ai-assignee-name" style="--name-c: {assigneeColor}">
          {resolvedAssignee.display_name}
        </span>
      </span>
    {:else if !editing && showUnassigned}
      <!-- Phase 3 (#104): dashed-border placeholder chip when there's
           no FK and no inline <name> marker in the description. Acts
           as a subdued "deliberately unassigned" signal so the row
           doesn't read as missing data. `role="img"` + aria-label
           so screen readers announce it alongside the visible label
           for sighted users. -->
      <span
        class="ai-assignee-empty"
        title="No assignee"
      >
        <span
          class="ai-assignee-empty-dot"
          role="img"
          aria-label="Unassigned"
        ></span>
        <span class="ai-assignee-empty-label">Unassigned</span>
      </span>
    {/if}
  </span>
  {#if canEdit && !editing}
    {#if confirmingDelete}
      <!-- Phase 3 inline confirm. Swaps the row-actions slot (pencil
           + trash) to [Delete | Cancel]. Zero-ms transition (no
           reflow: the buttons occupy the same grid column). Escape
           inside the slot maps to Cancel. Keydown listener is on the
           wrapper so Escape works regardless of which inner button
           has focus — both children are interactive buttons, so the
           a11y intent holds. -->
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <span
        class="ai-confirm"
        role="group"
        aria-label="Confirm delete action item {index + 1}"
        onkeydown={onConfirmKeydown}
      >
        {#if deleteError}
          <span class="ai-confirm-err" role="alert">{deleteError}</span>
          <button
            type="button"
            class="ai-confirm-retry"
            disabled={deleting}
            onclick={handleDeleteConfirm}
          >
            Retry
          </button>
          <button
            type="button"
            class="ai-confirm-cancel"
            disabled={deleting}
            onclick={handleDeleteCancel}
          >
            Cancel
          </button>
        {:else}
          <button
            type="button"
            class="ai-confirm-delete"
            disabled={deleting}
            onclick={handleDeleteConfirm}
          >
            {deleting ? "Deleting…" : "Delete"}
          </button>
          <button
            type="button"
            class="ai-confirm-cancel"
            disabled={deleting}
            onclick={handleDeleteCancel}
          >
            Cancel
          </button>
        {/if}
      </span>
    {:else}
      <span class="ai-actions">
        <button
          type="button"
          class="ai-edit"
          aria-label="Edit action item {index + 1}"
          onclick={handleEditClick}
        >
          <!-- 12px pencil glyph. `currentColor` so hover-opacity +
               focus rings work against any ink background without a
               new token. -->
          <svg
            width="12"
            height="12"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M11.5 2.5l2 2-8 8H3.5v-2l8-8z" />
          </svg>
        </button>
        <button
          type="button"
          class="ai-del"
          aria-label="Delete action item {index + 1}"
          onclick={handleDeleteRequest}
        >
          <!-- 12px trash glyph. Stroke-only, `currentColor` so the
               hover-opacity + reduced-motion rules in `.ai-del`
               match `.ai-edit`. Destroy-forever semantic per
               ui-phase-3 §"Trash glyph vs. xmark". -->
          <svg
            width="12"
            height="12"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M3 4.5h10" />
            <path d="M6 4.5V3a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1.5" />
            <path d="M4.5 4.5l.5 8.2a1.3 1.3 0 0 0 1.3 1.2h3.4a1.3 1.3 0 0 0 1.3-1.2l.5-8.2" />
          </svg>
        </button>
      </span>
    {/if}
  {/if}
</li>

<style>
  /* Row shape. Grid for columns (checkbox / idx / body / actions).
     Body is a flex row so the optional assignee chip can sit inline
     on wide viewports and wrap beneath on narrow ones without a
     media query. The fourth column (`ai-edit`) carries `auto` width
     so rows without the pencil collapse it. */
  .ai-row {
    display: grid;
    grid-template-columns: 20px 28px 1fr auto;
    gap: 0.55rem;
    align-items: start;
    padding: 0.6rem 0;
    border-bottom: 1px solid var(--hairline);
    color: var(--bone-1);
    font-size: 0.9rem;
    line-height: 1.5;
  }
  /* Last row in the parent <ul> sheds the separator so the block
     bottom doesn't look like it's hanging a line. Parent is
     expected to be a <ul> with no extra padding. */
  :global(.ai-row:last-child) {
    border-bottom: none;
  }

  .ai-check {
    /* Keep the checkbox on the text baseline — Grid's
       `align-items: start` + a small top pad reads as visually
       aligned with the idx pill and first line of the body. */
    margin: 0.22rem 0 0 0;
    width: 16px;
    height: 16px;
    /* `cursor: default` when the parent hasn't wired an `ontoggle`
       (call-detail surfaces without Phase 4 plumbing); flips to
       pointer via `.ai-check-live` below when the toggle is wired. */
    cursor: default;
    accent-color: var(--olive);
  }
  /* Phase 4 (#105): wired-up toggle state. `.ai-check-live` is added
     alongside `.ai-check` whenever `ontoggle` is passed and the row
     isn't currently mid-edit / mid-save. Cursor flips to pointer so
     the interactive affordance is discoverable. */
  .ai-check-live {
    cursor: pointer;
  }

  .ai-idx {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--accent);
    letter-spacing: 0.04em;
    padding-top: 0.22rem;
  }
  /* Spacer keeps the grid columns stable on the /actions page where
     the per-call idx pill is replaced by the call-row context. */
  .ai-idx-spacer {
    width: 0;
  }

  .ai-body {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.4rem 0.75rem;
  }

  /* Done state: strikethrough the prose and the assignee chip label,
     soften the body color, dim the avatar. Strike-through on the
     idx pill itself reads poorly on a 2-digit mono number, so the
     pill stays colored as a stable anchor (design.md §Semantic
     colors rule-of-thumb). */
  .ai-done {
    color: var(--bone-3);
  }
  .ai-done .ai-body {
    text-decoration: line-through;
    text-decoration-color: color-mix(
      in srgb,
      var(--bone-3) 80%,
      transparent
    );
  }
  .ai-done .ai-assignee-name {
    text-decoration: line-through;
  }
  .ai-done :global(.ai-assignee .avatar-initials) {
    opacity: 0.6;
  }

  .ai-assignee {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    margin-left: auto;
    padding: 0 0.3rem 0 0.15rem;
    border-radius: 999px;
    line-height: 1.4;
    /* Nudge the chip onto the text baseline; Avatar renders as a
       block-level round, so this keeps it centered on the line of
       prose to its left. */
    align-self: baseline;
    white-space: nowrap;
  }

  .ai-assignee-name {
    font-weight: 500;
    font-size: 0.85rem;
    color: var(--name-c, var(--accent));
  }

  /* Empty-description backstop. Shouldn't happen from valid data —
     an LLM item always has a description and the manual-add UI
     (Phase 3) validates non-empty. Keeps the row structurally
     present if somehow it does. */
  .ai-empty {
    color: var(--bone-4);
    font-style: normal;
    letter-spacing: 0.08em;
  }

  /* Focus ring — inherits the app-level 2px accent outline on the
     checkbox itself. `:focus-visible` so mouse clicks don't paint
     one. */
  .ai-check:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* ── Phase 2 edit affordance (#19) ───────────────────────────── */

  .ai-edit {
    padding: 0.22rem 0.35rem;
    margin-top: 0.1rem;
    border: none;
    background: transparent;
    color: var(--bone-2);
    cursor: pointer;
    border-radius: 4px;
    line-height: 0;
    /* Hover-reveal on desktop. Touch / no-hover viewports get 0.7
       always so the affordance is discoverable without pointer
       hover. `:focus-visible` pops to 1 so keyboard users can find
       it after tabbing in. */
    opacity: 0.4;
    transition: opacity 150ms linear;
  }
  .ai-row:hover .ai-edit,
  .ai-edit:focus-visible {
    opacity: 1;
  }
  .ai-edit:hover {
    color: var(--accent);
    background: var(--ink-2);
  }
  @media (hover: none) {
    .ai-edit {
      opacity: 0.7;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-edit {
      transition: none;
    }
  }
  .ai-edit:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  /* Editor block. Occupies the full body column when the row flips
     into edit mode. `max-width: 100%` + `min-width: 0` keep flex
     overflow in check on narrow viewports. */
  .ai-editor {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    width: 100%;
    min-width: 0;
  }

  .ai-edit-desc {
    width: 100%;
    min-height: 5.5rem;
    padding: 0.55rem 0.7rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 10px;
    background: var(--ink-1);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.9rem;
    line-height: 1.5;
    resize: vertical;
  }
  .ai-edit-desc:focus {
    outline: none;
    border-color: var(--accent);
  }
  .ai-edit-desc:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* Give the picker a reserved vertical lane — its combobox dropdown
     is absolute-positioned inside its own `.srp` root, but we still
     want some breathing room so the rendered roster rows don't
     bump into the Save / Cancel row immediately below. */
  .ai-edit-picker {
    position: relative;
  }

  .ai-edit-err {
    margin: 0;
    padding: 0.4rem 0.55rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.82rem;
  }

  .ai-edit-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.4rem;
    flex-wrap: wrap;
  }

  .ai-save,
  .ai-cancel {
    padding: 0.38rem 0.9rem;
    border-radius: 6px;
    font-size: 0.82rem;
    font: inherit;
    font-weight: 500;
    cursor: pointer;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
  }
  .ai-save {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--ink-0);
  }
  .ai-save:disabled,
  .ai-cancel:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* Editing row: hide the trailing assignee chip (the picker is
     editing it) so the layout doesn't double-up. The checkbox + idx
     pill stay on the left as spatial anchors so the user knows
     which row they're editing, per ui-phase-2 §D. */
  .ai-editing .ai-assignee,
  .ai-editing .ai-assignee-empty {
    display: none;
  }

  /* ── Phase 3 (#104): delete affordance + inline confirm ──────── */

  /* Wraps pencil + trash in the row-actions slot so they share the
     fourth grid column as a single flex cluster. The slot is
     single-use — either [pencil + trash] OR the inline confirm
     cluster, never both (see `{#if confirmingDelete}` branch in the
     template). */
  .ai-actions {
    display: inline-flex;
    align-items: center;
    gap: 0.15rem;
  }

  /* Trash button mirrors `.ai-edit` opacity rules. The only deltas
     are the hover color (warm red signalling destructive) and the
     margin-left (butt up against the pencil without the gap the
     `.ai-actions` flex container supplies). */
  .ai-del {
    padding: 0.22rem 0.35rem;
    margin-top: 0.1rem;
    border: none;
    background: transparent;
    color: var(--bone-2);
    cursor: pointer;
    border-radius: 4px;
    line-height: 0;
    opacity: 0.4;
    transition: opacity 150ms linear, color 150ms linear,
      background 150ms linear;
  }
  .ai-row:hover .ai-del,
  .ai-del:focus-visible {
    opacity: 1;
  }
  .ai-del:hover {
    color: var(--live);
    background: var(--ink-2);
  }
  @media (hover: none) {
    .ai-del {
      opacity: 0.7;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-del {
      transition: none;
    }
  }
  .ai-del:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  /* Inline-confirm cluster. Sits in the same grid column the
     `.ai-actions` wrapper occupies when the row is at rest — the
     zero-motion swap in ui-phase-3 §Motion relies on this being a
     drop-in replacement. `flex-wrap: wrap` so narrow rows stack
     Delete above Cancel without reaching into a new media query. */
  .ai-confirm {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-wrap: wrap;
    justify-content: flex-end;
    margin-top: 0.1rem;
  }

  .ai-confirm-delete,
  .ai-confirm-cancel,
  .ai-confirm-retry {
    padding: 0.28rem 0.7rem;
    border-radius: 6px;
    font: inherit;
    font-size: 0.78rem;
    font-weight: 500;
    cursor: pointer;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
  }
  .ai-confirm-delete,
  .ai-confirm-retry {
    /* Warm red for the destructive primary — same `--live`/`--live-soft`
       pairing used by `.ai-edit-err` so the two destructive surfaces
       read consistently. */
    border-color: var(--live);
    background: var(--live-soft);
    color: var(--live);
  }
  .ai-confirm-delete:hover:not(:disabled),
  .ai-confirm-retry:hover:not(:disabled) {
    background: var(--live);
    color: var(--ink-0);
  }
  .ai-confirm-cancel:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .ai-confirm-delete:disabled,
  .ai-confirm-cancel:disabled,
  .ai-confirm-retry:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .ai-confirm-err {
    padding: 0.22rem 0.5rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.78rem;
    line-height: 1.4;
  }

  /* ── Phase 3 (#104): Unassigned chip ─────────────────────────── */
  /*
     Renders when the FK is null AND the description has no inline
     <name> marker. Shape mirrors `.ai-assignee` (same margin-left,
     same baseline-alignment) so long-wrap behaviour stays
     consistent — the chip snaps inline at the tail of `.ai-body` on
     wide viewports and wraps beneath on narrow. The dashed-ring dot
     carries `role="img"` + aria-label="Unassigned" in markup so AT
     announces it; visible label is there for sighted users.
  */
  .ai-assignee-empty {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    margin-left: auto;
    padding: 0 0.3rem 0 0.15rem;
    border-radius: 999px;
    line-height: 1.4;
    align-self: baseline;
    white-space: nowrap;
    color: var(--bone-3);
  }
  .ai-assignee-empty-dot {
    width: 20px;
    height: 20px;
    border-radius: 50%;
    border: 1px dashed var(--hairline-hi);
    background: transparent;
    flex: 0 0 auto;
  }
  .ai-assignee-empty-label {
    font-weight: 400;
    font-size: 0.85rem;
    color: var(--bone-3);
  }
  /* Done rows strike through the unassigned chip's label via the
     cascade in `.ai-done .ai-body`, same as `.ai-assignee-name`. */
  .ai-done .ai-assignee-empty-label {
    text-decoration: line-through;
  }

  /* ── Phase 4 (#105): /actions call-context secondary line ───────── */
  /*
     Drops below the description + assignee chip via `flex-basis: 100%`
     on the `.ai-body` flex container — no media query needed, the
     existing flex-wrap rhythm handles narrow viewports (ui-phase-4
     §Responsive). Whole line is an anchor so the hit target matches
     the visual span; title + time resolve to the same destination.

     Done-row strike-through in `.ai-done .ai-body` cascades through
     the nested `<a>`; we override `.actx-context` color so the
     strike reads cleanly over the muted text (ui-phase-4 §C).
  */
  .actx-context {
    flex-basis: 100%;
    display: inline-flex;
    align-items: baseline;
    gap: 0.35rem;
    font-size: 0.78rem;
    color: var(--bone-3);
    text-decoration: none;
    margin-top: 0.1rem;
    transition: color 150ms linear;
  }
  .actx-context:hover,
  .actx-context:focus-visible {
    color: var(--bone-1);
    text-decoration: underline;
  }
  .actx-context:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 3px;
  }
  @media (prefers-reduced-motion: reduce) {
    .actx-context {
      transition: none;
    }
  }
  .actx-title {
    font-weight: 500;
    color: inherit;
  }
  .actx-sep {
    color: var(--bone-4);
  }
  .actx-time {
    font-variant-numeric: tabular-nums;
    color: inherit;
  }
  .ai-done .actx-context {
    color: var(--bone-4);
  }
</style>
