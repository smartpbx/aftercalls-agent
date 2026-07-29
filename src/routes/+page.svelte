<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { getVersion } from "@tauri-apps/api/app";
  import { goto } from "$app/navigation";
  import { onMount, onDestroy } from "svelte";
  import {
    notifyRecordStop,
    notifyPipelineDone,
    notifyPipelineFailed,
    notifyAutoDetect,
  } from "$lib/notify";
  import {
    detectPlatform,
    loadRecordingPrefs,
    playStartCueIfEnabled,
  } from "$lib/compliance";
  import { confirmAutoStart } from "$lib/autoStart";
  import NotesPanel from "$lib/NotesPanel.svelte";
  import LiveAssistPanel from "$lib/LiveAssistPanel.svelte";
  import CoPilotPanel from "$lib/CoPilotPanel.svelte";
  import { portalErrorToText } from "$lib/portalError";
  import { openBilling } from "$lib/billing";
  import { liveSession } from "$lib/stores/liveSession.svelte";
  import type {
    SzmPriorPush,
    SzmPushResponse,
  } from "@aftercalls/shared/ui/SendToZohoModal.svelte";
  import type {
    AskChip,
    AskAnswer,
    KnowledgeAnswer,
    LiveSegment,
    CopilotMode,
    CrmContextDeal,
    LinkedDealResponse,
    SpeakerIdentityAssignArgs,
    SpeakerIdentitiesResponse,
  } from "@aftercalls/shared/types";

  // Platform string reported to the backend on recording-ack (#44).
  // Kept here (not Rust-side) because navigator UA is already in the
  // detectPlatform / loadRecordingPrefs / playStartCueIfEnabled are
  // imported from $lib/compliance so this Record page and the
  // layout's auto-detect slide-out share one prefs cache and one
  // cue-play policy.

  type PipelineEvent =
    | { stage: "started"; session_dir: string }
    | { stage: "transcribing" }
    | { stage: "transcribed"; session_dir: string; call_id: string }
    | { stage: "summarizing" }
    | { stage: "writing_note" }
    | { stage: "uploading" }
    | { stage: "done"; session_dir: string; note_path: string; call_id: string }
    | { stage: "failed"; error: string };

  type AutoDetectEvent =
    | { kind: "prompt_start"; app: string }
    | { kind: "prompt_end"; app: string }
    | { kind: "cleared" };

  let recording = $state(false);
  let sessionDir = $state("");
  let error = $state("");
  // #142 follow-up — Call / Note mode toggle. "call" routes the
  // primary record button through start_recording (mic + system
  // loopback, full pipeline). "note" routes through start_self_note
  // (mic-only dictation, capped by max_self_note_minutes). Mirrors
  // the Mine/All-team scope-pill pattern shipped in #176.
  // Disabled while a recording is live so the user can't flip mode
  // mid-capture and confuse which shortcut is bound.
  let recordMode = $state<"call" | "note">("call");
  // Resolved shortcut strings for the kbd-row + record-button title.
  // Loaded from `get_app_prefs` on mount; null = user disabled the
  // hotkey in Settings, in which case the kbd-row is hidden for
  // that mode.
  let recordToggleShortcut = $state<string | null>("Super+Shift+R");
  let selfNoteShortcut = $state<string | null>("Super+Shift+N");
  let pipelineStage = $state<string>("");
  let pipelineError = $state("");
  // #602/WS-G — set when recording creation is billing-gated (backend
  // returns 402 `subscription_inactive`). Swaps the raw error line for a
  // friendly "subscribe to keep recording" block; never surface the raw
  // 402 string for this case.
  let subGate = $state(false);

  // #live — Live-transcript draft (Phase 1). `liveTranscriptEnabled` gates
  // the whole panel on the org feature flag (read from current_user on
  // mount, refreshed best-effort via refresh_current_user — #659).
  // #659 — the live stream itself (segments / status / coaching) now lives
  // in the persistent `liveSession` store, fed by the layout's `live-*`
  // listeners, so it survives route navigation while the Rust relay keeps
  // running. This page reads from that store; it no longer owns the buffer
  // or the listeners.
  let liveTranscriptEnabled = $state(false);

  // #653 — in-call co-pilot. `copilotEnabled` (org flag) + Call mode gates
  // the CoPilotPanel (transcript + CRM context + coaching lanes), which
  // mounts PRE-call so the "who are you calling?" picker is available before
  // Start. `copilotIsAdmin` drives the CRM lane's no-Zoho prompt (admins get
  // a Connect button; members get "ask an admin"). `contactHint` is the
  // picked Zoho contact id, raised from the CRM lane and threaded into
  // start_recording so the backend resolves the counterpart off the audio
  // hot path. All degrade cleanly: copilot OFF keeps today's plain panel.
  let copilotEnabled = $state(false);
  let copilotIsAdmin = $state(false);
  let contactHint = $state<string | null>(null);
  // #659 P5a — the org's default co-pilot persona (Sales / Support), read
  // from /auth/me. Seeds the CoPilotPanel mode toggle; the panel then owns
  // the per-call override. Defaults to "sales" until the me bundle lands.
  let copilotDefaultMode = $state<CopilotMode>("sales");

  // Co-pilot follow-ups (D) — call-end lifecycle. When a Call-mode recording stops the
  // co-pilot panel swaps to a "Call ended" state (Open post-call / Dismiss)
  // instead of lingering on the stale live dashboard until the next
  // record-start. `sessionWasCall` is captured from the recorder START event
  // (Rust emits mode "call" / "self_note"), so a mic-only self-note — even one
  // started via the global hotkey while the panel is mounted — never trips the
  // ended state. `callEnded` gates the ended panel; `copilotDismissed` hides the
  // whole co-pilot region after an explicit Dismiss so it does NOT fall back to
  // the stale ended dashboard (that would re-introduce the very "lingering" the
  // fix removes). Both clear on the next record-start; navigating away unmounts
  // this page, clearing them inherently (no blind N-second auto-hide timer). The
  // finished call_id arrives seconds later via the `pipeline` event →
  // `openableCallId`, which enables the "Open post-call" CTA.
  let sessionWasCall = $state(false);
  let callEnded = $state(false);
  let copilotDismissed = $state(false);

  function dismissCallEnded() {
    // Close the ended card AND suppress the co-pilot region until the next
    // record-start — never fall back to the stale ended live dashboard.
    callEnded = false;
    copilotDismissed = true;
  }

  // #660 co-pilot P1 — ask-chip + one-click highlight (star) handlers. +page
  // owns the Tauri invokes + optimistic store writes; the derived state
  // (`liveSession.askAnswer` / `.askInFlight` / `.highlighted` / `.talkMetric`)
  // and these callbacks are threaded into CoPilotPanel → LiveTranscriptLane.
  // Both degrade CALM: an unavailable ask / a highlight failure never surfaces
  // a red panel — the ask lands a bone degrade line, the star simply reverts.

  /** Fire one ask preset. Single in-flight (the lane also disables the chips
   *  while `askInFlight` is set — belt + braces). The backend degrades
   *  calm-200 so a resolved answer always renders; a gate/transport error
   *  (`live_ask` throws) lands the same calm "not available" line, never an
   *  error surface, never a vendor name. */
  async function handleAsk(chip: AskChip) {
    const su = liveSession.sessionUuid;
    if (!su || liveSession.askInFlight) return;
    liveSession.setAskBusy(chip);
    try {
      const res = (await invoke("live_ask", {
        sessionUuid: su,
        chip,
      })) as AskAnswer;
      liveSession.setAskAnswer(
        chip,
        res?.answer ?? "Still working — give it another tap.",
        typeof res?.based_on_turns === "number" ? res.based_on_turns : null,
        res?.empty === true,
      );
    } catch {
      // No retry-shame, no vendor name — a warm re-tap line, never a red
      // panel (#660 P1). Not an empty-state (empty:false) so it reads as the
      // error/unavailable treatment, distinct from a calm "nothing yet".
      liveSession.setAskAnswer(chip, "Still working — give it another tap.", null, false);
    }
  }

  function handleDismissAsk() {
    liveSession.dismissAsk();
  }

  /** #659 P5b — Support-mode "get an answer" affordance. Fires one grounded,
   *  cited knowledge answer over the org's KB. Single in-flight (the lane also
   *  disables the button while `knowledgeInFlight`). No manual query for MVP —
   *  the backend derives it from the last counterpart turn. The backend
   *  degrades calm-200 so a resolved answer always renders; a gate/transport
   *  error (`live_knowledge` throws) lands the same calm "not available" line,
   *  never an error surface, never a vendor name. */
  async function handleKnowledge() {
    const su = liveSession.sessionUuid;
    if (!su || liveSession.knowledgeInFlight) return;
    liveSession.setKnowledgeBusy();
    try {
      const res = (await invoke("live_knowledge", {
        sessionUuid: su,
      })) as KnowledgeAnswer;
      liveSession.setKnowledgeAnswer({
        answer: res?.answer ?? "That's not available right now.",
        sources: Array.isArray(res?.sources) ? res.sources : [],
      });
    } catch {
      liveSession.setKnowledgeAnswer({
        answer: "That's not available right now.",
        sources: [],
      });
    }
  }

  function handleDismissKnowledge() {
    liveSession.dismissKnowledge();
  }

  /** #646 (Phase 2) — assign (or clear) one speaker's identity. Optimistic: set
   *  the store immediately so every existing + future line of that speaker
   *  re-labels at once, then (in-call) invoke `live_speaker_identity` and
   *  RECONCILE the map wholesale from the response (the endpoint is authoritative
   *  for the primary). Pre-call there's no session to write to — the optimistic
   *  label stands and the primary contact rides `start_recording` as
   *  `contact_hint` (raised separately via `onpick` → `contactHint`). A failed
   *  write leaves the optimistic label in place (a label is advisory — never a
   *  red surface); the next successful assign reconciles to the truth. */
  async function handleAssignSpeaker(
    args: Omit<SpeakerIdentityAssignArgs, "sessionUuid">,
  ) {
    if (args.clear) {
      liveSession.clearSpeakerIdentity(args.channel, args.speakerLabel);
    } else {
      liveSession.setSpeakerIdentity({
        channel: args.channel,
        speaker_label: args.speakerLabel,
        kind: args.kind,
        display_name: args.displayName,
        contact_id: args.contactId,
        user_id: args.userId,
        is_primary: args.isPrimary,
      });
    }
    const su = liveSession.sessionUuid;
    if (!su) {
      // Pre-call: no session to write to yet. STAGE the assignment so it's
      // replayed to the backend the instant recording starts (the layout drains
      // the staged set), instead of being wiped by `resetForNewSession`. The
      // primary also rides `start_recording` as `contact_hint` (via `onpick`).
      if (args.clear) {
        liveSession.unstagePendingIdentity(args.channel, args.speakerLabel);
      } else {
        liveSession.stagePendingIdentity({
          channel: args.channel,
          speaker_label: args.speakerLabel,
          kind: args.kind,
          display_name: args.displayName,
          contact_id: args.contactId,
          user_id: args.userId,
          is_primary: args.isPrimary,
        });
      }
      return;
    }
    try {
      const res = (await invoke("live_speaker_identity", {
        sessionUuid: su,
        ...args,
      })) as SpeakerIdentitiesResponse;
      liveSession.reconcileSpeakerIdentities(res?.identities ?? []);
    } catch {
      // Optimistic label stands — no error surface. A later assign reconciles.
    }
  }

  /** Toggle a highlight (star) on one transcript turn. Optimistic: flip the
   *  store immediately, invoke the backend toggle, revert on failure. */
  async function handleHighlight(seg: LiveSegment, starred: boolean) {
    const su = liveSession.sessionUuid;
    if (!su) return;
    liveSession.setHighlighted(seg.channel, seg.start_ms, starred);
    try {
      await invoke("live_highlight", {
        sessionUuid: su,
        channel: seg.channel,
        startMs: seg.start_ms,
        endMs: seg.end_ms,
        speaker: seg.speaker ?? null,
        text: seg.text,
        starred,
      });
    } catch {
      // Backend rejected the toggle — revert the optimistic star so the UI
      // matches persisted state. No error surface (a star is advisory).
      liveSession.setHighlighted(seg.channel, seg.start_ms, !starred);
    }
  }

  /** Phase 3 — link a Zoho deal to this call (mid-call). One deal at a time —
   *  linking a new deal replaces the prior scalar. Optimistic: set the store
   *  immediately so the CRM tile + the ended card reflect it, then invoke
   *  `live_linked_deal` and RECONCILE from the response (the backend is the
   *  source of truth). No session (belt + braces — the affordance is
   *  session-gated in the tile) → the optimistic value stands. A failed write
   *  leaves the optimistic value in place; the ended-card [Push] still targets
   *  it. Never a red surface — a link is advisory. */
  async function handleLinkDeal(deal: CrmContextDeal) {
    liveSession.setLinkedDeal({
      module: "Deals",
      record_id: deal.id,
      record_name: deal.name,
      stage: deal.stage,
      amount: deal.amount,
      url: deal.url,
    });
    const su = liveSession.sessionUuid;
    if (!su) return;
    try {
      const res = (await invoke("live_linked_deal", {
        sessionUuid: su,
        module: "Deals",
        recordId: deal.id,
        recordName: deal.name,
        stage: deal.stage,
        amount: deal.amount,
      })) as LinkedDealResponse;
      liveSession.setLinkedDeal(res?.linked_deal ?? null);
    } catch {
      // Optimistic label stands — a later link/unlink reconciles.
    }
  }

  /** Phase 3 — unlink the call's deal. Optimistic clear, then a best-effort
   *  `clear: true` write reconciled from the response. */
  async function handleUnlinkDeal() {
    const prev = liveSession.linkedDeal;
    liveSession.setLinkedDeal(null);
    const su = liveSession.sessionUuid;
    if (!su || !prev) return;
    try {
      const res = (await invoke("live_linked_deal", {
        sessionUuid: su,
        module: prev.module,
        recordId: prev.record_id,
        recordName: prev.record_name,
        clear: true,
      })) as LinkedDealResponse;
      liveSession.setLinkedDeal(res?.linked_deal ?? null);
    } catch {
      // Optimistic clear stands.
    }
  }

  // Phase 3 — call-end CRM push state. ONE prior-push probe fires once the call
  // is `done` + openable; it drives BOTH the prompt-mode "Push this call?" ask
  // AND the "Pushed to <Deal>" confirmation (auto mode already pushed
  // backend-side, so the probe finds it — the client never needs to know the
  // mode). Best-effort: a missing/failed probe degrades to "no prior push" (the
  // [Push] prompt still shows when a deal was linked), never a red panel.
  let endedPriorPush = $state<SzmPriorPush | null>(null);
  let endedPushLoaded = $state(false);
  let endedPushing = $state(false);
  let endedSkipped = $state(false);
  // Non-reactive guard so the probe fires once per finished call.
  let endedPushProbedFor = "";

  async function probeEndedPriorPush(callId: string) {
    try {
      const p = (await invoke("zoho_prior_push", { callId })) as
        | SzmPriorPush
        | null;
      endedPriorPush = p ?? null;
    } catch {
      // Degrade to "no prior push" — the ended card falls back to the [Push]
      // prompt when a deal was linked, never surfaces an error.
      endedPriorPush = null;
    } finally {
      endedPushLoaded = true;
    }
  }

  // Fire the probe on the transcribed→done edge, once the call is openable.
  // `pipelineStage==='done'` gates it so the prompt/confirmation only appears
  // when the summary is ready (a still-processing call shows plain progress).
  $effect(() => {
    const ready = callEnded && !!openableCallId && pipelineStage === "done";
    const cid = openableCallId;
    if (!ready) return;
    if (endedPushProbedFor === cid) return;
    endedPushProbedFor = cid;
    endedPriorPush = null;
    endedPushLoaded = false;
    endedSkipped = false;
    void probeEndedPriorPush(cid);
  });

  /** [Push] on the ended card — push the finished call to the linked deal,
   *  reusing the existing manual push path pre-targeted. On success, flip to the
   *  "Pushed to <Deal>" confirmation. Best-effort: a failure leaves the [Push]
   *  prompt in place (retryable), never a red panel. */
  async function pushEndedCall() {
    const deal = liveSession.linkedDeal;
    if (!openableCallId || !deal || endedPushing) return;
    endedPushing = true;
    try {
      const resp = (await invoke("zoho_push_call", {
        callId: openableCallId,
        body: {
          module: deal.module,
          record_id: deal.record_id,
          record_name: deal.record_name,
        },
      })) as SzmPushResponse;
      endedPriorPush = {
        module: deal.module,
        record_id: deal.record_id,
        record_name: deal.record_name,
        pushed_at: new Date().toISOString(),
        zoho_url: resp.zoho_url,
      };
    } catch {
      // Best-effort — leave the [Push] prompt available (retryable).
    } finally {
      endedPushing = false;
    }
  }

  /** [Skip] — dismiss the push prompt (keeps the rest of the ended card). */
  function skipEndedPush() {
    endedSkipped = true;
  }

  function openZohoUrl(url: string) {
    void openUrl(url).catch((e) => console.warn("openUrl failed", e));
  }

  /** True when a failed-pipeline error string carries the backend's
   *  `subscription_inactive` 402 code. The create_call helper bails with
   *  `backend 402 Payment Required: {"error":"subscription_inactive"}`,
   *  so we match on the stable error code (not the numeric status, which
   *  `usage_cap_reached` also uses). */
  function isSubscriptionInactive(err: string): boolean {
    return /subscription_inactive/i.test(err ?? "");
  }

  /** #498 — strip internal vendor names from a free-form error
   *  string so the user-facing surface stays vendor-opaque (per
   *  CLAUDE.md hard rule #2). Replaces the names with "the
   *  transcription provider" / "the summarization provider" /
   *  generic fallbacks so the underlying cause stays readable
   *  ("rate-limit hit", "504 from upstream") even when the vendor
   *  name is redacted. */
  function redactVendorNames(s: string): string {
    if (!s) return s;
    return s
      .replace(/\bAssemblyAI\b/gi, "the transcription provider")
      .replace(/\bOpenAI\b/gi, "the summarization provider")
      .replace(/\bPostmark\b/gi, "the email provider")
      .replace(/\bDigitalOcean\b/gi, "the storage provider")
      .replace(/\bSpaces\b/g, "object storage");
  }
  // Latest known call id for the in-flight pipeline. Populated on
  // `transcribed` — before summary/action-items finish — so the
  // user can pop the call open while the rest of the pipeline
  // keeps working. Overwritten by `done` (same value; explicit).
  let openableCallId = $state("");
  let prompt = $state<AutoDetectEvent | null>(null);
  let elapsedMs = $state(0);
  let importing = $state(false);

  // #523 — "Recently recorded" strip: in-memory list of calls completed
  // during the current app session. Capped at 3 entries (newest first).
  // No backend fetch — populated only from pipeline `done` events fired
  // while this webview is open. Shown at the bottom of the idle Record
  // screen so the user has a quick way back to the last few calls.
  type RecentCall = { id: string; completedAt: number };
  let recentCalls = $state<RecentCall[]>([]);

  // PIPEDA recording-ack modal (#44). We show it at most once per
  // user per device; after the POST resolves the backend + auth.json
  // both mark the user acknowledged and this short-circuits every
  // subsequent Start Recording click.
  //
  // `pendingStart` remembers which path triggered the ack so we can
  // resume the right action after "I understand" —
  //   "manual" → invoke start_recording
  //   "auto"   → invoke confirm_auto_start (the detector prompt's
  //              "Yes, record" button)
  let ackModalOpen = $state(false);
  let ackChecked = $state(false);
  let ackSubmitting = $state(false);
  let ackError = $state("");
  let pendingStart = $state<"manual" | "auto" | null>(null);
  // Mirror of the cached `recording_acknowledged` flag. Populated
  // from `current_user` on mount so the ack check is a local lookup
  // in the common case. Flipped true after a successful POST; the
  // next time the layout refetches `current_user` this stays in
  // sync automatically.
  let ackCached = $state(false);

  // #302 Slice C — whether screen capture is ON for this user (org flag
  // AND per-user opt-in). Widens the pre-call disclosure: the recording-
  // ack modal body + the participant-facing Copy notice both name the
  // screen so the disclosed consent covers video. Self-notes never
  // screen-capture, so their surfaces are untouched.
  let screenCaptureOn = $state(false);

  // Copy-notice button UX (#45).
  let copiedNotice = $state(false);
  let copyingNotice = $state(false);
  let copyError = $state("");
  // #479 — preview the notice text before copying. Null = closed;
  // string = the rendered notice text ready to copy. The user sees the
  // full message first, then clicks "Copy" inside the preview panel to
  // push it to the clipboard.
  let noticePreview = $state<string | null>(null);

  // #531 — optional title for Note-to-self recordings. Shown when
  // recordMode === "note" and not yet recording. Persisted via save_title
  // Tauri command when the user types and starting recording flushes it.
  // Resets to "" after stop so the next note starts blank.
  let noteTitle = $state("");
  let noteTitleSaved = $state(false); // optimistic "saved" feedback
  let noteTitleTimer = 0;

  function scheduleNoteTitleSave(title: string) {
    if (!currentSessionId) return;
    clearTimeout(noteTitleTimer);
    noteTitleTimer = window.setTimeout(async () => {
      try {
        await invoke("save_title", {
          sessionId: currentSessionId,
          title,
        });
        noteTitleSaved = true;
        setTimeout(() => { noteTitleSaved = false; }, 1500);
      } catch (e) {
        console.warn("save_title failed", e);
      }
    }, 400);
  }

  // Manual notes panel (#73). Opt-in per user via Settings. When on,
  // the record page shows a CodeMirror editor during an active
  // recording; text is debounced-saved to notes.md in the session_dir
  // via the save_notes Tauri command. The pipeline picks that up at
  // create_call time so the backend gets the notes + include flag.
  let manualNotesEnabled = $state(false);
  // #346 — per-session notes toggle. Lets the user open the notes
  // panel directly from the Record page without visiting Settings.
  // Resets when the session changes so a new recording always starts
  // at the Settings-driven default. If the Settings pref is already
  // on, the inline toggle is redundant but stays consistent.
  let sessionNotesOpen = $state(false);
  let showNotes = $derived(
    manualNotesEnabled || sessionNotesOpen,
  );

  // Linux-only in-app note that explains why Super+Shift+R doesn't
  // fire when the app is unfocused on most desktop environments (the
  // X11 grab through XWayland doesn't receive unfocused keystrokes)
  // and points the user at the help page for the CLI-bound
  // workaround. `hotkeyNoteOS` is populated on mount from
  // `platform_os`; the note renders only when it equals "linux" AND
  // the dismiss pref is false. `hotkeyNoteAppPrefs` caches the full
  // AppPrefs snapshot so the Dismiss click can round-trip every
  // other pref untouched (mirrors the settings-page pattern).
  let hotkeyNoteOS = $state("");
  let hotkeyNoteDismissed = $state(true); // default hidden until mount resolves
  let hotkeyNoteAppPrefs = $state<{
    close_to_tray: boolean;
    auto_detect: boolean;
    auto_detect_popup: boolean;
    telemetry_enabled: boolean;
    sounds_enabled: boolean;
    max_recording_minutes: number;
    manual_notes_enabled: boolean;
    wayland_hotkey_notice_dismissed: boolean;
    input_device: string | null;
  } | null>(null);

  // Mic-fallback toast (#3). Fired by the agent when a saved
  // input-device pref doesn't resolve and we silently fell back to
  // the system default. Dedupe is handled Rust-side per session per
  // saved name, so this listener can render unconditionally on every
  // event without nagging. Auto-dismisses after 12s; user can also
  // dismiss manually.
  type MicFallback = {
    saved: string;
    reason: "not_found" | "enumeration_failed";
  };
  let micFallback = $state<MicFallback | null>(null);
  let micFallbackTimer = 0;
  let showHotkeyNote = $derived(
    hotkeyNoteOS === "linux" && !hotkeyNoteDismissed,
  );

  async function openHotkeyHelp() {
    try {
      await openUrl("https://aftercalls.io/help#global-shortcut-linux");
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }

  async function dismissHotkeyNote() {
    // Optimistic hide; roll back if the save fails so the note
    // doesn't appear to "stick dismissed" when the config write
    // actually didn't persist.
    const prev = hotkeyNoteDismissed;
    hotkeyNoteDismissed = true;
    const prefs = hotkeyNoteAppPrefs;
    if (!prefs) return;
    try {
      await invoke("set_app_prefs", {
        closeToTray: prefs.close_to_tray,
        autoDetect: prefs.auto_detect,
        autoDetectPopup: prefs.auto_detect_popup,
        telemetryEnabled: prefs.telemetry_enabled,
        soundsEnabled: prefs.sounds_enabled,
        maxRecordingMinutes: prefs.max_recording_minutes,
        manualNotesEnabled: prefs.manual_notes_enabled,
        waylandHotkeyNoticeDismissed: true,
        // Round-trip the input-device pref untouched (#3). Dropping
        // it here would clear the user's chosen mic any time they
        // dismiss the hotkey note.
        inputDevice: prefs.input_device,
      });
      hotkeyNoteAppPrefs = {
        ...prefs,
        wayland_hotkey_notice_dismissed: true,
      };
    } catch (e) {
      hotkeyNoteDismissed = prev;
      console.warn("dismiss hotkey note failed", e);
    }
  }
  let currentSessionId = $derived(
    sessionDir ? sessionDir.split(/[\\/]/).filter(Boolean).pop() ?? "" : "",
  );
  let notesSaveTimer = 0;
  let notesPending: string | null = null;
  // #194: seed value for the TipTap editor on remount mid-recording.
  // Notes are written to <session_dir>/notes.md during typing, but a
  // route-nav unmounts the editor and the next mount comes up empty
  // unless we re-read from disk. notesInitialFor tracks which
  // session_id we've already issued a load_notes for, so the same
  // session never gets seeded twice. notesUserTyped flips on the
  // first onchange callback and prevents a slow load_notes resolve
  // from clobbering keystrokes the user managed to land in the gap.
  let notesInitial = $state("");
  let notesInitialFor = "";
  let notesUserTyped = false;
  function scheduleNotesSave(text: string) {
    if (!currentSessionId) return;
    notesUserTyped = true;
    notesPending = text;
    clearTimeout(notesSaveTimer);
    notesSaveTimer = window.setTimeout(async () => {
      if (notesPending === null || !currentSessionId) return;
      const payload = notesPending;
      notesPending = null;
      try {
        await invoke("save_notes", {
          sessionId: currentSessionId,
          notes: payload,
        });
      } catch (e) {
        console.warn("save_notes failed", e);
      }
    }, 500);
  }
  // Whenever the active session changes, reset the seed and try to
  // rehydrate from disk. Empty/missing notes.md → empty string, so
  // a fresh recording starts blank without a special branch.
  // Also reset the per-session notes toggle (#346) so a new recording
  // starts from the Settings-driven default state.
  $effect(() => {
    const sid = currentSessionId;
    if (!sid) {
      notesInitial = "";
      notesInitialFor = "";
      notesUserTyped = false;
      sessionNotesOpen = false;
      return;
    }
    if (notesInitialFor === sid) return;
    notesInitialFor = sid;
    notesUserTyped = false;
    notesInitial = "";
    invoke<string>("load_notes", { sessionId: sid })
      .then((text) => {
        // Only seed if (a) we're still on the same session, and
        // (b) the user hasn't already started typing in the editor
        // during the IPC round-trip — otherwise their keystrokes
        // would be overwritten by setContent in NotesPanel's effect.
        if (notesInitialFor === sid && !notesUserTyped) {
          notesInitial = text ?? "";
        }
      })
      .catch((e) => console.warn("load_notes failed", e));
  });

  async function openCallInBrowser() {
    if (!openableCallId) return;
    try {
      await openUrl(`https://app.aftercalls.io/calls/${openableCallId}`);
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }

  async function openCallInApp() {
    if (!openableCallId) return;
    await goto(`/calls/${openableCallId}`);
  }

  let unlisten: UnlistenFn | null = null;
  let unlistenState: UnlistenFn | null = null;
  let unlistenAuto: UnlistenFn | null = null;
  let unlistenMicFallback: UnlistenFn | null = null;
  let timer = 0;
  let startAt = 0;

  function dismissMicFallback() {
    micFallback = null;
    if (micFallbackTimer) {
      clearTimeout(micFallbackTimer);
      micFallbackTimer = 0;
    }
  }

  onMount(async () => {
    unlisten = await listen<PipelineEvent>("pipeline", (evt) => {
      const p = evt.payload;
      pipelineError = "";
      subGate = false;
      pipelineStage = p.stage;
      if (p.stage === "failed") {
        if (isSubscriptionInactive(p.error)) {
          // #602/WS-G — recording creation was billing-gated (402
          // subscription_inactive). Show the friendly subscribe block
          // instead of the raw 402 string; the "Failed" status row is
          // suppressed too (see the status lane below).
          subGate = true;
        } else {
          // #498 — backend pipeline errors can name internal vendors
          // (AssemblyAI / OpenAI / Postmark / DigitalOcean) when an
          // upstream call fails. Per CLAUDE.md hard rule #2, public-
          // facing copy stays vendor-opaque. Redact vendor names from
          // the pass-through string before showing it to the user;
          // the staff `/staff/agent-logs` view still has the raw
          // unredacted error for debugging.
          pipelineError = redactVendorNames(p.error);
        }
        notifyPipelineFailed();
      }
      if (p.stage === "transcribed") openableCallId = p.call_id;
      if (p.stage === "done") {
        openableCallId = p.call_id;
        notifyPipelineDone();
        // #523 — record to in-memory recent list (newest-first, capped at 3).
        recentCalls = [{ id: p.call_id, completedAt: Date.now() }, ...recentCalls].slice(0, 3);
      }
      // #344 — the notes panel stays mounted through pipeline
      // upload, then unmounts on done/failed. Flush any pending
      // debounced save before that unmount so the last keystrokes
      // landed during the post-stop window are persisted.
      if ((p.stage === "done" || p.stage === "failed") && notesPending !== null && currentSessionId) {
        const payload = notesPending;
        const sid = currentSessionId;
        notesPending = null;
        clearTimeout(notesSaveTimer);
        invoke("save_notes", { sessionId: sid, notes: payload }).catch(
          (e) => console.warn("save_notes (pipeline-end flush) failed", e),
        );
      }
    });
    unlistenState = await listen<{
      recording: boolean;
      mode?: string;
      session_dir?: string;
    }>(
      "recording-state",
      (evt) => {
        const wasRecording = recording;
        recording = evt.payload.recording;
        // #164: the manual toggle sets sessionDir from start_recording's
        // return value, but auto-detect routes through confirm_auto_start
        // (no return). Capture session_dir from the recording-state event
        // so the notes panel mounts for both paths.
        if (recording && evt.payload.session_dir) {
          sessionDir = evt.payload.session_dir;
        }
        // Start cue plays BEFORE start_recording (see
        // actuallyStartRecording / confirmAutoStart) so the system
        // loopback doesn't capture it. Stop cue still plays on the
        // transition — no capture concern after the recorder has
        // stopped.
        if (!recording && wasRecording) {
          notifyRecordStop();
          // Co-pilot follow-ups (D) — a Call-mode session just ended. Swap the co-pilot
          // panel to the "Call ended" state instead of leaving it on the
          // stale live dashboard. Gated on a real call session + copilot ON
          // (a self-note never trips it); the pipeline event will surface the
          // finished call_id (openableCallId) seconds later.
          if (sessionWasCall && copilotEnabled) callEnded = true;
        }
        if (recording) {
          pipelineStage = "";
          pipelineError = "";
          subGate = false;
          openableCallId = "";
          // Co-pilot follow-ups (D) — remember whether THIS session is a call (Rust emits
          // mode "call" / "self_note") and clear any prior ended/dismissed
          // state: a fresh record-start closes the "Call ended" card and
          // re-shows the live co-pilot.
          sessionWasCall = evt.payload.mode === "call";
          callEnded = false;
          copilotDismissed = false;
          // Phase 3 — reset the call-end CRM push surface so a fresh call never
          // inherits the previous call's push prompt/confirmation. (The store's
          // `linkedDeal` is reset in `resetForNewSession` on the layout side.)
          endedPriorPush = null;
          endedPushLoaded = false;
          endedSkipped = false;
          endedPushProbedFor = "";
          // #659 — the per-session live-draft reset (clear segments +
          // coaching, seed status) now lives in the layout's
          // recording-state handler via liveSession.resetForNewSession(),
          // so it fires even when the user isn't on /record at start.
          startAt = Date.now();
          timer = window.setInterval(
            () => (elapsedMs = Date.now() - startAt),
            250,
          );
        } else {
          clearInterval(timer);
          elapsedMs = 0;
          // #531 — clear the note title so the next recording starts blank.
          noteTitle = "";
          noteTitleSaved = false;
        }
      },
    );
    unlistenMicFallback = await listen<MicFallback>("mic-fallback", (evt) => {
      // Rust dedupes per-session-per-name, so every event we receive
      // is one the user hasn't seen yet this run. Reset the auto-
      // dismiss timer on each arrival so the full 12s applies to the
      // most recent one.
      micFallback = evt.payload;
      if (micFallbackTimer) clearTimeout(micFallbackTimer);
      micFallbackTimer = window.setTimeout(() => {
        micFallback = null;
        micFallbackTimer = 0;
      }, 12000);
    });
    // #659 — the `live-segment` / `live-session` / `live-coaching`
    // listeners now live in +layout.svelte (registered once, for the
    // app's lifetime) and write into the persistent `liveSession` store,
    // so the draft survives route navigation. This page reads from that
    // store; it no longer registers or tears down these listeners.
    unlistenAuto = await listen<AutoDetectEvent>("auto-detect", (evt) => {
      // prompt_start rendered here as an inline banner when the user
      // is already on /record (#60). The layout's slide-out suppresses
      // itself on /record so there's no duplicate UI. prompt_end
      // (mid-recording idle-mic) always lives here since the user is
      // almost always on /record during a live recording.
      const next = evt.payload.kind === "cleared" ? null : evt.payload;
      if (next && !prompt) notifyAutoDetect();
      prompt = next;
    });

    // Cache the user's PIPEDA ack state so the Start Recording
    // click path can make a local decision (see maybeShowRecordingAck).
    // We fall back to a backend check on click if this is false, so
    // a stale auth.json (user acknowledged on another device) doesn't
    // re-prompt.
    try {
      const u = await invoke<{
        recording_acknowledged?: boolean;
        role?: string;
        copilot_default_mode?: CopilotMode;
        features?: { live_transcript?: boolean; copilot?: boolean };
      } | null>("current_user");
      ackCached = !!u?.recording_acknowledged;
      // #live — gate the whole panel on the org feature flag.
      liveTranscriptEnabled = !!u?.features?.live_transcript;
      // #653 — co-pilot gate + admin capability for the CRM lane's no-Zoho
      // prompt. Admin roles can connect Zoho from the portal; members can't.
      copilotEnabled = !!u?.features?.copilot;
      copilotIsAdmin =
        u?.role === "admin" ||
        u?.role === "owner" ||
        u?.role === "superadmin";
      // #659 P5a — seed the mode toggle from the org default.
      copilotDefaultMode = u?.copilot_default_mode === "support" ? "support" : "sales";
    } catch {}

    // #659 — best-effort network refresh of the org feature flags. The
    // sync `current_user` read above is the instant, offline-safe paint;
    // this re-fetches `/v1/auth/me`, persists the fresh bundle to
    // auth.json, and re-gates the panels so a feature enabled server-side
    // (e.g. copilot bought mid-session) surfaces on route re-entry
    // without a full re-login. Fire-and-forget so it never blocks the
    // rest of mount; on error (offline / expired refresh token) the
    // cached values above stand — the panel is never blanked.
    invoke<{
      recording_acknowledged?: boolean;
      role?: string;
      copilot_default_mode?: CopilotMode;
      features?: { live_transcript?: boolean; copilot?: boolean };
    }>("refresh_current_user")
      .then((u) => {
        ackCached = !!u?.recording_acknowledged;
        liveTranscriptEnabled = !!u?.features?.live_transcript;
        copilotEnabled = !!u?.features?.copilot;
        copilotIsAdmin =
          u?.role === "admin" ||
          u?.role === "owner" ||
          u?.role === "superadmin";
        copilotDefaultMode = u?.copilot_default_mode === "support" ? "support" : "sales";
      })
      .catch(() => {});

    // Warm the recording-prefs cache so the first Copy-notice click
    // and the first Start Recording don't stall on a round-trip.
    // Silently best-effort; any failure is deferred to click-time.
    loadRecordingPrefs();

    // #302 Slice C — resolve whether screen capture is on for this user
    // so the pre-call disclosure copy can widen to name the screen.
    void loadScreenCaptureOn();

    // Load manual_notes_enabled so the record page knows whether to
    // render the notes panel during active recording, and
    // wayland_hotkey_notice_dismissed so we know whether to show the
    // Linux in-app hotkey note below the kbd-row. Failures here
    // default to `false` (panel hidden) / `true` (note hidden) —
    // silent-failure of a pref load should never surface a new UI
    // element the user hasn't seen before.
    try {
      const prefs = await invoke<{
        close_to_tray: boolean;
        auto_detect: boolean;
        auto_detect_popup: boolean;
        telemetry_enabled: boolean;
        sounds_enabled: boolean;
        max_recording_minutes: number;
        manual_notes_enabled: boolean;
        wayland_hotkey_notice_dismissed: boolean;
        input_device: string | null;
        record_toggle_shortcut: string | null;
        self_note_shortcut: string | null;
      }>("get_app_prefs");
      manualNotesEnabled = !!prefs?.manual_notes_enabled;
      hotkeyNoteAppPrefs = prefs;
      hotkeyNoteDismissed = !!prefs?.wayland_hotkey_notice_dismissed;
      recordToggleShortcut = prefs?.record_toggle_shortcut ?? null;
      selfNoteShortcut = prefs?.self_note_shortcut ?? null;
    } catch (e) {
      console.warn("get_app_prefs failed", e);
    }

    // Platform detection for the Linux-only hotkey note. Tauri
    // exposes std::env::consts::OS via the `platform_os` command,
    // which is cheaper and more reliable than `navigator.platform`
    // (Tauri's bundled webview can report varying UA strings across
    // distros).
    try {
      hotkeyNoteOS = await invoke<string>("platform_os");
    } catch (e) {
      console.warn("platform_os failed", e);
    }

    // "recording-state" only fires on transitions, so a remount
    // mid-recording (e.g. tray hide+show, route nav) wouldn't know
    // the truth. Ask the backend directly + rebuild the timer from
    // the real start time.
    try {
      const status = await invoke<{
        recording: boolean;
        started_at_ms: number | null;
        session_dir: string | null;
      }>("is_recording");
      if (status.recording && !recording) {
        recording = true;
        if (status.started_at_ms) {
          startAt = status.started_at_ms;
          elapsedMs = Date.now() - startAt;
          timer = window.setInterval(
            () => (elapsedMs = Date.now() - startAt),
            250,
          );
        }
        // #185: sessionDir is component-local $state that dies on
        // route-nav unmount. Without it, currentSessionId stays empty
        // and the manual-notes render gate fails on re-entry. The
        // backend now carries the active session_dir on
        // RecordingStatus so we can reseed here — mirroring the
        // recording-state event handler's behaviour for the
        // start-transition path.
        if (status.session_dir) {
          sessionDir = status.session_dir;
        }
      }
    } catch {}
  });

  onDestroy(() => {
    unlisten?.();
    unlistenState?.();
    unlistenAuto?.();
    unlistenMicFallback?.();
    if (micFallbackTimer) {
      clearTimeout(micFallbackTimer);
      micFallbackTimer = 0;
    }
    clearInterval(timer);
    // Flush any pending debounced notes save so a route-nav
    // mid-type doesn't drop the last few keystrokes.
    if (notesPending !== null && currentSessionId) {
      const payload = notesPending;
      const sid = currentSessionId;
      notesPending = null;
      clearTimeout(notesSaveTimer);
      invoke("save_notes", {
        sessionId: sid,
        notes: payload,
      }).catch((e) => console.warn("save_notes (flush) failed", e));
    }
  });

  async function toggle() {
    error = "";
    try {
      if (recording) {
        sessionDir = await invoke<string>("stop_recording");
        return;
      }
      // Note mode bypasses the PIPEDA ack — a self-note is mic-only
      // dictation by the user, no other participants to consent.
      // Same shape the keyboard shortcut + tray menu have used since
      // v0.4.5.
      if (recordMode === "note") {
        await actuallyStartSelfNote();
        return;
      }
      // Gate on PIPEDA ack (#44) before touching the recorder.
      const ok = await ensureRecordingAcknowledged("manual");
      if (!ok) return; // Modal opened (or aborted) — resume happens later.
      await actuallyStartRecording();
    } catch (e) {
      error = portalErrorToText(e);
    }
  }

  async function actuallyStartRecording() {
    pipelineStage = "";
    pipelineError = "";
    subGate = false;
    openableCallId = "";
    // Play the start cue BEFORE invoking the recorder so the system
    // loopback doesn't capture the beep. Await blocks for ~350ms
    // when sounds are enabled; no-op when off. Any failure here is
    // swallowed so a flaky audio stack never blocks recording.
    try {
      await playStartCueIfEnabled();
    } catch (e) {
      console.warn("start cue failed", e);
    }
    // #653 — pass the pre-picked Zoho contact id (if any) so the backend can
    // resolve the counterpart off the audio hot path. `null` → the command's
    // `Option<String>` decodes to `None` and today's behavior is unchanged.
    sessionDir = await invoke<string>("start_recording", {
      contactHint: contactHint ?? undefined,
    });
  }

  // #142 follow-up — note-to-self path from the record-page tab.
  // Same Tauri command the global hotkey + tray menu invoke; mic-only
  // capture, capped by `max_self_note_minutes`. No PIPEDA ack: a
  // self-note has only one participant.
  async function actuallyStartSelfNote() {
    pipelineStage = "";
    pipelineError = "";
    subGate = false;
    openableCallId = "";
    try {
      // #56 — selfNote: true plays the chime but suppresses the
      // spoken consent announcement. A self-note has only one
      // participant (the user dictating); no other party to disclose
      // to. Mirrors the PIPEDA-ack bypass for self-notes above.
      await playStartCueIfEnabled({ selfNote: true });
    } catch (e) {
      console.warn("start cue failed", e);
    }
    sessionDir = await invoke<string>("start_self_note");
    // #531 — flush the user-provided title now that we have a session_dir.
    // session_id is the last path component of session_dir.
    const sessionId = sessionDir.split(/[\\/]/).filter(Boolean).pop() ?? "";
    if (noteTitle.trim() && sessionId) {
      invoke("save_title", { sessionId, title: noteTitle.trim() }).catch(
        (e) => console.warn("save_title (start flush) failed", e),
      );
    }
  }

  // Returns true when the caller may proceed with recording
  // immediately. Returns false when we opened the ack modal (or the
  // user is signed out); in that case the resume happens inside
  // submitAck once the POST completes.
  async function ensureRecordingAcknowledged(
    source: "manual" | "auto",
  ): Promise<boolean> {
    if (ackCached) return true;
    // Fallback to the backend: the cache might be stale (acked on
    // another device, or an older auth.json predates the field).
    try {
      const ok = await invoke<boolean>("get_recording_ack");
      if (ok) {
        ackCached = true;
        return true;
      }
    } catch (e) {
      // Surface the error in the modal — a user who can't reach the
      // backend shouldn't silently start recording and then get a
      // network error three seconds later.
      console.warn("get_recording_ack failed", e);
    }
    pendingStart = source;
    ackChecked = false;
    ackError = "";
    ackModalOpen = true;
    return false;
  }

  async function submitAck() {
    if (!ackChecked || ackSubmitting) return;
    ackSubmitting = true;
    ackError = "";
    try {
      const agentVersion = await getVersion().catch(() => "unknown");
      await invoke("post_recording_ack", {
        agentVersion,
        platform: detectPlatform(),
      });
      ackCached = true;
      const resume = pendingStart;
      ackModalOpen = false;
      pendingStart = null;
      // Resume the original action the user was trying to take.
      if (resume === "manual") {
        await actuallyStartRecording();
      } else if (resume === "auto") {
        // Shared helper: play start cue then confirm_auto_start (#465).
        await confirmAutoStart();
      }
    } catch (e) {
      ackError = portalErrorToText(e).replace(/^Error:\s*/, "");
    } finally {
      ackSubmitting = false;
    }
  }

  function cancelAck() {
    if (ackSubmitting) return;
    ackModalOpen = false;
    pendingStart = null;
    ackChecked = false;
    ackError = "";
  }

  async function openConsentGuide() {
    try {
      await openUrl("https://aftercalls.io/help#privacy-consent");
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }

  // #302 Slice C — screen capture is "on" only when the org flag is set
  // AND the user opted in. Best-effort; any failure leaves the disclosure
  // copy in its default (audio-only) shape.
  async function loadScreenCaptureOn() {
    try {
      const u = await invoke<{
        features?: { screen_capture?: boolean };
      } | null>("current_user");
      if (!u?.features?.screen_capture) {
        screenCaptureOn = false;
        return;
      }
      const prefs = await invoke<{ enabled: boolean }>(
        "get_screen_capture_prefs",
      );
      screenCaptureOn = !!prefs.enabled;
    } catch (e) {
      console.warn("loadScreenCaptureOn failed", e);
      screenCaptureOn = false;
    }
  }

  // #479 — open the preview panel so the user can inspect the notice
  // text before it hits the clipboard.
  async function openNoticePreview() {
    if (noticePreview !== null) {
      // Second click closes the panel (toggle behaviour).
      noticePreview = null;
      return;
    }
    copyError = "";
    copyingNotice = true;
    try {
      const prefs = await loadRecordingPrefs();
      if (!prefs) {
        copyError = "Couldn't load your org's recording preferences.";
        return;
      }
      noticePreview =
        `Heads up — I'm using aftercalls to record and transcribe this ` +
        `call for the purpose of ${prefs.recording_purpose}.` +
        (screenCaptureOn
          ? ` My screen may also be recorded as video while the call is ` +
            `recording.`
          : ``) +
        ` The recording is stored on Canadian cloud infrastructure and ` +
        `used only for that purpose. If you'd prefer I didn't, let me ` +
        `know and I'll stop.`;
    } catch (e) {
      copyError = portalErrorToText(e).replace(/^Error:\s*/, "");
    } finally {
      copyingNotice = false;
    }
  }

  // Called from inside the preview panel — actually copies to clipboard.
  async function copyNotice() {
    if (!noticePreview) return;
    copyError = "";
    try {
      await writeText(noticePreview);
      copiedNotice = true;
      noticePreview = null;
      setTimeout(() => {
        copiedNotice = false;
      }, 2000);
    } catch (e) {
      copyError = portalErrorToText(e).replace(/^Error:\s*/, "");
    }
  }

  async function importRecording() {
    error = "";
    const picked = await openDialog({
      multiple: false,
      filters: [
        {
          name: "Audio",
          extensions: ["wav", "mp3", "m4a", "mp4", "ogg", "opus", "flac", "webm"],
        },
      ],
    });
    if (!picked || Array.isArray(picked)) return;
    importing = true;
    pipelineStage = "";
    pipelineError = "";
    subGate = false;
    openableCallId = "";
    try {
      sessionDir = await invoke<string>("process_imported_file", {
        sourcePath: picked,
      });
    } catch (e) {
      error = portalErrorToText(e);
    } finally {
      importing = false;
    }
  }

  // prompt_start is rendered inline on this page when the user is
  // already on /record (#60). The layout's slide-out suppresses
  // itself on this route so only one UI shows.
  async function confirmStart() {
    const ok = await ensureRecordingAcknowledged("auto");
    if (!ok) return;
    // Shared helper: play start cue then confirm_auto_start (#465).
    await confirmAutoStart();
  }
  async function dismissStart() {
    await invoke("dismiss_auto_start");
  }
  async function confirmEnd() {
    await invoke("confirm_auto_end");
  }
  async function keepRecording() {
    await invoke("keep_auto_recording");
  }

  // #142 follow-up — the kbd-row + record-button title attribute
  // both pull from these. `activeShortcut` reflects whichever mode
  // is currently selected; `null` means the user disabled that
  // hotkey in Settings, in which case the kbd-row is hidden for
  // that mode (tooltip falls back to the action verb only).
  let activeShortcut = $derived(
    recordMode === "call" ? recordToggleShortcut : selfNoteShortcut,
  );
  // Split a "Super+Shift+R" canonical shortcut into individual
  // <span class="kbd"> chunks for the kbd-row. Filters falsy parts
  // so the row collapses cleanly when the shortcut is null.
  let activeShortcutKeys = $derived(
    (activeShortcut ?? "")
      .split("+")
      .map((s) => s.trim())
      .filter(Boolean),
  );
  // Title-attribute string used as the record-button tooltip. Folds
  // the shortcut + mode into one line so the user always sees the
  // configured combo (or a "Set in Settings" hint when disabled).
  let recordBtnTitle = $derived.by(() => {
    const verb = recording
      ? "Stop recording"
      : recordMode === "note"
        ? "Start note to self"
        : "Start recording";
    if (activeShortcut) return `${verb} (${activeShortcut})`;
    return `${verb} — set a global shortcut in Settings`;
  });

  function setRecordMode(next: "call" | "note") {
    if (recording) return; // never flip mode mid-capture
    recordMode = next;
  }

  function fmtTimer(ms: number) {
    const s = Math.floor(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const r = s % 60;
    const pad = (n: number) => String(n).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(r)}` : `${pad(m)}:${pad(r)}`;
  }

  // #523 — format a timestamp as a short relative label ("just now",
  // "2 min ago", "1 hr ago") for the recently-recorded strip.
  function fmtRelative(ms: number): string {
    const diff = Date.now() - ms;
    const mins = Math.floor(diff / 60_000);
    if (mins < 1) return "just now";
    if (mins < 60) return `${mins} min ago`;
    const hrs = Math.floor(mins / 60);
    return `${hrs} hr ago`;
  }

  const pipelineLabels: Record<string, string> = {
    started: "Processing",
    transcribing: "Transcribing",
    // Transcribed = transcript is in; the call is already openable.
    // Summarizing fires immediately after, so users see "Drafting
    // summary" as the ongoing state even though the transcribed
    // moment briefly lights up this row.
    transcribed: "Transcript ready",
    summarizing: "Drafting summary",
    writing_note: "Writing vault note",
    uploading: "Syncing to cloud",
    done: "Saved",
    failed: "Failed",
  };
</script>

<main class="page reveal" class:copilot-active={copilotEnabled && recordMode === "call"}>
  <header class="head" style="--i: 0">
    <h1>Record</h1>
    <p class="sub">
      Capture a call with your mic and system audio. We'll transcribe,
      summarize, and file the note into your vault.
    </p>
  </header>

  <!-- Mic-fallback banner (#3). Fires when a saved input-device
       pref didn't resolve and we silently fell back to the system
       default. Rust dedupes per-session-per-name so this only
       appears once per stale device per launch. Sig-gold accent —
       a heads-up, not an error. Reuses the .banner pattern the
       auto-detect notices already use on this page. -->
  {#if micFallback}
    <div class="banner" style="--i: 0.5" role="status" aria-live="polite">
      <div class="banner-body">
        <p class="banner-label">Mic fallback</p>
        <p class="banner-text">
          {#if micFallback.reason === "enumeration_failed"}
            Couldn't check saved mic
            <strong title={micFallback.saved}>"{micFallback.saved}"</strong>
            — using system default.
          {:else}
            Saved mic
            <strong title={micFallback.saved}>"{micFallback.saved}"</strong>
            not found — using system default.
          {/if}
        </p>
      </div>
      <div class="banner-actions">
        <button class="btn ghost" onclick={dismissMicFallback}>Dismiss</button>
      </div>
    </div>
  {/if}

  <!-- Auto-detect banners. prompt_start fires here when the user is
       already on /record; the layout slide-out suppresses itself on
       this route (#60). prompt_end always lives here since the user
       is practically always on /record while a recording is live. -->
  {#if prompt?.kind === "prompt_start"}
    <div class="banner" style="--i: 1">
      <div class="banner-body">
        <p class="banner-label">Detected</p>
        <p class="banner-text">
          <strong>{prompt.app}</strong> is using the microphone. Record this call?
        </p>
      </div>
      <div class="banner-actions">
        <button class="btn primary" onclick={confirmStart}>Start recording</button>
        <button class="btn ghost" onclick={dismissStart}>Dismiss</button>
      </div>
    </div>
  {:else if prompt?.kind === "prompt_end"}
    <div class="banner" style="--i: 1">
      <div class="banner-body">
        <p class="banner-label">Idle mic</p>
        <p class="banner-text">
          The microphone has been quiet for a while. End the recording?
        </p>
      </div>
      <div class="banner-actions">
        <button class="btn primary" onclick={confirmEnd}>Save + transcribe</button>
        <button class="btn ghost" onclick={keepRecording}>Keep recording</button>
      </div>
    </div>
  {/if}

  <section class="cta" style="--i: 2">
    <!-- #142 follow-up — Call / Note mode tabs. Same scope-pill
         visual the admin Mine/All-team toggle uses on /calls and
         /calls/trash (#176). Disabled mid-capture so the user can't
         flip the mode while a recording is live (the running session
         was started in the previous mode; switching the label would
         lie about what the Stop button is going to do). Copy-notice
         button is hidden in Note mode — a self-note has only one
         participant, no consent message to paste. -->
    <div class="record-tabs" role="tablist" aria-label="Recording mode">
      <button
        type="button"
        role="tab"
        aria-selected={recordMode === "call"}
        class="scope-pill"
        class:active={recordMode === "call"}
        disabled={recording}
        onclick={() => setRecordMode("call")}
      >
        Call
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={recordMode === "note"}
        class="scope-pill"
        class:active={recordMode === "note"}
        disabled={recording}
        onclick={() => setRecordMode("note")}
      >
        Note
      </button>
    </div>
    <!-- #443 — subtitle clarifies what each mode captures and which
         path triggers consent. Helps a user who misclicks Note when
         they meant Call (or vice versa) realise the consent flow
         differs. -->
    <p class="record-tabs-sub" aria-live="polite">
      {#if recordMode === "call"}
        Call: records your mic + system audio for transcription.
        Requires participant consent acknowledgement.
      {:else}
        Note: records your mic only — for personal dictation. No
        consent prompt; only you are captured.
      {/if}
    </p>

    <!-- #531 — optional note title. Shown in Note mode when idle so the
         user can name the note before hitting record. Flushed to title.txt
         in the session_dir via save_title when recording actually starts.
         Hidden while recording so the UI doesn't shift mid-capture. -->
    {#if recordMode === "note" && !recording}
      <div class="note-title-row">
        <label class="note-title-label" for="note-title">
          Title
          <span class="note-title-opt">(optional)</span>
        </label>
        <input
          id="note-title"
          type="text"
          class="note-title-input"
          placeholder="e.g. Meeting recap, Idea dump…"
          bind:value={noteTitle}
          maxlength="200"
          autocomplete="off"
          oninput={() => {
            if (currentSessionId) scheduleNoteTitleSave(noteTitle);
          }}
        />
        {#if noteTitleSaved}
          <span class="note-title-saved" aria-live="polite">Saved</span>
        {/if}
      </div>
    {/if}

    <div class="cta-row">
      <!-- Primary action — button flips to live state with inline timer.
           In Note mode the verb + aria-label change to "note to self"
           and the keyboard-shortcut tooltip surfaces the combo bound
           to that mode. -->
      <button
        class="record-btn"
        class:live={recording}
        onclick={toggle}
        aria-label={recording
          ? "Stop recording"
          : recordMode === "note"
            ? "Start note to self"
            : "Start recording"}
        title={recordBtnTitle}
      >
        <span class="record-icon">
          {#if recording}
            <svg viewBox="0 0 20 20" width="14" height="14" aria-hidden="true">
              <rect x="4.5" y="4.5" width="11" height="11" rx="1.5" fill="currentColor" />
            </svg>
          {:else if recordMode === "note"}
            <!-- Mic glyph for Note mode so the icon also signals
                 "mic-only dictation" (no system loopback). -->
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
              <rect x="6" y="2" width="4" height="7" rx="2" fill="none" stroke="currentColor" stroke-width="1.4" />
              <path d="M4 8.5a4 4 0 0 0 8 0" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
              <path d="M8 12.5v1.5M6.2 14h3.6" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
            </svg>
          {:else}
            <svg viewBox="0 0 20 20" width="14" height="14" aria-hidden="true">
              <circle cx="10" cy="10" r="5" fill="currentColor" />
            </svg>
          {/if}
        </span>
        <span class="record-text">
          {recording
            ? "Stop recording"
            : recordMode === "note"
              ? "Start note to self"
              : "Start recording"}
        </span>
        {#if recording}
          <span class="record-timer">{fmtTimer(elapsedMs)}</span>
        {/if}
      </button>

      <!-- Copy a PIPEDA-compliant recording notice to the clipboard
           (#45) so the user can paste it into meeting chat as the
           "I'm recording, here's why, here's the retention posture"
           heads-up to other participants. Hidden in Note mode — no
           other participant on a self-note.
           #479 — clicking opens a preview panel first so the user can
           read the full message before it goes to the clipboard. -->
      {#if recordMode === "call"}
        <div class="notice-wrap">
          <button
            type="button"
            class="copy-notice-btn"
            class:active={noticePreview !== null}
            onclick={openNoticePreview}
            disabled={copyingNotice}
            aria-expanded={noticePreview !== null}
            aria-haspopup="true"
            aria-live="polite"
          >
            {copiedNotice ? "Copied ✓" : copyingNotice ? "Loading…" : "Copy notice"}
          </button>
          {#if noticePreview !== null}
            <!-- Dismiss on outside click -->
            <div
              class="notice-backdrop"
              role="button"
              tabindex="-1"
              aria-label="Close preview"
              onclick={() => (noticePreview = null)}
              onkeydown={(e) => e.key === "Escape" && (noticePreview = null)}
            ></div>
            <div class="notice-preview" role="dialog" aria-label="Recording notice preview" aria-modal="false">
              <p class="notice-preview-label">Notice text</p>
              <p class="notice-preview-text">{noticePreview}</p>
              <div class="notice-preview-actions">
                <button
                  type="button"
                  class="btn primary"
                  onclick={copyNotice}
                >
                  Copy to clipboard
                </button>
                <button
                  type="button"
                  class="btn ghost"
                  onclick={() => (noticePreview = null)}
                >
                  Cancel
                </button>
              </div>
            </div>
          {/if}
        </div>
      {/if}
    </div>

    <div class="cta-secondary">
      {#if activeShortcutKeys.length}
        <span class="kbd-row" title={recordBtnTitle}>
          {#each activeShortcutKeys as part (part)}
            <span class="kbd">{part}</span>
          {/each}
        </span>
        <span class="sep">·</span>
      {:else}
        <!-- #512 — when no shortcut is bound, give users a one-click
             path to Settings instead of a dead-end tooltip. -->
        <a class="link set-shortcut" href="/settings#shortcuts">
          Set shortcut →
        </a>
        <span class="sep">·</span>
      {/if}
      <button class="link" disabled={importing} onclick={importRecording}>
        {importing ? "Importing…" : "Import a file"}
      </button>
    </div>

    <!-- Linux-only persistent-dismissible note (#6). Points users at
         the help page for the compositor-bound CLI workaround — the
         in-app plugin hotkey doesn't reach the app when it's
         unfocused on most Linux desktop environments. Copy stays
         vendor-opaque (no compositor/DE names); the help page is
         where specifics live. -->
    {#if showHotkeyNote}
      <div class="hotkey-note" role="note">
        <p class="hotkey-note-text">
          For this shortcut to work when the app isn't focused, you'll
          need to set it up in your desktop environment's keyboard
          shortcut settings.
          <button
            type="button"
            class="hotkey-note-link"
            onclick={openHotkeyHelp}
          >
            See the help page
          </button>
          for instructions.
        </p>
        <button
          type="button"
          class="hotkey-note-dismiss"
          onclick={dismissHotkeyNote}
          aria-label="Dismiss"
          title="Dismiss"
        >
          ×
        </button>
      </div>
    {/if}

    {#if copyError}
      <p class="inline-error">{copyError}</p>
    {/if}
  </section>

  <!-- Manual notes panel (#73 #346). Visible during an active
       recording AND through the post-stop pipeline window (#344).
       Shown when manualNotesEnabled (Settings pref) OR sessionNotesOpen
       (inline toggle, #346). Keyed on session_id so a new recording
       gets a fresh editor state. #194: value={notesInitial} seeds the
       editor from notes.md on re-entry. Unmounts on done/failed. -->
  {#if showNotes && currentSessionId && (recording || (pipelineStage && pipelineStage !== "done" && pipelineStage !== "failed"))}
    <section class="notes" style="--i: 3">
      {#key currentSessionId}
        <NotesPanel value={notesInitial} onchange={scheduleNotesSave} />
      {/key}
    </section>
  {/if}

  <!-- #653 — in-call co-pilot. When the org has copilot ON and we're in
       Call mode, the CoPilotPanel mounts PRE-call (so the "who are you
       calling?" picker is available before Start) and stays through the
       post-stop draft. It composes the transcript, CRM-context, and coaching
       lanes. Note mode has no CRM/coaching counterpart, so it falls through
       to the plain Phase-1 panel (which stays hidden with no live segments).
       Copilot OFF preserves today's behavior exactly: the plain
       LiveAssistPanel renders when live_transcript is ON. -->
  {#if copilotEnabled && recordMode === "call"}
    {#if callEnded}
      <!-- Co-pilot follow-ups (D) — call-end state. Once a Call-mode recording stops the
           co-pilot swaps to this ended panel (rather than lingering on the
           stale live dashboard). While the finished call_id isn't known yet it
           shows the pipeline progress; once `openableCallId` arrives the
           primary "Open post-call" CTA is enabled. Dismiss closes it; a new
           record-start / navigating away also clears it — never a blind timer.
           Copy is vendor-opaque. -->
      <section class="call-ended" style="--i: 2.5" aria-label="Call ended">
        <div class="ce-head">
          <span
            class="ce-pip"
            class:ready={!!openableCallId}
            class:failed={pipelineStage === "failed"}
            aria-hidden="true"
          ></span>
          <span class="ce-title">Call ended</span>
          <button
            type="button"
            class="ce-dismiss"
            aria-label="Dismiss"
            title="Dismiss"
            onclick={dismissCallEnded}
          >
            <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
          </button>
        </div>
        {#if openableCallId}
          <p class="ce-body">Your call is ready to review.</p>
          <div class="ce-actions">
            <button type="button" class="ce-open" onclick={openCallInApp}>
              Open post-call
            </button>
          </div>
          <!-- Phase 3 — call-end CRM push. One prior-push probe (fired once
               `done`) drives BOTH states: a prior push (auto mode fired it, or a
               manual push) → the "Pushed to <Deal>" confirmation; else a deal
               linked this call → the "Push this call?" prompt. No linked deal +
               no prior push → nothing (the Phase-1 card is unchanged). Copy is
               vendor-opaque — "Zoho" is the sanctioned name. -->
          {#if pipelineStage === "done" && endedPushLoaded}
            {#if endedPriorPush}
              <div class="ce-crm" role="status" aria-live="polite">
                <span class="ce-crm-pip" aria-hidden="true"></span>
                <span class="ce-crm-text">
                  Pushed to {endedPriorPush.record_name ?? "your CRM"}
                </span>
                {#if endedPriorPush.zoho_url}
                  <button
                    type="button"
                    class="ce-crm-link"
                    onclick={() => openZohoUrl(endedPriorPush!.zoho_url!)}
                  >
                    View in Zoho ↗
                  </button>
                {/if}
              </div>
            {:else if liveSession.linkedDeal && !endedSkipped}
              <div class="ce-crm ce-crm-prompt">
                <span class="ce-crm-text">
                  Push this call to {liveSession.linkedDeal.record_name}?
                </span>
                <div class="ce-crm-actions">
                  <button
                    type="button"
                    class="ce-crm-push"
                    disabled={endedPushing}
                    onclick={pushEndedCall}
                  >
                    {endedPushing ? "Pushing…" : "Push"}
                  </button>
                  <button
                    type="button"
                    class="ce-crm-skip"
                    disabled={endedPushing}
                    onclick={skipEndedPush}
                  >
                    Skip
                  </button>
                </div>
              </div>
            {/if}
          {/if}
        {:else if pipelineStage === "failed"}
          <p class="ce-body ce-body-muted">
            We couldn't finish preparing this call. It'll appear in your calls
            list if it becomes available.
          </p>
        {:else}
          <p class="ce-body ce-body-muted" role="status" aria-live="polite">
            {(pipelineStage && pipelineLabels[pipelineStage]) ||
              "Preparing your call"}…
          </p>
        {/if}
      </section>
    {:else if !copilotDismissed}
      <CoPilotPanel
        segments={liveSession.segments}
        status={liveSession.status}
        {recording}
        isAdmin={copilotIsAdmin}
        defaultMode={copilotDefaultMode}
        {liveTranscriptEnabled}
        coaching={liveSession.coaching}
        cues={liveSession.liveCues}
        checklist={liveSession.checklist}
        questions={liveSession.questions}
        sessionUuid={liveSession.sessionUuid}
        {elapsedMs}
        onToggleRecording={toggle}
        onnotes={() => (sessionNotesOpen = true)}
        askAnswer={liveSession.askAnswer}
        askInFlight={liveSession.askInFlight}
        knowledgeAnswer={liveSession.knowledgeAnswer}
        knowledgeInFlight={liveSession.knowledgeInFlight}
        highlighted={liveSession.highlighted}
        speakerIdentities={liveSession.speakerIdentities}
        talkMetric={liveSession.talkMetric}
        onask={handleAsk}
        ondismissAsk={handleDismissAsk}
        onknowledge={handleKnowledge}
        ondismissKnowledge={handleDismissKnowledge}
        onhighlight={handleHighlight}
        onassignSpeaker={handleAssignSpeaker}
        onpick={(id) => (contactHint = id)}
        linkedDeal={liveSession.linkedDeal}
        onlinkdeal={handleLinkDeal}
        onunlinkdeal={handleUnlinkDeal}
      />
    {/if}
  {:else if liveTranscriptEnabled && (recording || liveSession.segments.length > 0 || liveSession.status === "error")}
    <LiveAssistPanel segments={liveSession.segments} status={liveSession.status} />
  {/if}

  <!-- Status lane — stacks so a pipeline still in flight is still
       visible while the user records the next call. -->
  <section class="status" style="--i: 4">
    {#if recording}
      <div class="row row-live">
        <span class="row-dot live"></span>
        <span class="row-title">Recording mic + system audio</span>
        <!-- #346 — inline notes toggle. Surfaces the panel without
             requiring a trip to Settings. Disabled once the session is
             off (the button becomes irrelevant after stop). -->
        <button
          class="notes-toggle"
          class:notes-toggle-on={showNotes}
          onclick={() => {
            sessionNotesOpen = !showNotes;
            // If the Settings pref is already on, turning the toggle
            // "off" here only kills the session flag; the Settings
            // pref wins on the next recording. That's the correct
            // behaviour — we don't mutate Settings from this button.
            if (manualNotesEnabled) {
              sessionNotesOpen = !manualNotesEnabled;
            }
          }}
          title={showNotes ? "Hide notes" : "Open notes"}
          aria-pressed={showNotes}
        >
          <svg viewBox="0 0 14 14" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M2 2h10v10H2zM2 5h10M5 5v7"/>
          </svg>
          {showNotes ? "Notes on" : "Notes"}
        </button>
      </div>
    {/if}
    <!-- #602/WS-G — the generic "Failed" row is suppressed when the
         failure is a billing gate; the friendly subscribe block below
         is the single, clear message for that case. -->
    {#if pipelineStage && !subGate}
      <div
        class="row"
        class:row-done={pipelineStage === "done"}
        class:row-failed={pipelineStage === "failed"}
      >
        <span class="row-dot {pipelineStage}"></span>
        <div class="row-body">
          <p class="row-title">
            {pipelineLabels[pipelineStage] ?? pipelineStage}
          </p>
        </div>
        <!-- #351 — Open buttons held until the pipeline reaches done.
             Early intermediate stages (transcribed, summarizing, etc.)
             produce an incomplete call record; surfacing the link too
             early can confuse users who land on a call with no summary
             yet. We wait for pipelineStage === "done" so the full
             record is ready. -->
        {#if openableCallId && pipelineStage === "done"}
          <div class="row-actions">
            <button class="open-app" onclick={openCallInApp}>
              Open in app
            </button>
            <!-- Demoted to icon-only external link; tooltip carries the
                 full label so keyboard + pointer users can discover it
                 without the button competing visually with the primary. -->
            <button
              class="open-web"
              onclick={openCallInBrowser}
              aria-label="Open on web"
              title="Open on web"
            >↗</button>
          </div>
        {/if}
      </div>
    {/if}
    {#if !recording && !pipelineStage}
      <p class="idle">Ready when you are.</p>
    {/if}

    <!-- #602/WS-G — subscription gate. Recording captured fine but the
         backend declined to file the call because the org's plan isn't
         active (402 subscription_inactive). Friendly, action-oriented
         block — full billing management lives on the portal, opened in
         the system browser. -->
    {#if subGate}
      <div class="sub-gate" role="alert">
        <div class="sub-gate-body">
          <p class="sub-gate-title">Couldn't save this recording</p>
          <p class="sub-gate-text">
            Recording needs an active plan. Subscribe to keep recording,
            transcribing, and saving your calls.
          </p>
        </div>
        <div class="sub-gate-actions">
          <button type="button" class="btn primary" onclick={openBilling}>
            Subscribe <span aria-hidden="true">↗</span>
          </button>
        </div>
      </div>
    {/if}
    {#if pipelineError}
      <p class="inline-error">{pipelineError}</p>
    {/if}
    {#if error}
      <p class="inline-error">{error}</p>
    {/if}
  </section>

  <!-- #523 — Recently recorded strip. Shown when idle (not recording,
       no active pipeline) and at least one call completed this session.
       In-memory only — no backend fetch; populated from pipeline `done`
       events fired while the webview is open. -->
  {#if !recording && !pipelineStage && recentCalls.length > 0}
    <section class="recent" style="--i: 5">
      <p class="recent-label">Recently recorded</p>
      <ul class="recent-list">
        {#each recentCalls as rc (rc.id)}
          <li>
            <a href="/calls/{rc.id}" class="recent-entry">
              <span class="recent-icon" aria-hidden="true">
                <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M10 2a3 3 0 0 0-3 3v3a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" fill="currentColor" stroke="none"/>
                  <path d="M5 7v1a5 5 0 0 0 10 0V7M10 13v2M7.5 15h5" />
                </svg>
              </span>
              <span class="recent-name">Recorded call</span>
              <span class="recent-time">{fmtRelative(rc.completedAt)}</span>
              <span class="recent-arrow" aria-hidden="true">→</span>
            </a>
          </li>
        {/each}
      </ul>
    </section>
  {/if}
</main>

<!-- Recording-ack modal (#44). Blocking — the user can only proceed
     by ticking the box and confirming, or by cancelling (no record).
     Visual vocabulary (rn-backdrop/rn-modal/rn-head/rn-actions) mirrors
     the release-notes modal in +layout.svelte so we don't fork styling. -->
{#if ackModalOpen}
  <div
    class="rn-backdrop"
    role="button"
    tabindex="-1"
    onclick={cancelAck}
    onkeydown={(e) => {
      if (e.key === "Escape") cancelAck();
    }}
  >
    <div
      class="rn-modal ack-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="ack-title"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      tabindex="-1"
    >
      <div class="rn-head">
        <h2 id="ack-title">Get consent before recording</h2>
      </div>
      <p class="ack-body">
        By recording you confirm that you'll tell everyone on the call
        that it's being recorded, get their consent, and use the
        recording only for the purpose you disclosed.{#if screenCaptureOn}
          You also confirm that you'll tell them their screen-share may
          be recorded as video while the call is recording.{/if} Your
        audio will be transcribed and summarized by automated services
        to generate notes for you.
      </p>

      <label class="ack-check">
        <input
          type="checkbox"
          bind:checked={ackChecked}
          disabled={ackSubmitting}
        />
        <span>I'll get consent from everyone on this call.</span>
      </label>

      <!-- #409 — "Read full guide" opens the help page in the system
           browser via openUrl; the modal stays open with the checkbox
           state intact so the user can return and confirm without
           re-reading or re-checking. -->
      <button type="button" class="ack-guide" onclick={openConsentGuide}>
        Read full guide <span aria-hidden="true">↗</span>
      </button>

      {#if ackError}
        <p class="inline-error ack-err">{ackError}</p>
      {/if}

      <div class="rn-actions ack-actions">
        <button
          type="button"
          class="ack-cancel"
          onclick={cancelAck}
          disabled={ackSubmitting}
        >
          Cancel
        </button>
        <button
          type="button"
          class="rn-dismiss"
          onclick={submitAck}
          disabled={!ackChecked || ackSubmitting}
        >
          {ackSubmitting ? "Saving…" : "I understand — start recording"}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .page {
    max-width: 560px;
    margin: 0 auto;
    padding: 2.4rem 2rem 4rem;
    display: flex;
    flex-direction: column;
    gap: 1.4rem;
    position: relative;
    z-index: 2;
  }

  /* #653 — the co-pilot panel (transcript + CRM + coaching) needs room the
     narrow record column can't give. Widen the column only when copilot is
     active (Call mode). This rule is component-scoped (`.page` lives ONLY in
     this route's <style>, verified 0 occurrences in app.css), so hard-rule #1
     is not engaged — no app.css edit, no byte-identical diff to maintain.
     The narrower children (.cta, .record-btn, .note-title-row) keep their own
     max-widths and simply center in the wider column; only the co-pilot panel
     consumes the extra width. */
  .page.copilot-active {
    max-width: 900px;
  }

  .head h1 {
    font-size: 1.55rem;
    margin-bottom: 0.35rem;
  }

  .sub {
    margin: 0;
    color: var(--bone-2);
    font-size: 0.88rem;
    max-width: 44ch;
  }

  /* ── Banner ────────────────────────────────────────────────────────── */
  .banner {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: 0.9rem 1rem;
    border: 1px solid var(--hairline);
    border-left: 2px solid var(--sig);
    border-radius: var(--radius);
    background: var(--ink-1);
  }

  .banner-body {
    flex: 1;
    min-width: 0;
  }

  .banner-label {
    margin: 0 0 0.15rem;
    font-family: var(--font-mono);
    font-size: 0.68rem;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--sig);
  }

  .banner-text {
    margin: 0;
    color: var(--bone-0);
    font-size: 0.9rem;
  }

  .banner-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }

  /* ── CTA ───────────────────────────────────────────────────────────── */
  .cta {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.85rem;
    padding: 0.4rem 0 0.8rem;
  }

  /* #142 follow-up — Call / Note mode tabs. Mirrors the .scope-pill
     pattern shipped in #176 (agent + portal trash views) so the
     visual vocabulary across "binary toggle on top of a list" is
     consistent. Pills are local to this route — design.md treats
     scope pills as a shared idiom but each route currently inlines
     them, so we follow precedent rather than promote into app.css. */
  .record-tabs {
    display: flex;
    gap: 0.3rem;
  }
  .scope-pill {
    padding: 0.4rem 0.9rem;
    border: 1px solid var(--hairline);
    background: var(--ink-1);
    color: var(--bone-2);
    border-radius: 999px;
    font-size: 0.82rem;
    cursor: pointer;
    transition:
      border-color 0.15s,
      color 0.15s,
      background 0.15s;
  }
  .scope-pill:hover:not(:disabled):not(.active) {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .scope-pill.active {
    border-color: var(--accent);
    color: var(--accent-hi);
    background: var(--accent-soft);
  }
  .scope-pill:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  /* #443 — subtitle row beneath the Call/Note pills. Sits above the
     CTA row, shares the .cta-secondary text weight so it reads as
     hint copy, not chrome. Max-width keeps it from outrunning the
     pills on a wide-enough viewport. */
  .record-tabs-sub {
    margin: 0.45rem 0 0;
    max-width: 38ch;
    font-size: 0.78rem;
    line-height: 1.35;
    color: var(--bone-3);
  }

  /* #531 — optional note title row */
  .note-title-row {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    width: 100%;
    max-width: 360px;
  }
  .note-title-label {
    font-size: 0.78rem;
    color: var(--bone-3);
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .note-title-opt {
    color: var(--bone-4);
    font-size: 0.7rem;
  }
  .note-title-input {
    width: 100%;
    padding: 0.5rem 0.7rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 7px;
    background: var(--ink-1);
    color: var(--bone-0);
    font-size: 0.88rem;
    font-family: var(--font-sans);
    transition: border-color 0.15s, box-shadow 0.15s;
    box-sizing: border-box;
  }
  .note-title-input::placeholder {
    color: var(--bone-4);
  }
  .note-title-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-glow);
  }
  .note-title-saved {
    font-size: 0.72rem;
    color: var(--olive);
  }

  /* Primary + Copy notice share a row; Copy notice is the ghost
     counterpart to the filled record button. Wraps to stack on very
     narrow widths so the timer-expanded pill doesn't squeeze Copy
     notice off the edge. */
  .cta-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex-wrap: wrap;
  }

  /* #479 — notice wrapper provides positioning context for the preview
     popover. (#608: stacking-context bits live with the popover rules
     a few blocks below.) */
  .notice-wrap {
    position: relative;
  }

  .copy-notice-btn {
    display: inline-flex;
    align-items: center;
    padding: 0.6rem 0.95rem;
    border-radius: 999px;
    border: 1px solid var(--hairline-hi);
    background: transparent;
    color: var(--bone-1);
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }
  .copy-notice-btn:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--bone-0);
  }
  .copy-notice-btn.active {
    border-color: var(--accent);
    color: var(--accent-hi);
    background: var(--accent-soft);
  }
  .copy-notice-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }

  /* #479 — notice preview popover.
     #608 — bumped z-index (and lifted notice-wrap to a dedicated
     stacking context) so the popover paints above the Notes panel
     rendered below it on the Record screen. The previous values
     (10/9) lost to the Notes editor's own paint order on Linux
     WebKitGTK, leaving the bottom of the notice card (Cancel button)
     hidden behind the toolbar. */
  .notice-wrap {
    isolation: isolate;
    z-index: 50;
  }
  .notice-backdrop {
    position: fixed;
    inset: 0;
    z-index: 99;
  }
  .notice-preview {
    position: absolute;
    top: calc(100% + 0.5rem);
    left: 0;
    z-index: 100;
    width: 320px;
    max-width: calc(100vw - 2rem);
    padding: 1rem;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-1);
    box-shadow: 0 4px 20px rgba(0, 0, 0, 0.35);
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  .notice-preview-label {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 0.65rem;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--bone-3);
  }
  .notice-preview-text {
    margin: 0;
    font-size: 0.82rem;
    line-height: 1.55;
    color: var(--bone-1);
    white-space: pre-wrap;
  }
  .notice-preview-actions {
    display: flex;
    gap: 0.4rem;
    justify-content: flex-end;
    padding-top: 0.2rem;
  }

  .record-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.85rem 1.4rem;
    border-radius: 999px;
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.95rem;
    font-weight: 600;
    letter-spacing: -0.005em;
    transition:
      background 0.15s,
      transform 0.1s,
      box-shadow 0.2s;
    box-shadow:
      0 1px 0 rgba(255, 255, 255, 0.12) inset,
      0 12px 28px -14px var(--accent-glow);
  }

  .record-btn:hover {
    background: var(--accent-hi);
    box-shadow:
      0 1px 0 rgba(255, 255, 255, 0.14) inset,
      0 14px 32px -14px var(--accent-glow);
  }
  .record-btn:active {
    transform: translateY(1px);
  }

  .record-btn.live {
    background: var(--live);
    color: #fff;
    box-shadow:
      0 1px 0 rgba(255, 255, 255, 0.18) inset,
      0 0 0 6px var(--live-soft);
    animation: live-breathe 1.6s ease-in-out infinite;
  }

  @keyframes live-breathe {
    0%,
    100% {
      box-shadow:
        0 1px 0 rgba(255, 255, 255, 0.18) inset,
        0 0 0 6px var(--live-soft);
    }
    50% {
      box-shadow:
        0 1px 0 rgba(255, 255, 255, 0.18) inset,
        0 0 0 10px rgba(255, 80, 50, 0.08);
    }
  }

  .record-icon {
    display: inline-flex;
    align-items: center;
  }

  .record-text {
    display: inline-block;
  }

  .record-timer {
    font-family: var(--font-mono);
    font-weight: 500;
    font-size: 0.85rem;
    letter-spacing: 0.04em;
    padding-left: 0.6rem;
    margin-left: 0.2rem;
    border-left: 1px solid rgba(255, 255, 255, 0.25);
  }

  .cta-secondary {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.8rem;
    color: var(--bone-3);
  }

  .kbd-row {
    display: inline-flex;
    gap: 0.18rem;
  }

  .kbd {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    padding: 0.12rem 0.4rem;
    border: 1px solid var(--hairline);
    border-radius: 4px;
    background: var(--ink-2);
    color: var(--bone-2);
    letter-spacing: 0.02em;
  }

  .sep {
    color: var(--bone-4);
  }

  .link {
    color: var(--bone-1);
    font: inherit;
    padding: 0;
    transition: color 0.15s;
  }
  .link:hover:not(:disabled) {
    color: var(--accent);
  }
  /* #512 — actionable "Set shortcut →" link surfaced next to the
     record button when no shortcut is bound. Anchor styling so it
     matches the kbd-row visual weight. */
  a.set-shortcut {
    text-decoration: none;
  }
  .link:disabled {
    opacity: 0.6;
    cursor: default;
  }

  /* ── Dismissible inline note ───────────────────────────────────────── */
  /* Persistent, pref-backed variant of .banner — small, ambient, and
     dismissed to a user pref (not just local component state) so it
     doesn't come back on the next restart. See design.md §Dismissible
     inline note. */
  .hotkey-note {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.55rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
    margin-top: 0.1rem;
  }
  .hotkey-note-text {
    flex: 1;
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--bone-2);
  }
  .hotkey-note-link {
    color: var(--accent);
    font: inherit;
    padding: 0;
    background: transparent;
    border: 0;
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .hotkey-note-link:hover {
    color: var(--accent-hi);
  }
  .hotkey-note-dismiss {
    flex-shrink: 0;
    width: 1.4rem;
    height: 1.4rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: 0;
    padding: 0;
    background: transparent;
    color: var(--bone-3);
    font-size: 1.1rem;
    line-height: 1;
    border-radius: 4px;
    cursor: pointer;
    transition: color 0.15s, background 0.15s;
  }
  .hotkey-note-dismiss:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }

  /* ── Shared button ─────────────────────────────────────────────────── */
  .btn {
    padding: 0.42rem 0.85rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 8px;
    background: var(--ink-2);
    color: var(--bone-0);
    font-size: 0.82rem;
    font-weight: 500;
    transition: all 0.15s;
  }
  .btn:hover {
    border-color: var(--bone-3);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--ink-0);
  }
  .btn.primary:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .btn.ghost {
    background: transparent;
  }

  /* ── #602/WS-G — subscription gate block ───────────────────────────── */
  /* Shares the .banner anatomy (sig-gold left border = "heads up, check
     this" per design.md §Semantic colors) with a title + action layout.
     The CTA reuses the page's own .btn.primary (accent) — Subscribe is a
     call to action, not a destructive/error signal. */
  .sub-gate {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: 1rem 1.1rem;
    border: 1px solid var(--hairline);
    border-left: 3px solid var(--sig);
    border-radius: var(--radius);
    background: var(--ink-1);
  }
  .sub-gate-body {
    flex: 1;
    min-width: 0;
  }
  .sub-gate-title {
    margin: 0 0 0.25rem;
    font-size: 0.95rem;
    font-weight: 600;
    color: var(--bone-0);
  }
  .sub-gate-text {
    margin: 0;
    font-size: 0.85rem;
    line-height: 1.5;
    color: var(--bone-2);
    max-width: 44ch;
  }
  .sub-gate-actions {
    flex-shrink: 0;
  }

  /* ── Notes panel (#73) ─────────────────────────────────────────────── */
  .notes {
    display: block;
  }

  /* ── Status lane ───────────────────────────────────────────────────── */
  .status {
    min-height: 3rem;
  }

  .idle {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.88rem;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 0.85rem;
    padding: 0.75rem 1rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
  }

  .row-live {
    border-color: rgba(255, 80, 50, 0.3);
  }
  .row-done {
    border-color: rgba(143, 175, 114, 0.3);
  }
  .row-failed {
    border-color: rgba(255, 80, 50, 0.4);
  }

  .row-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--bone-3);
    flex-shrink: 0;
  }
  .row-dot.live {
    background: var(--live);
    box-shadow: 0 0 8px var(--live);
    animation: pip-pulse 1.1s ease-in-out infinite;
  }
  .row-dot.started,
  .row-dot.transcribing,
  .row-dot.summarizing,
  .row-dot.writing_note,
  .row-dot.uploading {
    background: var(--sig);
    animation: pip-blink 1s infinite;
  }
  .row-dot.done {
    background: var(--olive);
  }
  .row-dot.failed {
    background: var(--live);
  }

  .row-body {
    flex: 1;
    min-width: 0;
  }

  .row-title {
    margin: 0;
    font-size: 0.9rem;
    color: var(--bone-0);
    font-weight: 500;
  }

  .row-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }

  /* #346 — inline notes toggle on the live-recording row. */
  .notes-toggle {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.25rem 0.6rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 6px;
    background: var(--ink-2);
    color: var(--bone-2);
    font-size: 0.78rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
    flex-shrink: 0;
  }
  .notes-toggle:hover {
    color: var(--bone-0);
    border-color: var(--accent);
  }
  .notes-toggle-on {
    color: var(--accent);
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .notes-toggle-on:hover {
    color: var(--accent-hi);
  }
  .open-app {
    padding: 0.35rem 0.75rem;
    border: 1px solid var(--accent);
    border-radius: 6px;
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.78rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }
  .open-app:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  /* Demoted external-link: icon-only square pill. Tooltip carries the
     full "Open on web" label; visually it just shows ↗ so it doesn't
     compete with the filled primary button. */
  .open-web {
    padding: 0.35rem 0.5rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 6px;
    background: transparent;
    color: var(--bone-3);
    font-size: 0.78rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }
  .open-web:hover {
    color: var(--bone-0);
    border-color: var(--bone-2);
  }

  /* ── Co-pilot follow-ups (D) · Call-ended panel ────────────────────────────────────
     The co-pilot region's post-stop state: a compact card that carries the
     pipeline progress until the call is openable, then the primary
     "Open post-call" CTA. Component-scoped (`.ce-*` live ONLY in this
     route's <style>) — no app.css touch. */
  .call-ended {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
    padding: 1rem 1.1rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
  }
  .ce-head {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  .ce-pip {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--sig);
    flex-shrink: 0;
    animation: pip-blink 1s infinite;
  }
  .ce-pip.ready {
    background: var(--olive);
    animation: none;
  }
  .ce-pip.failed {
    background: var(--live);
    animation: none;
  }
  @media (prefers-reduced-motion: reduce) {
    .ce-pip {
      animation: none;
    }
  }
  .ce-title {
    font-size: 0.72rem;
    letter-spacing: 0.07em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .ce-dismiss {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    margin-left: auto;
    width: 1.5rem;
    height: 1.5rem;
    padding: 0;
    border: none;
    border-radius: 50%;
    background: transparent;
    color: var(--bone-3);
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }
  .ce-dismiss:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .ce-dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .ce-body {
    margin: 0;
    font-size: 0.9rem;
    line-height: 1.45;
    color: var(--bone-0);
  }
  .ce-body-muted {
    color: var(--bone-2);
  }
  .ce-actions {
    display: flex;
    gap: 0.5rem;
  }
  .ce-open {
    padding: 0.45rem 0.95rem;
    border: 1px solid var(--accent);
    border-radius: 6px;
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.82rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }
  .ce-open:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .ce-open:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* ── Phase 3 · Call-end CRM push (prompt + confirmation) ───────────────────
     A quiet sub-block under the "Open post-call" action. The confirmation reads
     as saved (olive pip, per design.md §Semantic — "complete/saved"); the prompt
     is a plain ask with an accent Push CTA + a subordinate Skip. Component-scoped
     (`.ce-crm-*` live ONLY in this route's <style>) — no app.css touch. */
  .ce-crm {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
    padding-top: 0.7rem;
    margin-top: 0.1rem;
    border-top: 1px solid var(--hairline);
  }
  .ce-crm-prompt {
    justify-content: space-between;
  }
  .ce-crm-pip {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--olive);
    box-shadow: 0 0 5px rgba(143, 175, 114, 0.5);
    flex-shrink: 0;
  }
  .ce-crm-text {
    font-size: 0.85rem;
    color: var(--bone-1);
    min-width: 0;
  }
  .ce-crm-link {
    display: inline-flex;
    align-items: center;
    padding: 0.2rem 0.55rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: transparent;
    color: var(--accent-hi);
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s, background 0.15s;
  }
  .ce-crm-link:hover {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-hi);
  }
  .ce-crm-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .ce-crm-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .ce-crm-push {
    padding: 0.35rem 0.85rem;
    border: 1px solid var(--accent);
    border-radius: 6px;
    background: var(--accent);
    color: var(--ink-0);
    font: inherit;
    font-size: 0.8rem;
    font-weight: 500;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s;
  }
  .ce-crm-push:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .ce-crm-push:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .ce-crm-push:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .ce-crm-skip {
    padding: 0.35rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: transparent;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
  }
  .ce-crm-skip:hover:not(:disabled) {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }
  .ce-crm-skip:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .ce-crm-skip:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .inline-error {
    margin: 0.5rem 0 0;
    color: var(--live);
    font-size: 0.85rem;
  }

  @keyframes pip-pulse {
    0%,
    100% {
      transform: scale(1);
    }
    50% {
      transform: scale(1.35);
    }
  }
  @keyframes pip-blink {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.5;
    }
  }

  /* ── Recently recorded strip (#523) ───────────────────────────────── */
  .recent {
    padding-top: 0.4rem;
    border-top: 1px solid var(--hairline);
  }
  .recent-label {
    margin: 0 0 0.6rem;
    font-family: var(--font-mono);
    font-size: 0.66rem;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--bone-4);
  }
  .recent-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
  }
  .recent-entry {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.45rem 0.65rem;
    border: 1px solid var(--hairline);
    border-radius: 7px;
    background: var(--ink-1);
    color: var(--bone-1);
    font-size: 0.84rem;
    text-decoration: none;
    transition: background 0.12s, border-color 0.12s, color 0.12s;
  }
  .recent-entry:hover {
    background: var(--ink-2);
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }
  .recent-icon {
    color: var(--bone-3);
    flex-shrink: 0;
    display: inline-flex;
  }
  .recent-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .recent-time {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-3);
    flex-shrink: 0;
  }
  .recent-arrow {
    color: var(--bone-4);
    font-size: 0.8rem;
    flex-shrink: 0;
  }

  /* ── PIPEDA ack modal (#44) ──────────────────────────────────────
     Reuses the rn-backdrop / rn-modal / rn-head / rn-actions /
     rn-dismiss visual vocabulary — the same tokens the release-notes
     modal uses. The shared rules for those classes are hoisted out
     of both call sites and live in app.css (§Release-notes modal in
     design.md); local styles below only add the ack-specific bits. */

  .ack-body {
    margin: 0 0 0.85rem;
    font-size: 0.9rem;
    line-height: 1.55;
    color: var(--bone-1);
  }
  .ack-body.ack-emph {
    color: var(--bone-0);
    font-weight: 500;
  }
  .ack-check {
    display: flex;
    align-items: flex-start;
    gap: 0.55rem;
    margin: 0.4rem 0 0.35rem;
    padding: 0.6rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    font-size: 0.9rem;
    color: var(--bone-0);
    cursor: pointer;
  }
  .ack-check input[type="checkbox"] {
    margin-top: 0.2rem;
    width: 14px;
    height: 14px;
    accent-color: var(--accent);
    cursor: pointer;
  }
  .ack-guide {
    appearance: none;
    background: transparent;
    border: none;
    padding: 0.2rem 0 0.6rem;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.82rem;
    cursor: pointer;
    text-align: left;
    transition: color 0.15s;
  }
  .ack-guide:hover {
    color: var(--accent-hi);
  }
  .ack-err {
    margin-top: 0.5rem;
  }
  .ack-actions {
    margin-top: 0.4rem;
  }
  .ack-cancel {
    padding: 0.55rem 1rem;
    border: 1px solid var(--hairline-hi);
    background: transparent;
    color: var(--bone-1);
    font-size: 0.88rem;
    font-weight: 500;
    border-radius: 8px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .ack-cancel:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--bone-3);
  }
  .ack-cancel:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
</style>
