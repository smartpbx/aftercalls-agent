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

  import Avatar from "./Avatar.svelte";

  type Props = {
    text: string | null | undefined;
    users?: SummaryMember[];
    colorFor?: (name: string) => string;
  };

  let { text, users = [], colorFor }: Props = $props();

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
</script>

<span class="summary-text">
  {#each segments as seg, i (i)}
    {#if seg.kind === "text"}
      <span>{seg.value}</span>
    {:else}
      {@const matches = matchFor(seg.inner)}
      {#if matches.length === 1}
        {@const m = matches[0]}
        {@const c = colorFor ? colorFor(m.display_name) : "var(--accent)"}
        <span
          class="name-chip name-linked"
          style="--name-c: {c}"
          title="{m.display_name} — linked teammate"
        >
          <Avatar name={m.display_name} color={c} size={18} />
          <span class="name-chip-label">{seg.inner}</span>
        </span>
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
