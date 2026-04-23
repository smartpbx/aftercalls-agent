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
  let pipelineStage = $state<string>("");
  let pipelineError = $state("");
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
  let currentSessionId = $derived(
    sessionDir ? sessionDir.split(/[\\/]/).filter(Boolean).pop() ?? "" : "",
  );
  let notesSaveTimer = 0;
  let notesPending: string | null = null;
  function scheduleNotesSave(text: string) {
    if (!currentSessionId) return;
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
  let timer = 0;
  let startAt = 0;

  onMount(async () => {
    unlisten = await listen<PipelineEvent>("pipeline", (evt) => {
      const p = evt.payload;
      pipelineError = "";
      pipelineStage = p.stage;
      if (p.stage === "failed") {
        pipelineError = p.error;
        notifyPipelineFailed();
      }
      if (p.stage === "transcribed") openableCallId = p.call_id;
      if (p.stage === "done") {
        openableCallId = p.call_id;
        notifyPipelineDone();
      }
    });
    unlistenState = await listen<{ recording: boolean }>(
      "recording-state",
      (evt) => {
        const wasRecording = recording;
        recording = evt.payload.recording;
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
    // render the notes panel during active recording. Failures here
    // default to `false` (panel hidden) which matches the pref's
    // fresh-install default.
    try {
      const prefs = await invoke<{ manual_notes_enabled?: boolean }>("get_app_prefs");
      manualNotesEnabled = !!prefs?.manual_notes_enabled;
    } catch (e) {
      console.warn("get_app_prefs failed", e);
    }

    // "recording-state" only fires on transitions, so a remount
    // mid-recording (e.g. tray hide+show, route nav) wouldn't know
    // the truth. Ask the backend directly + rebuild the timer from
    // the real start time.
    try {
      const status = await invoke<{
        recording: boolean;
        started_at_ms: number | null;
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
      }
    } catch {}
  });

  onDestroy(() => {
    unlisten?.();
    unlistenState?.();
    unlistenAuto?.();
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
      // Gate on PIPEDA ack (#44) before touching the recorder.
      const ok = await ensureRecordingAcknowledged("manual");
      if (!ok) return; // Modal opened (or aborted) — resume happens later.
      await actuallyStartRecording();
    } catch (e) {
      error = String(e);
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
      ackError = String(e).replace(/^Error:\s*/, "");
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
      copyError = String(e).replace(/^Error:\s*/, "");
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
      error = String(e);
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
    <div class="cta-row">
      <!-- Primary action — button flips to live state with inline timer. -->
      <button
        class="record-btn"
        class:live={recording}
        onclick={toggle}
        aria-label={recording ? "Stop recording" : "Start recording"}
      >
        <span class="record-icon">
          {#if recording}
            <svg viewBox="0 0 20 20" width="14" height="14" aria-hidden="true">
              <rect x="4.5" y="4.5" width="11" height="11" rx="1.5" fill="currentColor" />
            </svg>
          {:else}
            <svg viewBox="0 0 20 20" width="14" height="14" aria-hidden="true">
              <circle cx="10" cy="10" r="5" fill="currentColor" />
            </svg>
          {/if}
        </span>
        <span class="record-text">
          {recording ? "Stop recording" : "Start recording"}
        </span>
        {#if recording}
          <span class="record-timer">{fmtTimer(elapsedMs)}</span>
        {/if}
      </button>

      <!-- Copy a PIPEDA-compliant recording notice to the clipboard
           (#45) so the user can paste it into meeting chat as the
           "I'm recording, here's why, here's the retention posture"
           heads-up to other participants. -->
      <button
        type="button"
        class="copy-notice-btn"
        onclick={copyNotice}
        disabled={copyingNotice}
        aria-live="polite"
      >
        {copiedNotice ? "Copied ✓" : copyingNotice ? "Copying…" : "Copy notice"}
      </button>
    </div>

    <div class="cta-secondary">
      <span class="kbd-row">
        <span class="kbd">Super</span>
        <span class="kbd">Shift</span>
        <span class="kbd">R</span>
      </span>
      <span class="sep">·</span>
      <button class="link" disabled={importing} onclick={importRecording}>
        {importing ? "Importing…" : "Import a file"}
      </button>
    </div>

    {#if copyError}
      <p class="inline-error">{copyError}</p>
    {/if}
  </section>

  <!-- Manual notes panel (#73). Opt-in via Settings; only visible
       during an active recording so there's no empty editor on the
       page when idle. Keyed on session_id so a new recording gets a
       fresh editor state (the CodeMirror instance is destroyed + re-
       created rather than carrying stale text between calls). -->
  {#if manualNotesEnabled && recording && currentSessionId}
    <section class="notes" style="--i: 3">
      {#key currentSessionId}
        <NotesPanel onchange={scheduleNotesSave} />
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
        Under Canadian PIPEDA — and equivalent privacy laws in most
        jurisdictions — you are responsible for notifying every
        participant that the call is being recorded, obtaining their
        consent, and using the recording only for the purpose you
        disclosed.
      </p>
      <p class="ack-body ack-emph">aftercalls doesn't automate consent. You do.</p>

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
  .link:disabled {
    opacity: 0.6;
    cursor: default;
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
