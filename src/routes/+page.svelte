<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onMount, onDestroy } from "svelte";

  type PipelineEvent =
    | { stage: "started"; session_dir: string }
    | { stage: "transcribing" }
    | { stage: "summarizing" }
    | { stage: "writing_note" }
    | { stage: "done"; session_dir: string; note_path: string }
    | { stage: "failed"; error: string };

  let recording = $state(false);
  let sessionDir = $state("");
  let error = $state("");
  let pipelineStage = $state<string>("");
  let pipelineError = $state("");
  let notePath = $state("");

  let unlisten: UnlistenFn | null = null;

  let unlistenState: UnlistenFn | null = null;

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
        }
      },
    );
  });

  onDestroy(() => {
    unlisten?.();
    unlistenState?.();
  });

  async function start() {
    error = "";
    pipelineStage = "";
    pipelineError = "";
    notePath = "";
    try {
      sessionDir = await invoke<string>("start_recording");
      recording = true;
    } catch (e) {
      error = String(e);
    }
  }

  async function stop() {
    error = "";
    try {
      sessionDir = await invoke<string>("stop_recording");
      recording = false;
    } catch (e) {
      error = String(e);
    }
  }

  const stageLabel: Record<string, string> = {
    started: "Processing…",
    transcribing: "Transcribing…",
    summarizing: "Summarizing…",
    writing_note: "Writing note…",
    done: "Saved",
    failed: "Failed",
  };
</script>

<main class="container">
  <h1>callscribe</h1>

  <div class="row">
    {#if !recording}
      <button onclick={start}>Start recording</button>
    {:else}
      <button class="stop" onclick={stop}>Stop recording</button>
    {/if}
  </div>

  {#if recording}
    <p class="status recording">● Recording — {sessionDir}</p>
  {:else if sessionDir}
    <p class="status">Saved to {sessionDir}</p>
  {/if}

  {#if pipelineStage && !recording}
    <p class="status {pipelineStage}">
      {stageLabel[pipelineStage] ?? pipelineStage}
    </p>
  {/if}

  {#if notePath}
    <p class="status done">Note: {notePath}</p>
  {/if}

  {#if pipelineError}
    <p class="error">{pipelineError}</p>
  {/if}

  {#if error}
    <p class="error">{error}</p>
  {/if}
</main>

<style>
  :root {
    font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
    font-size: 16px;
    color: #f6f6f6;
    background-color: #1a1a1a;
  }

  .container {
    margin: 0;
    padding-top: 10vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 1.5rem;
  }

  h1 {
    margin: 0;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .row {
    display: flex;
    justify-content: center;
  }

  button {
    padding: 0.75em 2em;
    font-size: 1.05rem;
    font-weight: 500;
    border-radius: 999px;
    border: 1px solid #3a3a3a;
    background-color: #2a2a2a;
    color: inherit;
    cursor: pointer;
    transition: border-color 0.15s, background-color 0.15s;
  }

  button:hover {
    border-color: #24c8db;
  }

  button.stop {
    border-color: #ff6b6b;
  }

  .status {
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: 0.85rem;
    color: #a0a0a0;
    margin: 0;
    word-break: break-all;
    max-width: 80ch;
    text-align: center;
  }

  .status.recording {
    color: #ff6b6b;
  }

  .status.transcribing,
  .status.summarizing,
  .status.writing_note,
  .status.started {
    color: #e0b050;
  }

  .status.done {
    color: #7abf7a;
  }

  .status.failed {
    color: #ff6b6b;
  }

  .error {
    color: #ff6b6b;
    font-size: 0.9rem;
    margin: 0;
  }
</style>
