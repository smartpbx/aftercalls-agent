<!--
  LiveChecklist — #659 (P3) auto-checking agenda / checklist adherence.

  A passive, zero-effort "what have I covered, what's left" strip, pinned at the
  TOP of the Coaching lane (above P2's fast cues + the reflective coaching
  cards). Items auto-tick as the live call covers them; coverage is append-only
  on the backend (an item never un-ticks once covered), so a later snapshot only
  ever gains ticks. A header shows the template label + progress ("2/6") + a thin
  meter; the body lists each item with a pending/covered tick and subtly
  emphasizes the next uncovered item ("what's left"). Collapsible to the header
  strip. On call end it freezes as a "covered X/N" summary.

  Each frame is a FULL snapshot; the component renders it wholesale (no reconcile
  needed — the item set is fixed per template and the store swaps the snapshot).

  HARD RULES honored:
   - All chrome is component-scoped `.checklist-*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque — no STT/LLM/infra provider name anywhere (HR#2).
   - Item labels are BUILT-IN template text (never transcript-derived), rendered
     as plain `{...}` interpolation ONLY, never `{@html}`.
   - Reuses the `.coach-*` idioms (glyph, `--olive` = complete/saved).
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { liveSession } from "$lib/stores/liveSession.svelte";
  import type {
    ChecklistSnapshot,
    ChecklistItemState,
  } from "@aftercalls/shared/types";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    checklist = null,
    status = "idle",
  }: {
    /** Latest FULL snapshot; null pre-first-frame (and cleared on new session). */
    checklist?: ChecklistSnapshot | null;
    /** Drives the ended freeze (final "covered X/N" summary). */
    status?: LiveStatus;
  } = $props();

  // ── Motion gate (Svelte can't disable meter transitions via CSS alone) ────
  let reduceMotion = $state(false);
  onMount(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = mq.matches;
    const handler = (e: MediaQueryListEvent) => (reduceMotion = e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  });

  // Collapsible to the header strip; default EXPANDED (seeing what's left IS
  // the value). User toggle persists across the 20s snapshot swaps (local state
  // is never reset by a new frame — only a new session unmounts the panel).
  let collapsed = $state(false);

  let isEnded = $derived(status === "ended");

  // #659 (P5c) — a `confirm_required` (compliance) template: model coverage
  // arrives as "likely" and needs a human tap to become "covered". Absent /
  // false (discovery) → items auto-tick as in P3.
  let confirmRequired = $derived(!!checklist?.confirm_required);

  // #659 (P5c) — EFFECTIVE item states: overlay the user's confirmations from
  // the store (a "likely" item the user tapped becomes "covered"). For an
  // auto-tick template the confirmed set is empty, so effective === backend
  // state and the P3 discovery behaviour is unchanged. All progress below is
  // derived from these effective states, so the compliance meter reflects
  // CONFIRMED items (the human-verified ones), not the model's suggestions.
  type EffItem = { id: string; label: string; state: ChecklistItemState };
  let effItems = $derived<EffItem[]>(
    (checklist?.items ?? []).map((it) => ({
      id: it.id,
      label: it.label,
      state: liveSession.checklistConfirmed.has(it.id) ? "covered" : it.state,
    })),
  );

  let coveredCount = $derived(
    effItems.filter((it) => it.state === "covered").length,
  );
  let totalCount = $derived(checklist?.total_count ?? effItems.length);

  // Overall completeness, derived from the EFFECTIVE counts (mirrors the
  // backend's `completeness_label`): not-started (0) / in-progress (1..n-1) /
  // complete (n).
  type Completeness = "not_started" | "in_progress" | "complete";
  let completeness = $derived<Completeness>(
    coveredCount === 0
      ? "not_started"
      : totalCount > 0 && coveredCount >= totalCount
        ? "complete"
        : "in_progress",
  );

  let pct = $derived(
    totalCount > 0 ? Math.round((coveredCount / totalCount) * 100) : 0,
  );

  // Index of the first still-PENDING item — the "next up" the rep still owes.
  // "likely" items are NOT "next" (they carry the confirm affordance instead).
  // -1 when nothing is pending (or on the ended freeze, where nothing is next).
  let nextPendingIdx = $derived(
    !checklist || isEnded
      ? -1
      : effItems.findIndex((it) => it.state === "pending"),
  );

  // Tap-to-confirm a compliance item (or undo). Frozen once the call ends.
  function toggleConfirm(id: string) {
    if (isEnded) return;
    liveSession.toggleChecklistConfirm(id);
  }
</script>

<!-- Tick marker for one item, by EFFECTIVE state. Shared by the static rows and
     the compliance confirm button so the glyph switch lives in one place:
     covered → olive check, likely → accent dashed ring (suggested, awaiting a
     human confirm), pending → hollow dot. -->
{#snippet tick(state: ChecklistItemState)}
  <span class="checklist-tick" class:instant={reduceMotion} aria-hidden="true">
    {#if state === "covered"}
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
    {:else if state === "likely"}
      <span class="checklist-likely-ring"></span>
    {:else}
      <span class="checklist-dot"></span>
    {/if}
  </span>
{/snippet}

{#if checklist}
  <section class="checklist" class:complete={completeness === "complete"}>
    <button
      type="button"
      class="checklist-head"
      aria-expanded={!collapsed}
      aria-controls="checklist-body"
      onclick={() => (collapsed = !collapsed)}
    >
      <span class="checklist-label">{checklist.template_label}</span>
      <svg
        class="checklist-chevron"
        class:open={!collapsed}
        viewBox="0 0 24 24"
        width="12"
        height="12"
        fill="none"
        stroke="currentColor"
        stroke-width="2.2"
        stroke-linecap="round"
        stroke-linejoin="round"
        aria-hidden="true"><polyline points="9 6 15 12 9 18" /></svg
      >
    </button>

    <!-- Thin progress meter — fills olive when complete, accent while live. -->
    <div
      class="checklist-meter"
      role="progressbar"
      aria-valuemin="0"
      aria-valuemax={totalCount}
      aria-valuenow={coveredCount}
      aria-label={`${coveredCount} of ${totalCount} covered`}
    >
      <span
        class="checklist-meter-fill"
        class:complete={completeness === "complete"}
        class:instant={reduceMotion}
        style={`width:${pct}%`}
      ></span>
    </div>

    {#if !collapsed}
      <ul id="checklist-body" class="checklist-items">
        {#each effItems as item, i (item.id)}
          {@const interactive =
            confirmRequired && item.state !== "pending" && !isEnded}
          <li
            class="checklist-item"
            class:covered={item.state === "covered"}
            class:likely={item.state === "likely"}
            class:next={i === nextPendingIdx}
          >
            {#if interactive}
              <!-- #659 (P5c) — compliance item: the model's coverage is a
                   SUGGESTION ("likely"); a deliberate human tap confirms it as
                   covered (aria-pressed toggle; tap again to undo). A false
                   "disclosure given" tick can never happen on the model alone. -->
              <button
                type="button"
                class="checklist-confirm"
                class:on={item.state === "covered"}
                aria-pressed={item.state === "covered"}
                aria-label={item.state === "covered"
                  ? `Confirmed covered: ${item.label}. Activate to undo.`
                  : `Likely covered: ${item.label}. Activate to confirm as covered.`}
                onclick={() => toggleConfirm(item.id)}
              >
                {@render tick(item.state)}
                <span class="checklist-item-label">{item.label}</span>
                <span class="checklist-confirm-tag" aria-hidden="true">
                  {item.state === "covered" ? "CONFIRMED" : "CONFIRM"}
                </span>
              </button>
            {:else}
              {@render tick(item.state)}
              <span class="checklist-item-label">{item.label}</span>
              {#if item.state === "covered"}
                <span class="checklist-sr">covered</span>
              {:else if item.state === "likely"}
                <span class="checklist-sr">likely covered, not confirmed</span>
              {:else if i === nextPendingIdx}
                <span class="checklist-next-tag" aria-hidden="true">NEXT</span>
                <span class="checklist-sr">up next</span>
              {/if}
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>
{/if}

<style>
  .checklist {
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--accent);
    border-radius: var(--radius-sm);
    padding: 0.5rem 0.6rem 0.6rem;
    margin-bottom: 0.7rem;
  }
  /* Complete → the whole strip settles to the olive "done/saved" accent. */
  .checklist.complete {
    border-left-color: var(--olive);
  }

  .checklist-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    width: 100%;
    padding: 0;
    border: none;
    background: transparent;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .checklist-head:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: var(--radius-sm);
  }
  .checklist-label {
    font-size: 0.72rem;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--bone-2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .checklist-chevron {
    margin-left: auto;
    color: var(--bone-3);
    flex-shrink: 0;
    transition: transform 150ms ease, color 150ms ease;
  }
  .checklist-chevron.open {
    transform: rotate(90deg);
  }
  .checklist-head:hover .checklist-chevron {
    color: var(--bone-1);
  }
  @media (prefers-reduced-motion: reduce) {
    .checklist-chevron {
      transition: none;
    }
  }

  /* ── Progress meter ─────────────────────────────────────────────────── */
  .checklist-meter {
    position: relative;
    height: 3px;
    margin: 0.5rem 0 0.1rem;
    border-radius: 999px;
    background: var(--hairline);
    overflow: hidden;
  }
  .checklist-meter-fill {
    display: block;
    height: 100%;
    border-radius: 999px;
    background: var(--accent);
    transition: width 260ms ease;
  }
  .checklist-meter-fill.complete {
    background: var(--olive);
  }
  .checklist-meter-fill.instant {
    transition: none;
  }

  /* ── Item list ──────────────────────────────────────────────────────── */
  .checklist-items {
    list-style: none;
    margin: 0.45rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.05rem;
  }
  .checklist-item {
    display: flex;
    align-items: flex-start;
    gap: 0.45rem;
    padding: 0.18rem 0.1rem;
    min-width: 0;
  }
  .checklist-tick {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 14px;
    height: 1.35em; /* align the tick with the first label line */
    color: var(--bone-4);
    flex-shrink: 0;
    transition: color 150ms ease;
  }
  .checklist-tick.instant {
    transition: none;
  }
  .checklist-item.covered .checklist-tick {
    color: var(--olive);
  }
  /* Pending marker — a small hollow dot in the tick column. */
  .checklist-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    border: 1.5px solid currentColor;
  }
  /* #659 (P5c) — "likely" marker: an accent dashed ring, reading as
     "suggested, awaiting your confirm" — between the muted pending dot and the
     solid olive covered check. */
  .checklist-item.likely .checklist-tick {
    color: var(--accent);
  }
  .checklist-likely-ring {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    border: 1.5px dashed currentColor;
  }
  .checklist-item-label {
    font-size: 0.82rem;
    line-height: 1.35;
    color: var(--bone-1);
    overflow-wrap: anywhere;
    min-width: 0;
  }
  /* Covered → quiet (done, receded). */
  .checklist-item.covered .checklist-item-label {
    color: var(--bone-3);
  }
  /* Next uncovered → emphasized (the "what's left" focus). */
  .checklist-item.next .checklist-item-label {
    color: var(--bone-0);
    font-weight: 500;
  }
  /* #659 (P5c) — a likely (unconfirmed) compliance item draws attention: it is
     the thing awaiting the rep's confirm. */
  .checklist-item.likely .checklist-item-label {
    color: var(--bone-0);
  }
  .checklist-next-tag {
    margin-left: auto;
    align-self: center;
    font-size: 0.58rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--accent);
    border: 1px solid var(--hairline-hi);
    border-radius: 3px;
    padding: 0.02rem 0.26rem;
    line-height: 1.4;
    flex-shrink: 0;
  }

  /* ── #659 (P5c) — compliance confirm control ──────────────────────────
     The whole row is a deliberate tap target: a "likely" item shows CONFIRM
     (accent, inviting the action); a confirmed item shows CONFIRMED (olive,
     with an undo affordance via the pressed toggle). Component-scoped — no
     app.css touch. */
  .checklist-confirm {
    display: flex;
    align-items: flex-start;
    gap: 0.45rem;
    width: 100%;
    padding: 0.05rem 0.1rem;
    border: none;
    background: transparent;
    font: inherit;
    text-align: left;
    cursor: pointer;
    border-radius: var(--radius-sm);
  }
  .checklist-confirm:hover .checklist-item-label {
    color: var(--bone-0);
  }
  .checklist-confirm:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  /* CONFIRM / CONFIRMED pill — the action's state cue. */
  .checklist-confirm-tag {
    margin-left: auto;
    align-self: center;
    font-size: 0.58rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--accent);
    border: 1px solid var(--accent);
    border-radius: 3px;
    padding: 0.02rem 0.3rem;
    line-height: 1.4;
    flex-shrink: 0;
    white-space: nowrap;
  }
  /* Confirmed → the pill settles to the olive "done/saved" accent. */
  .checklist-confirm.on .checklist-confirm-tag {
    color: var(--olive);
    border-color: var(--olive);
  }

  /* Visually-hidden text for screen readers (component-scoped — no app.css). */
  .checklist-sr {
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
