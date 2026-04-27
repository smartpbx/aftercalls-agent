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
  import NotesPanel from "$lib/NotesPanel.svelte";
  import { portalErrorToText } from "$lib/portalError";

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

  // Copy-notice button UX (#45).
  let copiedNotice = $state(false);
  let copyingNotice = $state(false);
  let copyError = $state("");

  // Manual notes panel (#73). Opt-in per user via Settings. When on,
  // the record page shows a CodeMirror editor during an active
  // recording; text is debounced-saved to notes.md in the session_dir
  // via the save_notes Tauri command. The pipeline picks that up at
  // create_call time so the backend gets the notes + include flag.
  let manualNotesEnabled = $state(false);

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
  $effect(() => {
    const sid = currentSessionId;
    if (!sid) {
      notesInitial = "";
      notesInitialFor = "";
      notesUserTyped = false;
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
      pipelineStage = p.stage;
      if (p.stage === "failed") {
        // #498 — backend pipeline errors can name internal vendors
        // (AssemblyAI / OpenAI / Postmark / DigitalOcean) when an
        // upstream call fails. Per CLAUDE.md hard rule #2, public-
        // facing copy stays vendor-opaque. Redact vendor names from
        // the pass-through string before showing it to the user;
        // the staff `/staff/agent-logs` view still has the raw
        // unredacted error for debugging.
        pipelineError = redactVendorNames(p.error);
        notifyPipelineFailed();
      }
      if (p.stage === "transcribed") openableCallId = p.call_id;
      if (p.stage === "done") {
        openableCallId = p.call_id;
        notifyPipelineDone();
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
        if (!recording && wasRecording) notifyRecordStop();
        if (recording) {
          pipelineStage = "";
          pipelineError = "";
          openableCallId = "";
          startAt = Date.now();
          timer = window.setInterval(
            () => (elapsedMs = Date.now() - startAt),
            250,
          );
        } else {
          clearInterval(timer);
          elapsedMs = 0;
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
      } | null>("current_user");
      ackCached = !!u?.recording_acknowledged;
    } catch {}

    // Warm the recording-prefs cache so the first Copy-notice click
    // and the first Start Recording don't stall on a round-trip.
    // Silently best-effort; any failure is deferred to click-time.
    loadRecordingPrefs();

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
    sessionDir = await invoke<string>("start_recording");
  }

  // #142 follow-up — note-to-self path from the record-page tab.
  // Same Tauri command the global hotkey + tray menu invoke; mic-only
  // capture, capped by `max_self_note_minutes`. No PIPEDA ack: a
  // self-note has only one participant.
  async function actuallyStartSelfNote() {
    pipelineStage = "";
    pipelineError = "";
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
        try {
          await playStartCueIfEnabled();
        } catch (e) {
          console.warn("start cue failed", e);
        }
        await invoke("confirm_auto_start");
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

  async function copyNotice() {
    copyError = "";
    copyingNotice = true;
    try {
      const prefs = await loadRecordingPrefs();
      if (!prefs) {
        copyError = "Couldn't load your org's recording preferences.";
        return;
      }
      const notice =
        `Heads up — I'm using aftercalls to record and transcribe this ` +
        `call for the purpose of ${prefs.recording_purpose}. The recording ` +
        `is stored on Canadian cloud infrastructure and used only for that ` +
        `purpose. If you'd prefer I didn't, let me know and I'll stop.`;
      await writeText(notice);
      copiedNotice = true;
      setTimeout(() => {
        copiedNotice = false;
      }, 2000);
    } catch (e) {
      copyError = portalErrorToText(e).replace(/^Error:\s*/, "");
    } finally {
      copyingNotice = false;
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
    try {
      await playStartCueIfEnabled();
    } catch (e) {
      console.warn("start cue failed", e);
    }
    await invoke("confirm_auto_start");
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

<main class="page reveal">
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
           other participant on a self-note. -->
      {#if recordMode === "call"}
        <button
          type="button"
          class="copy-notice-btn"
          onclick={copyNotice}
          disabled={copyingNotice}
          aria-live="polite"
        >
          {copiedNotice ? "Copied ✓" : copyingNotice ? "Copying…" : "Copy notice"}
        </button>
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

  <!-- Manual notes panel (#73). Opt-in via Settings; visible during
       an active recording AND through the post-stop pipeline window
       (#344) so a final-second thought typed while the chime is
       still ringing isn't lost. Keyed on session_id so a new
       recording gets a fresh editor state (the TipTap instance is
       destroyed + re-created rather than carrying stale text
       between calls). #194: value={notesInitial} seeds the editor
       from notes.md when the user navigates away and back mid-
       recording. The panel unmounts once the pipeline reaches done
       (or failed) — at that point the call-detail page owns notes
       editing. -->
  {#if manualNotesEnabled && currentSessionId && (recording || (pipelineStage && pipelineStage !== "done" && pipelineStage !== "failed"))}
    <section class="notes" style="--i: 3">
      {#key currentSessionId}
        <NotesPanel value={notesInitial} onchange={scheduleNotesSave} />
      {/key}
    </section>
  {/if}

  <!-- Status lane — stacks so a pipeline still in flight is still
       visible while the user records the next call. -->
  <section class="status" style="--i: 4">
    {#if recording}
      <div class="row row-live">
        <span class="row-dot live"></span>
        <span class="row-title">Recording mic + system audio</span>
      </div>
    {/if}
    {#if pipelineStage}
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
        <!-- Open buttons appear the moment the transcript lands
             (openableCallId set by `transcribed`), NOT only on done.
             The row continues to show Drafting summary / Writing
             note / Saved beneath; the call-detail page polls for
             the rest to fill in live. -->
        {#if openableCallId && pipelineStage !== "failed"}
          <div class="row-actions">
            <button class="open-app" onclick={openCallInApp}>
              Open in app
            </button>
            <button class="open-web" onclick={openCallInBrowser}>
              Open on web ↗
            </button>
          </div>
        {/if}
      </div>
    {/if}
    {#if !recording && !pipelineStage}
      <p class="idle">Ready when you are.</p>
    {/if}

    {#if pipelineError}
      <p class="inline-error">{pipelineError}</p>
    {/if}
    {#if error}
      <p class="inline-error">{error}</p>
    {/if}
  </section>
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
        <h2 id="ack-title">Before you record</h2>
      </div>
      <p class="ack-body">
        You're responsible for telling everyone on the call that
        you're recording, getting their consent, and only using the
        recording for the purpose you disclosed. aftercalls doesn't
        automate that — you do.
      </p>

      <label class="ack-check">
        <input
          type="checkbox"
          bind:checked={ackChecked}
          disabled={ackSubmitting}
        />
        <span>I understand and will get consent from everyone I record.</span>
      </label>

      <button type="button" class="ack-guide" onclick={openConsentGuide}>
        See our recording-consent guide <span aria-hidden="true">↗</span>
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
  .copy-notice-btn:disabled {
    opacity: 0.6;
    cursor: default;
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
  .open-web,
  .open-app {
    padding: 0.35rem 0.75rem;
    border: 1px solid var(--accent);
    border-radius: 6px;
    background: transparent;
    color: var(--accent);
    font-size: 0.78rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }
  .open-app {
    /* Primary action lives in-app — filled accent, external link
       gets the lighter outlined treatment. */
    background: var(--accent);
    color: var(--ink-0);
  }
  .open-web:hover {
    background: var(--accent);
    color: var(--ink-0);
  }
  .open-app:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
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
