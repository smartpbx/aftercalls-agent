<!--
  LiveQuestions — Phase 4 (live↔after-call continuity). A very simple "open
  questions / who asked / answered + what the answer was" list. It owns the
  "Questions" pane of `CoPilotPanel`'s on-demand drawer — a peer of the
  transcript pane, not a strip stacked above it (a long ledger used to squeeze
  the transcript out of the fixed-height dashboard drawer; the pane scrolls
  itself). Both sides count: a
  question the rep OR the counterpart asks that should get answered, each
  attributed to who asked it.

  Open questions render FIRST (a hollow dot + an asker chip + the question);
  answered ones follow, checked off (an `--olive` check) with the captured
  answer beneath. Answers are sticky on the backend (a question never
  un-answers on its own), so a model pass only ever gains answers — the REP can
  reopen one by hand.

  Each frame is a FULL snapshot; the component renders it wholesale (no
  reconcile — the store swaps the snapshot). On call end it simply freezes as
  the final list.

  TWO live corrections the rep can make, both mid-call:
   1. **Asker relabel (automatic).** `asker_display` is resolved backend-side at
      CAPTURE, so a question asked before its speaker was identified froze on
      "Speaker ?". `displayFor` re-resolves it here against the SAME
      `speakerIdentities` map the transcript lane uses, keyed on the question's
      `asker_label` — so assigning a speaker renames every question they already
      asked, instantly, without waiting for the backend's next coach pass. A
      `asker_pinned` entry (the rep typed the name) is never overridden.
   2. **Manual edit.** Edit the text, retype who asked, mark answered / reopen,
      delete, or add a question the model missed — each one `POST`s to
      `/v1/live/questions` via the `live_question_edit` shim and swaps in the
      returned snapshot.

  HARD RULES honored:
   - All chrome is component-scoped `.lq-*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque — no STT/LLM/infra provider name anywhere (HR#2).
   - Question + answer text are TRANSCRIPT- or REP-derived — rendered as plain
     `{...}` interpolation ONLY, never `{@html}`.
   - Reuses the `LiveChecklist` visual idiom (`--olive` check = answered, hollow
     dot = open).
-->
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type {
    LiveQuestion,
    QuestionsSnapshot,
    SpeakerIdentity,
  } from "@aftercalls/shared/types";
  import { speakerIdentityKey } from "$lib/stores/liveSession.svelte";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    questions = null,
    status = "idle",
    speakerIdentities = new Map<string, SpeakerIdentity>(),
    sessionUuid = null,
    onsnapshot = undefined,
  }: {
    /** Latest FULL snapshot; null pre-first-frame (and cleared on new session). */
    questions?: QuestionsSnapshot | null;
    /** Drives the ended freeze (kept for parity with LiveChecklist; the list
     *  simply stops updating). */
    status?: LiveStatus;
    /** #646 (Phase 2) — the same per-speaker identity map the transcript lane
     *  relabels off. Consulted per question so an assignment renames the asker
     *  immediately (see `displayFor`). */
    speakerIdentities?: Map<string, SpeakerIdentity>;
    /** Live session anchor for edits. Null pre-call / post-reset — editing is
     *  hidden without it (there is no ledger to write to). */
    sessionUuid?: string | null;
    /** Raised with the full post-edit snapshot the endpoint returns, so the
     *  parent can swap it into the store exactly like a WS frame. */
    onsnapshot?: (next: QuestionsSnapshot) => void;
  } = $props();

  let all = $derived(questions?.questions ?? []);
  // Open-first: open questions are the live "what still needs answering"; the
  // answered ones recede below, checked off. Snapshot order is preserved within
  // each group (the backend orders by asked_at).
  let openList = $derived(all.filter((q) => q.status === "open"));
  let answeredList = $derived(all.filter((q) => q.status === "answered"));
  // Prefer the backend's counts (authoritative); fall back to the list lengths.
  let openCount = $derived(questions?.open_count ?? openList.length);

  // On call-end the list freezes (the store retains the last snapshot). The
  // live-accent left border recedes to a quiet hairline then — it's no longer a
  // "answer these now" prompt, just the call's record.
  let isEnded = $derived(status === "ended");
  // Editing needs a live session to write the ledger on. After the call ends the
  // after-call view owns corrections (`/v1/calls/{id}/questions`).
  let canEdit = $derived(!!sessionUuid && !isEnded);

  // ── Asker display (live re-resolution) ──────────────────────────────────
  // The backend resolves `asker_display` when the question is CAPTURED. A
  // speaker identified thirty seconds later doesn't retroactively fix that
  // string until the next coach pass re-resolves the ledger — so resolve it
  // here too, off the live identity map, keyed on the raw diarization label the
  // question carries. The far side is always the `system` channel; the near
  // "You" side is never renamed off an assignment.
  function displayFor(q: LiveQuestion): string {
    if (q.asker_pinned || q.asker_side === "you" || !q.asker_label) {
      return q.asker_display;
    }
    const idn = speakerIdentities.get(
      speakerIdentityKey("system", q.asker_label),
    );
    return idn?.display_name || q.asker_display;
  }

  // ── Edit state ──────────────────────────────────────────────────────────
  // One row at a time; `"new"` is the add-a-question composer.
  let editingId = $state<string | null>(null);
  let draftText = $state("");
  let draftAsker = $state("");
  let draftSide = $state<"you" | "them">("you");
  let busyId = $state<string | null>(null);
  let editError = $state("");
  let announce = $state("");

  function startEdit(q: LiveQuestion) {
    editingId = q.id;
    draftText = q.text;
    draftAsker = displayFor(q);
    draftSide = q.asker_side;
    editError = "";
  }
  function startAdd() {
    editingId = "new";
    draftText = "";
    draftAsker = "You";
    draftSide = "you";
    editError = "";
  }
  function cancelEdit() {
    editingId = null;
    draftText = "";
    draftAsker = "";
    editError = "";
  }

  /** One ledger mutation. Swaps in the returned snapshot on success; on failure
   *  leaves the list untouched and surfaces a plain-bone message (no red panel
   *  — design.md semantic-colour rule) so the rep can retry. */
  async function commit(
    op: "add" | "edit" | "delete" | "answer" | "reopen",
    id: string | null,
    fields: {
      text?: string;
      askerSide?: "you" | "them";
      askerDisplay?: string;
      answerText?: string;
    } = {},
  ) {
    if (!sessionUuid || busyId) return;
    busyId = id ?? "new";
    editError = "";
    try {
      const next = (await invoke("live_question_edit", {
        sessionUuid,
        op,
        id: id ?? undefined,
        text: fields.text,
        askerSide: fields.askerSide,
        askerDisplay: fields.askerDisplay,
        answerText: fields.answerText,
      })) as QuestionsSnapshot;
      onsnapshot?.(next);
      editingId = null;
      announce =
        op === "delete"
          ? "Question removed."
          : op === "answer"
            ? "Question marked answered."
            : op === "reopen"
              ? "Question reopened."
              : op === "add"
                ? "Question added."
                : "Question updated.";
    } catch (e) {
      console.warn("live_question_edit failed", e);
      editError = "Couldn't save that just now.";
    } finally {
      busyId = null;
    }
  }

  function saveEdit(q: LiveQuestion) {
    const text = draftText.trim();
    if (!text) {
      editError = "A question needs some text.";
      return;
    }
    const asker = draftAsker.trim();
    void commit("edit", q.id, {
      text,
      askerSide: draftSide,
      // Only send the display when the rep actually changed it — sending it
      // unchanged would PIN the asker and stop it tracking future speaker
      // assignments for no reason.
      askerDisplay: asker && asker !== displayFor(q) ? asker : undefined,
    });
  }

  function saveAdd() {
    const text = draftText.trim();
    if (!text) {
      editError = "A question needs some text.";
      return;
    }
    void commit("add", null, {
      text,
      askerSide: draftSide,
      askerDisplay: draftAsker.trim() || undefined,
    });
  }

  function onDraftKeydown(e: KeyboardEvent, q: LiveQuestion | null) {
    e.stopPropagation();
    if (e.key === "Escape") {
      e.preventDefault();
      cancelEdit();
      return;
    }
    // Enter commits (the field is one line); Shift+Enter is left alone so a
    // future multi-line field keeps the newline.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (q) saveEdit(q);
      else saveAdd();
    }
  }
</script>

{#if questions && (all.length > 0 || canEdit)}
  <section class="lq" class:ended={isEnded} aria-label="Questions">
    <span class="lq-sr" aria-live="polite">{announce}</span>
    <div class="lq-head">
      <span class="lq-label">Questions</span>
      {#if all.length === 0}
        <span class="lq-count">None yet</span>
      {:else if openCount > 0}
        <span class="lq-count">{openCount} open</span>
      {:else}
        <span class="lq-count all-answered">All answered</span>
      {/if}
    </div>

    <ul class="lq-items">
      {#each openList as q (q.id)}
        <li class="lq-item open">
          {#if editingId === q.id}
            {@render editor(q)}
          {:else}
            <span class="lq-tick" aria-hidden="true">
              <span class="lq-dot"></span>
            </span>
            <div class="lq-body">
              <p class="lq-q">
                <span class="lq-asker" class:you={q.asker_side === "you"}
                  >{displayFor(q)}</span
                >
                <span class="lq-text">{q.text}</span>
              </p>
              {#if canEdit}
                {@render rowActions(q)}
              {/if}
            </div>
          {/if}
        </li>
      {/each}

      {#each answeredList as q (q.id)}
        <li class="lq-item answered">
          {#if editingId === q.id}
            {@render editor(q)}
          {:else}
            <span class="lq-tick" aria-hidden="true">
              <svg
                viewBox="0 0 24 24"
                width="12"
                height="12"
                fill="none"
                stroke="currentColor"
                stroke-width="2.6"
                stroke-linecap="round"
                stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg
              >
            </span>
            <div class="lq-body">
              <p class="lq-q">
                <span class="lq-asker" class:you={q.asker_side === "you"}
                  >{displayFor(q)}</span
                >
                <span class="lq-text">{q.text}</span>
                <span class="lq-sr">answered</span>
              </p>
              {#if q.answer_text}
                <p class="lq-answer">{q.answer_text}</p>
              {/if}
              {#if canEdit}
                {@render rowActions(q)}
              {/if}
            </div>
          {/if}
        </li>
      {/each}

      {#if editingId === "new"}
        <li class="lq-item open">{@render editor(null)}</li>
      {/if}
    </ul>

    {#if editError}
      <p class="lq-err">{editError}</p>
    {/if}

    {#if canEdit && editingId === null}
      <button type="button" class="lq-add" onclick={startAdd}>
        <span class="lq-add-glyph" aria-hidden="true">+</span>
        Add a question
      </button>
    {/if}
  </section>
{/if}

<!-- Per-row actions: answered/reopen toggle, edit, delete. Quiet by default;
     they surface on row hover / keyboard focus so the list still reads as a
     list, not a toolbar. -->
{#snippet rowActions(q: LiveQuestion)}
  <div class="lq-actions">
    {#if q.status === "open"}
      <button
        type="button"
        class="lq-act"
        disabled={busyId !== null}
        onclick={() => commit("answer", q.id)}
      >
        Mark answered
      </button>
    {:else}
      <button
        type="button"
        class="lq-act"
        disabled={busyId !== null}
        onclick={() => commit("reopen", q.id)}
      >
        Reopen
      </button>
    {/if}
    <button
      type="button"
      class="lq-act"
      disabled={busyId !== null}
      onclick={() => startEdit(q)}
    >
      Edit
    </button>
    <button
      type="button"
      class="lq-act lq-act-del"
      disabled={busyId !== null}
      aria-label="Delete question"
      title="Delete question"
      onclick={() => commit("delete", q.id)}
    >
      <svg
        viewBox="0 0 24 24"
        width="12"
        height="12"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        stroke-linecap="round"
        stroke-linejoin="round"
        ><line x1="18" y1="6" x2="6" y2="18" /><line
          x1="6"
          y1="6"
          x2="18"
          y2="18"
        /></svg
      >
    </button>
  </div>
{/snippet}

<!-- The edit / add composer. `q` is null for the add case. -->
{#snippet editor(q: LiveQuestion | null)}
  <div class="lq-editor">
    <div class="lq-editor-who">
      <button
        type="button"
        class="lq-side"
        class:active={draftSide === "you"}
        aria-pressed={draftSide === "you"}
        onclick={() => {
          draftSide = "you";
          if (!draftAsker.trim() || draftAsker === "Them") draftAsker = "You";
        }}
      >
        You asked
      </button>
      <button
        type="button"
        class="lq-side"
        class:active={draftSide === "them"}
        aria-pressed={draftSide === "them"}
        onclick={() => {
          draftSide = "them";
          if (!draftAsker.trim() || draftAsker === "You") draftAsker = "Them";
        }}
      >
        They asked
      </button>
    </div>
    <input
      class="lq-input lq-input-asker"
      type="text"
      bind:value={draftAsker}
      placeholder="Who asked"
      aria-label="Who asked this question"
      onkeydown={(e) => onDraftKeydown(e, q)}
    />
    <!-- svelte-ignore a11y_autofocus -->
    <input
      class="lq-input"
      type="text"
      bind:value={draftText}
      autofocus
      placeholder="What was asked"
      aria-label="Question text"
      onkeydown={(e) => onDraftKeydown(e, q)}
    />
    <div class="lq-editor-actions">
      <button
        type="button"
        class="lq-act lq-act-save"
        disabled={busyId !== null}
        onclick={() => (q ? saveEdit(q) : saveAdd())}
      >
        {busyId !== null ? "Saving…" : "Save"}
      </button>
      <button type="button" class="lq-act" onclick={cancelEdit}>Cancel</button>
    </div>
  </div>
{/snippet}

<style>
  .lq {
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--accent);
    border-radius: var(--radius-sm);
    padding: 0.5rem 0.6rem 0.6rem;
    margin-bottom: 0.6rem;
  }
  /* Call ended → the live-accent left border recedes to a quiet record. */
  .lq.ended {
    border-left-color: var(--hairline-hi);
  }

  .lq-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .lq-label {
    font-size: 0.72rem;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .lq-count {
    margin-left: auto;
    font-family: var(--font-mono);
    font-size: 0.66rem;
    color: var(--bone-3);
    background: var(--ink-2);
    border-radius: 999px;
    padding: 0.02rem 0.45rem;
    flex-shrink: 0;
  }
  /* Everything answered → settle the count chip to the olive "done" accent. */
  .lq-count.all-answered {
    color: var(--olive);
  }

  .lq-items {
    list-style: none;
    margin: 0.45rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .lq-item {
    display: flex;
    align-items: flex-start;
    gap: 0.45rem;
    min-width: 0;
  }
  .lq-tick {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 14px;
    height: 1.35em; /* align the tick with the first question line */
    color: var(--bone-4);
    flex-shrink: 0;
  }
  .lq-item.answered .lq-tick {
    color: var(--olive);
  }
  /* Open marker — a small hollow dot in the tick column. */
  .lq-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    border: 1.5px solid currentColor;
  }

  .lq-body {
    min-width: 0;
    flex: 1;
  }
  .lq-q {
    margin: 0;
    font-size: 0.82rem;
    line-height: 1.35;
    color: var(--bone-0);
    overflow-wrap: anywhere;
  }
  /* Answered question recedes (done); the answer beneath carries the value. */
  .lq-item.answered .lq-q {
    color: var(--bone-2);
  }
  .lq-asker {
    display: inline;
    margin-right: 0.4rem;
    font-size: 0.66rem;
    font-weight: 600;
    letter-spacing: 0.02em;
    color: var(--bone-2);
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 0.02rem 0.4rem;
    white-space: nowrap;
  }
  /* "You" reads as the rep's own ask (accent-tinted); "them" stays neutral. */
  .lq-asker.you {
    color: var(--accent-hi);
    background: var(--accent-soft);
    border-color: var(--accent);
  }
  .lq-answer {
    margin: 0.25rem 0 0;
    font-size: 0.8rem;
    line-height: 1.35;
    color: var(--bone-1);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }

  /* ── Row actions ── quiet until the row is hovered / focused, so the list
     still reads as a list rather than a row of toolbars. */
  .lq-actions {
    display: flex;
    gap: 0.3rem;
    margin-top: 0.2rem;
    opacity: 0;
    transition: opacity 0.15s;
  }
  .lq-item:hover .lq-actions,
  .lq-actions:focus-within {
    opacity: 1;
  }
  @media (prefers-reduced-motion: reduce) {
    .lq-actions {
      transition: none;
    }
  }
  .lq-act {
    padding: 0.08rem 0.4rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.68rem;
    cursor: pointer;
    transition:
      border-color 0.15s,
      color 0.15s;
  }
  .lq-act:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .lq-act:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .lq-act:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .lq-act-del {
    display: inline-flex;
    align-items: center;
    padding-inline: 0.3rem;
  }
  .lq-act-save {
    color: var(--accent-hi);
    background: var(--accent-soft);
    border-color: var(--accent);
  }

  /* ── Editor / composer ── */
  .lq-editor {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    width: 100%;
    min-width: 0;
  }
  .lq-editor-who {
    display: flex;
    gap: 0.3rem;
  }
  .lq-side {
    padding: 0.12rem 0.5rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    color: var(--bone-2);
    font: inherit;
    font-size: 0.68rem;
    cursor: pointer;
    transition:
      border-color 0.15s,
      color 0.15s,
      background 0.15s;
  }
  .lq-side:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .lq-side.active {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .lq-side:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .lq-input {
    width: 100%;
    min-width: 0;
    padding: 0.35rem 0.5rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.82rem;
  }
  .lq-input-asker {
    font-size: 0.74rem;
  }
  .lq-input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-color: var(--accent);
  }
  .lq-editor-actions {
    display: flex;
    gap: 0.3rem;
  }

  /* Soft degrade — plain bone text, never a red panel (design.md). */
  .lq-err {
    margin: 0.4rem 0 0;
    font-size: 0.74rem;
    color: var(--bone-2);
  }

  .lq-add {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    margin-top: 0.45rem;
    padding: 0.12rem 0.45rem 0.12rem 0.3rem;
    border: 1px dashed var(--hairline);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--bone-3);
    font: inherit;
    font-size: 0.7rem;
    cursor: pointer;
    transition:
      border-color 0.15s,
      color 0.15s;
  }
  .lq-add:hover {
    color: var(--bone-1);
    border-color: var(--hairline-hi);
  }
  .lq-add:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .lq-add-glyph {
    color: var(--accent);
    font-size: 0.9rem;
    font-weight: 600;
    line-height: 1;
  }

  /* Visually-hidden text for screen readers (component-scoped — no app.css). */
  .lq-sr {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
  }
</style>
