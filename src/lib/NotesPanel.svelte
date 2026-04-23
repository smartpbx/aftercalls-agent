<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { EditorSelection, EditorState } from '@codemirror/state';
  import {
    EditorView,
    keymap,
    placeholder as cmPlaceholder,
  } from '@codemirror/view';
  import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
  import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
  import { markdown } from '@codemirror/lang-markdown';
  import { tags as t } from '@lezer/highlight';

  // Manual notes editor for the record screen (#73). Same design as
  // the portal NotesEditor: markdown source stays visible, syntax
  // highlighting gives visual weight, a toolbar covers common
  // formatting gestures, Cmd/Ctrl+B/I/E/K shortcuts cover power users.
  //
  // Notes are private to the user and do NOT feed the summarizer.

  type Props = {
    initialNotes?: string;
    onChange: (notes: string) => void;
  };
  let { initialNotes = '', onChange }: Props = $props();

  let host: HTMLDivElement | undefined = $state();
  let view: EditorView | null = null;

  function wrapSelection(v: EditorView, before: string, after = before) {
    const spec = v.state.changeByRange((range) => {
      const doc = v.state.doc;
      if (range.empty) {
        return {
          changes: { from: range.from, insert: before + after },
          range: EditorSelection.cursor(range.from + before.length),
        };
      }
      const head = doc.sliceString(
        Math.max(0, range.from - before.length),
        range.from,
      );
      const tail = doc.sliceString(range.to, range.to + after.length);
      if (head === before && tail === after) {
        return {
          changes: [
            { from: range.from - before.length, to: range.from, insert: '' },
            { from: range.to, to: range.to + after.length, insert: '' },
          ],
          range: EditorSelection.range(
            range.from - before.length,
            range.to - before.length,
          ),
        };
      }
      return {
        changes: [
          { from: range.from, insert: before },
          { from: range.to, insert: after },
        ],
        range: EditorSelection.range(
          range.from + before.length,
          range.to + before.length,
        ),
      };
    });
    v.dispatch(spec);
    v.focus();
  }

  function togglePrefix(v: EditorView, prefix: string) {
    const { state } = v;
    const ranges = state.selection.ranges;
    const touchedLines = new Set<number>();
    for (const r of ranges) {
      const startLine = state.doc.lineAt(r.from).number;
      const endLine = state.doc.lineAt(r.to).number;
      for (let i = startLine; i <= endLine; i++) touchedLines.add(i);
    }
    const lines = [...touchedLines]
      .sort((a, b) => a - b)
      .map((n) => state.doc.line(n));
    const allHavePrefix = lines.every((l) => l.text.startsWith(prefix));
    const changes = lines.map((l) =>
      allHavePrefix
        ? { from: l.from, to: l.from + prefix.length, insert: '' }
        : { from: l.from, insert: prefix },
    );
    v.dispatch({ changes });
    v.focus();
  }

  function doBold() { if (view) wrapSelection(view, '**'); }
  function doItalic() { if (view) wrapSelection(view, '*'); }
  function doCode() { if (view) wrapSelection(view, '`'); }
  function doH1() { if (view) togglePrefix(view, '# '); }
  function doH2() { if (view) togglePrefix(view, '## '); }
  function doBullet() { if (view) togglePrefix(view, '- '); }
  function doNumbered() { if (view) togglePrefix(view, '1. '); }
  function doQuote() { if (view) togglePrefix(view, '> '); }
  function doLink() {
    if (!view) return;
    const { state } = view;
    const r = state.selection.main;
    const selected = state.sliceDoc(r.from, r.to);
    const label = selected || 'text';
    const insert = `[${label}](url)`;
    const urlStart = r.from + label.length + 3;
    view.dispatch({
      changes: { from: r.from, to: r.to, insert },
      selection: EditorSelection.range(urlStart, urlStart + 3),
    });
    view.focus();
  }

  const notesHighlight = HighlightStyle.define([
    { tag: t.heading1, fontSize: '1.4em', fontWeight: '700', color: 'var(--bone-0)' },
    { tag: t.heading2, fontSize: '1.22em', fontWeight: '700', color: 'var(--bone-0)' },
    { tag: t.heading3, fontSize: '1.1em', fontWeight: '600', color: 'var(--bone-0)' },
    { tag: t.heading4, fontWeight: '600', color: 'var(--bone-0)' },
    { tag: t.heading5, fontWeight: '600', color: 'var(--bone-0)' },
    { tag: t.heading6, fontWeight: '600', color: 'var(--bone-0)' },
    { tag: t.strong, fontWeight: '700', color: 'var(--bone-0)' },
    { tag: t.emphasis, fontStyle: 'italic', color: 'var(--bone-0)' },
    { tag: t.strikethrough, textDecoration: 'line-through' },
    { tag: t.link, color: 'var(--accent-hi)', textDecoration: 'underline' },
    { tag: t.url, color: 'var(--accent-hi)' },
    { tag: t.monospace, fontFamily: 'var(--font-mono)', color: 'var(--bone-0)' },
    { tag: t.quote, color: 'var(--bone-2)', fontStyle: 'italic' },
    { tag: t.list, color: 'var(--bone-1)' },
    { tag: t.processingInstruction, color: 'var(--bone-3)' },
    { tag: t.meta, color: 'var(--bone-3)' },
  ]);

  const agentTheme = EditorView.theme(
    {
      '&': {
        color: 'var(--bone-1)',
        backgroundColor: 'transparent',
        fontSize: '0.92rem',
        fontFamily: 'var(--font-sans)',
        height: '100%',
      },
      '.cm-content': {
        padding: '0.6rem 0.2rem',
        caretColor: 'var(--accent-hi)',
        lineHeight: '1.55',
      },
      '&.cm-focused': { outline: 'none' },
      '.cm-line': { padding: '0 0.2rem' },
      '.cm-cursor': {
        borderLeftColor: 'var(--accent-hi)',
        borderLeftWidth: '2px',
      },
      '&.cm-focused .cm-selectionBackground, ::selection': {
        backgroundColor: 'var(--accent-soft)',
      },
      '.cm-selectionBackground': { backgroundColor: 'var(--accent-soft)' },
      '.cm-placeholder': { color: 'var(--bone-3)', fontStyle: 'italic' },
      '.cm-scroller': { fontFamily: 'var(--font-sans)', overflow: 'auto' },
      '.cm-editor': { height: '100%' },
    },
    { dark: true },
  );

  onMount(() => {
    if (!host) return;
    const toolbarKeymap = keymap.of([
      { key: 'Mod-b', run: (v) => { wrapSelection(v, '**'); return true; } },
      { key: 'Mod-i', run: (v) => { wrapSelection(v, '*'); return true; } },
      { key: 'Mod-e', run: (v) => { wrapSelection(v, '`'); return true; } },
      { key: 'Mod-k', run: (v) => {
        const r = v.state.selection.main;
        const selected = v.state.sliceDoc(r.from, r.to);
        const label = selected || 'text';
        const insert = `[${label}](url)`;
        const urlStart = r.from + label.length + 3;
        v.dispatch({
          changes: { from: r.from, to: r.to, insert },
          selection: EditorSelection.range(urlStart, urlStart + 3),
        });
        return true;
      } },
    ]);
    const state = EditorState.create({
      doc: initialNotes,
      extensions: [
        history(),
        toolbarKeymap,
        keymap.of([...defaultKeymap, ...historyKeymap]),
        markdown(),
        syntaxHighlighting(notesHighlight),
        EditorView.lineWrapping,
        cmPlaceholder('Take notes during the call…'),
        agentTheme,
        EditorView.updateListener.of((u) => {
          if (!u.docChanged) return;
          onChange(u.state.doc.toString());
        }),
      ],
    });
    view = new EditorView({ state, parent: host });
  });

  onDestroy(() => {
    view?.destroy();
  });
</script>

<section class="notes-panel">
  <header class="notes-header">
    <strong>Notes</strong>
  </header>
  <div class="notes-wrap">
    <div class="toolbar" role="toolbar" aria-label="Formatting">
      <button type="button" class="tb-btn" onclick={doBold} title="Bold (Ctrl+B)"><strong>B</strong></button>
      <button type="button" class="tb-btn" onclick={doItalic} title="Italic (Ctrl+I)"><em>I</em></button>
      <button type="button" class="tb-btn mono" onclick={doCode} title="Inline code (Ctrl+E)">{"</>"}</button>
      <span class="divider" aria-hidden="true"></span>
      <button type="button" class="tb-btn" onclick={doH1} title="Heading 1">H1</button>
      <button type="button" class="tb-btn" onclick={doH2} title="Heading 2">H2</button>
      <span class="divider" aria-hidden="true"></span>
      <button type="button" class="tb-btn" onclick={doBullet} title="Bullet list" aria-label="Bullet list">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <circle cx="2.5" cy="4" r="1.3" fill="currentColor"/>
          <rect x="6" y="3.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
          <circle cx="2.5" cy="8" r="1.3" fill="currentColor"/>
          <rect x="6" y="7.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
          <circle cx="2.5" cy="12" r="1.3" fill="currentColor"/>
          <rect x="6" y="11.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
        </svg>
      </button>
      <button type="button" class="tb-btn" onclick={doNumbered} title="Numbered list" aria-label="Numbered list">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <text x="0.5" y="5.5" font-size="4.5" font-family="monospace" fill="currentColor" font-weight="600">1.</text>
          <rect x="6" y="3.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
          <text x="0.5" y="9.5" font-size="4.5" font-family="monospace" fill="currentColor" font-weight="600">2.</text>
          <rect x="6" y="7.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
          <text x="0.5" y="13.5" font-size="4.5" font-family="monospace" fill="currentColor" font-weight="600">3.</text>
          <rect x="6" y="11.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
        </svg>
      </button>
      <button type="button" class="tb-btn" onclick={doQuote} title="Quote" aria-label="Quote">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <rect x="2" y="2" width="1.6" height="12" rx="0.3" fill="currentColor"/>
          <rect x="5.5" y="3" width="8.5" height="1.5" rx="0.5" fill="currentColor"/>
          <rect x="5.5" y="7.25" width="6" height="1.5" rx="0.5" fill="currentColor"/>
          <rect x="5.5" y="11.5" width="7" height="1.5" rx="0.5" fill="currentColor"/>
        </svg>
      </button>
      <span class="divider" aria-hidden="true"></span>
      <button type="button" class="tb-btn" onclick={doLink} title="Link (Ctrl+K)" aria-label="Link">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <path d="M7 9 a3 3 0 0 1 0 -4 L9 3 a3 3 0 0 1 4 4 L12 8" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/>
          <path d="M9 7 a3 3 0 0 1 0  4 L7 13 a3 3 0 0 1 -4 -4 L4 8" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/>
        </svg>
      </button>
    </div>
    <div class="notes-host" bind:this={host}></div>
  </div>
</section>

<style>
  .notes-panel {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    margin-top: 1.2rem;
  }
  .notes-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .notes-wrap {
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
    min-height: 240px;
    height: 240px;
    resize: vertical;
    overflow: hidden;
    transition: border-color 0.15s;
    display: flex;
    flex-direction: column;
  }
  .notes-wrap:focus-within {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-glow);
  }

  .toolbar {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    padding: 0.3rem 0.5rem;
    border-bottom: 1px solid var(--hairline);
    background: var(--ink-2);
    flex-wrap: wrap;
  }
  .tb-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 28px;
    height: 26px;
    padding: 0 0.45rem;
    font-size: 0.82rem;
    color: var(--bone-2);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 4px;
    cursor: pointer;
    transition: background-color 0.1s, color 0.1s, border-color 0.1s;
    font-family: var(--font-sans);
    line-height: 1;
  }
  .tb-btn.mono {
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }
  .tb-btn:hover {
    color: var(--bone-0);
    background: var(--ink-3);
    border-color: var(--hairline-hi);
  }
  .tb-btn:active {
    background: var(--ink-0);
  }
  .tb-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .divider {
    display: inline-block;
    width: 1px;
    height: 16px;
    margin: 0 0.25rem;
    background: var(--hairline-hi);
  }

  .notes-host {
    flex: 1;
    min-height: 0;
    overflow: hidden;
    padding: 0.1rem 0.6rem;
  }
  .notes-host :global(.cm-editor) {
    height: 100%;
  }
  .notes-host :global(.cm-scroller) {
    font-family: var(--font-sans);
  }
</style>
