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
  /** Zoho Desk — a SEPARATE opt-in connection from CRM (its own OAuth +
   *  org connection). Gates the whole in-call Tickets surface (the
   *  CrmContextLane Tickets section, link-a-ticket, the call-end "Add to
   *  ticket" prompt, and the after-call linked-ticket line). Raw staff
   *  toggle, default-off. Optional so an older backend / auth.json decodes
   *  cleanly as OFF. */
  zoho_desk?: boolean;
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

/** Zoho Desk — one open support Ticket row, surfaced BESIDE Deals/Cases when
 *  `features.zoho_desk` is on. `subject` may carry the customer's worded
 *  complaint (PII) — it renders in the lane only and is NEVER written into
 *  AI-grounding state (same posture as `CrmContextCase.subject`). Every field
 *  but `id`/`web_url` is optional (Desk omits blanks). `web_url` deep-links to
 *  the ticket on the org's OWN Desk host (a different host from the CRM
 *  `url`). */
export type CrmContextTicket = {
  id: string;
  ticket_number?: string;
  subject?: string;
  status?: string;
  priority?: string;
  created_time?: string;
  /** Deep link to the ticket on the org's OWN Zoho Desk host. */
  web_url: string;
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
  /** Zoho Desk — the contact's open Tickets, surfaced BESIDE Deals/Cases (NOT
   *  swapped by the Sales/Support mode toggle). Same per-section degrade
   *  posture as `deals`/`cases`; `"unavailable"` covers a Desk fetch that
   *  failed or a contact that couldn't be cross-resolved into Desk. Optional
   *  on the wire so a backend/proxy that predates the field decodes cleanly —
   *  the lane simply renders no Tickets section. */
  tickets?: {
    status: "ok" | "empty" | "unavailable";
    items: CrmContextTicket[];
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

/** Phase 4 (live↔after-call continuity) — one tracked question in the live
 *  co-pilot's session-sticky question ledger. Pushed over the live WS inside a
 *  `{"type":"questions", …}` frame (backend `live_questions`) and surfaced in
 *  the transcript drawer via `LiveQuestions`. Both sides count: a question the
 *  rep OR the counterpart asks that should get answered. `asker_side` is
 *  resolved at capture against the live identity map so no live↔final re-map is
 *  needed; `asker_display` is the already-resolved label to render ("You", or
 *  the counterpart's assigned name / diarization label). `text` + `answer_text`
 *  are transcript-derived → rendered as plain text, NEVER `{@html}`. An answered
 *  question is sticky (first answer wins — the never-un-answer analog of the
 *  checklist's never-un-tick). Mirrors `ChecklistItem`: built from a stable id +
 *  a full-snapshot render, the agent does not reconcile deltas client-side. */
export type LiveQuestion = {
  id: string;
  /** The question, as captured from the transcript. */
  text: string;
  /** Which party asked: the rep (`"you"`) or the counterpart (`"them"`). */
  asker_side: "you" | "them";
  /** The raw diarization/transcript label the model saw (debug/trace only). */
  asker_label?: string;
  /** The resolved display label the UI renders ("You" / assigned name / label). */
  asker_display: string;
  status: "open" | "answered";
  /** The captured answer once the transcript answers the question. */
  answer_text?: string;
  asked_at?: string;
  answered_at?: string;
};

/** Phase 4 — auto-extracted questions snapshot pushed over the live WS as a
 *  `{"type":"questions", …}` frame (backend `live_questions`), emitted to the
 *  agent as a `live-questions` Tauri event and surfaced in the transcript
 *  drawer. Each frame is a FULL snapshot — the lane replaces its list wholesale
 *  (`seq` is a monotonic per-session ordinal; the agent replaces wholesale and
 *  does not currently consume it). Session-sticky on the backend (a question
 *  never un-answers once answered), so a later snapshot only ever gains answers.
 *  `open_count` / `total_count` are computed backend-side (the drawer badge
 *  reads `open_count` directly). Old agents hit the `forward_incoming` default
 *  arm and ignore the unknown frame — additive + non-breaking. Mirrors
 *  `ChecklistSnapshot`. */
export type QuestionsSnapshot = {
  type: "questions";
  seq: number;
  questions: LiveQuestion[];
  open_count: number;
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

/** Phase 3 — a Zoho Deal linked to the live call so the finished call can be
 *  pushed to it at call-end (prompt or auto). Live capture writes it to
 *  `state.copilot.linked_deal` (scalar JSONB) via `POST /v1/live/linked-deal`;
 *  enrichment projects it into a durable `call_links` row. `module` is always
 *  `"Deals"` today; `record_id` / `record_name` name the Zoho record; the
 *  optional `stage` / `amount` / `url` mirror the deal's display fields so the
 *  after-call surfaces render without a re-fetch. Every optional field decodes
 *  cleanly from an older/absent backend. */
export type LinkedDeal = {
  module: string;
  record_id: string;
  record_name: string;
  stage?: string;
  amount?: string;
  url?: string;
};

/** Phase 3 — arguments for the `live_linked_deal` Tauri command (backend
 *  `POST /v1/live/linked-deal`). camelCase to match the invoke arg contract.
 *  `clear: true` removes any linked deal on the session (the remaining fields
 *  are then ignored). One linked deal at a time — a new link REPLACES the prior
 *  scalar server-side. */
export type LinkedDealAssignArgs = {
  sessionUuid: string;
  module: string;
  recordId: string;
  recordName: string;
  stage?: string;
  amount?: string;
  clear?: boolean;
};

/** Phase 3 — response of `live_linked_deal`: the reconciled linked deal for the
 *  session (or `null` after a clear). The store replaces its scalar wholesale so
 *  a server-side reconcile stays consistent client-side. */
export type LinkedDealResponse = {
  linked_deal: LinkedDeal | null;
};

/** Zoho Desk — a support Ticket linked to the live call so the finished call
 *  can be pushed to it at call-end (prompt or auto) as an internal note. Live
 *  capture writes it to `state.copilot.linked_ticket` (scalar JSONB) via
 *  `POST /v1/live/linked-ticket`; enrichment projects it into a durable
 *  `call_links` row (`kind='desk_ticket'`). A linked Deal AND a linked Ticket
 *  coexist on one call (independent scalars, pushed independently). `ticket_id`
 *  names the Desk record; the optional `ticket_number` / `subject` / `web_url`
 *  mirror the ticket's display fields so the after-call surfaces render without
 *  a re-fetch. Every optional field decodes cleanly from an older/absent
 *  backend. */
export type LinkedTicket = {
  ticket_id: string;
  ticket_number?: string;
  subject?: string;
  web_url?: string;
};

/** Zoho Desk — arguments for the `live_linked_ticket` Tauri command (backend
 *  `POST /v1/live/linked-ticket`). camelCase to match the invoke arg contract.
 *  `clear: true` removes any linked ticket on the session (the remaining fields
 *  are then ignored). One linked ticket at a time — a new link REPLACES the
 *  prior scalar server-side (independent of the linked Deal). */
export type LinkedTicketAssignArgs = {
  sessionUuid: string;
  ticketId: string;
  ticketNumber?: string;
  subject?: string;
  webUrl?: string;
  clear?: boolean;
};

/** Zoho Desk — response of `live_linked_ticket`: the reconciled linked ticket
 *  for the session (or `null` after a clear). The store replaces its scalar
 *  wholesale so a server-side reconcile stays consistent client-side. */
export type LinkedTicketResponse = {
  linked_ticket: LinkedTicket | null;
};

/** Zoho Desk — response of `zoho_desk_push_call` (backend
 *  `POST /v1/calls/{id}/zoho-desk/push`): the finished call was added to the
 *  ticket as a private internal note. Every field is optional so an older
 *  backend decodes cleanly — the ended-card confirmation deep-links from the
 *  linked ticket's `web_url`, so it never depends on this shape. */
export type ZohoDeskPushResponse = {
  push_id?: string;
  comment_id?: string;
  web_url?: string;
};

/** Phase 3 — per-user call-end CRM push mode. `"prompt"` (default) asks on the
 *  call-ended card before pushing a linked deal; `"auto"` lets the pipeline push
 *  it automatically. Mirrors the `users.zoho_autopush_mode` column; the
 *  `me_zoho_autopush_get/patch` Tauri commands round-trip it. */
export type ZohoAutopushMode = "prompt" | "auto";

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

/** Phase 3 — the Zoho Deal this call was linked to mid-call, folded into the
 *  call-detail payload (`GET /v1/calls/{id}`) so the after-call page can show a
 *  "Linked to <Deal> ↗" line even when the prompt-mode push prompt was skipped
 *  (i.e. no prior push exists). Distinct from the live-session `LinkedDeal`:
 *  the after-call surface only needs the display name plus a deep link to the
 *  Deal record. `zoho_url` is absent when the backend can't build it (no
 *  connection / unparseable data centre). Optional on the wire so an older
 *  backend that omits the field decodes cleanly. */
export type CallLinkedDeal = {
  record_name: string;
  zoho_url?: string;
};

/** Zoho Desk — the support Ticket this call was linked to mid-call, folded into
 *  the call-detail payload (`GET /v1/calls/{id}`) so the after-call page can
 *  show a "Linked to ticket #N ↗" line even when the call-end "Add to ticket"
 *  prompt was skipped. Distinct from the live-session `LinkedTicket`: the
 *  after-call surface only needs the ticket number (#N) plus a deep link.
 *  `web_url` is absent when the backend can't build it. Optional on the wire so
 *  an older backend that omits the field decodes cleanly. Coexists with
 *  `CallLinkedDeal` — a call can carry both. */
export type CallLinkedTicket = {
  ticket_number?: string;
  subject?: string;
  web_url?: string;
};

/** Phase 4 (live↔after-call continuity) — a durable question projected from the
 *  live ledger at enrichment, folded into the call-detail payload
 *  (`GET /v1/calls/{id}`) so the after-call page renders the same open/answered
 *  list the live drawer showed. Distinct from the live `LiveQuestion`: the
 *  after-call surface only needs who asked (`asker_display`), the question, its
 *  status, and (when answered) the answer. `id` is the row's stable UUID — the
 *  target for the manual-edit CRUD (add/edit/toggle/delete); it's also the
 *  return shape of those mutations. `question_text` / `answer_text` are
 *  transcript-derived → rendered as plain text, NEVER `{@html}`. */
export type CallQuestion = {
  id: string;
  asker_side: "you" | "them";
  asker_display: string;
  question_text: string;
  status: "open" | "answered";
  answer_text?: string;
};

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
  // Phase 3 — the Zoho Deal this call was linked to mid-call (from the durable
  // `call_links` row), folded in by the backend so the after-call detail can
  // surface it independently of any push. `null`/absent when the call has no
  // linked Deal.
  linked_deal?: CallLinkedDeal | null;
  // Zoho Desk — the support Ticket this call was linked to mid-call (from the
  // durable `call_links` row, `kind='desk_ticket'`), folded in by the backend
  // so the after-call detail can surface a "Linked to ticket #N ↗" line
  // independently of any push. `null`/absent when the call has no linked
  // Ticket; coexists with `linked_deal`.
  linked_ticket?: CallLinkedTicket | null;
  // Phase 4 (live↔after-call continuity) — the questions extracted during the
  // call (projected from the live ledger into the durable `call_questions`
  // table at enrichment), folded into the detail payload so the after-call page
  // renders the open/answered list. Additive: absent on older backends and `[]`
  // when the call has none, so the after-call section simply renders nothing.
  questions?: CallQuestion[];
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
