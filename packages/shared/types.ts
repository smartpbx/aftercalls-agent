export type PendingTos = {
  id: string;
  kind: "terms" | "privacy" | string;
  slug: string;
  effective_at: string;
};

export type Features = {
  zoho: boolean;
  zoho_meeting?: boolean;
  zoho_cliq?: boolean;
  smartpbx?: boolean;
  sso?: boolean;
};

export type Me = {
  id?: string;
  user_id?: string;
  email: string;
  first_name?: string;
  last_name?: string;
  display_name: string;
  role: string;
  is_platform_staff?: boolean;
  org_id?: string;
  org_slug?: string;
  org_display_name: string;
  pending_tos?: PendingTos[];
  onboarded_at?: string | null;
  features?: Partial<Features>;
  pending_email?: string | null;
  org_has_agent_recording?: boolean;
  /** #634 — caller's count of unread complete calls. Riding on the
   *  existing `/auth/me` payload so the layout's poll doesn't need a
   *  second endpoint. Optional so older backends decode cleanly:
   *  missing → treated as zero, no chip rendered. */
  unread_calls?: number;
};

export type TagKind = "client" | "purpose" | "topic" | "custom";
export type Tag = {
  kind: TagKind;
  value: string;
};

export type TagSuggestion = {
  kind: TagKind;
  value: string;
  count: number;
};

export type CallListItem = {
  id: string;
  session_id: string;
  recorded_at: string;
  duration_ms: number;
  title: string | null;
  matched_client: string | null;
  status: string;
  source_app: string | null;
  source_kind: string | null;
  ingest_source?: string;
  tags?: Tag[];
  notes?: string;
  user_id?: string;
  user_display_name?: string;
  pinned_at?: string | null;
  snoozed_until?: string | null;
  deleted_at?: string;
  /** #634 — per-user read state for the calling user. The backend's
   *  `list_calls` row gains an `EXISTS (...)` subquery against
   *  `call_reads`. Optional so an older backend (or the
   *  /calls/trashed shape) decodes cleanly; missing → treat as read
   *  (no indicator), since we'd rather under-surface than flash a
   *  false unread. */
  is_read?: boolean;
};

export type Utterance = {
  idx: number;
  speaker: string;
  original_speaker: string;
  start_ms: number;
  end_ms: number;
  text: string;
  speaker_user_id: string | null;
};

export type ActionItemDueKind = "none" | "asap" | "dated";

export type ActionItemRow = {
  id: string;
  call_id: string;
  description: string;
  assignee_user_id: string | null;
  status: "open" | "done";
  completed_at: string | null;
  completed_by_user_id: string | null;
  source: "llm" | "manual";
  created_at: string;
  order_index: number;
  due_kind: ActionItemDueKind;
  due_at: string | null;
};

export type ActionItem = ActionItemRow;

export type Call = CallListItem & {
  summary_text: string | null;
  action_items: ActionItemRow[];
  participants: string[];
  note_markdown_path: string | null;
  utterances: Utterance[];
  tags: Tag[];
  notes: string;
};

export type Highlight = {
  id: string;
  call_id: string;
  start_ms: number;
  end_ms: number;
  kind: string;
  label: string | null;
  note: string | null;
  source: string;
  created_at: string;
};

export type CallsListResponse = {
  calls: CallListItem[];
  next_cursor: string | null;
};

export type OrgMember = {
  id: string;
  first_name: string;
  last_name: string;
  display_name: string;
  email: string;
};

export type CallShareIncludedSections = {
  manual_notes: boolean;
  summary: boolean;
  action_items: boolean;
  transcript: boolean;
  audio: boolean;
  allow_download: boolean;
};

export type CallShareCreated = {
  id: string;
  token: string;
  url: string;
  expires_at: string | null;
  created_at: string;
  included_sections: CallShareIncludedSections;
};

export type CallShareSummary = {
  id: string;
  call_id: string;
  url: null;
  created_by: string | null;
  created_at: string;
  expires_at: string | null;
  revoked_at: string | null;
  view_count: number;
  last_accessed_at?: string | null;
  status: "active" | "expired" | "revoked";
  included_sections: CallShareIncludedSections;
};

export type ActionsStatusFilter = "open" | "done" | "all";
export type ActionsDueFilter =
  | "all"
  | "overdue"
  | "today"
  | "week"
  | "none";

export type MeActionItem = ActionItemRow & {
  created_by_user_id: string | null;
  call_title: string | null;
  call_recorded_at: string;
};

export type MeActionItemsResponse = {
  items: MeActionItem[];
  next_cursor: string | null;
  total_open: number;
  total_done: number;
  total_all: number;
};

export type AcceptedTos = {
  version_id: string;
  kind: "terms" | "privacy";
  slug: string;
  effective_at: string;
  accepted_at: string;
};

export type MyAccessLogRow = {
  id: string;
  call_id: string;
  call_title: string | null;
  viewer_user_id: string;
  viewer_display_name: string;
  access_kind: "view" | "audio";
  accessed_at: string;
};

export type MyAccessLogPage = {
  rows: MyAccessLogRow[];
  next_cursor: string | null;
};

export type MyPrivacyBundle = {
  joined_at: string;
  tos_acceptances: AcceptedTos[];
  calls_count: number;
  access_log: MyAccessLogPage;
};

export type DataExportStatus =
  | "pending"
  | "running"
  | "ready"
  | "failed"
  | "expired";

export type DataExportRow = {
  id: string;
  status: DataExportStatus;
  requested_at: string;
  expires_at: string | null;
  finished_at: string | null;
  bytes: number | null;
  call_count: number | null;
  audio_count: number | null;
  error_message: string | null;
  progress_pct: number | null;
  download_url?: string;
};

export type DataExportListResponse = {
  exports: DataExportRow[];
};

export type DataExportCreateResponse = {
  id: string;
  status: DataExportStatus;
};

export type ImportCandidate = {
  id: string;
  ingest_source: "smartpbx" | "zoho_meeting";
  source_external_id: string;
  discovered_at: string;
  dismissed_at: string | null;
  imported_call_id: string | null;
  metadata: Record<string, unknown>;
};

export type ImportCandidatesResponse = {
  items: ImportCandidate[];
  next_cursor: string | null;
};

export type ImportCandidatePromoteResponse = {
  candidate_id: string;
  call_id: string;
  was_new: boolean;
};
