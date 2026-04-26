// #282 — Shared keyboard-shortcut helper.
//
// Mirror pair: this file is byte-identical with
// `agent/src/lib/shortcuts.svelte.ts`. Edit both in the same commit.
//
// One window-level `keydown` listener fans out to per-page bindings
// keyed by a context name. Pages call `registerShortcuts(context,
// bindings)` on mount and the returned teardown on destroy. The
// global context (`?` opens the overlay, `Esc` closes it) is owned
// by the layout.
//
// The handler ignores keystrokes when the focused element is a text
// input — same shape as the welcome flow's onKeydown guard:
//
//   target.tagName === "INPUT" || "TEXTAREA" || target.isContentEditable
//
// so typing in a search box, the notes editor (TipTap = contenteditable),
// or the rename input never fires a page shortcut.
//
// Shortcuts use a normalized key string built from the event:
//   - Modifier prefixes (alphabetical): "shift+", "ctrl+", "alt+",
//     "meta+" (we don't bind super-prefixed shortcuts on the web —
//     those are agent-only Tauri global shortcuts).
//   - Key: the canonical name (`?`, `/`, `,`, `.`, `enter`, `escape`,
//     `arrowleft`, `arrowright`, `space`, single letters lowercased).
//
// The locked shortcut list (issue #282) — wired by callers, listed
// here for grep:
//
//   Global       : ? (overlay), Escape (close)
//   Calls list   : j/k (next/prev), Enter|o (open), / (focus search)
//   Call detail  : Space (play/pause), ←/→ (-10s/+10s),
//                  Shift+←/Shift+→ (-30s/+30s), , / . (rate -/+)
//   Action items : j/k (next/prev), Space (toggle), e (edit)
//   Recording    : Super+Shift+R, Super+Shift+N (Tauri-side, agent only)

export type ShortcutHandler = (e: KeyboardEvent) => void;

// One binding map per registered context. Callers pass any string as
// the context name (`"calls-list"`, `"call-detail"`, `"actions"`, ...);
// the overlay uses the same string to look up the section heading.
export type ShortcutBindings = Record<string, ShortcutHandler>;

export type ShortcutGroup = {
  // The id used at register-time. Stable across reloads.
  context: string;
  // Heading shown in the help overlay for this group.
  title: string;
  // Display rows: each entry maps a printable key combo (`"j"`,
  // `"shift+→"`, `"space"`) to a short label ("Next item",
  // "Seek -30s"). Order is preserved.
  items: Array<{ keys: string; label: string }>;
};

type ContextEntry = {
  context: string;
  title: string;
  bindings: ShortcutBindings;
  // Display order matches the order keys were registered.
  display: ShortcutGroup["items"];
};

// ── Reactive store ──────────────────────────────────────────────────
//
// Module-level $state survives page navigation (the layout owns the
// listener). `helpOpen` is read by `ShortcutsOverlay.svelte`; the
// list of registered contexts powers the overlay's grouped list.
//
// `.svelte.ts` extension lets runes work outside a component.

const contexts = $state<Map<string, ContextEntry>>(new Map());
let helpOpen = $state(false);

export const shortcutsState = {
  get helpOpen() {
    return helpOpen;
  },
  get contexts(): ShortcutGroup[] {
    // Stable order: insertion order. The layout registers "global"
    // first so it always heads the list.
    const out: ShortcutGroup[] = [];
    for (const entry of contexts.values()) {
      out.push({
        context: entry.context,
        title: entry.title,
        items: entry.display,
      });
    }
    return out;
  },
};

export function setHelpVisible(visible: boolean) {
  helpOpen = visible;
}

export function toggleHelp() {
  helpOpen = !helpOpen;
}

// ── Registration ────────────────────────────────────────────────────
//
// Pages call this on mount; the returned teardown is wired into
// `onDestroy`. Registering the same context twice replaces the
// previous registration (handy for HMR + for routes that re-register
// after a parameter change).

export function registerShortcuts(
  context: string,
  title: string,
  bindings: ShortcutBindings,
  display: ShortcutGroup["items"],
): () => void {
  contexts.set(context, { context, title, bindings, display });
  return () => {
    const cur = contexts.get(context);
    // Defensive: only delete if it's still ours (a re-register may
    // have replaced the entry already).
    if (cur && cur.bindings === bindings) {
      contexts.delete(context);
    }
  };
}

// ── Window listener ─────────────────────────────────────────────────
//
// Initialized once by the layout. Subsequent calls are no-ops so HMR
// doesn't double-bind.

let listenerAttached = false;

export function initShortcutListener() {
  if (listenerAttached || typeof window === "undefined") return;
  listenerAttached = true;
  window.addEventListener("keydown", onWindowKeydown);
}

function isTextInput(target: EventTarget | null): boolean {
  if (!target || !(target instanceof HTMLElement)) return false;
  if (target.tagName === "INPUT" || target.tagName === "TEXTAREA") return true;
  if (target.isContentEditable) return true;
  return false;
}

// Build a normalized key id like "shift+arrowright", "j", "?",
// "space", "enter". We lowercase letters but preserve punctuation
// and arrow keys verbatim (lowercased). Modifiers in fixed order so
// caller maps don't have to second-guess them.
function normalizeKey(e: KeyboardEvent): string {
  const k = e.key;
  // `e.key` for the spacebar is " " — name it explicitly so callers
  // can write `"space"` instead of a literal space character.
  let key: string;
  if (k === " ") key = "space";
  else if (k.length === 1) key = k.toLowerCase();
  else key = k.toLowerCase();

  const mods: string[] = [];
  if (e.shiftKey) mods.push("shift");
  if (e.ctrlKey) mods.push("ctrl");
  if (e.altKey) mods.push("alt");
  if (e.metaKey) mods.push("meta");
  // For printable characters that already encode shift (`?`, `/`,
  // uppercase letters), don't double-stamp the shift modifier — the
  // caller's binding for `?` would otherwise need to be written as
  // `shift+?`. Arrow keys + Space + Enter still take the modifier.
  const isPrintableShift =
    e.shiftKey && key.length === 1 && !/^[a-z0-9]$/.test(key);
  if (isPrintableShift) {
    const i = mods.indexOf("shift");
    if (i >= 0) mods.splice(i, 1);
  }
  return mods.length > 0 ? `${mods.join("+")}+${key}` : key;
}

function onWindowKeydown(e: KeyboardEvent) {
  // Esc always works regardless of focus (lets users escape out of
  // a search box, edit field, etc.). The overlay's own `?` re-toggle
  // is gated by the input-focus guard further down.
  const key = normalizeKey(e);

  if (key === "escape" && helpOpen) {
    e.preventDefault();
    helpOpen = false;
    return;
  }

  // Input-focus guard — copy of the welcome-flow pattern. Keystrokes
  // typed into a text input never fire a registered shortcut.
  if (isTextInput(e.target)) return;

  // Cooperate with element-level handlers: if the focused element
  // already handled this keystroke (e.g. a focused transcript line
  // calls preventDefault on Enter/Space to seek), don't fire a
  // second action from the window-level binding.
  if (e.defaultPrevented) return;

  // While the help overlay is open, only the global toggle (`?`) and
  // Escape fire — page-level shortcuts pause so a user pressing `j`
  // to scan the overlay doesn't also step the underlying calls list.
  if (helpOpen) {
    const globalEntry = contexts.get("global");
    const handler = globalEntry?.bindings[key];
    if (handler) {
      e.preventDefault();
      handler(e);
    }
    return;
  }

  // Walk contexts in registration order and dispatch the first match.
  // (Today no two contexts bind the same key; if they did, "global"
  // wins by virtue of being registered first.)
  for (const entry of contexts.values()) {
    const handler = entry.bindings[key];
    if (handler) {
      e.preventDefault();
      handler(e);
      return;
    }
  }
}
