<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { readFile } from "@tauri-apps/plugin-fs";
  import { writeText, writeHtml } from "@tauri-apps/plugin-clipboard-manager";
  import { page } from "$app/state";
  import { onMount, onDestroy } from "svelte";
  import Waveform from "$lib/Waveform.svelte";

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
  let audioUrls = $state<{ mic?: string; system?: string; mixed?: string }>({});
  let playing = $state(false);
  let rate = $state(1);
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
      try {
        audioUrls = await invoke("get_audio_urls", { id: page.params.id });
      } catch (e) {
        console.warn("audio-urls unavailable, falling back to local files", e);
      }
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
    if (audioSrc.startsWith("blob:")) URL.revokeObjectURL(audioSrc);
    audioSrc = "";

    const remote = audioUrls[which];
    if (remote) {
      try {
        const resp = await fetch(remote);
        if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
        const blob = await resp.blob();
        audioSrc = URL.createObjectURL(blob);
        return;
      } catch (e) {
        console.warn(`remote fetch failed for ${which}, falling back:`, e);
      }
    }

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
      flashCopied(label);
    } catch (e) {
      error = String(e);
    }
  }

  async function copyRich(html: string, plain: string, label: string) {
    try {
      await writeHtml(html, plain);
      flashCopied(label);
    } catch (e) {
      error = String(e);
    }
  }

  function flashCopied(label: string) {
    copiedLabel = label;
    setTimeout(() => {
      if (copiedLabel === label) copiedLabel = "";
    }, 1800);
  }

  function escapeHtml(s: string) {
    return s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function copyActionItems() {
    if (!call) return;
    const items = call.action_items;
    const plain = items.map((a) => `• ${a}`).join("\n");
    const html = `<ul>${items.map((a) => `<li>${escapeHtml(a)}</li>`).join("")}</ul>`;
    copyRich(html, plain, "actions");
  }

  function copyTranscript() {
    if (!call) return;
    const plain = call.utterances
      .map((u) => `${u.speaker}: ${u.text}`)
      .join("\n\n");
    const html = call.utterances
      .map(
        (u) =>
          `<p><strong>${escapeHtml(u.speaker)}:</strong> ${escapeHtml(u.text)}</p>`,
      )
      .join("");
    copyRich(html, plain, "transcript");
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
      if (audioEl.paused) audioEl.play();
    }
  }

  function onTimeUpdate() {
    if (audioEl) currentMs = Math.floor(audioEl.currentTime * 1000);
  }

  function onPlay() {
    playing = true;
  }
  function onPause() {
    playing = false;
  }

  function togglePlay() {
    if (!audioEl) return;
    if (audioEl.paused) audioEl.play();
    else audioEl.pause();
  }

  function cycleRate() {
    const order = [1, 1.25, 1.5, 2, 0.75];
    const idx = order.indexOf(rate);
    rate = order[(idx + 1) % order.length];
    if (audioEl) audioEl.playbackRate = rate;
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

  // Stable per-speaker accent colors. "You" always gets the brand accent; others
  // pick from a muted palette tuned for the warm dark surface.
  function speakerColor(speaker: string): string {
    if (speaker === "You") return "var(--accent)";
    const hash = [...speaker].reduce((a, c) => a + c.charCodeAt(0), 0);
    const palette = [
      "#c9a24a", // saffron
      "#8faf72", // sage
      "#d07e4e", // rust
      "#b06a8c", // wine
      "#8aa2c0", // slate
    ];
    return palette[hash % palette.length];
  }

  function fmtDateTitle(iso: string) {
    const d = new Date(iso);
    return d.toLocaleString(undefined, {
      weekday: "short",
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
  }

  const trackLabels = {
    mixed: "Everyone",
    mic: "You",
    system: "Others",
  } as const;
</script>

<main class="page reveal">
  {#if loading}
    <p class="state" style="--i: 0">Loading call…</p>
  {:else if error}
    <p class="state err" style="--i: 0">{error}</p>
  {:else if call}
    <header class="head" style="--i: 0">
      <a class="back" href="/calls">
        <svg viewBox="0 0 16 16" width="11" height="11" aria-hidden="true">
          <path d="M10 3 L5 8 L10 13" stroke="currentColor" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
        <span>Calls</span>
      </a>
      <div class="head-row">
        <div class="head-main">
          <p class="dateline">{fmtDateTitle(call.recorded_at)}</p>
          <h1>{call.title ?? "(untitled)"}</h1>
          {#if call.matched_client}
            <p class="client-row">
              <span class="chip chip-accent">{call.matched_client}</span>
            </p>
          {/if}
        </div>
        <button class="delete" disabled={deleting} onclick={deleteCall}>
          {deleting ? "Deleting…" : "Delete"}
        </button>
      </div>
    </header>

    <!-- ── Player ───────────────────────────────────────────────────────── -->
    <section class="player" style="--i: 1">
      <div class="wave-host">
        <Waveform
          src={audioSrc}
          audio={audioEl}
          bind:currentMs
          durationMs={call.duration_ms}
          onseek={(ms) => seekTo(ms)}
        />
      </div>

      <div class="transport">
        <button
          class="play"
          class:playing
          onclick={togglePlay}
          aria-label={playing ? "Pause" : "Play"}
          disabled={!audioSrc}
        >
          {#if playing}
            <svg viewBox="0 0 20 20" width="12" height="12" aria-hidden="true">
              <rect x="5" y="4" width="3.2" height="12" rx="0.8" fill="currentColor" />
              <rect x="11.8" y="4" width="3.2" height="12" rx="0.8" fill="currentColor" />
            </svg>
          {:else}
            <svg viewBox="0 0 20 20" width="12" height="12" aria-hidden="true">
              <path d="M6 4 L16 10 L6 16 Z" fill="currentColor" />
            </svg>
          {/if}
        </button>

        <div class="tracks">
          {#each ["mixed", "mic", "system"] as key (key)}
            <button
              class="track-pill"
              class:active={track === key}
              onclick={() => loadAudio(key as "mixed" | "mic" | "system")}
            >
              {trackLabels[key as keyof typeof trackLabels]}
            </button>
          {/each}
        </div>

        <button class="rate" onclick={cycleRate} aria-label="Playback rate">
          {rate}×
        </button>
      </div>

      {#if audioError}
        <p class="inline-err">{audioError}</p>
      {/if}

      <audio
        bind:this={audioEl}
        src={audioSrc}
        ontimeupdate={onTimeUpdate}
        onplay={onPlay}
        onpause={onPause}
        preload="auto"
      ></audio>
    </section>

    <!-- ── Two-column layout for Summary + Actions, transcript full-width ─ -->
    <div class="split" style="--i: 2">
      {#if call.summary_text}
        <section class="block">
          <div class="block-head">
            <h2>Summary</h2>
            <button
              class="copy-btn"
              onclick={() => copy(call!.summary_text ?? "", "summary")}
            >
              {copiedLabel === "summary" ? "Copied" : "Copy"}
            </button>
          </div>
          <p class="summary">{call.summary_text}</p>
        </section>
      {/if}

      {#if call.action_items.length > 0}
        <section class="block">
          <div class="block-head">
            <h2>Action items</h2>
            <button class="copy-btn" onclick={copyActionItems}>
              {copiedLabel === "actions" ? "Copied" : "Copy"}
            </button>
          </div>
          <ul class="actions">
            {#each call.action_items as item, i (i)}
              <li>
                <span class="action-idx">{String(i + 1).padStart(2, "0")}</span>
                <span>{item}</span>
              </li>
            {/each}
          </ul>
        </section>
      {/if}
    </div>

    {#if speakers.length > 0}
      <section class="block" style="--i: 3">
        <div class="block-head">
          <h2>Participants</h2>
        </div>
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
                class="chip speaker-chip"
                style="--c: {speakerColor(p.speaker)}"
                title="Rename {p.speaker}"
                onclick={() => startSpeakerRename(p.speaker)}
              >
                <span class="chip-dot"></span>
                <span class="chip-body">
                  <span class="chip-name">{p.speaker}</span>
                  <span class="chip-meta">
                    {p.count} {p.count === 1 ? "line" : "lines"} ·
                    {fmtSpeakingTime(p.totalMs)}
                  </span>
                </span>
              </button>
            {/if}
          {/each}
        </div>
      </section>
    {/if}

    <section class="block" style="--i: 4">
      <div class="block-head">
        <h2>Transcript</h2>
        <button class="copy-btn" onclick={copyTranscript}>
          {copiedLabel === "transcript" ? "Copied" : "Copy"}
        </button>
      </div>
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
                Apply to all "{u.speaker}"
              </label>
              <div class="editor-buttons">
                <button class="ed-save" disabled={savingEdit} onclick={saveEdit}>
                  {savingEdit ? "Saving…" : "Save"}
                </button>
                <button class="ed-cancel" onclick={cancelEdit}>Cancel</button>
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
              <span class="utt-ts">{fmtTime(u.start_ms)}</span>
              <button
                type="button"
                class="utt-speaker"
                style="--c: {speakerColor(u.speaker)}"
                title="Click to rename"
                onclick={(e) => {
                  e.stopPropagation();
                  startEdit(u);
                }}
              >
                <span class="utt-speaker-dot"></span>
                {u.speaker}
              </button>
              <span class="utt-text">{u.text}</span>
            </div>
          {/if}
        {/each}
      </div>
    </section>
  {/if}
</main>

<style>
  .page {
    max-width: 1000px;
    margin: 0 auto;
    padding: 1.4rem 2rem 5rem;
    position: relative;
    z-index: 2;
  }

  .state {
    color: var(--bone-3);
  }
  .state.err {
    color: var(--live);
  }

  /* ── Head ──────────────────────────────────────────────────────────── */
  .head {
    margin-bottom: 1.6rem;
  }

  .back {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.8rem;
    color: var(--bone-3);
    margin-bottom: 1rem;
    transition: color 0.15s;
  }
  .back:hover {
    color: var(--bone-0);
  }

  .head-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
  }

  .head-main {
    min-width: 0;
  }

  .dateline {
    margin: 0 0 0.35rem;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    letter-spacing: 0.04em;
    color: var(--bone-3);
  }

  .head-main h1 {
    font-size: 1.6rem;
    line-height: 1.2;
    margin: 0 0 0.55rem;
  }

  .client-row {
    margin: 0;
  }

  .delete {
    padding: 0.4rem 0.85rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    color: var(--bone-3);
    font-size: 0.78rem;
    transition: all 0.15s;
  }
  .delete:hover:not(:disabled) {
    border-color: var(--live);
    color: var(--live);
  }
  .delete:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* ── Chips ─────────────────────────────────────────────────────────── */
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-1);
    color: var(--bone-1);
    font: inherit;
    text-align: left;
    transition: border-color 0.15s, background 0.15s;
  }

  .chip:hover {
    border-color: var(--hairline-hi);
    background: var(--ink-2);
  }

  .chip-accent {
    border-color: rgba(58, 155, 146, 0.32);
    background: var(--accent-soft);
    color: var(--accent-hi);
    font-size: 0.75rem;
    font-weight: 500;
    padding: 0.2rem 0.6rem;
  }

  .speaker-chip .chip-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--c);
    flex-shrink: 0;
  }

  .chip-body {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    line-height: 1.25;
  }

  .chip-name {
    font-weight: 500;
    font-size: 0.85rem;
    color: var(--c);
  }

  .chip-meta {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    letter-spacing: 0.02em;
    color: var(--bone-3);
  }

  .chip-editing {
    border-color: var(--accent);
    background: var(--accent-soft);
  }

  .chip-input {
    padding: 0.35rem 0.55rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font-size: 0.85rem;
    width: 10rem;
  }

  .chip-save,
  .chip-cancel {
    padding: 0.35rem 0.7rem;
    font-size: 0.78rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
  }

  .chip-save {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 500;
  }
  .chip-save:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* ── Player ────────────────────────────────────────────────────────── */
  .player {
    margin-bottom: 2rem;
    padding: 1.1rem 1.2rem 1.2rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
  }

  .wave-host {
    padding: 0.3rem 0 1.4rem;
  }

  .transport {
    display: flex;
    align-items: center;
    gap: 0.7rem;
  }

  .play {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 34px;
    height: 34px;
    border-radius: 50%;
    border: 1px solid var(--hairline-hi);
    background: var(--ink-2);
    color: var(--bone-0);
    transition: all 0.15s;
    flex-shrink: 0;
  }

  .play:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .play.playing {
    background: var(--accent);
    color: var(--ink-0);
    border-color: var(--accent);
  }
  .play:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .tracks {
    display: flex;
    gap: 2px;
    padding: 2px;
    border-radius: 8px;
    background: var(--ink-2);
    border: 1px solid var(--hairline);
  }

  .track-pill {
    padding: 0.3rem 0.75rem;
    font-size: 0.78rem;
    font-weight: 500;
    color: var(--bone-3);
    border-radius: 6px;
    transition: all 0.15s;
  }

  .track-pill:hover {
    color: var(--bone-0);
  }

  .track-pill.active {
    background: var(--ink-0);
    color: var(--accent);
    box-shadow: inset 0 0 0 1px var(--hairline-hi);
  }

  .rate {
    margin-left: auto;
    padding: 0.35rem 0.75rem;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--bone-1);
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-2);
    transition: border-color 0.15s;
  }
  .rate:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  audio {
    display: none;
  }

  .inline-err {
    margin: 0.6rem 0 0;
    color: var(--live);
    font-size: 0.85rem;
  }

  /* ── Split ─────────────────────────────────────────────────────────── */
  .split {
    display: grid;
    grid-template-columns: 1fr;
    gap: 2rem;
    margin-bottom: 2rem;
  }
  @media (min-width: 860px) {
    .split {
      grid-template-columns: 1.3fr 1fr;
    }
  }

  /* ── Block common ──────────────────────────────────────────────────── */
  .block {
    margin-bottom: 2rem;
  }
  .split .block {
    margin-bottom: 0;
  }

  .block-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.8rem;
    margin-bottom: 0.7rem;
  }

  .copy-btn {
    padding: 0.3rem 0.7rem;
    font-size: 0.72rem;
    font-weight: 500;
    letter-spacing: 0.01em;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    color: var(--bone-2);
    transition: all 0.15s;
  }

  .copy-btn:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  /* ── Summary ───────────────────────────────────────────────────────── */
  .summary {
    margin: 0;
    font-size: 0.95rem;
    line-height: 1.6;
    color: var(--bone-1);
    white-space: pre-wrap;
  }

  /* ── Actions ───────────────────────────────────────────────────────── */
  .actions {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 0;
  }

  .actions li {
    display: grid;
    grid-template-columns: 32px 1fr;
    gap: 0.8rem;
    padding: 0.6rem 0;
    border-bottom: 1px solid var(--hairline);
    color: var(--bone-1);
    font-size: 0.9rem;
    line-height: 1.5;
  }
  .actions li:last-child {
    border-bottom: none;
  }

  .action-idx {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--accent);
    letter-spacing: 0.04em;
    padding-top: 0.22rem;
  }

  /* ── Chips row (participants) ──────────────────────────────────────── */
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.45rem;
  }

  /* ── Transcript ────────────────────────────────────────────────────── */
  .transcript {
    display: flex;
    flex-direction: column;
    border-top: 1px solid var(--hairline);
  }

  .utt {
    display: grid;
    grid-template-columns: 56px 120px 1fr;
    align-items: baseline;
    gap: 0.9rem;
    padding: 0.7rem 0.4rem;
    border-bottom: 1px solid var(--hairline);
    cursor: pointer;
    transition: background 0.1s;
  }

  .utt:hover {
    background: var(--ink-1);
  }

  .utt.active {
    background: linear-gradient(
      90deg,
      var(--accent-soft) 0%,
      transparent 80%
    );
    border-left: 2px solid var(--accent);
    padding-left: 0.7rem;
  }

  .utt-ts {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-3);
    letter-spacing: 0.02em;
  }

  .utt.active .utt-ts {
    color: var(--accent);
  }

  .utt-speaker {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.1rem 0.4rem 0.1rem 0.3rem;
    margin: -0.1rem -0.4rem;
    border-radius: 4px;
    color: var(--c);
    font: inherit;
    font-size: 0.78rem;
    font-weight: 500;
    text-align: left;
    transition: background 0.15s;
  }

  .utt-speaker:hover {
    background: var(--ink-2);
  }

  .utt-speaker-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--c);
    flex-shrink: 0;
  }

  .utt-text {
    color: var(--bone-1);
    font-size: 0.9rem;
    line-height: 1.55;
  }

  .utt.active .utt-text {
    color: var(--bone-0);
  }

  /* Editor inline */
  .utt-editor {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    padding: 0.85rem 1rem;
    margin: 0.3rem 0;
    border: 1px solid var(--accent);
    border-radius: var(--radius);
    background: var(--accent-soft);
  }

  .speaker-input {
    padding: 0.45rem 0.65rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font-size: 0.9rem;
    max-width: 18rem;
  }

  .apply-all {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.82rem;
    color: var(--bone-1);
  }

  .editor-buttons {
    display: flex;
    gap: 0.4rem;
  }

  .ed-save,
  .ed-cancel {
    padding: 0.35rem 0.9rem;
    font-size: 0.8rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
  }

  .ed-save {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 500;
  }
  .ed-save:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
