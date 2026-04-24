<script lang="ts" module>
  // Minimal shape this component needs per roster entry. Parent maps
  // its source-specific OrgMember row (portal api vs agent Tauri
  // invoke) onto this triple before passing. Matches the shape of
  // OrgMemberLite in SpeakerRenamePicker — callers can pass the same
  // array to both components.
  export type SummaryMember = {
    id: string;
    first_name: string;
    last_name: string;
    display_name: string;
  };

  // Tokenizer output. Either a plain text span (rendered via Svelte
  // `{segment.value}` — never `{@html}`) or a name-span the parent
  // renders as a resolved chip.
  type Segment =
    | { kind: "text"; value: string }
    | { kind: "name"; inner: string };

  // Split the LLM-emitted body on `<name>...</name>` markers. No
  // nested tags are expected (prompt forbids them); a simple regex is
  // sufficient. Any unmatched `<` / `>` stays in the text segment and
  // renders as literal characters through Svelte interpolation.
  export function tokenize(text: string): Segment[] {
    if (!text) return [];
    const out: Segment[] = [];
    const re = /<name>([^<]+)<\/name>/g;
    let lastIdx = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(text)) !== null) {
      if (m.index > lastIdx) {
        out.push({ kind: "text", value: text.slice(lastIdx, m.index) });
      }
      out.push({ kind: "name", inner: m[1] });
      lastIdx = m.index + m[0].length;
    }
    if (lastIdx < text.length) {
      out.push({ kind: "text", value: text.slice(lastIdx) });
    }
    return out;
  }

  // Synthesize the `First L.` form the LLM is instructed to emit for
  // each member. Trims input; falls back to the bare first name when
  // there's no last initial (mononyms). Used to match a `<name>` span
  // back to a roster row.
  export function firstLastInitial(m: SummaryMember): string {
    const first = (m.first_name ?? "").trim();
    const last = (m.last_name ?? "").trim();
    if (!first) return "";
    const initial = [...last][0];
    if (!initial) return first;
    return `${first} ${initial.toLocaleUpperCase()}.`;
  }

  // #140 · v0.4.5 — indexed chip-occurrence rewrite helper.
  //
  // Rewrite the `occurrenceIndex`-th `<name>...</name>` match in
  // `source`. `action === "rename"` swaps the inner text for
  // `replacement` (keeps the wrapper tags); `action === "unlink"`
  // strips the wrapper and leaves the bare inner text in place.
  //
  // Every other `<name>...</name>` in `source` — including ones
  // with the same inner string — stays untouched. The regex here
  // mirrors the `tokenize` pattern above so the index counter
  // stays in lockstep between render-order and rewrite-order.
  //
  // Caller is responsible for canonicalising `replacement` to the
  // `First L.` form via `firstLastInitial(member)` so the rewritten
  // token matches the tokenizer / resolver on the next render.
  export function rewriteChipOccurrence(
    source: string,
    occurrenceIndex: number,
    action: "rename" | "unlink",
    replacement?: string,
  ): string {
    if (!source) return source;
    const re = /<name>([^<]+)<\/name>/g;
    let idx = 0;
    let result = "";
    let lastEnd = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(source)) !== null) {
      result += source.slice(lastEnd, m.index);
      if (idx === occurrenceIndex) {
        if (action === "rename" && replacement !== undefined) {
          result += `<name>${replacement}</name>`;
        } else {
          // Unlink (or rename with no replacement supplied): strip
          // the wrapper, keep the inner text verbatim.
          result += m[1];
        }
      } else {
        result += m[0];
      }
      lastEnd = m.index + m[0].length;
      idx += 1;
    }
    result += source.slice(lastEnd);
    return result;
  }

  // Detail payload fired by the interactive chip surface (#140).
  // Parent mounts `<ChipMenu>` anchored to `anchor` and — on menu
  // action — uses `occurrenceIndex` with `rewriteChipOccurrence`
  // to target the exact `<name>` token that was clicked.
  export type ChipActionDetail = {
    inner: string;
    occurrenceIndex: number;
    anchor: HTMLElement;
  };
</script>

<script lang="ts">
  // Mirror-pair component — byte-identical between
  // portal/src/lib/SummaryText.svelte and
  // agent/src/lib/SummaryText.svelte (same discipline as Avatar.svelte
  // + SpeakerRenamePicker.svelte; see learnings.md #5, #80/#82).
  // Reviewer diffs the two on every touch.
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
  //   1. Unique match → Avatar + colored name chip.
  //   2. Ambiguous match (≥2 members synthesize to the same
  //      `First L.`) → neutral chip, no avatar, title enumerates.
  //   3. No match (external person) → italic non-chip span.
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

  import Avatar from "./Avatar.svelte";

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
  ) {
    if (!onchipaction) return;
    e.preventDefault();
    e.stopPropagation();
    const anchor = e.currentTarget as HTMLElement;
    onchipaction({ inner, occurrenceIndex, anchor });
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
            onclick={(e) => onChipClick(e, seg.inner, occ)}
          >
            <Avatar name={m.display_name} color={c} size={18} />
            <span class="name-chip-label">{seg.inner}</span>
          </button>
        {:else}
          <span
            class="name-chip name-linked"
            style="--name-c: {c}"
            title="{m.display_name} — linked teammate"
          >
            <Avatar name={m.display_name} color={c} size={18} />
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
      {:else}
        <span class="name-external">{seg.inner}</span>
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
     surrounding prose breaks cleanly around them. `--name-c` is only
     set by the linked branch; the ambiguous + external branches read
     from the token palette directly. */
  .name-chip {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    padding: 0 0.25rem 0 0.15rem;
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
     but keep the focusable / interactive affordances. */
  .name-chip-btn {
    border: none;
    background: transparent;
    color: inherit;
    font: inherit;
    padding: 0 0.25rem 0 0.15rem;
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
     the "proper noun" feel. */
  .name-external {
    color: var(--bone-1);
    font-style: italic;
  }
</style>
