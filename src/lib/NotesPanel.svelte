<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { EditorState, RangeSetBuilder } from '@codemirror/state';
  import {
    EditorView,
    keymap,
    placeholder as cmPlaceholder,
    Decoration,
    type DecorationSet,
    ViewPlugin,
    type ViewUpdate,
  } from '@codemirror/view';
  import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
  import {
    HighlightStyle,
    syntaxHighlighting,
    syntaxTree,
  } from '@codemirror/language';
  import { markdown } from '@codemirror/lang-markdown';
  import { tags as t } from '@lezer/highlight';

  // Manual notes editor for the record screen (#73). Shares the live-
  // preview decoration approach with the portal's NotesEditor — the
  // decoration plugin walks the Lezer markdown tree and hides mark
  // characters (**/*/#/-/>) on every line except the one the cursor is
  // on, so as-you-type formatting feels like Obsidian's Live Preview.

  type Props = {
    initialNotes?: string;
    initialInclude?: boolean;
    onChange: (notes: string, includeInSummary: boolean) => void;
  };
  let { initialNotes = '', initialInclude = true, onChange }: Props = $props();

  let host: HTMLDivElement | undefined = $state();
  let view: EditorView | null = null;
  let include = $state(initialInclude);
  let currentText = initialNotes;

  // ── Live-preview decoration plugin ─────────────────────────────────
  const HIDE = Decoration.replace({});

  function buildLivePreviewDecorations(view: EditorView): DecorationSet {
    const builder = new RangeSetBuilder<Decoration>();
    const { state } = view;
    const activeLines = new Set<number>();
    for (const r of state.selection.ranges) {
      activeLines.add(state.doc.lineAt(r.from).number);
      if (r.from !== r.to) {
        activeLines.add(state.doc.lineAt(r.to).number);
      }
    }

    for (const { from, to } of view.visibleRanges) {
      syntaxTree(state).iterate({
        from,
        to,
        enter: (node) => {
          const name = node.name;
          const isMark =
            name === 'EmphasisMark' ||
            name === 'StrongEmphasisMark' ||
            name === 'StrikethroughMark' ||
            name === 'HeaderMark' ||
            name === 'ListMark' ||
            name === 'QuoteMark' ||
            name === 'LinkMark' ||
            name === 'CodeMark';
          if (!isMark) return;
          const lineNum = state.doc.lineAt(node.from).number;
          if (activeLines.has(lineNum)) return;
          builder.add(node.from, node.to, HIDE);
        },
      });
    }
    return builder.finish();
  }

  const livePreviewPlugin = ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildLivePreviewDecorations(view);
      }
      update(u: ViewUpdate) {
        if (u.docChanged || u.selectionSet || u.viewportChanged) {
          this.decorations = buildLivePreviewDecorations(u.view);
        }
      }
    },
    { decorations: (v) => v.decorations },
  );

  // ── Syntax-driven visual weight (bold / italic / headings / links) ──
  const notesHighlight = HighlightStyle.define([
    { tag: t.heading1, fontSize: '1.35em', fontWeight: '700', color: 'var(--bone-0)' },
    { tag: t.heading2, fontSize: '1.2em', fontWeight: '700', color: 'var(--bone-0)' },
    { tag: t.heading3, fontSize: '1.08em', fontWeight: '600', color: 'var(--bone-0)' },
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
      '.cm-placeholder': {
        color: 'var(--bone-3)',
        fontStyle: 'italic',
      },
      '.cm-scroller': {
        fontFamily: 'var(--font-sans)',
        overflow: 'auto',
      },
      '.cm-editor': { height: '100%' },
    },
    { dark: true },
  );

  onMount(() => {
    if (!host) return;
    const state = EditorState.create({
      doc: initialNotes,
      extensions: [
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        markdown(),
        syntaxHighlighting(notesHighlight),
        livePreviewPlugin,
        EditorView.lineWrapping,
        cmPlaceholder('Take notes during the call…'),
        agentTheme,
        EditorView.updateListener.of((u) => {
          if (!u.docChanged) return;
          currentText = u.state.doc.toString();
          onChange(currentText, include);
        }),
      ],
    });
    view = new EditorView({ state, parent: host });
  });

  onDestroy(() => {
    view?.destroy();
  });

  function toggleInclude() {
    include = !include;
    onChange(currentText, include);
  }
</script>

<section class="notes-panel">
  <header class="notes-header">
    <strong>Notes</strong>
    <label class="include-toggle" title="When on, your notes feed into the AI summary alongside the transcript.">
      <input type="checkbox" checked={include} onchange={toggleInclude} />
      Include in summary
    </label>
  </header>
  <div class="notes-wrap">
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
  .include-toggle {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.85rem;
    color: var(--bone-2);
    cursor: pointer;
    user-select: none;
  }
  .include-toggle input {
    cursor: pointer;
  }
  .notes-wrap {
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
    min-height: 200px;
    height: 200px;
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
