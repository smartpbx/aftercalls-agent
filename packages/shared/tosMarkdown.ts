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

// Inline: **bold**, _italic_, [text](url). Runs on already-HTML-
// escaped strings so the regex can't eat a real `<`. The URL is
// re-escaped on its way into the href attribute — the text half of
// the link was already escaped by the caller.
function inline(s: string): string {
  return s
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[\s(])_([^_]+)_/g, "$1<em>$2</em>")
    .replace(
      /\[([^\]]+)\]\(([^)]+)\)/g,
      (_, t, u) => `<a href="${esc(u)}" rel="noopener">${t}</a>`,
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
