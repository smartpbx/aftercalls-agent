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

  // Legacy (v0.4.1) combined-save payload. Retained for
  // compatibility with any call-site that still wants the single
  // description+assignee blob; #126 splits into granular callbacks.
  export type ActionItemEditSave = {
    itemId: string;
    description: string;
    assigneeUserId: string | null;
  };

  // #126 (v0.4.2): granular event payloads emitted by the row.
  //
  // Description-only save — fires on blur / Enter / Tab when the
  // draft is non-empty. Parent PATCHes `{ description }`.
  export type ActionItemDescriptionSave = {
    itemId: string;
    description: string;
  };
  // Owner-only save — fires on picker `onpick`, or on blur with an
  // empty input (explicit clear). Parent PATCHes
  // `{ assignee_user_id }`.
  export type ActionItemOwnerSave = {
    itemId: string;
    assigneeUserId: string | null;
  };
  // Phantom-row (optimistic add) commit — fires on first commit of
  // a `pending` row. Parent POSTs `/action-items/manual` with both
  // description + the phantom's carried assignee.
  export type ActionItemPendingSave = {
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
  // #126 / v0.4.2 — click-to-edit iteration.
  //   • The pencil button retires. Clicking the rendered description
  //     enters description-edit mode. Clicking the owner chip enters
  //     owner-edit mode. Both commit on blur / Enter / Tab; Escape
  //     cancels. No Save / Cancel buttons in the row anymore.
  //   • The add flow no longer opens a separate composer — the Add
  //     item button appends a phantom row (local only, prefixed
  //     `__pending__`) directly in description-edit mode. Phantom
  //     POSTs on first non-empty commit; blur-empty or Escape drops.
  //   • Per-row editing state is controlled by the parent:
  //       editingDescription — textarea swap for this row
  //       editingOwner       — picker swap for this row
  //       pending            — this row is a pre-POST phantom
  //       saving             — PATCH / POST in flight for this row
  //     Mutual exclusion is the parent's responsibility (at most one
  //     row is in any edit mode across the page); the parent emits
  //     the auto-commit transitions documented in the ui-spec §3 /
  //     architect's §5.4.
  //
  // State matrix (updated for #126):
  //   default                — read-only row
  //   editingDescription     — inline textarea swap for the description
  //   editingOwner           — inline picker swap for the owner chip
  //   pending                — phantom row, `pending: true`, id prefixed
  //   saving                 — PATCH / POST in flight; .ai-body dims
  //   error                  — inline error line below the editing
  //                            surface; draft preserved; commit
  //                            triggers block until the user retypes
  //   confirmingDelete       — unchanged from Phase 3
  //   deleting / deleteError — unchanged from Phase 3
  //
  // Phase 4 still owns `ontoggle` for check-off — wiring unchanged.

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
    // #113: total rows in the list this row belongs to. Used by the
    // row-scoped aria-live announcer ("Editing action item 3 of 7").
    // Optional; defaults to 0 and the announcer falls back to a
    // plain "Editing action item 3" phrasing if the parent hasn't
    // wired it yet.
    totalInList?: number;
    variant?: "call-detail" | "actions-page";
    callContext?: ActionItemCallContext;
    colorFor?: (name: string) => string;
    // #126 (v0.4.2): per-row edit state, now split by intent.
    //   editingDescription — textarea swap for this row. Mutually
    //                        exclusive with editingOwner on the
    //                        same row (parent enforces).
    //   editingOwner       — picker swap for this row.
    //   pending            — row is a pre-POST phantom. Parent
    //                        appends it to its local list with an
    //                        id prefixed `__pending__`; on commit
    //                        the parent POSTs and replaces with the
    //                        server row. Phantoms render like
    //                        normal rows except trash + checkbox
    //                        are suppressed.
    //   saving             — PATCH or POST in flight. Dims .ai-body
    //                        and disables commit-triggering
    //                        handlers so the user can't fire a
    //                        second blur-commit over an in-flight
    //                        save.
    //   editError          — inline error line below whichever
    //                        editor is active (textarea / picker).
    //                        Parent clears when the user edits the
    //                        draft again (retry-by-typing).
    //   canEdit            — permission gate. False → all click-to-
    //                        edit affordances (cursor / role /
    //                        tabindex) are suppressed.
    editingDescription?: boolean;
    editingOwner?: boolean;
    pending?: boolean;
    saving?: boolean;
    editError?: string;
    canEdit?: boolean;
    confirmingDelete?: boolean;
    deleting?: boolean;
    deleteError?: string;

    // #126 (v0.4.2): granular save / cancel / request callbacks.
    // The component fires these; the parent owns PATCH / POST and
    // updates `editingDescription` / `editingOwner` / `pending` in
    // response.
    onDescriptionEditRequest?: (payload: { item: ActionItem }) => void;
    onOwnerEditRequest?: (payload: { item: ActionItem }) => void;
    onDescriptionSave?: (payload: ActionItemDescriptionSave) => void;
    onOwnerSave?: (payload: ActionItemOwnerSave) => void;
    onDescriptionCancel?: (payload: { item: ActionItem }) => void;
    onOwnerCancel?: (payload: { item: ActionItem }) => void;
    onPendingSave?: (payload: ActionItemPendingSave) => void;
    onPendingDiscard?: () => void;
    // Fired on every keystroke while an error is visible — the
    // parent uses this to clear `actionItemErrors[item.id]` so the
    // next blur / Enter can attempt a fresh commit (retry-by-typing
    // per architect §5.3). Emitted at most once per edit-session
    // (parent manages the state lifecycle).
    onEditErrorClear?: (payload: { item: ActionItem }) => void;

    // Phase 3 (#104): delete flow. Unchanged.
    ondeleterequest?: (payload: { item: ActionItem }) => void;
    ondeleteconfirm?: (payload: { item: ActionItem }) => void;
    ondeletecancel?: (payload: { item: ActionItem }) => void;
    ontoggle?: (payload: {
      item: ActionItem;
      nextStatus: "open" | "done";
    }) => void;

    // #140 · v0.4.5 — chip-edit plumbing. The embedded SummaryText
    // inside the read-only description wrapper emits `onchipaction`
    // when a linked chip is clicked; the row forwards it up to the
    // page so a single <ChipMenu> can be mounted at the page level.
    // Page-level state picks `itemId` out of the callback to route
    // the PATCH to the right action-item row.
    onchipaction?: (detail: {
      inner: string;
      occurrenceIndex: number;
      anchor: HTMLElement;
      itemId: string;
      // #150 · v0.4.6 — true when the clicked `<name>` span was a
      // roster-miss (external mention). Page-level ChipMenu uses
      // this to flip into Link/Leave-text mode.
      isExternal: boolean;
    }) => void;
    // Occurrence index of the chip whose popover is currently open,
    // scoped to this action-item's description. Matching segment
    // renders with the `.name-chip-active` outline cue.
    activeChipOccurrenceIndex?: number | null;
  };

  let {
    item,
    users = [],
    callId: _callId,
    index,
    totalInList = 0,
    variant = "call-detail",
    callContext,
    colorFor,
    editingDescription = false,
    editingOwner = false,
    pending = false,
    saving = false,
    editError = "",
    canEdit = false,
    confirmingDelete = false,
    deleting = false,
    deleteError = "",
    onDescriptionEditRequest,
    onOwnerEditRequest,
    onDescriptionSave,
    onOwnerSave,
    onDescriptionCancel,
    onOwnerCancel,
    onPendingSave,
    onPendingDiscard,
    onEditErrorClear,
    ondeleterequest,
    ondeleteconfirm,
    ondeletecancel,
    ontoggle,
    onchipaction,
    activeChipOccurrenceIndex = null,
  }: Props = $props();

  // #140 — surface-bound wrapper around the page's chipaction
  // handler. Pins the `itemId` so the page router can tell a
  // summary chip apart from an action-item chip. #150 — forwards
  // the `isExternal` flag unchanged so the page-level ChipMenu
  // flips into Link/Leave-text mode for roster-miss mentions.
  function onChipActionForward(detail: {
    inner: string;
    occurrenceIndex: number;
    anchor: HTMLElement;
    isExternal: boolean;
  }) {
    onchipaction?.({ ...detail, itemId: item.id });
  }

  // Convenience derivation — used in the template to gate trash /
  // checkbox / hover affordances. "Anything editing" treats
  // description-edit, owner-edit, and phantom pending-edit the same
  // way for most surface rules.
  let anyEditing = $derived(editingDescription || editingOwner);

  // Phase 4 (#105): row check-off wiring. Disabled while editing,
  // saving, deleting, or when the row is a phantom (no backend id
  // yet to PATCH).
  let canToggle = $derived(
    ontoggle !== undefined &&
      !anyEditing &&
      !saving &&
      !deleting &&
      !pending,
  );

  // Secondary line on /actions rows — `{call-title} · {relative-time}`
  // linking to /calls/{id}. Suppressed on call-detail (no callContext)
  // and during any edit mode (the editor owns the body slot).
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

  // #113 — aria-live announcement for screen-reader users. The row
  // enters edit mode via click-to-edit (no pencil button, no visible
  // heading change) so a scoped live region narrates the transition
  // into and out of edit mode. Silent on mount (first-render `""`);
  // silent on Escape-cancel (first effect clears so a re-enter re-
  // announces). The second effect watches `saving` for a true→false
  // transition and, when the error channel is clean, announces "…
  // updated". Cancels don't flip `saving`, so they fall through to
  // the silent-clear path.
  let announceText = $state("");
  let wasSaving = $state(false);

  $effect(() => {
    if (editingDescription || editingOwner) {
      const n = Math.max(index + 1, 1);
      announceText =
        totalInList > 0
          ? `Editing action item ${n} of ${totalInList}`
          : `Editing action item ${n}`;
    } else if (!wasSaving) {
      announceText = "";
    }
  });

  $effect(() => {
    if (saving) {
      wasSaving = true;
    } else if (wasSaving) {
      wasSaving = false;
      if (!editError) {
        announceText = `Action item ${index + 1} updated`;
      }
    }
  });

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
  // markers so the announced text reads naturally. Truncated to avoid
  // a paragraph being announced on tab. On the /actions variant the
  // leading "Action item {n}:" prefix is dropped — positional
  // numbering is meaningless across calls (ui-phase-4 §Copy).
  let srLabel = $derived.by(() => {
    const bare = trimmedDesc.replace(/<name>([^<]+)<\/name>/g, "$1");
    const truncated = bare.length > 80 ? bare.slice(0, 77) + "…" : bare;
    const suffix = isDone ? " (completed)" : "";
    if (variant === "actions-page") {
      return `${truncated || "(no description)"}${suffix}`;
    }
    return `Action item ${index + 1}: ${truncated || "(no description)"}${suffix}`;
  });

  // #126: aria-label for the .ai-desc role=button wrapper — announces
  // the truncated description so SR users get the click-to-edit
  // intent. Falls back to a generic label on empty rows.
  let descEditLabel = $derived.by(() => {
    const bare = trimmedDesc.replace(/<name>([^<]+)<\/name>/g, "$1");
    const truncated = bare.length > 80 ? bare.slice(0, 77) + "…" : bare;
    return truncated
      ? `Edit description: ${truncated}`
      : "Edit description";
  });
  let ownerEditLabel = $derived.by(() => {
    if (resolvedAssignee) {
      return `Change owner: ${resolvedAssignee.display_name}`;
    }
    return "Assign owner";
  });

  let assigneeColor = $derived.by(() => {
    if (!resolvedAssignee) return "var(--accent)";
    if (colorFor) return colorFor(resolvedAssignee.display_name);
    return "var(--accent)";
  });

  // Phase 3 (#104): show the "Unassigned" chip only when the FK is
  // null AND the description has no `<name>` markers.
  let hasNameMarker = $derived(
    /<name>[^<]+<\/name>/.test(item.description ?? ""),
  );
  let showUnassigned = $derived(
    item.assignee_user_id === null && !hasNameMarker,
  );

  // ── Description-edit local state ───────────────────────────────
  //
  // Textarea draft reseeds on entry into description-edit so a
  // revert-then-reopen lands the user back at the stored description,
  // not at a half-typed draft from earlier. Autoresize uses a single
  // effect — seed `height=0` then set `scrollHeight` on every input.
  let descDraft = $state("");
  let descTextareaEl: HTMLTextAreaElement | undefined = $state();
  // Re-seeds the draft on entry into description-edit. `pending` rows
  // skip the seed — the parent appended the row with description=""
  // and the textarea starts empty by design.
  $effect(() => {
    if (editingDescription) {
      descDraft = item.description ?? "";
    }
  });
  // Autoresize: grow up to ~5 rows then scroll. Zero out the height
  // first so `scrollHeight` reports content-only size; set back to
  // that value capped by CSS max-height.
  function autoresize() {
    const el = descTextareaEl;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${el.scrollHeight}px`;
  }
  // Run autoresize on edit-mode entry + whenever the draft changes.
  // #137: depending on `editingDescription` guarantees an on-entry
  // resize for pre-existing multi-line content; otherwise the
  // first-render race between Svelte's bind-materialization and the
  // effect's first flush leaves the textarea stuck at its rows=2
  // intrinsic height. The `queueMicrotask` indirection is the
  // standard Svelte 5 idiom for "read a bind:this ref in the same
  // effect that causes its element to mount" — by the time the
  // microtask runs, the binding has settled.
  $effect(() => {
    if (!editingDescription) return;
    descDraft;
    queueMicrotask(() => {
      if (descTextareaEl) autoresize();
    });
  });

  // ── Owner-edit local state ─────────────────────────────────────
  let assigneeDraft = $state<string | null>(null);
  let assigneeValue = $state("");
  // The outer editor wrapper — bound so the outside-click listener
  // can ignore clicks inside the picker's absolute-positioned
  // listbox.
  let ownerEditorEl: HTMLDivElement | undefined = $state();
  let pickerRoster = $derived.by<OrgMemberLite[]>(() =>
    users.map((u) => ({
      id: u.id,
      display_name: u.display_name,
      email: u.email,
    })),
  );
  $effect(() => {
    if (editingOwner) {
      assigneeDraft = item.assignee_user_id ?? null;
      const current = item.assignee_user_id
        ? users.find((u) => u.id === item.assignee_user_id)
        : null;
      assigneeValue = current?.display_name ?? "";
    }
  });

  // ── Request handlers (click-to-edit entry points) ──────────────

  function requestDescriptionEdit() {
    if (!canEdit) return;
    if (saving || deleting) return;
    onDescriptionEditRequest?.({ item });
  }
  function requestOwnerEdit() {
    if (!canEdit) return;
    if (saving || deleting) return;
    onOwnerEditRequest?.({ item });
  }
  function onDescWrapperKeydown(e: KeyboardEvent) {
    // Keyboard entry into description-edit from the focused `.ai-desc`
    // wrapper. Enter / Space both open the editor. `preventDefault`
    // keeps Space from scrolling the page.
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      requestDescriptionEdit();
    }
  }
  function onOwnerBtnKeydown(e: KeyboardEvent) {
    // Native <button> handles Enter / Space already; this handler
    // exists only to keep the A11Y parity comment visible with the
    // description wrapper. No-op unless a future keystroke maps in.
    void e;
  }

  // ── Description commit / cancel ────────────────────────────────
  //
  // Commit fires on blur / Enter / Tab (blur path). Routing:
  //   • phantom (`pending`) + non-empty     → onPendingSave
  //   • phantom (`pending`) + empty         → onPendingDiscard
  //   • existing row + non-empty            → onDescriptionSave
  //   • existing row + empty                → silent revert
  // The parent decides whether to PATCH and whether to close edit
  // mode — this component just emits.
  function commitDescription() {
    const next = descDraft.trim();
    if (pending) {
      if (next.length === 0) {
        onPendingDiscard?.();
      } else {
        onPendingSave?.({
          description: next,
          assigneeUserId: assigneeDraft ?? item.assignee_user_id ?? null,
        });
      }
      return;
    }
    if (next.length === 0) {
      // Silent revert for existing rows — the user cleared the text
      // then blurred; dropping the change is safer than a backend
      // round-trip. They can delete via trash if that was intent.
      onDescriptionCancel?.({ item });
      return;
    }
    onDescriptionSave?.({ itemId: item.id, description: next });
  }
  function cancelDescription() {
    // Escape path — always discards (no PATCH / POST). For phantoms
    // this unmounts the row entirely.
    if (pending) {
      onPendingDiscard?.();
      return;
    }
    onDescriptionCancel?.({ item });
  }

  function onDescKeydown(e: KeyboardEvent) {
    // Enter (with or without Cmd/Ctrl) commits; Shift+Enter inserts
    // a newline; Escape cancels. Tab doesn't get a handler here; the
    // textarea's native blur fires `onblur` which commits.
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      cancelDescription();
      return;
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      commitDescription();
      return;
    }
  }
  function onDescInput() {
    // Typing clears the inline error so the next commit trigger
    // isn't pre-blocked — retry-by-editing per architect §5.3.
    if (editError) {
      onEditErrorClear?.({ item });
    }
  }
  function onDescBlur(e: FocusEvent) {
    // Blur-commit. Skip if focus moved onto the inline error retry
    // path (there's no retry button in #126 but this guard is cheap).
    // Also skip if the row is mid-save — avoids firing a second
    // blur-commit while the first is still in flight.
    if (saving) return;
    // If an error is showing we block blur-commit until the user
    // types (clearing the error) — architect §5.3.
    if (editError) {
      // Return focus so the error stays actionable. Using
      // requestAnimationFrame so the blur event completes first.
      const el = e.currentTarget as HTMLTextAreaElement;
      requestAnimationFrame(() => el?.focus());
      return;
    }
    commitDescription();
  }

  // ── Owner commit / cancel ──────────────────────────────────────
  //
  // Owner picker fires `onpick` when a roster row is clicked or
  // Enter commits the active row. Free-form text is NOT a valid
  // assignee (FK-only backend). Blur-with-empty clears assignee;
  // blur-with-unmatched-text is treated as cancel (per D5).
  function onPickerPick(pick: SpeakerPick) {
    if (pick.user) {
      // Update local state first (covers the phantom case where
      // there's no row to PATCH), then emit to parent. For phantoms
      // the parent updates its `pendingRow` local state via
      // onOwnerSave (no PATCH); the description commit (later) will
      // POST both fields. For existing rows this triggers the PATCH.
      assigneeDraft = pick.user.id;
      assigneeValue = pick.user.display_name;
      onOwnerSave?.({ itemId: item.id, assigneeUserId: pick.user.id });
    } else {
      // Free-form text — keep the input showing what they typed but
      // don't save anything yet. Blur will decide: empty input →
      // explicit clear; non-empty unmatched → cancel.
      assigneeDraft = null;
      assigneeValue = pick.freeText;
    }
  }
  function onPickerCancel() {
    // Escape inside the picker → revert to chip without PATCH.
    onOwnerCancel?.({ item });
  }
  // The × glyph clears the assignee directly. Fires immediate clear
  // PATCH on existing rows; on phantoms, just mutates local state.
  function clearAssignee() {
    if (!canEdit) return;
    if (saving || deleting) return;
    assigneeDraft = null;
    assigneeValue = "";
    // Phantoms: parent mutates local state only. Existing rows:
    // parent fires the clear PATCH. Same callback, same payload.
    onOwnerSave?.({ itemId: item.id, assigneeUserId: null });
  }

  // Owner outside-click: commit-on-unmatched-text rule from ui-spec
  // §1.b.6. Empty → PATCH null; unmatched text → treat as cancel.
  $effect(() => {
    if (!editingOwner) return;
    function onDocPointerDown(e: PointerEvent) {
      if (saving) return;
      if (!ownerEditorEl) return;
      const target = e.target as Node | null;
      if (target && ownerEditorEl.contains(target)) return;
      const trimmed = assigneeValue.trim();
      if (trimmed.length === 0) {
        // Explicit clear.
        if (pending) {
          assigneeDraft = null;
          onOwnerSave?.({ itemId: item.id, assigneeUserId: null });
          return;
        }
        onOwnerSave?.({ itemId: item.id, assigneeUserId: null });
        return;
      }
      // Unmatched text OR a roster name the user typed manually.
      // Roster-match-by-name: treat as pick.
      const match = pickerRoster.find(
        (m) => m.display_name.toLowerCase() === trimmed.toLowerCase(),
      );
      if (match) {
        assigneeDraft = match.id;
        assigneeValue = match.display_name;
        onOwnerSave?.({ itemId: item.id, assigneeUserId: match.id });
        return;
      }
      // Unmatched free-form text → cancel (ui-spec D5).
      onOwnerCancel?.({ item });
    }
    document.addEventListener("pointerdown", onDocPointerDown, true);
    return () => {
      document.removeEventListener("pointerdown", onDocPointerDown, true);
    };
  });

  // ── Phase 3 (#104): delete affordance + inline confirm ─────────

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
  class:ai-editing={editingDescription}
  class:ai-owner-editing={editingOwner}
  class:ai-pending={pending}
  class:ai-saving={saving}
  class:ai-actions-page={variant === "actions-page"}
>
  <!-- #113: scoped aria-live announcer. Narrates click-to-edit
       transitions (silent on mount + on Escape-cancel). CSS is
       component-scoped — `.ai-sr-announce` does not exist in
       app.css (mirror-pair invariant). -->
  <div class="ai-sr-announce" aria-live="polite" aria-atomic="true">
    {announceText}
  </div>
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
    {#if editingDescription}
      <!-- #126: inline description textarea. Autoresize via
           `autoresize()` on input; max-height caps at ~5 visible rows
           via CSS. Commit on blur / Enter (non-shift); Escape
           cancels. -->
      <div class="ai-desc-editor">
        <!-- svelte-ignore a11y_autofocus — click-to-edit UX
             deliberately lands caret in the textarea on mount.
             Parent only sets editingDescription=true when the user
             explicitly clicked or pressed Enter on the row, so
             focus-capture is user-driven. -->
        <textarea
          class="ai-edit-desc"
          bind:this={descTextareaEl}
          bind:value={descDraft}
          disabled={saving}
          rows="2"
          placeholder="Describe the task…"
          aria-label="Action item description"
          autofocus
          onkeydown={onDescKeydown}
          oninput={onDescInput}
          onblur={onDescBlur}
        ></textarea>
        {#if editError}
          <p class="ai-edit-err" role="alert">{editError}</p>
        {/if}
      </div>
    {:else if isEmpty}
      {#if canEdit}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <span
          class="ai-desc ai-desc-empty"
          role="button"
          tabindex="0"
          aria-label={descEditLabel}
          onclick={requestDescriptionEdit}
          onkeydown={onDescWrapperKeydown}
        >
          <em class="ai-empty">—</em>
        </span>
      {:else}
        <em class="ai-empty">—</em>
      {/if}
    {:else if canEdit}
      <!-- #126: clickable description wrapper. role="button" +
           tabindex gives keyboard + AT parity with the pointer
           affordance. The nested SummaryText contains its own
           chip children (name-linked / name-ambiguous / name-
           external). In v0.4.5 (#140) the unique-match chips are
           interactive buttons when `onchipaction` is wired — the
           wrapper's onclick still fires for clicks on plain text,
           and the chip buttons stopPropagation on their own click
           so the two surfaces don't fight. role=button on the
           wrapper stays valid since interactive nested buttons
           are allowed under the WAI-ARIA 1.2 relaxed rules used
           elsewhere in the app. -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <span
        class="ai-desc"
        role="button"
        tabindex="0"
        aria-label={descEditLabel}
        onclick={requestDescriptionEdit}
        onkeydown={onDescWrapperKeydown}
      >
        <SummaryText
          text={item.description}
          {users}
          {colorFor}
          onchipaction={onchipaction ? onChipActionForward : undefined}
          activeOccurrenceIndex={activeChipOccurrenceIndex}
        />
      </span>
    {:else}
      <SummaryText
        text={item.description}
        {users}
        {colorFor}
        onchipaction={onchipaction ? onChipActionForward : undefined}
        activeOccurrenceIndex={activeChipOccurrenceIndex}
      />
    {/if}
    {#if callContext && variant === "actions-page" && !editingDescription}
      <!-- Phase 4 (#105): /actions row context. -->
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
    <!-- Owner column. Three branches:
         • owner-edit mode → inline SpeakerRenamePicker in a reserved
           right-side lane. Chip unmounts; picker mounts in place.
         • read-only + resolved assignee → button wraps the chip so
           click + keyboard entry is symmetric; × glyph (v0.4.1
           clearAssignee, reactivated here) appears on hover.
         • read-only + unassigned → button wraps the dashed-ring
           chip with the same semantics. -->
    {#if editingOwner}
      <div
        class="ai-owner-editor"
        bind:this={ownerEditorEl}
      >
        <SpeakerRenamePicker
          bind:value={assigneeValue}
          roster={pickerRoster}
          rosterLoaded={true}
          saving={false}
          variant="stack"
          autofocus={true}
          placeholder="Assign to a teammate…"
          noMatchHint="No match — leave empty to keep this item unassigned."
          onpick={onPickerPick}
          oncancel={onPickerCancel}
        />
        {#if editError}
          <p class="ai-edit-err" role="alert">{editError}</p>
        {/if}
      </div>
    {:else if resolvedAssignee}
      {#if canEdit}
        <span class="ai-assignee-wrap">
          <button
            type="button"
            class="ai-assignee-btn"
            aria-label={ownerEditLabel}
            onclick={requestOwnerEdit}
            onkeydown={onOwnerBtnKeydown}
          >
            <span
              class="ai-assignee"
              title="Assigned to {resolvedAssignee.display_name} — click to change"
            >
              <Avatar
                name={resolvedAssignee.display_name}
                color={assigneeColor}
                size={20}
              />
              <span
                class="ai-assignee-name"
                style="--name-c: {assigneeColor}"
              >
                {resolvedAssignee.display_name}
              </span>
            </span>
          </button>
          <!-- #126 (A1): × clear-assignee glyph. Hover-revealed like
               `.ai-del`; always visible on touch via @media hover:none.
               Clicking fires an immediate PATCH to clear. -->
          <button
            type="button"
            class="ai-assignee-clear"
            aria-label="Clear assignee"
            disabled={saving || deleting}
            onclick={clearAssignee}
          >
            <svg
              width="10"
              height="10"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              stroke-width="1.75"
              stroke-linecap="round"
              aria-hidden="true"
            >
              <path d="M4 4l8 8" />
              <path d="M12 4l-8 8" />
            </svg>
          </button>
        </span>
      {:else}
        <span
          class="ai-assignee"
          title="Assigned to {resolvedAssignee.display_name}"
        >
          <Avatar
            name={resolvedAssignee.display_name}
            color={assigneeColor}
            size={20}
          />
          <span
            class="ai-assignee-name"
            style="--name-c: {assigneeColor}"
          >
            {resolvedAssignee.display_name}
          </span>
        </span>
      {/if}
    {:else if showUnassigned}
      {#if canEdit}
        <button
          type="button"
          class="ai-assignee-btn ai-assignee-btn-empty"
          aria-label={ownerEditLabel}
          onclick={requestOwnerEdit}
          onkeydown={onOwnerBtnKeydown}
        >
          <span
            class="ai-assignee-empty"
            title="No assignee — click to assign"
          >
            <span
              class="ai-assignee-empty-dot"
              role="img"
              aria-label="Unassigned"
            ></span>
            <span class="ai-assignee-empty-label">Unassigned</span>
          </span>
        </button>
      {:else}
        <span class="ai-assignee-empty" title="No assignee">
          <span
            class="ai-assignee-empty-dot"
            role="img"
            aria-label="Unassigned"
          ></span>
          <span class="ai-assignee-empty-label">Unassigned</span>
        </span>
      {/if}
    {/if}
  </span>
  {#if canEdit && !editingDescription && !pending}
    {#if confirmingDelete}
      <!-- Phase 3 inline confirm. Unchanged from v0.4.1 except for
           the outer guard — pending (phantom) rows don't render trash
           at all, so confirm is unreachable there. -->
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
          class="ai-del"
          aria-label="Delete action item {index + 1}"
          onclick={handleDeleteRequest}
        >
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
  /* #113 — visually-hidden aria-live region. Clip-pattern mirrors
     ActionsList.svelte's `.actions-sr-count` so the two announcers
     use the same SR posture. Scoped to this component; app.css is
     intentionally untouched (mirror-pair invariant). */
  .ai-sr-announce {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  /* Row shape. Grid for columns (checkbox / idx / body / actions).
     Body is a flex row so the optional assignee chip can sit inline
     on wide viewports and wrap beneath on narrow ones without a
     media query. The fourth column carries `auto` width so rows
     without the trash collapse it. */
  .ai-row {
    display: grid;
    grid-template-columns: 20px 28px 1fr auto;
    gap: 0.55rem;
    /* #132.1 — anchor the four columns to the same first-baseline
       so checkbox + idx + body + assignee chip read as visually
       on the same line. Replaces the hand-tuned per-cell margin /
       padding offsets that compensated for `align-items: start`. */
    align-items: first baseline;
    padding: 0.6rem 0;
    border-bottom: 1px solid var(--hairline);
    color: var(--bone-1);
    font-size: 0.9rem;
    line-height: 1.5;
  }
  :global(.ai-row:last-child) {
    border-bottom: none;
  }

  /* #132.2 — custom-draw the checkbox so the tick glyph is ours to
     size and position. Drops the `accent-color` reliance, which
     renders a different glyph per OS theme and clips on Linux
     webkit2gtk. The 16x16 box stays — only the chrome changes.
     Tick stroke is the cream hex %23f4efe3 hardcoded in the SVG
     data-URI below (data: URIs cannot interpolate CSS vars); it
     contrasts the olive accent in both light and dark themes. */
  .ai-check {
    appearance: none;
    -webkit-appearance: none;
    margin: 0;
    width: 16px;
    height: 16px;
    box-sizing: border-box;
    flex: 0 0 16px;
    border: 1.5px solid var(--hairline-hi);
    border-radius: 4px;
    background: var(--ink-1);
    cursor: default;
    position: relative;
    transition: background 150ms linear, border-color 150ms linear;
    /* Center on the first line-box so the box doesn't drift down on
       multi-line rows. */
    align-self: center;
  }
  .ai-check-live {
    cursor: pointer;
  }
  .ai-check:hover:not(:disabled) {
    border-color: var(--olive);
  }
  .ai-check:checked {
    background: var(--olive);
    border-color: var(--olive);
  }
  /* Tick glyph. SVG path drawn in a 16x16 viewBox, painted at 14x14
     centred — the path lives in the middle 10x10 sub-region with
     ~3px padding on every side, so no clipping under any browser
     anti-aliasing or sub-pixel rounding. Stroke colour is cream
     (%23f4efe3) hardcoded inline because data: URIs can't read
     CSS vars; the same hex reads with AA contrast on olive in
     both themes. */
  .ai-check:checked::after {
    content: "";
    position: absolute;
    inset: 0;
    background-image: url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><path d='M3.5 8.5l3 3 6-7' fill='none' stroke='%23f4efe3' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'/></svg>");
    background-repeat: no-repeat;
    background-position: center;
    background-size: 14px 14px;
  }
  .ai-check:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-check {
      transition: none;
    }
  }

  .ai-idx {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--accent);
    letter-spacing: 0.04em;
    /* #132.1 — drop the hand-tuned padding-top: 0.22rem; baseline
       grid handles vertical alignment now. */
    align-self: baseline;
  }
  .ai-idx-spacer {
    width: 0;
  }

  .ai-body {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.4rem 0.75rem;
    min-width: 0;
  }

  /* Done state: strikethrough + dim. */
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

  /* #126: read-mode description wrapper. Click-to-edit affordance
     lives here — cursor: text hints at the editable prose; focus
     ring covers keyboard / AT users. Nested SummaryText renders
     its own chips; none are interactive so role=button on the
     outer span is ARIA-compliant. */
  .ai-desc {
    display: inline;
    cursor: text;
    border-radius: 4px;
  }
  .ai-desc:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .ai-desc-empty {
    display: inline-block;
  }

  .ai-assignee {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0 0.3rem 0 0.15rem;
    border-radius: 999px;
    line-height: 1.4;
    align-self: baseline;
    white-space: nowrap;
  }

  .ai-assignee-name {
    font-weight: 500;
    font-size: 0.85rem;
    color: var(--name-c, var(--accent));
  }

  /* Empty-description backstop. */
  .ai-empty {
    color: var(--bone-4);
    font-style: normal;
    letter-spacing: 0.08em;
  }

  .ai-check:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* #126: description-edit inline surface. No borders or Save
     bar — the textarea IS the editor. Autoresize via JS; CSS
     caps max-height at ~5 rows before scrolling. */
  .ai-desc-editor {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    width: 100%;
    min-width: 0;
  }
  .ai-edit-desc {
    width: 100%;
    padding: 0.45rem 0.65rem;
    border: 1px solid var(--hairline);
    border-radius: 10px;
    background: var(--ink-1);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.9rem;
    line-height: 1.5;
    /* Autoresize cap: 5 visible rows before the textarea scrolls.
       JS autoresize in $effect owns the height on every keystroke,
       so no CSS `resize` rule — it would only fight the JS on drag. */
    max-height: calc(5 * 1.5em + 0.9rem);
    overflow-y: auto;
    transition: opacity 150ms linear;
  }
  .ai-edit-desc:focus {
    outline: none;
    border-color: var(--accent);
  }
  .ai-edit-desc:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .ai-edit-err {
    margin: 0;
    padding: 0.4rem 0.55rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.82rem;
  }

  /* #126: owner-edit inline surface. Replaces the chip's DOM at the
     right-side lane. Reserves the picker's width so the row doesn't
     shift wildly when swapping chip → picker. */
  .ai-owner-editor {
    margin-left: auto;
    align-self: baseline;
    min-width: 11rem;
    max-width: min(22rem, 70%);
    flex: 0 1 auto;
  }

  /* #126: owner chip as a clickable button. Wraps the chip's visual
     in a transparent button so click + focus-ring land naturally.
     Hover gets a 150ms bg tint — understated, matches Linear-style
     chips. */
  .ai-assignee-wrap {
    display: inline-flex;
    align-items: center;
    gap: 0.1rem;
    margin-left: auto;
    align-self: baseline;
  }
  .ai-assignee-btn {
    display: inline-flex;
    align-items: center;
    padding: 0;
    border: none;
    background: transparent;
    cursor: pointer;
    border-radius: 999px;
    font: inherit;
    color: inherit;
    transition: background 150ms linear;
  }
  .ai-assignee-btn-empty {
    margin-left: auto;
  }
  .ai-assignee-btn:hover {
    background: var(--ink-2);
  }
  .ai-assignee-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-assignee-btn {
      transition: none;
    }
  }

  /* #126 (A1): × clear-assignee glyph. Hover-revealed like .ai-del
     (Phase 3 precedent) — opacity 0.4 at rest, 1 on row-hover or
     focus-visible. Touch viewports get 0.7 always so the affordance
     is still discoverable without pointer hover. */
  .ai-assignee-clear {
    padding: 0.15rem 0.2rem;
    margin-left: -0.1rem;
    border: none;
    background: transparent;
    color: var(--bone-3);
    cursor: pointer;
    border-radius: 4px;
    line-height: 0;
    opacity: 0;
    transition: opacity 150ms linear, color 150ms linear,
      background 150ms linear;
  }
  .ai-row:hover .ai-assignee-clear,
  .ai-assignee-clear:focus-visible {
    opacity: 1;
  }
  .ai-assignee-clear:hover {
    color: var(--live);
    background: var(--ink-2);
  }
  @media (hover: none) {
    .ai-assignee-clear {
      opacity: 0.7;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-assignee-clear {
      transition: none;
    }
  }
  .ai-assignee-clear:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .ai-assignee-clear:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  /* During description-edit: hide the trailing assignee chip + trash
     so the editor owns the row's horizontal space. Owner-edit does
     NOT hide trash (ui-spec D7). */
  .ai-editing .ai-assignee,
  .ai-editing .ai-assignee-empty,
  .ai-editing .ai-assignee-wrap,
  .ai-editing .ai-assignee-btn {
    display: none;
  }
  .ai-editing .ai-actions {
    display: none;
  }

  /* Saving dim — covers description, owner, and phantom POST
     saves. Applied to .ai-body so interactive surfaces stay
     readable but the user sees "something is in flight". */
  .ai-saving .ai-body {
    opacity: 0.65;
    transition: opacity 150ms linear;
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-saving .ai-body {
      transition: none;
    }
  }

  /* Pending (phantom) row — shares the .ai-editing visuals (row
     already carries .ai-editing because the parent opens it in
     description-edit mode on mount). The class is reserved for
     future visual distinction if testers flag the "which is saved
     which is phantom" question. */

  /* ── Phase 3 (#104): delete affordance + inline confirm ──────── */

  .ai-actions {
    display: inline-flex;
    align-items: center;
    gap: 0.15rem;
    /* #132.1 — center on the body's first line. The grid's
       first-baseline alignment doesn't apply cleanly to a cell
       containing only an icon button (no text baseline), so we
       give it an explicit anchor. */
    align-self: center;
  }

  .ai-del {
    padding: 0.22rem 0.35rem;
    /* #132.1 — drop the hand-tuned margin-top: 0.1rem; the
       parent's align-self: center handles vertical alignment. */
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

  .ai-confirm {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-wrap: wrap;
    justify-content: flex-end;
    /* #132.1 — drop margin-top: 0.1rem (compensated for the old
       align-items: start). Centre on the first body line. */
    align-self: center;
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
  .ai-assignee-empty {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
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
  .ai-done .ai-assignee-empty-label {
    text-decoration: line-through;
  }

  /* ── Phase 4 (#105): /actions call-context secondary line ───────── */
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
