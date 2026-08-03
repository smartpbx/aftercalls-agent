// Escape-first markdown renderer for the Terms / Privacy admin preview
// pane. Mirrors the identical implementation inlined in
// `site/terms/index.html` and `site/privacy/index.html` — any change
// here needs to be reflected there (the site pages are plain HTML with
// no build step so they can't import this module).
//
// Why escape-first instead of `marked` or any parser: we store legal
// prose as raw markdown in the DB. A typo in the source — or a
// deliberate paste attack — must never render as executable HTML.
// By HTML-escaping the entire input before any block/inline regex
// runs, every `<`, `>`, `"`, `&`, `'` in the source becomes harmless
// entity text. The block and inline passes then run on a string that
// literally cannot contain a live `<script>` tag or attribute.
// See learnings #97 #102.
//
// Supported subset: `# / ## / ###` headings, `- ` unordered lists
// (one level of nesting tracked via indent), paragraphs separated by
// blank lines, `**bold**`, `_italic_`, `[text](url)`. No tables, no
// code blocks, no HTML pass-through — and that's the point.

function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    })[c] ?? c,
  );
}

const SAFE_LINK_SCHEMES = new Set(["http:", "https:", "mailto:"]);
const LINK_SCHEME_RE = /^([a-z][a-z0-9+.-]*):/i;
const LINK_CONTROL_RE =
  /[\u0000-\u0020\u007f-\u009f\u00a0\u1680\u2000-\u200f\u2028-\u202f\u205f\u2060-\u206f\u3000\ufeff]/u;
const LINK_POLICY_CONTROL_RE =
  /[\u0000-\u001f\u007f-\u009f\u00a0\u1680\u2000-\u200f\u2028-\u202f\u205f\u2060-\u206f\u3000\ufeff]/u;

// Build a conservative policy probe in addition to checking the literal
// href. The renderer runs after HTML escaping, so character references are
// already inert, but decoding their protocol-significant forms here keeps
// the allowlist fail-closed if that pipeline is ever rearranged. Percent-
// encoded schemes and protocol-relative prefixes are treated the same way.
function linkPolicyProbe(url: string): string {
  let probe = url.replace(/&amp;/gi, "&");
  probe = probe
    .replace(
      /&#(?:x([0-9a-f]+)|([0-9]+));?/gi,
      (entity, hex, decimal) => {
        const value = Number.parseInt(hex ?? decimal, hex ? 16 : 10);
        return value <= 0x10ffff && !(value >= 0xd800 && value <= 0xdfff)
          ? String.fromCodePoint(value)
          : entity;
      },
    )
    .replace(/&(?:colon|#0*58|#x0*3a);?/gi, ":")
    .replace(/&(?:sol|#0*47|#x0*2f);?/gi, "/")
    .replace(/&(?:bsol|#0*92|#x0*5c);?/gi, "\\")
    .replace(/&(?:tab|#0*9|#x0*9);?/gi, "\t")
    .replace(/&(?:newline|#0*10|#x0*a);?/gi, "\n");
  try {
    return decodeURIComponent(probe);
  } catch {
    return probe;
  }
}

/**
 * Return the escaped URL only when it is safe to place in an href.
 * Relative URLs plus http, https, and mailto are the complete allowlist.
 */
export function safeLinkHref(url: string): string | null {
  if (!url || LINK_CONTROL_RE.test(url) || url.includes("\\")) return null;

  const probe = linkPolicyProbe(url);
  const compactProbe = probe.replace(LINK_POLICY_CONTROL_RE, "");
  if (
    LINK_POLICY_CONTROL_RE.test(probe) ||
    compactProbe.startsWith("//") ||
    compactProbe.includes("\\")
  ) {
    return null;
  }

  // A decoded policy probe catches encoded or entity-obfuscated schemes.
  const probeScheme = compactProbe.match(LINK_SCHEME_RE)?.[1]?.toLowerCase();
  if (probeScheme && !SAFE_LINK_SCHEMES.has(`${probeScheme}:`)) return null;

  const literalScheme = url.match(LINK_SCHEME_RE)?.[1]?.toLowerCase();
  if (!literalScheme) return url;
  const protocol = `${literalScheme}:`;
  if (!SAFE_LINK_SCHEMES.has(protocol)) return null;

  try {
    const parsed = new URL(url);
    if (
      (protocol === "http:" || protocol === "https:") &&
      (!parsed.hostname || parsed.username !== "" || parsed.password !== "")
    ) {
      return null;
    }
  } catch {
    return null;
  }

  return url;
}

// Inline: **bold**, _italic_, [text](url). Runs on already-HTML-
// escaped strings so the regex can't eat a real `<`. The URL and text
// keep that single escape layer when inserted into the generated tag.
function inline(s: string): string {
  return s
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[\s(])_([^_]+)_/g, "$1<em>$2</em>")
    .replace(
      /\[([^\]]+)\]\(([^)]+)\)/g,
      (_, t, u) => {
        const href = safeLinkHref(u);
        return href === null
          ? t
          : `<a href="${href}" rel="noopener">${t}</a>`;
      },
    );
}

/**
 * Render a markdown string as HTML using the project's escape-first
 * pipeline. Safe to drop into `{@html ...}` — see file header.
 */
export function renderMarkdown(md: string): string {
  const lines = md.replace(/\r\n?/g, "\n").split("\n");
  const out: string[] = [];
  let para: string[] = [];
  const listStack: number[] = []; // stack of open <ul> indents

  function flushPara() {
    if (para.length) {
      out.push("<p>" + inline(esc(para.join(" "))) + "</p>");
      para = [];
    }
  }
  function closeListsTo(indent: number) {
    while (
      listStack.length &&
      listStack[listStack.length - 1]! > indent
    ) {
      out.push("</ul>");
      listStack.pop();
    }
  }

  for (const raw of lines) {
    const line = raw.replace(/\s+$/, "");
    if (!line) {
      flushPara();
      closeListsTo(-1);
      continue;
    }
    const h = line.match(/^(#{1,3})\s+(.*)$/);
    if (h) {
      flushPara();
      closeListsTo(-1);
      const lvl = h[1]!.length;
      out.push(`<h${lvl}>${inline(esc(h[2]!))}</h${lvl}>`);
      continue;
    }
    const li = line.match(/^(\s*)-\s+(.*)$/);
    if (li) {
      flushPara();
      const indent = li[1]!.length;
      if (
        !listStack.length ||
        indent > listStack[listStack.length - 1]!
      ) {
        out.push("<ul>");
        listStack.push(indent);
      } else {
        closeListsTo(indent);
        if (
          !listStack.length ||
          listStack[listStack.length - 1]! !== indent
        ) {
          out.push("<ul>");
          listStack.push(indent);
        }
      }
      out.push(`<li>${inline(esc(li[2]!))}</li>`);
      continue;
    }
    para.push(line);
  }
  flushPara();
  closeListsTo(-1);
  return out.join("\n");
}
