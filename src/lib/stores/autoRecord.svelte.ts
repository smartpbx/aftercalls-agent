// #596 — runed store wrapping the auto-record settings bundle.
//
// One singleton; both the Settings page and the layout-level event
// listener share state through it. The Settings page reads `state` and
// calls `refresh()` / `setMaster()` / `toggleApp()` / `forget()` for
// edits; the layout's `observed-apps-updated` listener bumps
// `refresh()` so a freshly-observed app appears in the list without
// the user having to toggle a route.
//
// Network shape: `auto_record_settings_get` returns the full bundle in
// one round-trip. Two-call commands (set + re-get) are avoided in
// favour of optimistic local mutation — the IPC surface only persists,
// it doesn't echo.

import {
  autoRecord,
  type AutoRecordApp,
  type AutoRecordSettings,
} from "$lib/api";

type State = {
  loaded: boolean;
  /** True while the initial load (or an explicit `refresh()`) is in
   *  flight. The Settings page uses this to render a subtle skeleton
   *  rather than the empty-state copy. */
  loading: boolean;
  /** Last error (e.g. sqlite open failure on a hardened machine).
   *  Settings renders an inline retry message when set. */
  error: string | null;
  data: AutoRecordSettings | null;
};

function createStore() {
  const state = $state<State>({
    loaded: false,
    loading: false,
    error: null,
    data: null,
  });

  async function refresh(): Promise<void> {
    state.loading = true;
    state.error = null;
    try {
      state.data = await autoRecord.get();
      state.loaded = true;
    } catch (e: unknown) {
      state.error = String((e as { message?: string })?.message ?? e);
    } finally {
      state.loading = false;
    }
  }

  async function setMaster(start: boolean, stop: boolean): Promise<void> {
    // Optimistic local mutation; if the persist fails, refresh() rolls
    // us back to whatever the disk thinks is current.
    if (state.data) {
      state.data = { ...state.data, start_enabled: start, stop_enabled: stop };
    }
    try {
      await autoRecord.setMaster(start, stop);
    } catch (e: unknown) {
      state.error = String((e as { message?: string })?.message ?? e);
      await refresh();
    }
  }

  async function toggleApp(bundleId: string, enabled: boolean): Promise<void> {
    if (state.data) {
      state.data = {
        ...state.data,
        apps: state.data.apps.map((a) =>
          a.bundle_id === bundleId ? { ...a, enabled } : a,
        ),
      };
    }
    try {
      await autoRecord.toggleApp(bundleId, enabled);
    } catch (e: unknown) {
      state.error = String((e as { message?: string })?.message ?? e);
      await refresh();
    }
  }

  async function forget(bundleId: string): Promise<void> {
    if (state.data) {
      state.data = {
        ...state.data,
        apps: state.data.apps.filter((a) => a.bundle_id !== bundleId),
      };
    }
    try {
      await autoRecord.forgetApp(bundleId);
    } catch (e: unknown) {
      state.error = String((e as { message?: string })?.message ?? e);
      await refresh();
    }
  }

  return {
    get loaded(): boolean {
      return state.loaded;
    },
    get loading(): boolean {
      return state.loading;
    },
    get error(): string | null {
      return state.error;
    },
    get startEnabled(): boolean {
      return state.data?.start_enabled ?? false;
    },
    get stopEnabled(): boolean {
      return state.data?.stop_enabled ?? false;
    },
    get platformSupported(): boolean {
      return state.data?.platform_supported ?? false;
    },
    get apps(): AutoRecordApp[] {
      return state.data?.apps ?? [];
    },
    refresh,
    setMaster,
    toggleApp,
    forget,
  };
}

/** Singleton store. Settings page mounts it on its own onMount;
 *  +layout.svelte refreshes it on `observed-apps-updated`. */
export const autoRecordStore = createStore();
