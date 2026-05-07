// Thin Tauri-invoke wrappers for the agent's `/settings/privacy`
// surface (#592). The portal carries a much larger `api.ts` that
// fronts every backend route — the agent talks to the backend
// through Rust-side `#[tauri::command]` shims (see `agent/src-tauri/
// src/portal.rs`) rather than direct `fetch`, so this module exists
// only to give the privacy page a typed surface that mirrors the
// portal's `myPrivacy` and `dataExports` clients byte-for-byte.
//
// When the umbrella `packages/shared` lift (#355 / #446) lands, the
// types below should re-export from there and this module becomes a
// thin re-binding to invoke names. Until then, mirror types here so
// the privacy page can `import * as api from "$lib/api"` exactly the
// way the portal page does. Wire shapes (field names, optionality,
// status enum values) match the portal's `api.ts` — keep them in
// sync.

import { invoke } from "@tauri-apps/api/core";
import type {
  AcceptedTos,
  DataExportCreateResponse,
  DataExportListResponse,
  DataExportRow,
  ImportCandidate,
  ImportCandidatePromoteResponse,
  ImportCandidatesResponse,
  MyAccessLogPage,
  MyAccessLogRow,
  MyPrivacyBundle,
} from "@aftercalls/shared/types";

export type {
  AcceptedTos,
  DataExportCreateResponse,
  DataExportListResponse,
  DataExportRow,
  DataExportStatus,
  ImportCandidate,
  ImportCandidatePromoteResponse,
  ImportCandidatesResponse,
  MyAccessLogPage,
  MyAccessLogRow,
  MyPrivacyBundle,
} from "@aftercalls/shared/types";

// ── /settings/privacy bundle (#514, mirrored for #592) ──────────────

export const myPrivacy = {
  bundle: (): Promise<MyPrivacyBundle> =>
    invoke<MyPrivacyBundle>("me_privacy_bundle"),
  accessLog: (
    cursor: string | null = null,
    limit = 25,
  ): Promise<MyAccessLogPage> =>
    invoke<MyAccessLogPage>("me_privacy_access_log", {
      cursor: cursor ?? undefined,
      limit,
    }),
};

// ── Data export (#506, mirrored for #592) ───────────────────────────

export const dataExports = {
  /** Request a new export. 202 on success; the backend's 400 cooldown
   *  response carries `retry_after_seconds=N` in the message body so
   *  callers can surface a precise hint. */
  request: (): Promise<DataExportCreateResponse> =>
    invoke<DataExportCreateResponse>("data_export_request"),
  /** Newest-first list of the caller's exports. No URLs — call
   *  `getStatus` for the download URL of a specific row. */
  list: (): Promise<DataExportListResponse> =>
    invoke<DataExportListResponse>("data_export_list"),
  /** Single row + a fresh `download_url` when the row is `ready` and
   *  within its retention window. */
  getStatus: (id: string): Promise<DataExportRow> =>
    invoke<DataExportRow>("data_export_get_status", { id }),
};

// ── #595 — per-user import-candidate flow ───────────────────────────
//
// Mirror of the portal's `importCandidates` client (see
// `portal/src/lib/api.ts`). The agent's `/calls` page lists candidates
// alongside real call rows; Import promotes a candidate (server-side:
// ingest_recording_from_url + stamp imported_call_id) and Dismiss
// soft-deletes it. The Tauri shims live in
// `agent/src-tauri/src/portal.rs` (`import_candidates_*`) and ride the
// same auth-aware header dance as the rest of this surface — kind-based
// PortalError on non-2xx so the page can surface a precise toast.

export const importCandidates = {
  /** GET /v1/import-candidates — caller's own open candidates (not
   *  org-wide). `source` narrows by ingest_source; omit for both.
   *  `includeDismissed` flips the default filter (off = open only). */
  list: (opts?: {
    source?: "smartpbx" | "zoho_meeting";
    includeDismissed?: boolean;
  }): Promise<ImportCandidatesResponse> =>
    invoke<ImportCandidatesResponse>("import_candidates_list", {
      source: opts?.source ?? null,
      includeDismissed: opts?.includeDismissed ?? false,
    }),
  /** POST /v1/import-candidates/{id}/import — promote to a real call.
   *  Server idempotent: a second click while the first is mid-flight
   *  returns `was_new=false` referencing the same call_id. */
  import: (id: string): Promise<ImportCandidatePromoteResponse> =>
    invoke<ImportCandidatePromoteResponse>("import_candidate_import", { id }),
  /** POST /v1/import-candidates/{id}/dismiss — soft-delete. Idempotent;
   *  cross-org / unknown id → 404 surfaces as a PortalError. */
  dismiss: (id: string): Promise<void> =>
    invoke<void>("import_candidate_dismiss", { id }),
};

// ── #596 — auto-record per-app whitelist ────────────────────────────
//
// Local, agent-only surface (no portal mirror). Drives the Settings →
// Auto-record section + the 5s cancel toast in +layout.svelte. The
// `apps` table is privacy-sensitive — never sent upstream — so this
// client lives in the agent's api.ts only.

/** One row of the observed-apps list. Maps 1:1 to the AutoRecordAppRow
 *  serializer in the Rust IPC layer. */
export type AutoRecordApp = {
  bundle_id: string;
  friendly_name: string;
  /** RFC-3339 UTC. */
  first_seen_at: string;
  /** RFC-3339 UTC. */
  last_seen_at: string;
  enabled: boolean;
};

/** Bundle returned by `auto_record_settings_get` — drives a single
 *  paint of the Settings → Auto-record section. */
export type AutoRecordSettings = {
  start_enabled: boolean;
  stop_enabled: boolean;
  /** False on macOS (and any future OS without a working observer);
   *  the Settings UI shows the "App detection isn't supported on this
   *  OS yet" banner instead of the apps list. */
  platform_supported: boolean;
  apps: AutoRecordApp[];
};

export const autoRecord = {
  /** Fetch the full Settings bundle (master toggles + apps list). */
  get: (): Promise<AutoRecordSettings> =>
    invoke<AutoRecordSettings>("auto_record_settings_get"),
  /** Persist both master toggles. Both are passed every call so the
   *  Rust side can save once. */
  setMaster: (start: boolean, stop: boolean): Promise<void> =>
    invoke<void>("auto_record_settings_set_master", { start, stop }),
  /** Flip one row's `enabled` flag. Errors when the row was forgotten
   *  between paint and click. */
  toggleApp: (bundleId: string, enabled: boolean): Promise<void> =>
    invoke<void>("auto_record_settings_toggle_app", {
      bundleId,
      enabled,
    }),
  /** Drop a row. Reappears next time that app actually grabs the mic. */
  forgetApp: (bundleId: string): Promise<void> =>
    invoke<void>("auto_record_settings_forget_app", { bundleId }),
  /** Cancel an in-flight pending start (5s grace toast). The
   *  `pending_id` comes from the `auto-record-pending` event. */
  cancelPending: (pendingId: string): Promise<void> =>
    invoke<void>("confirm_auto_record_cancel", { pendingId }),
};

/** Payload the agent emits on the `auto-record-pending` event. */
export type AutoRecordPendingPayload = {
  pending_id: string;
  bundle_id: string;
  friendly_name: string;
};

/** Payload the agent emits on the `auto-record-fired` event. */
export type AutoRecordFiredPayload = {
  bundle_id: string;
};

/** Payload the agent emits on the `auto-record-cancelled` event. */
export type AutoRecordCancelledPayload = {
  bundle_id: string;
  reason: "user" | "app_stopped" | "error";
};

// ── #630 — per-user summary style ───────────────────────────────────
//
// Mirror of the portal's `mySummaryStyle` TS client. Same wire shape
// (the agent's IPC layer just routes the JSON through `/v1/me/summary-style`).
//   - `style` is the user's stored override (`null` = inherit).
//   - `effective_style` is the resolved style for the next summary.
//   - `org_default` lets the UI render the "Use team default (X)" hint
//     without a second round-trip.
// PATCH `style: null` reverts to inherit. Unknown values reject 400
// at the backend boundary, surfaced as a `PortalError`.

export type SummaryStyle = "narrative" | "hybrid" | "bulleted";

export type MySummaryStyle = {
  style: SummaryStyle | null;
  effective_style: SummaryStyle;
  org_default: SummaryStyle;
};

export const mySummaryStyle = {
  get: (): Promise<MySummaryStyle> =>
    invoke<MySummaryStyle>("me_summary_style_get"),
  patch: (style: SummaryStyle | null): Promise<MySummaryStyle> =>
    invoke<MySummaryStyle>("me_summary_style_patch", { style }),
};

// ── #634 — per-user unread call state ───────────────────────────────
//
// Mirror of the portal's `calls.markRead/markAllRead/markBulkRead` TS
// client. The wire shapes match what `portal/src/lib/api.ts` ships:
//   - POST /v1/calls/{id}/read         — idempotent upsert, 204 on success.
//   - POST /v1/calls/read-bulk         — body discriminated on
//                                        `{ all: true }` | `{ call_ids: [uuid] }`,
//                                        returns `{ marked: <count> }`.
// The bulk-mark endpoint is wrapped in two TS helpers (`markAllRead` +
// `markBulkRead`) so callers don't have to construct the discriminated
// body themselves.
//
// Unread-count surface rides on `/v1/auth/me`'s existing payload via
// `me.unread_calls`; `meUnreadCount()` is a thin reach-around that
// fetches the live value without disturbing the cached `auth.json`
// (`current_user` keeps reading from disk for cheap layout paints).

export type MarkBulkResponse = { marked: number };

export const calls = {
  /** POST /v1/calls/{id}/read — mark a single call read for the
   *  caller. Idempotent server-side. Throws PortalError on cross-org
   *  / unknown id (404). */
  markRead: (id: string): Promise<void> =>
    invoke<void>("mark_call_read", { id }),
  /** POST /v1/calls/read-bulk with `{ all: true }` — mark every
   *  unread complete call in the caller's org as read. Returns the
   *  number of newly-marked rows. */
  markAllRead: (): Promise<MarkBulkResponse> =>
    invoke<MarkBulkResponse>("mark_calls_read_bulk", { all: true }),
  /** POST /v1/calls/read-bulk with `{ call_ids: [...] }` — mark a
   *  specific set of calls read. Empty array short-circuits to
   *  `{ marked: 0 }` server-side. */
  markBulkRead: (ids: string[]): Promise<MarkBulkResponse> =>
    invoke<MarkBulkResponse>("mark_calls_read_bulk", { callIds: ids }),
};

/** Fetch the live unread-call count from `/v1/auth/me`. Returns 0 on
 *  any error so the layout poll never throws into a render path; the
 *  next successful tick refines the value. The agent's cached
 *  `current_user` shim reads `auth.json` (no live count), so this
 *  helper exists to surface the fresh `/auth/me.unread_calls` field
 *  without rewriting the auth bundle on every poll. */
export const meUnreadCount = (): Promise<number> =>
  invoke<number>("me_unread_count").catch(() => 0);
