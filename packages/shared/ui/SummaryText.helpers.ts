// #148 (v0.4.7) — pure-TS helpers extracted from SummaryText.svelte's
// `<script module>` block so vitest can exercise them without parsing
// a Svelte file.
//
// Shared package helper used by `SummaryText.svelte`.
//
// `SummaryText.svelte` re-exports the three symbols below from its
// module-script so existing component imports keep working with no
// call-site churn — only the underlying definition moved.

// Minimal shape this helper set needs per roster entry. Callers map
// their source-specific OrgMember row (portal api vs agent Tauri
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
export type Segment =
  | { kind: "text"; value: string }
  | { kind: "name"; inner: string };

// Split the LLM-emitted body on `<name>...</name>` markers. No nested
// tags are expected (prompt forbids them); a simple regex is
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
