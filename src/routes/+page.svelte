<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  let recording = $state(false);
  let sessionDir = $state("");
  let error = $state("");

  async function start() {
    error = "";
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

  .error {
    color: #ff6b6b;
    font-size: 0.9rem;
    margin: 0;
  }
</style>
