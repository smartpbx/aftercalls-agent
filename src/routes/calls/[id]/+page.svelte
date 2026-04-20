<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { readFile } from "@tauri-apps/plugin-fs";
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { page } from "$app/state";
  import { onMount, onDestroy } from "svelte";

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
  let track = $state<"mixed" | "mic" | "system">("mixed");
  let currentMs = $state(0);
  let audioEl = $state<HTMLAudioElement | undefined>(undefined);
  let deleting = $state(false);
  let copiedLabel = $state("");
  let editingIdx = $state<number | null>(null);
  let editValue = $state("");
  let applyToAll = $state(false);
  let savingEdit = $state(false);

  let editingSpeaker = $state<string | null>(null);
  let speakerEditValue = $state("");
  let savingSpeaker = $state(false);

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

  async function loadAudio(which: "mixed" | "mic" | "system") {
    if (!call) return;
    audioError = "";
    track = which;
    // Revoke old blob URL to avoid leaking memory across track switches.
    if (audioSrc.startsWith("blob:")) URL.revokeObjectURL(audioSrc);
    audioSrc = "";
    try {
      const path = await invoke<string>("get_session_audio_path", {
        sessionId: call.session_id,
        track: which,
      });
      const bytes = await readFile(path);
      const blob = new Blob([new Uint8Array(bytes)], { type: "audio/wav" });
      audioSrc = URL.createObjectURL(blob);
    } catch (e) {
      audioError = String(e);
      audioSrc = "";
    }
  }

  onDestroy(() => {
    if (audioSrc.startsWith("blob:")) URL.revokeObjectURL(audioSrc);
  });

  async function copy(text: string, label: string) {
    try {
      await writeText(text);
      copiedLabel = label;
      setTimeout(() => {
        if (copiedLabel === label) copiedLabel = "";
      }, 1500);
    } catch (e) {
      error = String(e);
    }
  }

  function copyActionItems() {
    if (!call) return;
    copy(call.action_items.map((a) => `- ${a}`).join("\n"), "actions");
  }

  function startEdit(u: Utterance) {
    editingIdx = u.idx;
    editValue = u.speaker;
    applyToAll = false;
  }

  function cancelEdit() {
    editingIdx = null;
    editValue = "";
    applyToAll = false;
  }

  async function saveEdit() {
    if (!call || editingIdx === null) return;
    const current = call.utterances.find((x) => x.idx === editingIdx);
    if (!current) return;
    const to = editValue.trim();
    if (!to || to === current.speaker) {
      cancelEdit();
      return;
    }
    savingEdit = true;
    try {
      if (applyToAll) {
        const from = current.speaker;
        await invoke<number>("rename_speaker", {
          id: call.id,
          from,
          to,
        });
        call.utterances = call.utterances.map((u) =>
          u.speaker === from ? { ...u, speaker: to } : u,
        );
      } else {
        await invoke("update_utterance_speaker", {
          id: call.id,
          idx: editingIdx,
          speaker: to,
        });
        call.utterances = call.utterances.map((u) =>
          u.idx === editingIdx ? { ...u, speaker: to } : u,
        );
      }
      cancelEdit();
    } catch (e) {
      error = String(e);
    } finally {
      savingEdit = false;
    }
  }

  async function deleteCall() {
    if (!call) return;
    if (!confirm(`Delete "${call.title ?? "this call"}"? Audio files stay on disk.`))
      return;
    deleting = true;
    try {
      await invoke("delete_call", { id: call.id });
      window.location.href = "/calls";
    } catch (e) {
      error = String(e);
      deleting = false;
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

  type SpeakerStat = { speaker: string; count: number; totalMs: number };

  let speakers = $derived.by<SpeakerStat[]>(() => {
    if (!call) return [];
    const order: string[] = [];
    const map = new Map<string, SpeakerStat>();
    for (const u of call.utterances) {
      const existing = map.get(u.speaker);
      if (existing) {
        existing.count++;
        existing.totalMs += u.end_ms - u.start_ms;
      } else {
        order.push(u.speaker);
        map.set(u.speaker, {
          speaker: u.speaker,
          count: 1,
          totalMs: u.end_ms - u.start_ms,
        });
      }
    }
    return order.map((s) => map.get(s)!);
  });

  function fmtSpeakingTime(ms: number) {
    const s = Math.round(ms / 1000);
    if (s < 60) return `${s}s`;
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}m ${r}s`;
  }

  function startSpeakerRename(current: string) {
    editingSpeaker = current;
    speakerEditValue = current;
  }

  function cancelSpeakerRename() {
    editingSpeaker = null;
    speakerEditValue = "";
  }

  async function saveSpeakerRename() {
    if (!call || !editingSpeaker) return;
    const from = editingSpeaker;
    const to = speakerEditValue.trim();
    if (!to || to === from) {
      cancelSpeakerRename();
      return;
    }
    savingSpeaker = true;
    try {
      await invoke<number>("rename_speaker", { id: call.id, from, to });
      call.utterances = call.utterances.map((u) =>
        u.speaker === from ? { ...u, speaker: to } : u,
      );
      cancelSpeakerRename();
    } catch (e) {
      error = String(e);
    } finally {
      savingSpeaker = false;
    }
  }

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
      <div class="title-row">
        <h1>{call.title ?? "(untitled)"}</h1>
        <button class="delete" disabled={deleting} onclick={deleteCall}>
          {deleting ? "Deleting…" : "Delete"}
        </button>
      </div>
      <p class="meta">
        {new Date(call.recorded_at).toLocaleString()}
        {#if call.matched_client}· <span class="client">{call.matched_client}</span>{/if}
      </p>
    </header>

    {#if speakers.length > 0}
      <section class="participants">
        <h2>Participants</h2>
        <div class="chips">
          {#each speakers as p (p.speaker)}
            {#if editingSpeaker === p.speaker}
              <div class="chip chip-editing">
                <input
                  class="chip-input"
                  bind:value={speakerEditValue}
                  onkeydown={(e) => {
                    if (e.key === "Enter") saveSpeakerRename();
                    if (e.key === "Escape") cancelSpeakerRename();
                  }}
                />
                <button
                  class="chip-save"
                  disabled={savingSpeaker}
                  onclick={saveSpeakerRename}
                >
                  {savingSpeaker ? "…" : "Save"}
                </button>
                <button class="chip-cancel" onclick={cancelSpeakerRename}>
                  Cancel
                </button>
              </div>
            {:else}
              <button
                type="button"
                class="chip"
                style="border-color: {speakerColor(p.speaker)}"
                title="Rename {p.speaker}"
                onclick={() => startSpeakerRename(p.speaker)}
              >
                <span class="chip-name" style="color: {speakerColor(p.speaker)}">
                  {p.speaker}
                </span>
                <span class="chip-meta">
                  {p.count} {p.count === 1 ? "line" : "lines"} ·
                  {fmtSpeakingTime(p.totalMs)}
                </span>
              </button>
            {/if}
          {/each}
        </div>
      </section>
    {/if}

    <section class="player">
      <div class="track-toggle">
        <button class:active={track === "mixed"} onclick={() => loadAudio("mixed")}>
          Everyone
        </button>
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
        <div class="section-head">
          <h2>Summary</h2>
          <button
            class="copy"
            onclick={() => copy(call!.summary_text ?? "", "summary")}
          >
            {copiedLabel === "summary" ? "Copied" : "Copy"}
          </button>
        </div>
        <p class="summary">{call.summary_text}</p>
      </section>
    {/if}

    {#if call.action_items.length > 0}
      <section>
        <div class="section-head">
          <h2>Action items</h2>
          <button class="copy" onclick={copyActionItems}>
            {copiedLabel === "actions" ? "Copied" : "Copy"}
          </button>
        </div>
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
          {#if editingIdx === u.idx}
            <div class="utt-editor">
              <input
                class="speaker-input"
                bind:value={editValue}
                onkeydown={(e) => {
                  if (e.key === "Enter") saveEdit();
                  if (e.key === "Escape") cancelEdit();
                }}
              />
              <label class="apply-all">
                <input type="checkbox" bind:checked={applyToAll} />
                Apply to all "{u.speaker}" in this call
              </label>
              <div class="editor-buttons">
                <button class="primary" disabled={savingEdit} onclick={saveEdit}>
                  {savingEdit ? "Saving…" : "Save"}
                </button>
                <button onclick={cancelEdit}>Cancel</button>
              </div>
            </div>
          {:else}
            <div
              class="utt"
              class:active={u.idx === activeIdx}
              role="button"
              tabindex="0"
              onclick={() => seekTo(u.start_ms)}
              onkeydown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  seekTo(u.start_ms);
                }
              }}
            >
              <span class="timestamp">{fmtTime(u.start_ms)}</span>
              <button
                type="button"
                class="speaker"
                style="color: {speakerColor(u.speaker)}"
                title="Click to rename"
                onclick={(e) => {
                  e.stopPropagation();
                  startEdit(u);
                }}
              >{u.speaker}</button>
              <span class="text">{u.text}</span>
            </div>
          {/if}
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

  .title-row {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    margin-bottom: 0.3rem;
  }

  .delete {
    border: 1px solid #552525;
    background-color: transparent;
    color: #ff6b6b;
    border-radius: 8px;
    padding: 0.35rem 0.9rem;
    font-size: 0.8rem;
    cursor: pointer;
    transition: background-color 0.15s, border-color 0.15s;
  }
  .delete:hover:not(:disabled) {
    background-color: #3a1c1c;
    border-color: #ff6b6b;
  }
  .delete:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  h1 {
    margin: 0;
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

  .participants {
    margin-bottom: 1.5rem;
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }

  .chip {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.15rem;
    padding: 0.45rem 0.85rem;
    border: 1px solid #3a3a3a;
    border-radius: 10px;
    background-color: #1e1e1e;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
    transition: background-color 0.12s;
  }

  .chip:hover {
    background-color: #252525;
  }

  .chip-name {
    font-weight: 500;
    font-size: 0.9rem;
  }

  .chip-meta {
    font-size: 0.75rem;
    color: #808080;
  }

  .chip-editing {
    flex-direction: row;
    align-items: center;
    gap: 0.4rem;
    border-color: #24c8db;
    background-color: #1e2a2e;
    cursor: default;
  }

  .chip-input {
    padding: 0.3rem 0.55rem;
    border-radius: 6px;
    border: 1px solid #3a3a3a;
    background-color: #2a2a2a;
    color: inherit;
    font: inherit;
    font-size: 0.85rem;
    width: 12rem;
  }

  .chip-save,
  .chip-cancel {
    padding: 0.3rem 0.7rem;
    font-size: 0.8rem;
    border-radius: 6px;
    border: 1px solid #3a3a3a;
    background-color: #2a2a2a;
    color: inherit;
    cursor: pointer;
  }

  .chip-save {
    border-color: #24c8db;
    background-color: #24c8db22;
  }

  .chip-save:disabled {
    opacity: 0.5;
    cursor: not-allowed;
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

  .speaker {
    cursor: pointer;
    border-radius: 4px;
    padding: 0 0.25rem;
    margin: 0 -0.25rem;
    background: none;
    border: none;
    font: inherit;
    text-align: left;
    font-weight: 500;
    font-size: 0.85rem;
    padding-top: 0.05rem;
  }

  .speaker:hover {
    background-color: #2e2e2e;
  }

  .section-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.75rem;
  }

  .section-head h2 {
    margin: 0;
  }

  .copy {
    padding: 0.25rem 0.7rem;
    font-size: 0.75rem;
    border-radius: 999px;
    border: 1px solid #3a3a3a;
    background-color: #252525;
    color: #c0c0c0;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
  }

  .copy:hover {
    border-color: #24c8db;
    color: #f6f6f6;
  }

  .utt-editor {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.6rem 0.7rem;
    margin: 0.2rem 0;
    border: 1px solid #24c8db;
    border-radius: 6px;
    background-color: #1e2a2e;
  }

  .speaker-input {
    padding: 0.35rem 0.6rem;
    border-radius: 6px;
    border: 1px solid #3a3a3a;
    background-color: #2a2a2a;
    color: inherit;
    font-size: 0.9rem;
    font-family: inherit;
  }

  .apply-all {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8rem;
    color: #c0c0c0;
  }

  .editor-buttons {
    display: flex;
    gap: 0.4rem;
  }

  .editor-buttons button {
    padding: 0.3rem 0.9rem;
    font-size: 0.8rem;
    border-radius: 6px;
    border: 1px solid #3a3a3a;
    background-color: #2a2a2a;
    color: inherit;
    cursor: pointer;
  }

  .editor-buttons button.primary {
    border-color: #24c8db;
    background-color: #24c8db22;
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
