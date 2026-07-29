<!--
  LiveQuestions — Phase 4 (live↔after-call continuity). A very simple "open
  questions / who asked / answered + what the answer was" strip, pinned at the
  TOP of the on-demand transcript drawer in `CoPilotPanel`. Both sides count: a
  question the rep OR the counterpart asks that should get answered, each
  attributed to who asked it.

  Open questions render FIRST (a hollow dot + an asker chip + the question);
  answered ones follow, checked off (an `--olive` check) with the captured
  answer beneath. Answers are sticky on the backend (a question never
  un-answers once answered), so a later snapshot only ever gains answers.

  Each frame is a FULL snapshot; the component renders it wholesale (no
  reconcile — the store swaps the snapshot). On call end it simply freezes as
  the final list.

  HARD RULES honored:
   - All chrome is component-scoped `.lq-*` — NO app.css touch (HR#1).
   - Copy is vendor-opaque — no STT/LLM/infra provider name anywhere (HR#2).
   - Question + answer text are TRANSCRIPT-DERIVED — rendered as plain `{...}`
     interpolation ONLY, never `{@html}`.
   - Reuses the `LiveChecklist` visual idiom (`--olive` check = answered, hollow
     dot = open).
-->
<script lang="ts">
  import type { QuestionsSnapshot } from "@aftercalls/shared/types";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    questions = null,
    status = "idle",
  }: {
    /** Latest FULL snapshot; null pre-first-frame (and cleared on new session). */
    questions?: QuestionsSnapshot | null;
    /** Drives the ended freeze (kept for parity with LiveChecklist; the list
     *  simply stops updating). */
    status?: LiveStatus;
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
</script>

{#if questions && all.length > 0}
  <section class="lq" class:ended={isEnded} aria-label="Questions">
    <div class="lq-head">
      <span class="lq-label">Questions</span>
      {#if openCount > 0}
        <span class="lq-count">{openCount} open</span>
      {:else}
        <span class="lq-count all-answered">All answered</span>
      {/if}
    </div>

    <ul class="lq-items">
      {#each openList as q (q.id)}
        <li class="lq-item open">
          <span class="lq-tick" aria-hidden="true">
            <span class="lq-dot"></span>
          </span>
          <div class="lq-body">
            <p class="lq-q">
              <span class="lq-asker" class:you={q.asker_side === "you"}
                >{q.asker_display}</span
              >
              <span class="lq-text">{q.text}</span>
            </p>
          </div>
        </li>
      {/each}

      {#each answeredList as q (q.id)}
        <li class="lq-item answered">
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
                >{q.asker_display}</span
              >
              <span class="lq-text">{q.text}</span>
              <span class="lq-sr">answered</span>
            </p>
            {#if q.answer_text}
              <p class="lq-answer">{q.answer_text}</p>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  </section>
{/if}

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
