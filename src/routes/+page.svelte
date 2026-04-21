<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { onMount, onDestroy } from "svelte";

  type PipelineEvent =
    | { stage: "started"; session_dir: string }
    | { stage: "transcribing" }
    | { stage: "summarizing" }
    | { stage: "writing_note" }
    | { stage: "uploading" }
    | { stage: "done"; session_dir: string; note_path: string }
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
  let notePath = $state("");
  let prompt = $state<AutoDetectEvent | null>(null);
  let elapsedMs = $state(0);
  let importing = $state(false);

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
      if (p.stage === "failed") pipelineError = p.error;
      if (p.stage === "done") notePath = p.note_path;
    });
    unlistenState = await listen<{ recording: boolean }>(
      "recording-state",
      (evt) => {
        recording = evt.payload.recording;
        if (recording) {
          pipelineStage = "";
          pipelineError = "";
          notePath = "";
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
      prompt = evt.payload.kind === "cleared" ? null : evt.payload;
    });

    // "recording-state" only fires on transitions, so a remount
    // mid-recording (e.g. nav away and back) wouldn't know the truth.
    // Ask the backend directly. Timer stays at 00:00 because the
    // recorder doesn't expose a start time yet — secondary to being
    // able to stop at all.
    try {
      const live = await invoke<boolean>("is_recording");
      if (live && !recording) recording = true;
    } catch {}
  });

  onDestroy(() => {
    unlisten?.();
    unlistenState?.();
    unlistenAuto?.();
    clearInterval(timer);
  });

  async function toggle() {
    error = "";
    try {
      if (recording) {
        sessionDir = await invoke<string>("stop_recording");
      } else {
        pipelineStage = "";
        pipelineError = "";
        notePath = "";
        sessionDir = await invoke<string>("start_recording");
      }
    } catch (e) {
      error = String(e);
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
    notePath = "";
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

  async function confirmStart() {
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
    summarizing: "Drafting summary",
    writing_note: "Writing vault note",
    uploading: "Syncing to backend",
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

  <!-- Auto-detect banners, pinned above the CTA. -->
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
  </section>

  <!-- Status lane — shows active recording or pipeline progress inline. -->
  <section class="status" style="--i: 3">
    {#if recording}
      <div class="row row-live">
        <span class="row-dot live"></span>
        <div class="row-body">
          <p class="row-title">Recording call</p>
          <p class="row-sub">Capturing mic + system audio</p>
        </div>
        <span class="row-meta">{sessionDir.split("/").slice(-1)[0]}</span>
      </div>
    {:else if pipelineStage}
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
          {#if notePath && pipelineStage === "done"}
            <p class="row-sub">Note filed in your vault</p>
          {/if}
        </div>
        {#if notePath && pipelineStage === "done"}
          <span class="row-meta">{notePath.split("/").slice(-1)[0]}</span>
        {/if}
      </div>
    {:else}
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

  .row-sub {
    margin: 0.1rem 0 0;
    font-size: 0.78rem;
    color: var(--bone-3);
  }

  .row-meta {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-3);
    letter-spacing: 0.04em;
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
</style>
