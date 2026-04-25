// Cross-tab Zoho-connection store. (#218)
//
// Mirror of portal/src/lib/stores/zoho.svelte.ts — keep the two in
// sync. The only divergence is the transport: the agent rides
// through a Tauri command (`zoho_status`) instead of `fetch`.
//
// `/calls/[id]` used to fetch the Zoho status once on mount and
// cache it in a local `let`. When an admin OAuth-connected Zoho in
// another tab/window, an already-open call-detail tab kept showing
// the pre-connect state ("Send to CRM" hidden) until a full reload.
// Confirmed on the agent during v0.7.1 field testing.
//
// This module exposes a single source of truth via a Svelte 5 rune.
// Pages read `zohoStore.connected` reactively; the mount probe still
// fires `refresh()` to seed it. We bump a `localStorage` sentinel
// after every refresh and listen for the `storage` event so other
// surfaces (a second window or a portal tab open in the system
// browser at the same time) pick up the change automatically.

import { invoke } from "@tauri-apps/api/core";

/** localStorage key used as the cross-tab sentinel. The value is a
 *  millisecond timestamp; we only care that it changes. */
const SENTINEL_KEY = "zoho-connected-at";

type ZohoState = {
  connected: boolean;
  connectedByDisplayName: string | null;
  /** True once the first refresh resolves (success or failure) so
   *  callers can distinguish "unknown yet" from "known false". */
  loaded: boolean;
};

function createStore() {
  const state = $state<ZohoState>({
    connected: false,
    connectedByDisplayName: null,
    loaded: false,
  });

  let inFlight: Promise<void> | null = null;
  let storageListenerAttached = false;

  /** Pull the latest org Zoho-connection status via Tauri. Failures
   *  (env-disabled, network) leave the store reporting
   *  `connected: false` rather than throwing — callers don't need to
   *  handle errors. Concurrent callers share the same in-flight
   *  request to avoid stampedes from a remount + storage event
   *  arriving back-to-back. */
  async function refresh(): Promise<void> {
    if (inFlight) return inFlight;
    inFlight = (async () => {
      try {
        const status = (await invoke("zoho_status")) as {
          connected: boolean;
          connected_by?: { display_name: string };
        };
        state.connected = !!status.connected;
        state.connectedByDisplayName =
          status.connected_by?.display_name ?? null;
      } catch {
        state.connected = false;
        state.connectedByDisplayName = null;
      } finally {
        state.loaded = true;
        // Cross-tab sentinel — flip to a fresh timestamp so other
        // tabs/windows' `storage` listeners re-fetch. Wrapped in a
        // try/catch because storage can be unavailable in odd
        // sandboxes.
        if (typeof window !== "undefined") {
          try {
            window.localStorage.setItem(
              SENTINEL_KEY,
              String(Date.now()),
            );
          } catch {}
        }
      }
    })();
    try {
      await inFlight;
    } finally {
      inFlight = null;
    }
  }

  /** Wire the cross-tab listener. Idempotent — safe to call from
   *  every page mount. Only attaches in the browser (SSR no-op). */
  function ensureCrossTabListener(): void {
    if (storageListenerAttached) return;
    if (typeof window === "undefined") return;
    window.addEventListener("storage", (e) => {
      // `e.key === null` happens on a `localStorage.clear()` from
      // another tab — refetch defensively in that case too.
      if (e.key !== null && e.key !== SENTINEL_KEY) return;
      refresh();
    });
    storageListenerAttached = true;
  }

  return {
    get connected() {
      return state.connected;
    },
    get connectedByDisplayName() {
      return state.connectedByDisplayName;
    },
    get loaded() {
      return state.loaded;
    },
    refresh,
    ensureCrossTabListener,
  };
}

export const zohoStore = createStore();
