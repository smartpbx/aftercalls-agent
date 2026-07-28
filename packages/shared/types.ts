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
  live_transcript?: boolean;
  /** #653 — org-level in-call co-pilot. Gates the Record page's
   *  `CoPilotPanel` (CRM context lane + contact picker) in Call mode.
   *  Optional so an older backend / auth.json decodes cleanly as OFF. */
  copilot?: boolean;
  /** #659 (Phase L) — live far-side speaker separation. When on, the
   *  live system/far stream renders anonymous "Speaker A/B/…" labels
   *  instead of one merged "Them" (mic stays "You"). Raw staff toggle,
   *  default-off. Optional so an older backend / auth.json decodes as
   *  OFF; the transcript renders any speaker string regardless. */
  live_speaker_separation?: boolean;
  /** #302 (Slice A) — screen capture during calls. When on, the agent
   *  may capture the screen as VIDEO alongside the call audio and the
   *  call-detail page surfaces a `<video>` player. Raw staff toggle,
   *  default-off. Optional so an older backend / auth.json decodes as
   *  OFF. The video is stored/displayed only — never fed to the AI. */
  screen_capture?: boolean;
};

/** #302 (Slice A) — lifecycle of the per-call screen VIDEO asset.
 *  `recording` → live capture in flight; `uploading` → resumable upload
 *  running; `ready` → finalized + streamable; `failed` → aborted /
 *  never finished; `expired` → swept past the org's retention window
 *  (object deleted, row kept). Mirrors the backend CHECK constraint. */
export type ScreenRecordingStatus =
  | "recording"
  | "uploading"
  | "ready"
  | "failed"
  | "expired";

/** #302 (Slice C) — screen-recording metadata for a call, returned by
 *  `GET /v1/calls/{id}/screen` (agent: `get_screen_recording` Tauri
 *  command; portal: `api.calls.screenRecording`). `url` is a short-lived
 *  presigned GET, present only when `status === "ready"`. A 404 (org flag
 *  off OR no row) decodes to `null` at the call site — the player renders
 *  nothing. `start_offset_ms` is the whole feature: how far into the call
 *  audio the video began, so the follower can align frames to the audio
 *  master clock. */
export type ScreenRecording = {
  recording_id: string;
  status: ScreenRecordingStatus;
  start_offset_ms: number;
  duration_ms?: number;
  width?: number;
  height?: number;
  fps?: number;
  codec?: string;
  byte_size?: number;
  expires_at?: string;
  /** Presigned GET (Spaces serves Range natively). Only set when
   *  `status === "ready"`. Both surfaces bind `<video src>` to it —
   *  cross-origin media playback needs no auth header (like the audio
   *  `<audio src>` path). */
  url?: string;
};

/** #659 P5a — in-call co-pilot persona. `"sales"` grounds the Contact lane
 *  on open Deals (today's behaviour); `"support"` grounds it on the
 *  contact's open Cases. Seeded from the org's `copilot_default_mode` and
 *  overridable per-call via the CoPilotPanel header toggle. */
export type CopilotMode = "sales" | "support";

/** #653 — in-call co-pilot CRM envelope, returned by the
 *  `live_crm_context` Tauri command (backend `GET /v1/live/crm-context`).
 *  Carries both `deals` (Sales mode) and `cases` (Support mode, #659 P5a) —
 *  the mode toggle swaps which section the Contact lane renders, purely
 *  client-side (`quotes` is still a later phase). The lane degrades
 *  per-section: `deals.status`, `cases.status`, and `zoho` move
 *  independently so the contact card can render even when a fetch fails,
 *  and a disconnected Zoho returns 200 with `zoho:"not_connected"` rather
 *  than erroring the panel. */
export type CrmContextDeal = {
  id: string;
  name: string;
  stage?: string;
  /** Zoho forwards the raw stored value untouched (currency-formatted or
   *  plain), so this stays a string, not a number. */
  amount?: string;
  close_date?: string;
  /** Deep link to the deal on the org's OWN Zoho host. */
  url: string;
};

/** #659 P5a — one open Case row, the Support-mode analogue of
 *  `CrmContextDeal`. `subject` may carry the customer's worded complaint
 *  (PII) — it is surfaced to the agent lane only and is NEVER written into
 *  `state.copilot.crm` on the backend (deal-facts-only grounding, extended
 *  to cases). Every field but `id`/`url` is optional (Zoho omits blanks). */
export type CrmContextCase = {
  id: string;
  subject?: string;
  case_number?: string;
  status?: string;
  priority?: string;
  created_time?: string;
  /** Deep link to the case on the org's OWN Zoho host. */
  url: string;
};

export type CrmContext = {
  contact: {
    id: string;
    name?: string;
    email?: string;
    phone?: string;
    account_name?: string;
  };
  resolved_via: string;
  confidence: string;
  deals: {
    status: "ok" | "empty" | "unavailable";
    items: CrmContextDeal[];
  };
  /** #659 P5a — the contact's open Cases (Support mode). Same per-section
   *  degrade posture as `deals`; `"unavailable"` covers the UNPROVEN Cases
   *  criteria shape being rejected — the lane degrades, the panel never
   *  errors. */
  cases: {
    status: "ok" | "empty" | "unavailable";
    items: CrmContextCase[];
  };
  zoho: "connected" | "not_connected";
  fetched_at: string;
  stale: boolean;
};

export type LiveSegment = {
  channel: "mic" | "system";
  speaker?: string;
  start_ms: number;
  end_ms: number;
  text: string;
  provisional: boolean;
};

/** #654 — live coaching snapshot pushed over the live WS as a
 *  `{"type":"coaching", …}` frame (backend `live_coach::CoachingUpdate`) and
 *  surfaced to the Coaching lane. Each frame is a FULL snapshot — the lane
 *  replaces its card list wholesale (`seq` monotonic per session). Deal-context
 *  aware but PII-free by construction: the backend feeds the model deal
 *  metadata only, never counterpart contact details. Card text is
 *  transcript-derived and rendered as plain text (never `{@html}`). */
export type CoachingSentimentLabel =
  | "positive"
  | "neutral"
  | "negative"
  | "mixed";

export type CoachingCardKind =
  | "question"
  | "talking_point"
  | "objection"
  | "next_action";

export type CoachingCardPriority = "high" | "normal";

/** #662 — the call's conversational posture, classified by the coach model
 *  from the transcript. Folds into the agent's CLIENT-SIDE auto-mode inference
 *  (weighed against open Deals/Cases counts). A PII-free constrained enum —
 *  never a provider string. */
export type CoachingPosture = "sales" | "support" | "mixed";

export type CoachingSentiment = {
  label: CoachingSentimentLabel;
  /** Short glanceable read; omitted by the backend when empty. */
  note?: string;
};

export type CoachingCard = {
  kind: CoachingCardKind;
  /** For `objection`, the concern the counterpart pushed back on. */
  title: string;
  /** Optional supporting line; for `objection`, the suggested response.
   *  Omitted when empty. */
  detail?: string;
  priority: CoachingCardPriority;
};

export type CoachingUpdate = {
  type: "coaching";
  /** Monotonic per session — drives the Coaching-tab fresh-update pulse. */
  seq: number;
  generated_at: string;
  based_on_turns: number;
  /** #662 — model-judged call posture; drives the panel's auto-mode inference.
   *  Optional so an older backend frame (pre-#662) still decodes cleanly —
   *  the panel treats a missing posture as "no signal yet". */
  posture?: CoachingPosture;
  sentiment: CoachingSentiment;
  cards: CoachingCard[];
};

/** #659 (P2) — one FAST-lane cue pushed over the live WS as a
 *  `{"type":"live_cue", …}` frame (backend `live_fast::LiveCue`) and surfaced
 *  in the IntelligenceLane ABOVE the reflective coaching. Deliberately NOT part
 *  of the wholesale-replace `coaching` snapshot: cues arrive one at a time,
 *  carry their own stable `id`, and battlecards auto-expire via `ttl_ms`
 *  client-side (the store prunes). One surviving kind (#662): `battlecard` —
 *  LLM, deal-grounded, time-critical (objection / pricing / competitor); TTL'd.
 *  The canned `risk` absence cues (pricing-not-mentioned / no-next-step) were
 *  retired — the discovery checklist now carries that coverage.
 *  `title` / `detail` are LLM-text → rendered as plain text, never `{@html}`.
 *  Old agents hit the `forward_incoming` default arm and ignore the unknown
 *  frame — additive + non-breaking. */
export type LiveCueCategory = "objection" | "pricing" | "competitor";

export type LiveCueKind = "battlecard";

export type LiveCue = {
  type: "live_cue";
  id: string;
  category: LiveCueCategory;
  kind: LiveCueKind;
  /** Short label / the concern being handled. */
  title: string;
  /** Ready-to-say line for the battlecard. */
  detail?: string;
  priority: "high" | "normal";
  /** Battlecards auto-expire after this many ms client-side. */
  ttl_ms?: number;
  generated_at: string;
};

/** #659 (P3) — auto-checking checklist / agenda-adherence snapshot pushed over
 *  the live WS as a `{"type":"checklist", …}` frame (backend
 *  `live_checklist::ChecklistSnapshot`) and surfaced in the IntelligenceLane
 *  ABOVE the fast cues + reflective coaching. Deliberately a SEPARATE frame from
 *  `coaching` (the wholesale-replace card snapshot) and from `live_cue`: agenda
 *  coverage is session-sticky (append-only — an item never un-ticks once
 *  covered) whereas cues are moment-specific. Each frame is a FULL snapshot —
 *  the lane replaces its item list wholesale (`seq` is a monotonic per-session
 *  ordinal; the agent replaces wholesale and does not currently consume it).
 *  PII-free by construction: matching uses the transcript
 *  + built-in template text only, never deal facts or counterpart PII. Every
 *  field is built-in template text or a stable id → rendered as plain text,
 *  never `{@html}`. Old agents hit the `forward_incoming` default arm and ignore
 *  the unknown frame — additive + non-breaking.
 *
 *  `mode` reflects the call's active persona (`state.copilot.mode`, P5a):
 *  `"sales"` runs the discovery agenda, `"support"` the compliance checklist
 *  (P5c). */
export type ChecklistItemState =
  | "pending"
  /** #659 P5c — the model judged this item addressed, but the template is
   *  `confirm_required` (compliance), so it needs a human tap in the agent to
   *  become `covered`. Only confirm-required templates emit `"likely"`; the
   *  agent overlays the user's confirmations client-side. */
  | "likely"
  | "covered";

export type ChecklistItem = {
  id: string;
  /** Built-in template label (never transcript-derived). */
  label: string;
  state: ChecklistItemState;
};

export type ChecklistSnapshot = {
  type: "checklist";
  /** Monotonic per-session ordinal for the wholesale-replace snapshot; the agent
   *  replaces its item list on every frame and does not currently consume it. */
  seq: number;
  template_id: string;
  template_label: string;
  mode: "sales" | "support";
  /** #659 P5c — when true (compliance), the model's coverage is a SUGGESTION:
   *  items arrive as `"likely"` and the lane renders a confirm affordance,
   *  overlaying the human's confirmations to reach `"covered"`. When false
   *  (discovery) items auto-tick. Guards a higher-stakes false "disclosure
   *  given" tick. Additive; absent on frames from older backends → treated as
   *  false (auto-tick). */
  confirm_required?: boolean;
  items: ChecklistItem[];
  /** Count of items DONE on the wire (state `"covered"`). For a confirm-required
   *  template this is 0 (its matched items are `"likely"`); the lane recomputes
   *  progress including its local confirmations overlay. */
  covered_count: number;
  total_count: number;
};

/** #660 co-pilot P1 — on-demand ask-chip preset. One tap generates an
 *  inline answer server-side over the live-transcript window. Wire values
 *  match the backend `AskChip` (`POST /v1/live/ask`); vendor-opaque labels
 *  live in the UI layer. */
export type AskChip =
  | "catch_me_up"
  | "summarize"
  | "what_did_they_ask"
  | "action_items";

/** #660 — response of the `live_ask` Tauri command (backend
 *  `POST /v1/live/ask`). `answer` is plain text (LLM-derived → rendered
 *  as text, never `{@html}`); the endpoint degrades calm-200 so a
 *  successful call always carries a renderable line even when the window
 *  is empty or generation was unavailable. `based_on_turns` is the count
 *  of finals the answer was generated over (0 on the empty degrade). */
export type AskAnswer = {
  answer: string;
  based_on_turns: number;
  /** #660 P1 — honest "nothing yet" signal. `true` when the answer is a VALID
   *  empty result (empty transcript, or the model judged nothing of the
   *  requested kind has surfaced yet) so the agent renders a calm "nothing yet"
   *  state distinct from a transport/gate error line. Optional so an older
   *  backend (pre-P1) decodes cleanly → treated as `false` (a normal answer). */
  empty?: boolean;
};

/** #659 P5b — one cited source on a knowledge answer. `title` is the KB
 *  snippet's admin-authored title, rendered as a plain-text citation chip
 *  (never `{@html}`); `id` is the stable snippet uuid. */
export type KnowledgeSource = {
  id: string;
  title: string;
};

/** #659 P5b — response of the `live_knowledge` Tauri command (backend
 *  `POST /v1/live/knowledge`). Support mode: a grounded, CITED answer over the
 *  org's own knowledge base. `answer` is plain text (LLM/snippet-derived →
 *  rendered as text, never `{@html}`); the endpoint degrades calm-200 so a
 *  successful call always carries a renderable line, even on a no-match /
 *  unavailable degrade. `sources` is empty on a no-match / degrade line and
 *  carries ≥1 citation on a grounded answer (grounding-first — a match always
 *  cites its snippet). */
export type KnowledgeAnswer = {
  answer: string;
  sources: KnowledgeSource[];
};

/** #660 — one live highlight (star) on a transcript turn, keyed by its
 *  natural wire key `channel + start_ms` (the agent never sees the backend
 *  `seq`). `starred` reflects the applied intent echoed by
 *  `POST /v1/live/highlight`; `false` means the turn was un-starred. */
export type LiveHighlight = {
  channel: "mic" | "system";
  start_ms: number;
  end_ms: number;
  speaker?: string;
  text: string;
  starred: boolean;
};

/** #646 (Phase 2) — per-speaker identity assignment for the live co-pilot.
 *  Maps one diarized speaker (natural key `channel + speaker_label`) to a real
 *  identity: a Zoho contact, an internal teammate, or a free-form ("adhoc")
 *  name. Drives the live transcript re-label (`LiveTranscriptLane.labelFor`) and
 *  the reference-rail speaker roster (`CrmContextLane`). One entry in the
 *  `POST /v1/live/speaker-identity` response (Tauri: `live_speaker_identity`).
 *  `speaker_label` is the CANONICAL diarization label the backend emits —
 *  `"Them"` for the merged far side when speaker separation is OFF, `"Speaker A"`
 *  / `"Speaker B"` when ON (the endpoint rejects an empty `speaker_label`).
 *  `is_primary` marks the ONE zoho_contact that grounds the deal/case card and
 *  the recording's `contact_hint`. `contact_id` / `user_id` are set per `kind`;
 *  every optional field decodes cleanly from an older/absent backend. */
export type SpeakerIdentityKind = "zoho_contact" | "internal_user" | "adhoc";

export type SpeakerIdentity = {
  channel: "mic" | "system";
  speaker_label: string;
  kind: SpeakerIdentityKind;
  display_name: string;
  /** Set when `kind === "zoho_contact"` — the Zoho Contacts record id. */
  contact_id?: string;
  /** Set when `kind === "internal_user"` — the teammate's user id. */
  user_id?: string;
  /** The single primary zoho_contact; grounds the deal/case card. */
  is_primary?: boolean;
  assigned_at?: string;
};

/** #646 (Phase 2) — arguments for the `live_speaker_identity` Tauri command
 *  (backend `POST /v1/live/speaker-identity`). camelCase to match the invoke
 *  arg contract. `clear: true` removes any identity on `channel + speakerLabel`
 *  (the remaining fields are then ignored). `isPrimary` designates the grounding
 *  zoho_contact (backend clears the flag off the others). */
export type SpeakerIdentityAssignArgs = {
  sessionUuid: string;
  channel: "mic" | "system";
  speakerLabel: string;
  kind: SpeakerIdentityKind;
  displayName: string;
  contactId?: string;
  userId?: string;
  isPrimary?: boolean;
  clear?: boolean;
};

/** #646 (Phase 2) — response of `live_speaker_identity`: the FULL reconciled
 *  identity set for the session (the store replaces its map wholesale, so a
 *  server-side primary re-shuffle stays consistent client-side). */
export type SpeakerIdentitiesResponse = {
  identities: SpeakerIdentity[];
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
  /** #659 P5a — the org's default in-call co-pilot persona. The agent
   *  seeds the CoPilotPanel mode toggle from this on mount. Optional so an
   *  older backend / auth.json decodes cleanly (missing → treated as
   *  "sales" by the panel). Only meaningful when `features.copilot` is on. */
  copilot_default_mode?: CopilotMode;
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
  // #646 Layer B — per-track upload quality summary shipped by the
  // backend in #645 Phase 1. `"full"` means every non-NULL audio key
  // is reachable on object storage; `"degraded"` means at least one
  // track was unavailable during upload. The call-detail page reads
  // this to render the soft `.track-quality-note` chip. Optional on
  // the wire so older backend builds (or proxies that strip unknown
  // fields) don't blow up the type — UI treats `undefined` as
  // `"full"` (no chip).
  track_quality?: "full" | "degraded";
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
