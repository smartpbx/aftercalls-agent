<script lang="ts" module>
  // Derive initials for a display name, Unicode-safe.
  //
  // Rules (see design.md §Pattern library "Avatar (initials)"):
  // - Split name.trim() on runs of whitespace.
  // - Take the first code-point of the first two non-empty words
  //   via [...word][0] so emoji / combining marks / CJK / RTL glyphs
  //   don't get sliced mid-codepoint.
  // - Single word → one code-point.
  // - Empty / whitespace-only / literal "Unknown" → bullet glyph.
  // - toLocaleUpperCase() is important for dotless-i (tr/az) and ß.
  export function initialsFor(name: string | null | undefined): string {
    if (!name) return "•";
    const trimmed = name.trim();
    if (trimmed === "" || trimmed === "Unknown") return "•";
    const words = trimmed.split(/\s+/).filter((w) => w.length > 0);
    if (words.length === 0) return "•";
    const take = words.slice(0, 2);
    const chars = take
      .map((w) => [...w][0] ?? "")
      .filter((c) => c !== "")
      .join("");
    if (chars === "") return "•";
    return chars.toLocaleUpperCase();
  }
</script>

<script lang="ts">
  // Initials avatar — CSS-only identity disc. The .avatar class lives in
  // app.css (byte-identical across portal + agent, mirror invariant),
  // so this component stays markup-only. See issue #5.

  type Props = {
    name: string;
    color?: string;
    size?: number;
    title?: string;
  };

  let { name, color, size = 24, title }: Props = $props();

  let glyph = $derived(initialsFor(name));
  // color is plumbed through --avatar-c; omitted → the .avatar rule's
  // own fallback of var(--accent) applies (rail-foot "self" case).
  let style = $derived(
    color
      ? `--avatar-c: ${color}; --avatar-s: ${size}px`
      : `--avatar-s: ${size}px`,
  );
</script>

<span class="avatar" aria-hidden="true" {style} {title}>{glyph}</span>
