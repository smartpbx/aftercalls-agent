// Local save-queue (#107) — agent surface. Persists to localStorage
// (we deliberately don't add the Tauri SQL plugin in this issue to
// avoid scope creep — see issue body's "stop and escalate" rules);
// localStorage in the agent webview is host-keyed to `tauri://localhost`
// so it survives webview reloads and process restarts the same way the
// auth.json file does for native state.
//
// Surface-specific sibling of `portal/src/lib/stores/saveQueue.svelte.ts`:
// the contract is identical, but `replayOne` rides Tauri `invoke` and
// the "stale conflict" check inspects the `PortalError` shape returned
// from agent-src-tauri commands instead of the portal's `parse()`-thrown
// HTTP error.

import { invoke } from "@tauri-apps/api/core";
import { isPortalError, type PortalError } from "$lib/portalError";
import { toast } from "@aftercalls/shared/stores/toast.svelte";
import { onlineStatus } from "$lib/stores/onlineStatus.svelte";
import type {
  QueuedSaveEntry,
  SaveOp,
  SaveQueueStore,
} from "@aftercalls/shared/contracts";

const STORAGE_KEY = "aftercalls:save-queue";

export type { SaveOp } from "@aftercalls/shared/contracts";
type QueuedEntry = QueuedSaveEntry;

function readPersisted(): QueuedEntry[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (e): e is QueuedEntry =>
        !!e && typeof e === "object" && typeof e.id === "string" &&
        typeof e.kind === "string",
    );
  } catch {
    return [];
  }
}

function writePersisted(items: QueuedEntry[]): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(items));
  } catch {
    // Quota exceeded — best-effort.
  }
}

/** A `PortalError` that means "your view is stale, replay won't help".
 *  `bad_request` covers backend validation rejects (missing FK, etc.),
 *  `not_found` covers a row that was deleted while offline, and we
 *  treat any future `kind: "server"` with a 4xx-shaped status the same
 *  way for symmetry. `unauthorized` / `forbidden` are NOT stale-conflicts:
 *  the auth-refresh path inside the Rust client handles 401s, and a
 *  forbidden write means the user lost permission while offline — the
 *  refresh toast is still the right copy in both cases, but they're
 *  intentionally rare so we route them through the same "drop op +
 *  notify" arm as the explicit conflicts. */
function isStaleConflictPortal(err: PortalError): boolean {
  switch (err.kind) {
    case "bad_request":
    case "not_found":
    case "unauthorized":
    case "forbidden":
      return true;
    case "server":
      return err.status >= 400 && err.status < 500;
    default:
      return false;
  }
}

function isTransientNetwork(err: unknown): boolean {
  if (isPortalError(err)) {
    if (err.kind === "network") return true;
    if (err.kind === "server" && err.status >= 500) return true;
    if (err.kind === "cooldown") return true;
    return false;
  }
  // A non-PortalError reject from `invoke` is usually a Tauri
  // serialisation hiccup or a panic — surface as transient so we
  // don't aggressively drop the user's work.
  return true;
}

function createStore() {
  const state = $state<{ items: QueuedEntry[]; replaying: boolean }>({
    items: readPersisted(),
    replaying: false,
  });

  function dedupeKey(op: SaveOp): string {
    switch (op.kind) {
      case "notes":
        return `notes:${op.callId}`;
      case "patchActionItem":
        return `patch:${op.callId}:${op.itemId}`;
    }
  }

  function persist() {
    writePersisted(state.items);
  }

  function enqueue(op: SaveOp): void {
    const key = dedupeKey(op);
    const existing = state.items.findIndex((e) => dedupeKey(e) === key);
    if (existing >= 0) {
      const prev = state.items[existing];
      let merged: QueuedEntry;
      if (op.kind === "patchActionItem" && prev.kind === "patchActionItem") {
        merged = {
          ...prev,
          body: { ...prev.body, ...op.body },
          at: op.at,
        };
      } else {
        merged = { ...op, id: prev.id };
      }
      state.items = [
        ...state.items.slice(0, existing),
        merged,
        ...state.items.slice(existing + 1),
      ];
    } else {
      state.items = [
        ...state.items,
        { ...op, id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}` },
      ];
    }
    persist();
  }

  async function replayOne(entry: QueuedEntry): Promise<void> {
    switch (entry.kind) {
      case "notes":
        await invoke("update_call_notes", {
          callId: entry.callId,
          notes: entry.notes,
        });
        return;
      case "patchActionItem":
        await invoke("patch_action_item", {
          callId: entry.callId,
          itemId: entry.itemId,
          body: entry.body,
        });
        return;
    }
  }

  async function flush(): Promise<void> {
    if (state.replaying) return;
    if (state.items.length === 0) return;
    state.replaying = true;
    try {
      while (state.items.length > 0) {
        const next = state.items[0];
        try {
          await replayOne(next);
          state.items = state.items.slice(1);
          persist();
          toast.success("Saved");
        } catch (err: unknown) {
          if (isPortalError(err) && isStaleConflictPortal(err)) {
            // 4xx-equivalent — drop the op, alert the user that their
            // local view diverged from the server's truth.
            state.items = state.items.slice(1);
            persist();
            toast.error("Couldn't save — please refresh");
            continue;
          }
          if (isTransientNetwork(err)) {
            const ok = await onlineStatus.probeNow();
            if (!ok) return;
            // Backend said healthy but the save still failed — back
            // off; the next online subscribe cycle will retry.
            return;
          }
          // Unknown shape — treat as stale-conflict to avoid an
          // infinite loop. The user will see the refresh toast.
          state.items = state.items.slice(1);
          persist();
          toast.error("Couldn't save — please refresh");
        }
      }
    } finally {
      state.replaying = false;
    }
  }

  if (typeof window !== "undefined") {
    onlineStatus.subscribe((online) => {
      if (online) void flush();
    });
  }

  return {
    get items(): QueuedEntry[] {
      return state.items;
    },
    enqueue,
    flush,
  };
}

export const saveQueue: SaveQueueStore = createStore();

export function isTransientNetworkError(err: unknown): boolean {
  return isTransientNetwork(err);
}
