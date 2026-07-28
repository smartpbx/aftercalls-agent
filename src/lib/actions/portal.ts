/**
 * portal — re-parent a node to `document.body` while `active`, restoring it to
 * its exact original slot when inactive (and on destroy).
 *
 * Why this exists: a `position:fixed` layer nested inside an ancestor that
 * establishes a stacking context (the app shell does — `.shell { position:
 * relative; z-index:1 }`, load-bearing for the rail slide-out) has its
 * `z-index` flattened to that ancestor's root rank. It then paints BELOW any
 * higher-ranked root-level sibling — the recording floater (z-index 80),
 * toasts/modals (60-70) — even though it geometrically fills the viewport. That
 * is the exact trap that got the NotesPanel fullscreen (#505) reverted.
 * Re-parenting the node up to `<body>` lets its `z-index` compete at the
 * document root instead.
 *
 * Why MOVE rather than re-render: moving the live node preserves every bit of
 * component state + reactivity — Svelte keeps patching it in place. Svelte 5
 * also registers delegated event listeners on `document` (not just the mount
 * container) specifically so manually-portaled nodes keep their handlers, so
 * clicks/keydowns inside the moved subtree still fire.
 *
 * Usage: `use:portal={condition}` — teleports only while `condition` is true,
 * so a node that must stay in the normal tree in one layout and escape it in
 * another (e.g. the co-pilot lane shell: inline when compact, full-screen when
 * dashboard) can share ONE mounted instance across both.
 */
export function portal(node: HTMLElement, active = true) {
  // A comment marks the node's home while it's away, so we can drop it back
  // exactly where it was (order preserved) on restore.
  let anchor: Comment | null = null;

  function teleport() {
    if (anchor) return; // already portaled
    anchor = document.createComment("portal");
    node.before(anchor);
    document.body.appendChild(node);
  }

  function restore() {
    if (!anchor) return;
    // `replaceWith` is a no-op if the anchor has no parent (the surrounding
    // block was already torn down), leaving the node for Svelte to remove.
    anchor.replaceWith(node);
    anchor = null;
  }

  if (active) teleport();

  return {
    update(next: boolean) {
      if (next) teleport();
      else restore();
    },
    destroy() {
      // Return the node to its anchor so Svelte's own teardown removes it
      // exactly once — no orphan left parented to <body>.
      restore();
    },
  };
}
