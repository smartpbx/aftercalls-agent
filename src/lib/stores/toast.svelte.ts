// Toast / transient-notification primitive (#119) — runed store.
//
// Idiomatic Svelte 5 store. Single instance, exported as `toast`.
// Pages call `toast.success(...)` / `toast.error(...)` etc.; a single
// `<ToastHost />` mounted in `+layout.svelte` reads `toast.items` and
// renders the live stack. See design.md §Pattern library "Toast".
//
// The queue state lives in this module's `$state` rune so the host
// component re-renders on push / dismiss. The actual queue mechanics
// live in `./toast.helpers.ts` so they can be unit-tested without
// pulling in the Svelte compiler — tests assert against the same pure
// functions this wrapper calls.
//
// Mirrored byte-identical into `agent/src/lib/stores/toast.svelte.ts`.

import {
  createQueueState,
  dismissToast as helperDismiss,
  dismissTop as helperDismissTop,
  pushToast,
  type ToastItem,
  type ToastOptions,
  type ToastVariant,
} from "./toast.helpers";

function createStore() {
  // The queue is wrapped in `$state` so consumers re-render when items
  // mutate. Helper fns mutate the same object in place; we re-assign
  // `state.items = [...]` after each mutation to be unambiguous about
  // the dependency for Svelte's reactivity tracker.
  const state = $state(createQueueState());

  /** Per-toast auto-dismiss timer handles, keyed by id. Hover-pause
   *  clears the timer; mouse-leave reschedules it for the remaining
   *  duration. */
  const timers = new Map<number, ReturnType<typeof setTimeout>>();
  /** Remaining time (ms) when a hover paused the timer; consumed on
   *  resume to schedule the rest of the duration. */
  const remaining = new Map<number, number>();
  /** Wall-clock when the current timer started; used to compute the
   *  remaining time on hover-pause. */
  const startedAt = new Map<number, number>();

  function scheduleDismiss(id: number, ms: number): void {
    if (typeof window === "undefined") return;
    if (!isFinite(ms)) return; // sticky — never auto-dismisses
    clearTimer(id);
    startedAt.set(id, Date.now());
    remaining.set(id, ms);
    const handle = setTimeout(() => dismiss(id), ms);
    timers.set(id, handle);
  }

  function clearTimer(id: number): void {
    const h = timers.get(id);
    if (h !== undefined) {
      clearTimeout(h);
      timers.delete(id);
    }
  }

  function push(
    variant: ToastVariant,
    message: string,
    opts?: ToastOptions,
  ): number {
    const { id } = pushToast(state, variant, message, opts);
    // Re-trigger reactivity by re-assigning the array reference. Some
    // Svelte 5 setups track .push() correctly; explicit re-assign is
    // a no-cost belt that survives runtime quirks.
    state.items = [...state.items];
    const item = state.items.find((t) => t.id === id);
    if (item) scheduleDismiss(id, item.duration);
    return id;
  }

  function dismiss(id: number): void {
    clearTimer(id);
    remaining.delete(id);
    startedAt.delete(id);
    helperDismiss(state, id);
    state.items = [...state.items];
  }

  /** Drop the topmost visible toast (Escape-key target). */
  function dismissTop(): void {
    const id = helperDismissTop(state);
    if (id !== null) {
      clearTimer(id);
      remaining.delete(id);
      startedAt.delete(id);
      state.items = [...state.items];
    }
  }

  /** Hover-pause: capture the elapsed time and clear the timer.
   *  Called on `mouseenter` / `focusin` from <Toast>. */
  function pause(id: number): void {
    const start = startedAt.get(id);
    const total = remaining.get(id);
    if (start === undefined || total === undefined) return;
    const elapsed = Date.now() - start;
    const left = Math.max(0, total - elapsed);
    clearTimer(id);
    remaining.set(id, left);
  }

  /** Resume after a hover-pause: schedule the remaining duration. */
  function resume(id: number): void {
    const left = remaining.get(id);
    if (left === undefined) return;
    if (!isFinite(left)) return;
    scheduleDismiss(id, left);
  }

  return {
    /** Reactive list of all toasts (visible + queued). The host
     *  component slices to MAX_VISIBLE itself. */
    get items(): ToastItem[] {
      return state.items;
    },
    info(message: string, opts?: ToastOptions): number {
      return push("info", message, opts);
    },
    success(message: string, opts?: ToastOptions): number {
      return push("success", message, opts);
    },
    warning(message: string, opts?: ToastOptions): number {
      return push("warning", message, opts);
    },
    error(message: string, opts?: ToastOptions): number {
      return push("error", message, opts);
    },
    dismiss,
    dismissTop,
    pause,
    resume,
  };
}

/** Singleton store. Import as:
 *
 *    import { toast } from "$lib/stores/toast.svelte";
 *    toast.success("Saved.");
 *    toast.error("Couldn't save.", { duration: 8_000 });
 *    toast.info("New version available.", { sticky: true });
 */
export const toast = createStore();
