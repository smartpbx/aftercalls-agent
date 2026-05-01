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
