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

  // #173 — discriminator for the due-date column. Wire tokens match
  // the backend `action_due_kind` enum 1:1.
  export type ActionItemDueKind = "none" | "asap" | "dated";

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
    // #173 — due-date metadata. `due_at` is `YYYY-MM-DD` only when
    // `due_kind === "dated"`; null otherwise.
    due_kind: ActionItemDueKind;
    due_at: string | null;
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

  // #173 — due-date save event. The row dispatches whichever of the
  // three kinds the user picked; on `dated` the picker chose a
  // `YYYY-MM-DD` date. Parent translates into a PATCH that sends
  // `due_kind` and (for dated) `due_at`.
  export type ActionItemDueSave =
    | { itemId: string; kind: "none" }
    | { itemId: string; kind: "asap" }
    | { itemId: string; kind: "dated"; dueAt: string };
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
  // State matrix (updated for #126; #254 retired the inline editError /
  // deleteError surfaces — failures now route through the toast store
  // owned by the parent page):
  //   default                — read-only row
  //   editingDescription     — inline textarea swap for the description
  //   editingOwner           — inline picker swap for the owner chip
  //   pending                — phantom row, `pending: true`, id prefixed
  //   saving                 — PATCH / POST in flight; .ai-body dims
  //   confirmingDelete       — unchanged from Phase 3
  //   deleting               — DELETE in flight; failures bubble to the
  //                            page-level toast store rather than an
  //                            inline `.ai-confirm-err` retry button.
  //
  // Phase 4 still owns `ontoggle` for check-off — wiring unchanged.

  import Avatar from "./Avatar.svelte";
  import SummaryText from "./SummaryText.svelte";
  import SpeakerRenamePicker, {
    type OrgMemberLite,
    type SpeakerPick,
  } from "./SpeakerRenamePicker.svelte";
  import DateInput from "./DateInput.svelte";
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
    //   canEdit            — permission gate. False → all click-to-
    //                        edit affordances (cursor / role /
    //                        tabindex) are suppressed.
    //
    // #254 — `editError` / `onEditErrorClear` / `deleteError` retired.
    // Save + delete failures now flow through the page-level toast
    // store; the row no longer renders an inline error line or a
    // confirm-retry button.
    editingDescription?: boolean;
    editingOwner?: boolean;
    // #173 — edit mode for the due-date row affordance. Mutually
    // exclusive with description / owner edit (parent enforces).
    editingDue?: boolean;
    pending?: boolean;
    saving?: boolean;
    canEdit?: boolean;
    confirmingDelete?: boolean;
    deleting?: boolean;
    // #282 — keyboard-driven highlight (j/k navigation on the
    // /actions page). Page-level state owns this; ActionItem just
    // paints `data-highlighted` on the row so the scoped CSS below
    // can show the active rail. No behavior change.
    highlighted?: boolean;

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
    // #173 — due-date edit lifecycle. Mirrors the description /
    // owner pattern: request opens the editor, save commits a PATCH,
    // cancel discards.
    onDueEditRequest?: (payload: { item: ActionItem }) => void;
    onDueSave?: (payload: ActionItemDueSave) => void;
    onDueCancel?: (payload: { item: ActionItem }) => void;

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
    editingDue = false,
    pending = false,
    saving = false,
    canEdit = false,
    confirmingDelete = false,
    deleting = false,
    onDescriptionEditRequest,
    onOwnerEditRequest,
    onDescriptionSave,
    onOwnerSave,
    onDescriptionCancel,
    onOwnerCancel,
    onPendingSave,
    onPendingDiscard,
    onDueEditRequest,
    onDueSave,
    onDueCancel,
    ondeleterequest,
    ondeleteconfirm,
    ondeletecancel,
    ontoggle,
    onchipaction,
    activeChipOccurrenceIndex = null,
    highlighted = false,
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
  let anyEditing = $derived(editingDescription || editingOwner || editingDue);

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
  // transition and announces "…updated". Failures route through the
  // page-level toast store (#254) and don't suppress the SR
  // announcement here — the toast carries the error narration via
  // its own `role="alert"` host.
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
      announceText = `Action item ${index + 1} updated`;
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

  // Every FK-null action item gets the "Unassigned" affordance so the
  // user can always invoke the owner-edit picker. Suppressing this on
  // rows with `<name>` markers (#104 Phase 3) silently dropped the
  // assign affordance when the AI couldn't auto-resolve the mention —
  // see #172 and `backend/src/routes/pipeline.rs` ≈L561 contract.
  let showUnassigned = $derived(item.assignee_user_id === null);

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
  function onDescBlur(_e: FocusEvent) {
    // Blur-commit. Skip if the row is mid-save — avoids firing a
    // second blur-commit while the first is still in flight. #254
    // retired the editError retry-by-typing gate; failed saves now
    // surface via the page-level toast store and the user can simply
    // re-edit + blur again to retry.
    if (saving) return;
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

  // ── #173: Due-date affordance + inline editor ──────────────────
  //
  // Read-mode chip surfaces the row's due state next to the assignee
  // chip. Three visual cases:
  //   - kind='dated' + due_at < today + status='open' → "Overdue {date}", live-red
  //   - kind='dated'                                    → "Due {date}", subtle teal
  //   - kind='asap'                                     → "ASAP", olive
  //   - kind='none'                                     → no chip, but a faint
  //                                                       "+ Due" affordance for
  //                                                       canEdit users.
  // Click → request edit; the edit lane renders a segmented
  // [None][ASAP][On date] control with a DateInput beneath when On
  // date is selected.

  // Local draft state for the due-date editor. Re-seeded on entry so
  // a cancel-then-reopen doesn't leak previous draft state.
  let dueDraftKind = $state<ActionItemDueKind>("none");
  let dueDraftDate = $state<string>("");
  $effect(() => {
    if (editingDue) {
      dueDraftKind = item.due_kind;
      // Seed the date picker with the existing date when present;
      // otherwise leave it empty so picking "On date" prompts the
      // user to choose.
      dueDraftDate = item.due_at ?? "";
    }
  });

  // Format helpers. We avoid `Intl.DateTimeFormat` for small chips —
  // the YYYY-MM-DD wire shape parses without TZ and the formatted
  // chip uses month-abbrev + day, no year (matching how Linear /
  // Things render due chips). The year shows up in the picker.
  const MONTH_ABBREV = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];
  function formatDueChipDate(iso: string): string {
    // ISO 'YYYY-MM-DD' → 'Apr 30'. Avoid `new Date(iso)` which
    // interprets ISO dates as UTC and shifts a day in negative-UTC
    // locales (the same DateInput-era trap).
    if (!/^\d{4}-\d{2}-\d{2}$/.test(iso)) return iso;
    const [, m, d] = iso.split("-").map(Number);
    return `${MONTH_ABBREV[m - 1]} ${d}`;
  }

  // Today as YYYY-MM-DD in local time. Used both for the overdue
  // comparison and for the DateInput's `min`-seed default.
  function todayIso(): string {
    const t = new Date();
    const y = t.getFullYear();
    const m = String(t.getMonth() + 1).padStart(2, "0");
    const d = String(t.getDate()).padStart(2, "0");
    return `${y}-${m}-${d}`;
  }

  // Lexical compare on YYYY-MM-DD is correct.
  let dueIsOverdue = $derived.by(() => {
    if (item.due_kind !== "dated" || !item.due_at) return false;
    if (item.status !== "open") return false;
    return item.due_at < todayIso();
  });
  let dueChipText = $derived.by(() => {
    if (item.due_kind === "asap") return "ASAP";
    if (item.due_kind === "dated" && item.due_at) {
      if (dueIsOverdue) return `Overdue ${formatDueChipDate(item.due_at)}`;
      return `Due ${formatDueChipDate(item.due_at)}`;
    }
    return "";
  });
  let hasDueChip = $derived(item.due_kind !== "none" && dueChipText !== "");

  function requestDueEdit() {
    if (!canEdit) return;
    if (saving || deleting) return;
    onDueEditRequest?.({ item });
  }

  function commitDue() {
    if (dueDraftKind === "none") {
      onDueSave?.({ itemId: item.id, kind: "none" });
      return;
    }
    if (dueDraftKind === "asap") {
      onDueSave?.({ itemId: item.id, kind: "asap" });
      return;
    }
    // dated — require a non-empty draft date; otherwise treat the
    // commit as a cancel (the user moved away without picking).
    const trimmed = dueDraftDate.trim();
    if (!trimmed) {
      onDueCancel?.({ item });
      return;
    }
    onDueSave?.({ itemId: item.id, kind: "dated", dueAt: trimmed });
  }

  function cancelDue() {
    onDueCancel?.({ item });
  }

  function pickDueKind(next: ActionItemDueKind) {
    dueDraftKind = next;
    // Auto-commit None and ASAP — there's no second step. Dated
    // waits for the DateInput's onchange before committing so the
    // user has a chance to pick a real date.
    if (next === "none" || next === "asap") {
      commitDue();
    } else if (next === "dated" && dueDraftDate) {
      // If the row was already dated and we re-clicked On date with
      // the existing date pre-loaded, commit the unchanged value as
      // a no-op so the editor closes. The parent's optimistic-flip
      // handler short-circuits when `due_kind` + `due_at` haven't
      // changed.
      commitDue();
    }
  }

  function onDueDatePicked(value: string) {
    dueDraftDate = value;
    // Picking a date on the inline DateInput auto-commits — same
    // pattern as the SpeakerRenamePicker's `onpick`. Empty value
    // (Clear button) demotes the row back to "no date" by sending
    // kind='none'.
    if (!value) {
      dueDraftKind = "none";
      onDueSave?.({ itemId: item.id, kind: "none" });
      return;
    }
    dueDraftKind = "dated";
    onDueSave?.({ itemId: item.id, kind: "dated", dueAt: value });
  }

  function onDueKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      cancelDue();
    }
  }

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
  data-action-item-id={item.id}
  data-shortcut-row={highlighted ? "active" : null}
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
          onblur={onDescBlur}
        ></textarea>
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
    <!-- #294 — meta cluster wrapper. Reserves a fixed-width column for
         the optional due-date / ASAP badge so the assignee chip's
         x-coordinate stays constant row-to-row regardless of badge
         presence. The owner-cell holds the chip (or its editor /
         unassigned placeholder); the due-cell holds the badge (or its
         editor / + Due affordance / a non-breaking-space placeholder
         when nothing else renders, so the column always occupies its
         reserved width). The grid only applies above the mobile
         breakpoint — see `.ai-meta` rules at the bottom of <style>. -->
    <span class="ai-meta">
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
    <!-- #173: due-date chip + edit lane. Three branches:
         1. editingDue          → segmented control + (optionally) DateInput
         2. read-mode w/ due    → coloured chip (Overdue red / Due teal / ASAP olive)
         3. read-mode no due    → faint "+ Due" affordance for canEdit users -->
    {#if editingDue}
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <div
        class="ai-due-editor"
        role="group"
        aria-label="Set due date for action item {index + 1}"
        onkeydown={onDueKeydown}
      >
        <div class="ai-due-segmented" role="radiogroup" aria-label="Due type">
          <button
            type="button"
            class="ai-due-seg"
            class:ai-due-seg-active={dueDraftKind === "none"}
            role="radio"
            aria-checked={dueDraftKind === "none"}
            disabled={saving}
            onclick={() => pickDueKind("none")}
          >
            None
          </button>
          <button
            type="button"
            class="ai-due-seg"
            class:ai-due-seg-active={dueDraftKind === "asap"}
            role="radio"
            aria-checked={dueDraftKind === "asap"}
            disabled={saving}
            onclick={() => pickDueKind("asap")}
          >
            ASAP
          </button>
          <button
            type="button"
            class="ai-due-seg"
            class:ai-due-seg-active={dueDraftKind === "dated"}
            role="radio"
            aria-checked={dueDraftKind === "dated"}
            disabled={saving}
            onclick={() => pickDueKind("dated")}
          >
            On date
          </button>
        </div>
        {#if dueDraftKind === "dated"}
          <DateInput
            value={dueDraftDate}
            onchange={onDueDatePicked}
            ariaLabel="Due date"
            placeholder="Pick a date"
          />
        {/if}
      </div>
    {:else if hasDueChip}
      {#if canEdit}
        <button
          type="button"
          class="ai-due-chip-btn"
          aria-label="Change due date"
          onclick={requestDueEdit}
        >
          <span
            class="ai-due-chip"
            class:ai-due-chip-overdue={dueIsOverdue}
            class:ai-due-chip-asap={item.due_kind === "asap"}
            class:ai-due-chip-dated={item.due_kind === "dated" && !dueIsOverdue}
          >
            {dueChipText}
          </span>
        </button>
      {:else}
        <span
          class="ai-due-chip"
          class:ai-due-chip-overdue={dueIsOverdue}
          class:ai-due-chip-asap={item.due_kind === "asap"}
          class:ai-due-chip-dated={item.due_kind === "dated" && !dueIsOverdue}
        >
          {dueChipText}
        </span>
      {/if}
    {:else if canEdit && !editingDescription && !editingOwner}
      <button
        type="button"
        class="ai-due-add"
        aria-label="Set due date"
        onclick={requestDueEdit}
      >
        <span aria-hidden="true">＋</span>
        <span>Due</span>
      </button>
    {:else}
      <!-- #294 — placeholder so the badge cell still occupies its
           reserved grid column on rows that render no due affordance
           (non-canEdit + no due). Not focusable, ignored by AT. -->
      <span class="ai-meta-placeholder" aria-hidden="true">&nbsp;</span>
    {/if}
    </span>
  </span>
  {#if canEdit && !editingDescription && !editingDue && !pending}
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
        <!-- #254 — failed deletes route through the page-level toast
             store. The user can simply press Delete again to retry,
             so the inline `.ai-confirm-err` + Retry button retired. -->
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
            width="16"
            height="16"
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
    padding: 0.6rem 0 0.6rem 0.6rem;
    border-bottom: 1px solid var(--hairline);
    color: var(--bone-1);
    font-size: 0.9rem;
    line-height: 1.5;
  }
  :global(.ai-row:last-child) {
    border-bottom: none;
  }
  /* #282 — keyboard-driven highlight (j/k on /actions). Mirrors the
     calls-list active-row styling: subtle accent rail, faint tint.
     Component-scoped — does not bleed into app.css. The 0.6rem
     left padding on .ai-row above is what keeps the 2px rail from
     visually intersecting the leftmost checkbox column. */
  .ai-row[data-shortcut-row="active"] {
    background: var(--ink-1);
    box-shadow: inset 2px 0 0 var(--accent);
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

  /* #294 — meta cluster: assignee chip + optional due badge. A
     2-column grid with a `minmax(80px, auto)` second column so the
     badge cell occupies its full width even when the row's due
     affordance is empty / hidden. Net result: the assignee chip's
     left edge stays at the same x-coordinate row-to-row, regardless
     of whether the row carries an "ASAP" / "Due Apr 26" badge.

     `margin-left: auto` takes over from the per-chip `margin-left:
     auto` rules below — those still apply when .ai-meta is reset on
     mobile (so the chip continues to hug the right inside the body
     flexbox on edge cases not visited here). */
  .ai-meta {
    display: grid;
    /* Badge column was minmax(80px, auto) — fits "Due May 1" /
       "ASAP" but the wider "Overdue Apr 24" badge auto-grows past
       80px and pushes the assignee column left, so rows with
       overdue badges no longer align with rows that have shorter
       badges. Pin the column to a fixed 112px (just over the
       widest "Overdue Mon Day" rendering) so every row's assignee
       chip lands at the same x-coordinate regardless of badge
       length. The per-chip `margin-left: auto` rules still apply
       on the mobile reset where this grid is unset. */
    grid-template-columns: auto 112px;
    align-items: baseline;
    gap: 0.4rem;
    margin-left: auto;
  }
  .ai-meta-placeholder {
    /* The placeholder still needs measurable width so the grid's
       minmax(80px, auto) pins the column; an `&nbsp;` alone collapses
       on some browsers. `display: inline-block` + min-width is the
       portable belt-and-braces. */
    display: inline-block;
    min-width: 1px;
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

  /* #254 — `.ai-edit-err` retired. Save failures now route through the
     page-level toast store; the row no longer paints an inline error
     line beneath the textarea / picker. */

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
     + due chip so the editor owns the row's horizontal space. Owner-
     edit does NOT hide trash (ui-spec D7). #294 also hides the .ai-
     meta wrapper outright so its reserved 80px badge column doesn't
     leave a phantom gap beside the editor. */
  .ai-editing .ai-meta,
  .ai-editing .ai-assignee,
  .ai-editing .ai-assignee-empty,
  .ai-editing .ai-assignee-wrap,
  .ai-editing .ai-assignee-btn,
  .ai-editing .ai-due-chip,
  .ai-editing .ai-due-chip-btn,
  .ai-editing .ai-due-add,
  .ai-editing .ai-due-editor {
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
    padding: 0.4rem 0.5rem;
    /* #132.1 — drop the hand-tuned margin-top: 0.1rem; the
       parent's align-self: center handles vertical alignment. */
    border: none;
    background: transparent;
    color: var(--bone-2);
    cursor: pointer;
    border-radius: 4px;
    line-height: 0;
    opacity: 0.55;
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
  .ai-confirm-cancel {
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
  .ai-confirm-delete {
    border-color: var(--live);
    background: var(--live-soft);
    color: var(--live);
  }
  .ai-confirm-delete:hover:not(:disabled) {
    background: var(--live);
    color: var(--ink-0);
  }
  .ai-confirm-cancel:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .ai-confirm-delete:disabled,
  .ai-confirm-cancel:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* #254 — `.ai-confirm-err` + `.ai-confirm-retry` retired alongside
     the inline retry button. Failed deletes now toast via the page-
     level store; the user re-presses Delete to retry. */

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

  /* ── #173: Due-date chip + editor ─────────────────────────────── */

  /* Pill-shaped chip; identical baseline alignment to the assignee
     chip so the two sit on the same row. Color tokens encode the
     three states (overdue → live red, dated → accent teal, asap →
     olive complete). All chips share the same shape so the eye
     reads "this is a metadata pill" regardless of state. */
  .ai-due-chip {
    display: inline-flex;
    align-items: center;
    padding: 0.05rem 0.45rem;
    border-radius: 999px;
    font-size: 0.75rem;
    font-weight: 500;
    line-height: 1.4;
    align-self: baseline;
    white-space: nowrap;
    border: 1px solid var(--hairline);
    color: var(--bone-1);
    background: var(--ink-1);
  }
  .ai-due-chip-dated {
    border-color: color-mix(in srgb, var(--accent) 50%, var(--hairline));
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 10%, var(--ink-1));
  }
  .ai-due-chip-asap {
    border-color: color-mix(in srgb, var(--olive) 50%, var(--hairline));
    color: var(--olive);
    background: color-mix(in srgb, var(--olive) 10%, var(--ink-1));
  }
  .ai-due-chip-overdue {
    border-color: var(--live);
    color: var(--live);
    background: var(--live-soft);
  }
  /* Done rows mute the chip — same as the assignee strikethrough
     pattern. */
  .ai-done .ai-due-chip {
    opacity: 0.5;
    text-decoration: line-through;
  }

  /* Wrapper button so click + focus-ring land naturally. Inherits
     the same low-key hover treatment as the assignee chip. */
  .ai-due-chip-btn {
    display: inline-flex;
    align-items: center;
    padding: 0;
    border: none;
    background: transparent;
    cursor: pointer;
    border-radius: 999px;
    font: inherit;
    color: inherit;
    transition: opacity 150ms linear;
  }
  .ai-due-chip-btn:hover {
    opacity: 0.85;
  }
  .ai-due-chip-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-due-chip-btn {
      transition: none;
    }
  }

  /* Faint "+ Due" affordance for canEdit users on rows without a
     due date. Hidden until row hover / focus to keep the empty
     state visually quiet. */
  .ai-due-add {
    display: inline-flex;
    align-items: center;
    gap: 0.2rem;
    padding: 0.05rem 0.4rem;
    border: 1px dashed var(--hairline-hi);
    border-radius: 999px;
    background: transparent;
    color: var(--bone-3);
    font: inherit;
    font-size: 0.72rem;
    cursor: pointer;
    opacity: 0;
    align-self: baseline;
    transition: opacity 150ms linear, border-color 150ms linear,
      color 150ms linear;
  }
  .ai-row:hover .ai-due-add,
  .ai-due-add:focus-visible {
    opacity: 1;
  }
  .ai-due-add:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  @media (hover: none) {
    .ai-due-add {
      opacity: 0.7;
    }
  }
  .ai-due-add:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-due-add {
      transition: none;
    }
  }

  /* Editor lane — segmented control + (optionally) DateInput stacked
     beneath it. Stack vertical because horizontal would push the row
     too wide on narrow viewports; the 11rem min-width matches the
     owner editor's reservation for visual rhythm. */
  .ai-due-editor {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    align-self: baseline;
    min-width: 11rem;
    margin-left: auto;
  }
  .ai-due-segmented {
    display: inline-flex;
    gap: 0.15rem;
    padding: 0.15rem;
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    border-radius: 6px;
  }
  .ai-due-seg {
    flex: 1 1 auto;
    padding: 0.25rem 0.55rem;
    border: none;
    background: transparent;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.75rem;
    font-weight: 500;
    cursor: pointer;
    border-radius: 4px;
    transition: background 120ms linear, color 120ms linear;
  }
  .ai-due-seg:hover:not(:disabled) {
    color: var(--bone-0);
    background: var(--ink-2);
  }
  .ai-due-seg-active {
    color: var(--bone-0);
    background: var(--ink-2);
    box-shadow: inset 0 0 0 1px var(--hairline-hi);
  }
  .ai-due-seg:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .ai-due-seg:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  @media (prefers-reduced-motion: reduce) {
    .ai-due-seg {
      transition: none;
    }
  }

  /* ── #255 part 5: mobile pass ─────────────────────────────────────
     Below 640px the grid row collapses into a card-stack: checkbox
     keeps its left column (≥44px tap target), the body wraps onto
     its own block with the description on top + assignee/due chips
     beneath, and the trash button gets an explicit ≥44px touch
     surface. Affordances that are hover-revealed on desktop (×
     clear-assignee, + Due) become always-visible on touch so users
     without a pointer can find them. Editor inputs go to 16px font
     to dodge iOS Safari's focus-zoom.

     All declarations are component-scoped — `app.css` mirror-pair
     invariant kept intact (#255 acceptance bar). Changes are purely
     additive @media; the desktop grid path is untouched. */
  @media (max-width: 640px) {
    .ai-row {
      /* Two columns: a 44px checkbox/idx lane + the body. Trash
         falls to the next row aligned right via grid-column placement
         on `.ai-actions`. Actions-page rows already hide the idx
         (spacer width 0) so the lane reads as a clean checkbox column
         on /actions and as a checkbox+idx column on call-detail. */
      grid-template-columns: 44px 1fr;
      column-gap: 0.5rem;
      row-gap: 0.4rem;
      padding: 0.85rem 0;
      align-items: start;
    }
    .ai-check {
      /* Keep the visual box at 16px but enlarge the tap target to
         ≥44px via padding-box. The native input gets `min-` extents
         so iOS / Android register full-cell taps even though the
         glyph stays small. */
      width: 16px;
      height: 16px;
      align-self: start;
      margin-top: 0.15rem;
      justify-self: center;
    }
    .ai-idx {
      grid-column: 1;
      justify-self: center;
      align-self: start;
      margin-top: 0.05rem;
    }
    /* On the /actions page the spacer occupies the same column as
       the checkbox; collapse it so the body owns the right-hand
       column outright. */
    .ai-idx-spacer {
      display: none;
    }
    .ai-body {
      grid-column: 2;
      flex-direction: column;
      align-items: stretch;
      gap: 0.4rem;
      min-width: 0;
    }
    /* Description + editor occupy the full body width on mobile. */
    .ai-desc,
    .ai-desc-empty,
    .ai-desc-editor {
      width: 100%;
    }
    /* iOS focus-zoom dodge — textarea + picker inputs land at 16px
       regardless of the desktop 0.9rem default. */
    .ai-edit-desc {
      font-size: 16px;
      line-height: 1.45;
    }
    /* #294 — drop the desktop 2-column grid on mobile. The card-stack
       puts chips beneath the description in a left-hugging row, so
       the badge-column reservation is unnecessary (and the grid would
       fight `.ai-body`'s column flexbox). Switch to inline-flex so
       chip + badge sit on the same line, wrapping if needed. */
    .ai-meta {
      display: flex;
      flex-wrap: wrap;
      align-items: baseline;
      gap: 0.4rem;
      margin-left: 0;
    }
    .ai-meta-placeholder {
      display: none;
    }
    /* Assignee + due chips share a meta row beneath the description.
       `margin-left: auto` from the desktop layout pushes both chips
       off-screen on a column flexbox; reset so they hug the left
       edge under the description. */
    .ai-assignee,
    .ai-assignee-wrap,
    .ai-assignee-btn-empty,
    .ai-assignee-empty {
      margin-left: 0;
    }
    .ai-assignee-wrap {
      gap: 0.2rem;
    }
    /* × clear-assignee + + Due are hover-revealed on desktop. On
       touch the user has no hover, so make both fully visible. The
       existing `@media (hover: none)` rule sets opacity 0.7; bump
       to 1 in the explicit narrow-viewport rule so the affordance
       reads cleanly when both queries match. ≥44px tap target via
       padding. */
    .ai-assignee-clear {
      opacity: 1;
      min-width: 32px;
      min-height: 32px;
      padding: 0.4rem;
    }
    .ai-due-add {
      opacity: 1;
      min-height: 32px;
      padding: 0.3rem 0.6rem;
      font-size: 0.78rem;
    }
    .ai-due-chip-btn {
      min-height: 32px;
    }
    /* Owner editor at narrow widths — stop reserving 11rem (which
       overflows the 320-360px viewports) and instead fill the body
       column. Reset margin-left so the editor doesn't push past the
       column. */
    .ai-owner-editor {
      margin-left: 0;
      min-width: 0;
      max-width: 100%;
      width: 100%;
    }
    .ai-due-editor {
      margin-left: 0;
      width: 100%;
    }
    /* Due segmented control — bump segment min-height to ≥44px so
       the three buttons are tappable. */
    .ai-due-seg {
      min-height: 44px;
      font-size: 0.85rem;
    }
    /* Trash button: card-stack drops it onto its own row, right-
       aligned beneath the body. ≥44px tap target. */
    .ai-actions {
      grid-column: 2;
      justify-self: end;
      align-self: center;
    }
    .ai-del {
      min-width: 44px;
      min-height: 44px;
      padding: 0.6rem;
      /* Visible at rest on touch — the desktop hover-reveal rule
         hides this for users without a pointer. */
      opacity: 0.85;
    }
    /* Inline delete-confirm: stack vertically so the buttons keep
       full width + ≥44px height on narrow viewports. */
    .ai-confirm {
      grid-column: 2;
      justify-content: flex-end;
      flex-wrap: wrap;
      gap: 0.4rem;
    }
    .ai-confirm-delete,
    .ai-confirm-cancel,
    .ai-confirm-retry {
      min-height: 44px;
      padding: 0.55rem 0.9rem;
      font-size: 0.85rem;
    }
    /* /actions secondary line — give the call-context link its own
       block + ≥44px tap target so users can navigate to the parent
       call comfortably from a touch device. */
    .actx-context {
      flex-basis: auto;
      min-height: 32px;
      padding: 0.2rem 0;
    }
  }
</style>
