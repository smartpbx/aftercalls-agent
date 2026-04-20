<script lang="ts">
  import { invoke, convertFileSrc } from "@tauri-apps/api/core";
  import { page } from "$app/state";
  import { onMount } from "svelte";

  type Utterance = {
    idx: number;
    speaker: string;
    original_speaker: string;
    start_ms: number;
    end_ms: number;
    text: string;
  };

  type Call = {
    id: string;
    session_id: string;
    recorded_at: string;
    duration_ms: number;
    title: string | null;
    matched_client: string | null;
    summary_text: string | null;
    action_items: string[];
    participants: string[];
    note_markdown_path: string | null;
    status: string;
    utterances: Utterance[];
  };

  let call = $state<Call | null>(null);
  let error = $state("");
  let loading = $state(true);

  let audioSrc = $state<string>("");
  let audioError = $state("");
  let track = $state<"mic" | "system">("system");
  let currentMs = $state(0);
  let audioEl = $state<HTMLAudioElement | undefined>(undefined);

  onMount(async () => {
    try {
      call = await invoke<Call>("get_call", { id: page.params.id });
      await loadAudio(track);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  });

  async function loadAudio(which: "mic" | "system") {
    if (!call) return;
    audioError = "";
    track = which;
    try {
      const path = await invoke<string>("get_session_audio_path", {
        sessionId: call.session_id,
        track: which,
      });
      audioSrc = convertFileSrc(path);
    } catch (e) {
      audioError = String(e);
      audioSrc = "";
    }
  }

  function fmtTime(ms: number) {
    const s = Math.floor(ms / 1000);
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${String(m).padStart(2, "0")}:${String(r).padStart(2, "0")}`;
  }

  function seekTo(ms: number) {
    if (audioEl) {
      audioEl.currentTime = ms / 1000;
      audioEl.play();
    }
  }

  function onTimeUpdate() {
    if (audioEl) currentMs = Math.floor(audioEl.currentTime * 1000);
  }

  let activeIdx = $derived.by(() => {
    if (!call) return -1;
    for (let i = call.utterances.length - 1; i >= 0; i--) {
      if (call.utterances[i].start_ms <= currentMs) return call.utterances[i].idx;
    }
    return -1;
  });

  function speakerColor(speaker: string): string {
    if (speaker === "You") return "#24c8db";
    const hash = [...speaker].reduce((a, c) => a + c.charCodeAt(0), 0);
    const palette = ["#e0b050", "#b080e0", "#7abf7a", "#ff9070", "#8aa8e0"];
    return palette[hash % palette.length];
  }
</script>

<main class="container">
  <a class="back" href="/calls">← Calls</a>

  {#if loading}
    <p class="muted">Loading…</p>
  {:else if error}
    <p class="error">{error}</p>
  {:else if call}
    <header>
      <h1>{call.title ?? "(untitled)"}</h1>
      <p class="meta">
        {new Date(call.recorded_at).toLocaleString()}
        {#if call.matched_client}· <span class="client">{call.matched_client}</span>{/if}
      </p>
    </header>

    <section class="player">
      <div class="track-toggle">
        <button
          class:active={track === "system"}
          onclick={() => loadAudio("system")}>Other participants</button>
        <button class:active={track === "mic"} onclick={() => loadAudio("mic")}>
          You
        </button>
      </div>
      {#if audioSrc}
        <!-- svelte-ignore a11y_media_has_caption -->
        <audio
          bind:this={audioEl}
          src={audioSrc}
          controls
          ontimeupdate={onTimeUpdate}
        ></audio>
      {:else if audioError}
        <p class="error small">{audioError}</p>
      {/if}
    </section>

    {#if call.summary_text}
      <section>
        <h2>Summary</h2>
        <p class="summary">{call.summary_text}</p>
      </section>
    {/if}

    {#if call.action_items.length > 0}
      <section>
        <h2>Action items</h2>
        <ul class="actions">
          {#each call.action_items as item (item)}
            <li>{item}</li>
          {/each}
        </ul>
      </section>
    {/if}

    <section>
      <h2>Transcript</h2>
      <div class="transcript">
        {#each call.utterances as u (u.idx)}
          <button
            class="utt"
            class:active={u.idx === activeIdx}
            onclick={() => seekTo(u.start_ms)}
          >
            <span class="timestamp">{fmtTime(u.start_ms)}</span>
            <span class="speaker" style="color: {speakerColor(u.speaker)}"
              >{u.speaker}</span
            >
            <span class="text">{u.text}</span>
          </button>
        {/each}
      </div>
    </section>
  {/if}
</main>

<style>
  .container {
    max-width: 900px;
    margin: 0 auto;
    padding: 1.5rem 1.5rem 4rem;
  }

  .back {
    display: inline-block;
    font-size: 0.85rem;
    color: #a0a0a0;
    text-decoration: none;
    margin-bottom: 1rem;
  }
  .back:hover {
    color: #f6f6f6;
  }

  header {
    margin-bottom: 1.25rem;
  }

  h1 {
    margin: 0 0 0.3rem;
    font-weight: 600;
    letter-spacing: -0.02em;
    font-size: 1.5rem;
  }
  h2 {
    margin: 0 0 0.75rem;
    font-weight: 500;
    font-size: 1rem;
    color: #c0c0c0;
  }

  .meta {
    margin: 0;
    color: #a0a0a0;
    font-size: 0.9rem;
  }
  .client {
    color: #24c8db;
    margin-left: 0.4rem;
  }

  .player {
    margin-bottom: 1.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }

  .track-toggle {
    display: flex;
    gap: 0.4rem;
  }

  .track-toggle button {
    padding: 0.3rem 0.9rem;
    font-size: 0.8rem;
    border-radius: 999px;
    border: 1px solid #3a3a3a;
    background-color: #2a2a2a;
    color: #a0a0a0;
    cursor: pointer;
  }

  .track-toggle button.active {
    border-color: #24c8db;
    color: #f6f6f6;
  }

  audio {
    width: 100%;
    background-color: #2a2a2a;
    border-radius: 8px;
  }

  section {
    margin-bottom: 1.5rem;
  }

  .summary {
    margin: 0;
    white-space: pre-wrap;
    line-height: 1.55;
    color: #d8d8d8;
  }

  .actions {
    margin: 0;
    padding-left: 1.2rem;
    color: #d8d8d8;
  }

  .actions li {
    margin-bottom: 0.35rem;
    line-height: 1.5;
  }

  .transcript {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    border: 1px solid #2a2a2a;
    border-radius: 10px;
    background-color: #1c1c1c;
    padding: 0.5rem;
    max-height: 60vh;
    overflow-y: auto;
  }

  .utt {
    display: grid;
    grid-template-columns: 4ch 12ch 1fr;
    gap: 0.75rem;
    text-align: left;
    padding: 0.5rem 0.7rem;
    border: none;
    background: none;
    color: inherit;
    cursor: pointer;
    border-radius: 6px;
    font-size: 0.9rem;
    line-height: 1.5;
    transition: background-color 0.1s;
  }

  .utt:hover {
    background-color: #2a2a2a;
  }

  .utt.active {
    background-color: #24c8db22;
  }

  .timestamp {
    color: #808080;
    font-family: ui-monospace, Menlo, monospace;
    font-size: 0.75rem;
    padding-top: 0.15rem;
  }

  .speaker {
    font-weight: 500;
    font-size: 0.85rem;
    padding-top: 0.05rem;
  }

  .text {
    color: #e0e0e0;
  }

  .error.small {
    font-size: 0.85rem;
  }

  .error {
    color: #ff6b6b;
  }

  .muted {
    color: #a0a0a0;
  }
</style>
