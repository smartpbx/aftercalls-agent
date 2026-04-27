<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { Editor } from '@tiptap/core';
  import StarterKit from '@tiptap/starter-kit';
  import Link from '@tiptap/extension-link';
  import Placeholder from '@tiptap/extension-placeholder';
  import { Markdown } from 'tiptap-markdown';

  // Manual notes editor (#73). TipTap WYSIWYG — bold is bold, bullets
  // render as dots, headings sized; no raw markdown syntax visible.
  // Storage stays markdown via tiptap-markdown. Same behaviour as the
  // portal NotesEditor; deliberately near-identical code.

  type Props = {
    value?: string;
    readonly?: boolean;
    placeholder?: string;
    showHeader?: boolean;
    onchange: (value: string) => void;
  };
  let {
    value = '',
    readonly = false,
    placeholder = 'Take notes during or after the call…',
    showHeader = true,
    onchange,
  }: Props = $props();

  let host: HTMLDivElement | undefined = $state();
  let editor: Editor | undefined = $state();
  let lastEmitted = '';
  // #505 — fullscreen/maximize toggle. When expanded, the panel renders as
  // a fixed overlay so the user has maximum writing space without leaving
  // the Record screen. Escape or the toggle button collapses it.
  let expanded = $state(false);

  function toggleExpanded() {
    expanded = !expanded;
    // Re-focus the editor after layout shift so the cursor stays active.
    requestAnimationFrame(() => editor?.commands.focus());
  }

  function onKeydownExpanded(e: KeyboardEvent) {
    if (e.key === 'Escape' && expanded) {
      e.stopPropagation();
      expanded = false;
    }
  }

  function runCmd(fn: (chain: ReturnType<Editor['chain']>) => ReturnType<Editor['chain']>) {
    if (!editor) return;
    fn(editor.chain().focus()).run();
  }

  function setLink() {
    if (!editor) return;
    const prev = editor.getAttributes('link').href ?? '';
    const url = window.prompt('Link URL', prev || 'https://');
    if (url === null) return;
    if (url === '') {
      editor.chain().focus().extendMarkRange('link').unsetLink().run();
      return;
    }
    editor.chain().focus().extendMarkRange('link').setLink({ href: url }).run();
  }

  let isBold = $state(false);
  let isItalic = $state(false);
  let isCode = $state(false);
  let isH1 = $state(false);
  let isH2 = $state(false);
  let isBullet = $state(false);
  let isNumbered = $state(false);
  let isQuote = $state(false);
  let isLink = $state(false);

  function refreshActive() {
    if (!editor) return;
    isBold = editor.isActive('bold');
    isItalic = editor.isActive('italic');
    isCode = editor.isActive('code');
    isH1 = editor.isActive('heading', { level: 1 });
    isH2 = editor.isActive('heading', { level: 2 });
    isBullet = editor.isActive('bulletList');
    isNumbered = editor.isActive('orderedList');
    isQuote = editor.isActive('blockquote');
    isLink = editor.isActive('link');
  }

  onMount(() => {
    if (!host) return;
    lastEmitted = value;
    editor = new Editor({
      element: host,
      extensions: [
        StarterKit,
        Link.configure({
          openOnClick: false,
          HTMLAttributes: { class: 'notes-link' },
        }),
        Placeholder.configure({ placeholder }),
        Markdown.configure({
          html: false,
          linkify: true,
          breaks: false,
          transformPastedText: true,
        }),
      ],
      content: value,
      editable: !readonly,
      onUpdate: ({ editor }) => {
        const md = (editor.storage as any).markdown.getMarkdown();
        if (md === lastEmitted) return;
        lastEmitted = md;
        onchange(md);
        refreshActive();
      },
      onSelectionUpdate: () => refreshActive(),
      onCreate: () => refreshActive(),
    });
  });

  onDestroy(() => {
    editor?.destroy();
    editor = undefined;
  });

  $effect(() => {
    if (!editor) return;
    if (value === lastEmitted) return;
    const current = (editor.storage as any).markdown.getMarkdown();
    if (current === value) {
      lastEmitted = value;
      return;
    }
    editor.commands.setContent(value, { emitUpdate: false });
    lastEmitted = value;
  });
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<section
  class="notes-panel"
  class:no-header={!showHeader}
  class:expanded
  onkeydown={onKeydownExpanded}
>
  {#if showHeader}
    <header class="notes-header">
      <strong>Notes</strong>
      <!-- #505 — fullscreen/maximize toggle -->
      <button
        type="button"
        class="expand-btn"
        onclick={toggleExpanded}
        title={expanded ? 'Collapse notes (Escape)' : 'Expand notes'}
        aria-label={expanded ? 'Collapse notes' : 'Expand notes'}
        aria-pressed={expanded}
      >
        {#if expanded}
          <!-- Compress icon -->
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">
            <path d="M6 2v4H2M10 2v4h4M6 14v-4H2M10 14v-4h4"/>
          </svg>
        {:else}
          <!-- Expand icon -->
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">
            <path d="M2 6V2h4M14 6V2h-4M2 10v4h4M14 10v4h-4"/>
          </svg>
        {/if}
      </button>
    </header>
  {/if}
  <div class="notes-wrap" class:readonly>
    {#if !readonly}
      <div class="toolbar" role="toolbar" aria-label="Formatting">
        <button
          type="button"
          class="tb-btn"
          class:active={isBold}
          onclick={() => runCmd((c) => c.toggleBold())}
          title="Bold (⌘B / Ctrl+B)"
          aria-label="Bold"
        ><strong>B</strong></button>
        <button
          type="button"
          class="tb-btn"
          class:active={isItalic}
          onclick={() => runCmd((c) => c.toggleItalic())}
          title="Italic (⌘I / Ctrl+I)"
          aria-label="Italic"
        ><em>I</em></button>
        <button
          type="button"
          class="tb-btn mono"
          class:active={isCode}
          onclick={() => runCmd((c) => c.toggleCode())}
          title="Inline code (⌘E / Ctrl+E)"
          aria-label="Inline code"
        >{"</>"}</button>
        <span class="divider" aria-hidden="true"></span>
        <button
          type="button"
          class="tb-btn"
          class:active={isH1}
          onclick={() => runCmd((c) => c.toggleHeading({ level: 1 }))}
          title="Heading 1"
        >H1</button>
        <button
          type="button"
          class="tb-btn"
          class:active={isH2}
          onclick={() => runCmd((c) => c.toggleHeading({ level: 2 }))}
          title="Heading 2"
        >H2</button>
        <span class="divider" aria-hidden="true"></span>
        <button
          type="button"
          class="tb-btn"
          class:active={isBullet}
          onclick={() => runCmd((c) => c.toggleBulletList())}
          title="Bullet list"
          aria-label="Bullet list"
        >
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <circle cx="2.5" cy="4" r="1.3" fill="currentColor"/>
            <rect x="6" y="3.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
            <circle cx="2.5" cy="8" r="1.3" fill="currentColor"/>
            <rect x="6" y="7.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
            <circle cx="2.5" cy="12" r="1.3" fill="currentColor"/>
            <rect x="6" y="11.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
          </svg>
        </button>
        <button
          type="button"
          class="tb-btn"
          class:active={isNumbered}
          onclick={() => runCmd((c) => c.toggleOrderedList())}
          title="Numbered list"
          aria-label="Numbered list"
        >
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <text x="0.5" y="5.5" font-size="4.5" fill="currentColor" font-weight="600">1.</text>
            <rect x="6" y="3.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
            <text x="0.5" y="9.5" font-size="4.5" fill="currentColor" font-weight="600">2.</text>
            <rect x="6" y="7.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
            <text x="0.5" y="13.5" font-size="4.5" fill="currentColor" font-weight="600">3.</text>
            <rect x="6" y="11.25" width="8" height="1.5" rx="0.5" fill="currentColor"/>
          </svg>
        </button>
        <button
          type="button"
          class="tb-btn"
          class:active={isQuote}
          onclick={() => runCmd((c) => c.toggleBlockquote())}
          title="Quote"
          aria-label="Quote"
        >
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <rect x="2" y="2" width="1.6" height="12" rx="0.3" fill="currentColor"/>
            <rect x="5.5" y="3" width="8.5" height="1.5" rx="0.5" fill="currentColor"/>
            <rect x="5.5" y="7.25" width="6" height="1.5" rx="0.5" fill="currentColor"/>
            <rect x="5.5" y="11.5" width="7" height="1.5" rx="0.5" fill="currentColor"/>
          </svg>
        </button>
        <span class="divider" aria-hidden="true"></span>
        <button
          type="button"
          class="tb-btn"
          class:active={isLink}
          onclick={setLink}
          title="Link (⌘K / Ctrl+K)"
          aria-label="Link"
        >
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path d="M7 9 a3 3 0 0 1 0 -4 L9 3 a3 3 0 0 1 4 4 L12 8" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/>
            <path d="M9 7 a3 3 0 0 1 0 4 L7 13 a3 3 0 0 1 -4 -4 L4 8" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/>
          </svg>
        </button>
      </div>
    {/if}
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
  .notes-panel.no-header {
    margin-top: 0;
  }
  /* #505 — expanded / fullscreen mode */
  .notes-panel.expanded {
    position: fixed;
    inset: 0;
    z-index: 120;
    margin: 0;
    padding: 1.2rem;
    background: var(--ink-0);
    gap: 0.6rem;
  }
  .notes-panel.expanded .notes-wrap {
    flex: 1;
    min-height: 0;
    height: auto;
    resize: none;
  }
  .notes-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  /* #505 — expand/collapse toggle button */
  .expand-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border: 1px solid transparent;
    border-radius: 5px;
    background: transparent;
    color: var(--bone-3);
    cursor: pointer;
    transition: color 0.12s, background 0.12s, border-color 0.12s;
  }
  .expand-btn:hover {
    color: var(--bone-0);
    background: var(--ink-2);
    border-color: var(--hairline-hi);
  }
  .expand-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
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
  .notes-wrap.readonly {
    background: var(--ink-0);
    resize: none;
  }

  .toolbar {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    padding: 0.3rem 0.5rem;
    border-bottom: 1px solid var(--hairline);
    background: var(--ink-2);
    flex-wrap: wrap;
    flex-shrink: 0;
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
  .tb-btn.active {
    color: var(--accent-hi);
    background: var(--accent-soft);
    border-color: var(--accent);
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
    overflow: auto;
    padding: 0.4rem 0.8rem;
    color: var(--bone-1);
    font-size: 0.92rem;
    line-height: 1.55;
  }

  .notes-host :global(.ProseMirror) {
    outline: none;
    min-height: 100%;
  }
  .notes-host :global(.ProseMirror p) { margin: 0 0 0.4rem; }
  .notes-host :global(.ProseMirror p:last-child) { margin-bottom: 0; }
  .notes-host :global(.ProseMirror h1) {
    font-size: 1.4em; font-weight: 700; margin: 0.6rem 0 0.4rem; color: var(--bone-0);
  }
  .notes-host :global(.ProseMirror h2) {
    font-size: 1.22em; font-weight: 700; margin: 0.55rem 0 0.35rem; color: var(--bone-0);
  }
  .notes-host :global(.ProseMirror h3) {
    font-size: 1.1em; font-weight: 600; margin: 0.5rem 0 0.3rem; color: var(--bone-0);
  }
  .notes-host :global(.ProseMirror h1:first-child),
  .notes-host :global(.ProseMirror h2:first-child),
  .notes-host :global(.ProseMirror h3:first-child) { margin-top: 0; }
  .notes-host :global(.ProseMirror strong) { font-weight: 700; color: var(--bone-0); }
  .notes-host :global(.ProseMirror em) { font-style: italic; color: var(--bone-0); }
  .notes-host :global(.ProseMirror s) { text-decoration: line-through; color: var(--bone-2); }
  .notes-host :global(.ProseMirror code) {
    font-family: var(--font-mono); font-size: 0.88em; padding: 0.05em 0.3em;
    border-radius: 3px; background: var(--ink-3); color: var(--bone-0);
  }
  .notes-host :global(.ProseMirror pre) {
    font-family: var(--font-mono); font-size: 0.88em; padding: 0.55rem 0.7rem;
    margin: 0.4rem 0; border-radius: 6px; background: var(--ink-3); color: var(--bone-0);
    overflow-x: auto;
  }
  .notes-host :global(.ProseMirror pre code) { padding: 0; background: transparent; }
  .notes-host :global(.ProseMirror ul),
  .notes-host :global(.ProseMirror ol) { padding-left: 1.3rem; margin: 0.2rem 0 0.4rem; }
  .notes-host :global(.ProseMirror li) { margin: 0.1rem 0; }
  .notes-host :global(.ProseMirror li p) { margin: 0; }
  .notes-host :global(.ProseMirror blockquote) {
    border-left: 3px solid var(--hairline-hi); padding-left: 0.7rem;
    margin: 0.3rem 0; color: var(--bone-2); font-style: italic;
  }
  .notes-host :global(.ProseMirror a),
  .notes-host :global(.ProseMirror .notes-link) {
    color: var(--accent-hi); text-decoration: underline; cursor: pointer;
  }
  .notes-host :global(.ProseMirror hr) {
    border: none; border-top: 1px solid var(--hairline); margin: 0.7rem 0;
  }
  .notes-host :global(.ProseMirror p.is-editor-empty:first-child::before) {
    content: attr(data-placeholder);
    float: left;
    color: var(--bone-3);
    font-style: italic;
    pointer-events: none;
    height: 0;
  }
</style>
