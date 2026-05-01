// Offline indicator (#107). Polls the backend's /health route and
// surfaces a single reactive `online` flag the layout reads to decide
// whether to show the OfflineBanner.
//
// Surface-specific sibling of `portal/src/lib/stores/onlineStatus.svelte.ts`:
// the contract (start / online / probeNow / subscribe) is identical so
// the OfflineBanner mirror-pair works against either, but the probe
// rides a Tauri command instead of a direct fetch — the agent webview
// would otherwise have to navigate cross-origin to the backend on
// every probe and burn a CORS preflight per call.
//
// Detection contract (mirrors the portal store comments):
//   - Healthy: poll every ~10s.
//   - Failed probe: drop into ~2s re-probe loop until we either
//     confirm offline (≥10s of consecutive failure) or recover.
//   - Recovery: first successful probe flips us back online; combined
//     with the fast-poll cadence the indicator clears within ~5s of
//     the network coming back, satisfying the issue's spec.
//   - `navigator.onLine` short-circuits the probe in clear offline
//     events but never marks us back online on its own (browsers lie).

import { invoke } from "@tauri-apps/api/core";
import type { OnlineStatusStore } from "@aftercalls/shared/contracts";

const FAST_POLL_MS = 2_000;
const SLOW_POLL_MS = 10_000;
const OFFLINE_AFTER_MS = 10_000;

async function probeOnce(): Promise<boolean> {
  if (typeof navigator !== "undefined" && navigator.onLine === false) {
    return false;
  }
  try {
    // The Rust side already enforces a short timeout (5s) and folds
    // every failure path into a `false`, so the JS catch is just
    // defence-in-depth against an unexpected IPC reject.
    return await invoke<boolean>("backend_health");
  } catch {
    return false;
  }
}

function createStore() {
  const state = $state({ online: true, started: false });
  const subscribers = new Set<(online: boolean) => void>();

  let firstFailureAt: number | null = null;
  let lastSuccessAt: number | null = null;
  let nextHandle: ReturnType<typeof setTimeout> | undefined;

  function setOnline(next: boolean) {
    if (state.online === next) return;
    state.online = next;
    for (const cb of subscribers) {
      try {
        cb(next);
      } catch {
        // Subscriber bugs must not break the polling loop.
      }
    }
  }

  async function tick() {
    const ok = await probeOnce();
    const now = Date.now();
    if (ok) {
      firstFailureAt = null;
      lastSuccessAt = now;
      setOnline(true);
      schedule(SLOW_POLL_MS);
      return;
    }
    if (firstFailureAt === null) firstFailureAt = now;
    const elapsed = now - firstFailureAt;
    if (elapsed >= OFFLINE_AFTER_MS) {
      setOnline(false);
    }
    schedule(FAST_POLL_MS);
  }

  function schedule(delay: number) {
    if (typeof window === "undefined") return;
    if (nextHandle !== undefined) clearTimeout(nextHandle);
    nextHandle = setTimeout(() => {
      void tick();
    }, delay);
  }

  function start() {
    if (state.started) return;
    if (typeof window === "undefined") return;
    state.started = true;
    if (typeof window !== "undefined") {
      window.addEventListener("online", () => void tick());
      window.addEventListener("offline", () => void tick());
    }
    void tick();
  }

  return {
    /** Reactive flag — true when we believe the backend is reachable. */
    get online(): boolean {
      return state.online;
    },
    /** Idempotent. Call once from the root layout's onMount. */
    start,
    /** Force an immediate probe — used by the save-queue replay loop
     *  after a 5xx so a transient blip doesn't strand the queue. */
    async probeNow(): Promise<boolean> {
      const ok = await probeOnce();
      const now = Date.now();
      if (ok) {
        firstFailureAt = null;
        lastSuccessAt = now;
        setOnline(true);
      } else {
        if (firstFailureAt === null) firstFailureAt = now;
        if (now - firstFailureAt >= OFFLINE_AFTER_MS) setOnline(false);
      }
      return ok;
    },
    /** Subscribe to online↔offline transitions. Returns an
     *  unsubscribe fn. */
    subscribe(cb: (online: boolean) => void): () => void {
      subscribers.add(cb);
      return () => subscribers.delete(cb);
    },
  };
}

export const onlineStatus: OnlineStatusStore = createStore();
