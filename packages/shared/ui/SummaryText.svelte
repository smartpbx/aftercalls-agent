<script lang="ts" module>
  // #148 (v0.4.7) — pure-TS helpers now live in
  // `./SummaryText.helpers.ts` so vitest can exercise them without
  // parsing a Svelte file. This module script re-exports them so
  // existing `@aftercalls/shared/ui/SummaryText.svelte` call-sites
  // keep working with no churn. Behaviour is unchanged.
  export {
    tokenize,
    firstLastInitial,
    rewriteChipOccurrence,
    type SummaryMember,
    type Segment,
  } from "./SummaryText.helpers";

  // Detail payload fired by the interactive chip surface (#140).
  // Parent mounts `<ChipMenu>` anchored to `anchor` and — on menu
  // action — uses `occurrenceIndex` with `rewriteChipOccurrence`
  // to target the exact `<name>` token that was clicked.
  //
  // #150 · v0.4.6 — `isExternal` is `true` when the clicked `<name>`
  // span didn't resolve against the roster (renders as an italic
  // external mention). Parent routes these through ChipMenu in
  // external mode ("Link to teammate" / "Leave as text") so a user
  // can upgrade the mention to a proper teammate chip.
  export type ChipActionDetail = {
    inner: string;
    occurrenceIndex: number;
    anchor: HTMLElement;
    isExternal: boolean;
  };
</script>

<script lang="ts">
  // Shared package component consumed by both portal and agent.
  //
  // Styles are component-scoped deliberately — none of these classes
  // live in app.css. The `diff portal/src/app.css agent/src/app.css`
  // invariant stays intact.
  //
  // Renders a summary / action-item body produced by the backend LLM
  // prompt (#102). The prompt wraps every named reference in
  // `<name>First L.</name>` markers; this component tokenizes on
  // those tags and promotes each one to an avatar-chip resolved
  // against the loaded member roster. Three match branches:
  //
  //   1. Unique match → coloured name chip (text-only; avatars live
  //      in the Participants + Mentioned sub-sections, not inline in
  //      prose — dropped in #167 to kill the baseline-alignment gap).
  //   2. Ambiguous match (≥2 members synthesize to the same
  //      `First L.`) → neutral chip, title enumerates.
  //   3. No match (external person) → italic span, per-name colour.
  //
  // XSS discipline: `text` can contain adversarial content (pasted
  // quotes, LLM hallucination of `<script>`). The tokenizer extracts
  // ONLY `<name>...</name>` spans; everything else is rendered via
  // plain Svelte `{segment.value}` interpolation. `{@html}` is not
  // used anywhere in this component. A literal `<script>` in the
  // summary body renders as visible text, never as a DOM node.
  //
  // Legacy summaries (stored before the marker prompt shipped) have
  // zero `<name>` tags; the tokenizer returns one text segment and
  // the output is plain text — no errors, no warnings.
  //
  // Interactive chip (#140 / v0.4.5). When `onchipaction` is
  // provided, unique-match chips render as `<button>` elements with
  // `aria-label="Edit mention: {name}"` and emit
  // `onchipaction({ inner, occurrenceIndex, anchor })` on activate.
  // The occurrence index counts only `<name>` segments in document
  // order so the parent can target the exact token via
  // `rewriteChipOccurrence(...)`. Without the prop chips render as
  // non-interactive spans (read-only call-sites keep their v0.4.0
  // behaviour). Ambiguous + external branches stay non-interactive
  // in either mode.

  import {
    tokenize,
    firstLastInitial,
    type SummaryMember,
  } from "./SummaryText.helpers";

  type Props = {
    text: string | null | undefined;
    users?: SummaryMember[];
    colorFor?: (name: string) => string;
    // #140 · v0.4.5 — chip-action callback. Set by the parent on
    // surfaces where clicking a chip should open the edit-menu.
    // Undefined keeps chips non-interactive (read-only surfaces).
    onchipaction?: (detail: ChipActionDetail) => void;
    // #140 · v0.4.5 — occurrence index of the chip whose popover is
    // currently open. When set, the matching chip renders with the
    // `.name-chip-active` outline cue so multi-occurrence edits
    // stay unambiguous. `null` / undefined → no active chip.
    activeOccurrenceIndex?: number | null;
  };

  let {
    text,
    users = [],
    colorFor,
    onchipaction,
    activeOccurrenceIndex = null,
  }: Props = $props();

  let segments = $derived(tokenize(text ?? ""));

  // Build a case-insensitive `First L.` → [member, ...] lookup once
  // per roster change so each name-segment doesn't re-scan the list.
  // Multiple matches mean the ambiguous branch fires at render time.
  let rosterByKey = $derived.by(() => {
    const map = new Map<string, SummaryMember[]>();
    for (const m of users) {
      const key = firstLastInitial(m).toLocaleLowerCase();
      if (!key) continue;
      const existing = map.get(key);
      if (existing) existing.push(m);
      else map.set(key, [m]);
    }
    return map;
  });

  function matchFor(inner: string): SummaryMember[] {
    const key = inner.trim().toLocaleLowerCase();
    if (!key) return [];
    return rosterByKey.get(key) ?? [];
  }

  function ambiguousTitle(inner: string, matches: SummaryMember[]): string {
    const names = matches.map((m) => m.display_name || firstLastInitial(m));
    return `${inner.trim()} — multiple matches (${names.join(", ")})`;
  }

  // Map each segment index → its occurrence index (count of
  // `<name>` segments before and including this one, minus one).
  // Text segments carry `-1`. Derived so it reruns whenever
  // `segments` updates.
  let occurrenceBySegmentIdx = $derived.by(() => {
    const out: number[] = [];
    let n = 0;
    for (const seg of segments) {
      if (seg.kind === "name") {
        out.push(n);
        n += 1;
      } else {
        out.push(-1);
      }
    }
    return out;
  });

  function onChipClick(
    e: MouseEvent,
    inner: string,
    occurrenceIndex: number,
    isExternal: boolean,
  ) {
    if (!onchipaction) return;
    e.preventDefault();
    e.stopPropagation();
    const anchor = e.currentTarget as HTMLElement;
    onchipaction({ inner, occurrenceIndex, anchor, isExternal });
  }
</script>

<span class="summary-text">
  {#each segments as seg, i (i)}
    {#if seg.kind === "text"}
      <span>{seg.value}</span>
    {:else}
      {@const matches = matchFor(seg.inner)}
      {@const occ = occurrenceBySegmentIdx[i]}
      {#if matches.length === 1}
        {@const m = matches[0]}
        {@const c = colorFor ? colorFor(m.display_name) : "var(--accent)"}
        {#if onchipaction}
          <button
            type="button"
            class="name-chip name-linked name-chip-btn"
            class:name-chip-active={activeOccurrenceIndex === occ}
            style="--name-c: {c}"
            title="{m.display_name} — click to edit"
            aria-label="Edit mention: {m.display_name}"
            aria-haspopup="menu"
            aria-expanded={activeOccurrenceIndex === occ ? "true" : "false"}
            onclick={(e) => onChipClick(e, seg.inner, occ, false)}
          >
            <span class="name-chip-label">{seg.inner}</span>
          </button>
        {:else}
          <span
            class="name-chip name-linked"
            style="--name-c: {c}"
            title="{m.display_name} — linked teammate"
          >
            <span class="name-chip-label">{seg.inner}</span>
          </span>
        {/if}
      {:else if matches.length > 1}
        <span
          class="name-chip name-ambiguous"
          title={ambiguousTitle(seg.inner, matches)}
        >
          <span class="name-chip-label">{seg.inner}</span>
        </span>
      {:else if onchipaction}
        <!-- #150 · v0.4.6 — external mention promoted to a button so
             the user can Link-to-teammate a roster-miss. The inner
             `<name>` span is still a tokenizer hit; it just didn't
             resolve via `First L.` lookup. ChipMenu opens in external
             mode (see isExternal below). -->
        <button
          type="button"
          class="name-external name-external-btn"
          class:name-chip-active={activeOccurrenceIndex === occ}
          style="--name-c: {colorFor ? colorFor(seg.inner) : 'var(--bone-1)'}"
          title="{seg.inner} — click to link to a teammate"
          aria-label="Link mention: {seg.inner}"
          aria-haspopup="menu"
          aria-expanded={activeOccurrenceIndex === occ ? "true" : "false"}
          onclick={(e) => onChipClick(e, seg.inner, occ, true)}
        >{seg.inner}</button>
      {:else}
        <span
          class="name-external"
          style="--name-c: {colorFor ? colorFor(seg.inner) : 'var(--bone-1)'}"
          >{seg.inner}</span
        >
      {/if}
    {/if}
  {/each}
</span>

<style>
  /* Container preserves newlines from the LLM body — summaries span
     2-3 paragraphs and action-items can include line breaks. The
     parent still controls block vs inline via CSS elsewhere; this
     span's whitespace handling keeps the text readable without
     forcing the parent to swap to a <pre>. */
  .summary-text {
    white-space: pre-wrap;
  }

  /* Shared chip hook. Three branches share the inline-flex shape so
     surrounding prose breaks cleanly around them. `--name-c` is set
     by the linked + external branches for per-name colouring; the
     ambiguous branch reads from the token palette directly. */
  .name-chip {
    display: inline-flex;
    align-items: center;
    padding: 0 0.25rem;
    border-radius: 999px;
    line-height: 1.4;
    vertical-align: baseline;
    font-weight: 500;
  }

  .name-linked {
    color: var(--name-c, var(--accent));
    cursor: default;
    transition: background-color 120ms ease;
  }
  .name-linked:hover {
    background: color-mix(in srgb, var(--name-c, var(--accent)) 14%, transparent);
  }

  /* #140 · v0.4.5 — chip-as-button surface. Reset the native button
     chrome so the inline paint stays identical to the span variant,
     but keep the focusable / interactive affordances. Deliberately
     omits `color: inherit` so `.name-linked`'s `var(--name-c)` wins
     the cascade (#167.1 — without this, the button variant lost the
     teammate colour while the span variant kept it). */
  .name-chip-btn {
    border: none;
    background: transparent;
    font: inherit;
    padding: 0 0.25rem;
    cursor: pointer;
    text-align: inherit;
  }
  .name-chip-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* Active-occurrence cue — outlined teammate colour stays, so the
     accent-hi ring reads as "this one" without overriding the
     chip's existing paint. Instant; the popover animation covers
     the transition. */
  .name-chip-active {
    outline: 1.5px solid var(--accent-hi);
    outline-offset: 2px;
  }

  .name-chip-label {
    font-size: inherit;
  }

  /* Ambiguous: no avatar, neutral color, tooltip-only disambiguation.
     Readable as "a teammate, can't say which". */
  .name-ambiguous {
    color: var(--bone-2);
    background: color-mix(in srgb, var(--bone-3) 20%, transparent);
  }

  /* External / unmatched — signals "named person, not a teammate"
     without suggesting clickability. No chip background, italic for
     the "proper noun" feel. Per-name colour (#167.2) so the summary
     body tracks the speaker-colour convention used elsewhere; italic
     stays as the "external" cue. Falls back to bone-1 when the parent
     doesn't wire `colorFor`. */
  .name-external {
    color: var(--name-c, var(--bone-1));
    font-style: italic;
  }

  /* #150 · v0.4.6 — external mention rendered as a button when the
     parent wires `onchipaction`. Reset the native chrome so the
     inline paint stays identical to the span variant (italic, bone-1
     text); add padding + hover + focus-visible so the affordance is
     discoverable without pulling visual weight away from the prose. */
  .name-external-btn {
    appearance: none;
    background: transparent;
    border: none;
    font: inherit;
    font-style: italic;
    padding: 0 0.15rem;
    border-radius: 4px;
    cursor: pointer;
    text-align: inherit;
    transition: background-color 120ms ease;
  }
  .name-external-btn:hover {
    background: color-mix(in srgb, var(--name-c, var(--accent)) 14%, transparent);
  }
  .name-external-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
