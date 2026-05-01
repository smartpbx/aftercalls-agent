// Toast / transient-notification primitive (#119) — pure helpers.
//
// The store wrapper at `./toast.svelte.ts` holds a `$state` rune for
// reactivity; this module contains the queue logic in plain TS so
// vitest can exercise push / dismiss / max-stack / pruning without
// having to compile a Svelte file.
//
// Two-file split mirrors the SummaryText pattern (`SummaryText.helpers.ts`
// + `SummaryText.svelte`) — keeps the runed surface minimal and the
// testable surface large.
//
// Shared package helper consumed by both portal and agent.

export type ToastVariant = "info" | "success" | "warning" | "error";

export type ToastAction = {
  label: string;
  onClick: () => void;
};

/** Caller-supplied options when pushing a toast. */
export type ToastOptions = {
  /** Override auto-dismiss duration in ms. `Infinity` = sticky. */
  duration?: number;
  /** Equivalent to `duration: Infinity`. */
  sticky?: boolean;
  /** Optional inline action button (e.g. "Undo", "Retry"). */
  action?: ToastAction;
};

/** A toast in the queue (visible or queued). */
export type ToastItem = {
  id: number;
  variant: ToastVariant;
  message: string;
  /** Resolved duration in ms; `Infinity` for sticky. */
  duration: number;
  /** Optional inline action. */
  action: ToastAction | null;
};

export type ToastQueueState = {
  /** All live toasts in arrival order; visible slice is the first
   *  MAX_VISIBLE. The rest are queued and surface as visible toasts
   *  drain. Capping the visible count keeps the bottom-right corner
   *  legible. */
  items: ToastItem[];
  /** Auto-incrementing id source. */
  nextId: number;
};

/** Per-variant default auto-dismiss durations. Errors stay longer
 *  because the user typically needs to read + decide what to do; info
 *  / success are quicker. */
export const DEFAULT_DURATIONS: Record<ToastVariant, number> = {
  info: 4_000,
  success: 4_000,
  warning: 6_000,
  error: 8_000,
};

/** Max number of toasts visible at the bottom-right at once. Excess
 *  is queued and shown as the visible stack drains. */
export const MAX_VISIBLE = 3;

/** Build the initial empty queue state. */
export function createQueueState(): ToastQueueState {
  return { items: [], nextId: 1 };
}

/** Resolve the effective duration for a push, respecting `sticky`,
 *  explicit `duration`, and the per-variant default. */
export function resolveDuration(
  variant: ToastVariant,
  opts: ToastOptions | undefined,
): number {
  if (opts?.sticky) return Infinity;
  if (opts?.duration !== undefined) return opts.duration;
  return DEFAULT_DURATIONS[variant];
}

/** Append a new toast; returns the new state and the assigned id so
 *  the caller can wire its own auto-dismiss timer. The state is
 *  mutated in place AND returned for ergonomic call sites that want
 *  to chain. The runed wrapper passes the same `state` object on
 *  every call so the rune sees the array push. */
export function pushToast(
  state: ToastQueueState,
  variant: ToastVariant,
  message: string,
  opts?: ToastOptions,
): { state: ToastQueueState; id: number } {
  const id = state.nextId++;
  state.items.push({
    id,
    variant,
    message,
    duration: resolveDuration(variant, opts),
    action: opts?.action ?? null,
  });
  return { state, id };
}

/** Drop a toast by id. No-op if not present. */
export function dismissToast(
  state: ToastQueueState,
  id: number,
): ToastQueueState {
  const idx = state.items.findIndex((t) => t.id === id);
  if (idx >= 0) state.items.splice(idx, 1);
  return state;
}

/** Drop the topmost VISIBLE toast (the one most recently surfaced
 *  in the visible slice). Used for the Escape-key handler. Returns
 *  the dismissed id, or null if there's nothing to dismiss. */
export function dismissTop(state: ToastQueueState): number | null {
  const visible = state.items.slice(0, MAX_VISIBLE);
  if (visible.length === 0) return null;
  // "Topmost" = the most recently added of the visible set, which
  // sits at the visual top of the bottom-right stack.
  const top = visible[visible.length - 1];
  dismissToast(state, top.id);
  return top.id;
}

/** Get the visible slice (the toasts currently rendered). The
 *  remainder are queued and surface FIFO as the stack drains. */
export function visibleToasts(state: ToastQueueState): ToastItem[] {
  return state.items.slice(0, MAX_VISIBLE);
}

/** Number of queued (not-yet-visible) toasts. Useful for tests +
 *  potential "+N more" badge in the future. */
export function queuedCount(state: ToastQueueState): number {
  return Math.max(0, state.items.length - MAX_VISIBLE);
}
