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
    source_app: string | null;
    source_kind: string | null;
    utterances: Utterance[];
  };

  function prettyApp(raw: string | null): string | null {
    if (!raw) return null;
    const key = raw.toLowerCase();
    const map: Record<string, string> = {
      zoom: "Zoom",
      "zoom.us": "Zoom",
      teams: "Teams",
      "teams-for-linux": "Teams",
      "microsoft teams": "Teams",
      slack: "Slack",
      discord: "Discord",
      "discord-canary": "Discord",
      firefox: "Firefox",
      "google chrome": "Chrome",
      chrome: "Chrome",
      chromium: "Chromium",
      "zen-browser": "Zen",
      zen: "Zen",
      brave: "Brave",
      "obs-studio": "OBS",
      obs: "OBS",
      smartpbx: "SmartPBX",
      ringotel: "Ringotel",
      signal: "Signal",
      telegram: "Telegram",
    };
    return map[key] ?? raw;
  }

  function sourceKindLabel(kind: string | null): string {
    switch (kind) {
      case "auto_detected":
        return "Auto-detected";
      case "imported":
        return "Imported file";
      case "manual":
        return "Manual";
      default:
        return "";
    }
  }

  type Highlight = {
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

  let call = $state<Call | null>(null);
  let highlights = $state<Highlight[]>([]);
  let error = $state("");
  let loading = $state(true);

  let audioSrc = $state<string>("");
  let audioError = $state("");
  let track = $state<"mixed" | "mic" | "system">("mixed");
  let currentMs = $state(0);
  let audioEl = $state<HTMLAudioElement | undefined>(undefined);
  let audioUrls = $state<{
    mic?: string;
    system?: string;
    mixed?: string;
    peaks_available?: boolean;
  }>({});
  let peaks = $state<Float32Array | null>(null);
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

  // Heavy tracing on this route while we're diagnosing the blank-on-click
  // crash. console.error because webkit2gtk has been swallowing warn/log
  // output intermittently — error shows up reliably.
  const trace = (step: string, extra?: unknown) => {
    if (extra === undefined) console.error("[call-detail]", step);
    else console.error("[call-detail]", step, extra);
  };

  onMount(async () => {
    trace("onMount start", { id: page.params.id });
    try {
      call = await invoke<Call>("get_call", { id: page.params.id });
      trace("get_call ok", {
        id: call?.id,
        utterances: call?.utterances?.length,
      });
      try {
        audioUrls = await invoke("get_audio_urls", { id: page.params.id });
        trace("get_audio_urls ok", audioUrls);
      } catch (e) {
        trace("get_audio_urls FAILED (fallback to local)", e);
      }
      if (audioUrls.peaks_available) {
        try {
          const doc = await invoke<{ peaks: number[] }>("get_peaks", {
            id: page.params.id,
          });
          if (Array.isArray(doc.peaks) && doc.peaks.length > 0) {
            peaks = new Float32Array(doc.peaks);
          }
          trace("get_peaks ok", { bytes: doc.peaks?.length ?? 0 });
        } catch (e) {
          trace("get_peaks FAILED", e);
        }
      }
      try {
        const hs = await invoke<Highlight[]>("list_highlights", {
          callId: page.params.id,
        });
        if (Array.isArray(hs)) highlights = hs;
        trace("list_highlights ok", { count: highlights.length });
      } catch (e) {
        trace("list_highlights FAILED", e);
      }
      trace("loadAudio start", { track });
      await loadAudio(track);
      trace("loadAudio done", { src: audioSrc, err: audioError });
    } catch (e) {
      trace("onMount FATAL", e);
      error = String(e);
    } finally {
      loading = false;
      trace("onMount end loading=false");
    }
  });

  async function createHighlight(range: { start_ms: number; end_ms: number }) {
    if (!call) return;
    try {
      const created = await invoke<Highlight>("create_highlight", {
        callId: call.id,
        body: {
          start_ms: range.start_ms,
          end_ms: range.end_ms,
          kind: "bookmark",
          label: null,
        },
      });
      highlights = [...highlights, created].sort((a, b) => a.start_ms - b.start_ms);
    } catch (e) {
      error = String(e);
    }
  }

  let autoDetecting = $state(false);
  let autoDetectResult = $state<string>("");

  async function autoDetect() {
    if (!call) return;
    autoDetecting = true;
    autoDetectResult = "";
    try {
      const resp = await invoke<{
        created: Highlight[];
        skipped: number;
      }>("auto_highlight", { callId: call.id });
      // Merge: drop the old AI highlights (backend replaced them), keep user's.
      const userOnly = highlights.filter((h) => h.source !== "ai");
      highlights = [...userOnly, ...resp.created].sort(
        (a, b) => a.start_ms - b.start_ms,
      );
      const note =
        resp.created.length === 0
          ? "No highlights detected."
          : `Detected ${resp.created.length} highlight${resp.created.length === 1 ? "" : "s"}` +
            (resp.skipped > 0 ? ` (${resp.skipped} skipped)` : "");
      autoDetectResult = note;
      setTimeout(() => {
        autoDetectResult = "";
      }, 4000);
    } catch (e) {
      error = String(e);
    } finally {
      autoDetecting = false;
    }
  }

  async function deleteHighlight(id: string) {
    try {
      await invoke("delete_highlight", { id });
      highlights = highlights.filter((h) => h.id !== id);
    } catch (e) {
      error = String(e);
    }
  }

  async function retitleHighlight(id: string, label: string, kind?: string) {
    try {
      await invoke("update_highlight", {
        id,
        body: { label, kind: kind ?? null },
      });
      highlights = highlights.map((h) =>
        h.id === id ? { ...h, label, kind: kind ?? h.kind } : h,
      );
    } catch (e) {
      error = String(e);
    }
  }

  let editingHighlight = $state<string | null>(null);
  let highlightLabelEdit = $state("");
  let highlightKindEdit = $state("bookmark");

  function startEditHighlight(h: Highlight) {
    editingHighlight = h.id;
    highlightLabelEdit = h.label ?? "";
    highlightKindEdit = h.kind;
  }

  async function saveEditHighlight() {
    if (!editingHighlight) return;
    await retitleHighlight(
      editingHighlight,
      highlightLabelEdit.trim(),
      highlightKindEdit,
    );
    editingHighlight = null;
  }

  const HIGHLIGHT_KINDS = [
    { value: "bookmark", label: "Bookmark" },
    { value: "decision", label: "Decision" },
    { value: "follow_up", label: "Follow-up" },
    { value: "question", label: "Question" },
    { value: "action", label: "Action" },
  ];

  async function loadAudio(which: "mixed" | "mic" | "system") {
    if (!call) return;
    audioError = "";
    track = which;
    if (audioSrc.startsWith("blob:")) URL.revokeObjectURL(audioSrc);
    audioSrc = "";

    // `audioUrls` is typed as an object but defensive-access it in case the
    // backend / invoke handler returns null on error — blew up a whole route
    // once because `audioUrls[which]` threw before the fallback branch.
    const remote = audioUrls && audioUrls[which];
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

  let confirmingDelete = $state(false);

  function askDeleteCall() {
    confirmingDelete = true;
  }

  async function confirmDeleteCall() {
    if (!call) return;
    deleting = true;
    confirmingDelete = false;
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

  function skip(sec: number) {
    if (!audioEl || !call) return;
    const maxSec = call.duration_ms / 1000;
    audioEl.currentTime = Math.max(0, Math.min(maxSec, audioEl.currentTime + sec));
  }

  function cycleRate() {
    const order = [1, 1.25, 1.5, 2, 0.75];
    const idx = order.indexOf(rate);
    rate = order[(idx + 1) % order.length];
    if (audioEl) audioEl.playbackRate = rate;
  }

  let activeIdx = $derived.by(() => {
    if (!call || !Array.isArray(call.utterances)) return -1;
    for (let i = call.utterances.length - 1; i >= 0; i--) {
      const u = call.utterances[i];
      if (u && (u.start_ms ?? 0) <= currentMs) return u.idx;
    }
    return -1;
  });

  type SpeakerStat = { speaker: string; count: number; totalMs: number };

  let speakers = $derived.by<SpeakerStat[]>(() => {
    if (!call || !Array.isArray(call.utterances)) return [];
    const order: string[] = [];
    const map = new Map<string, SpeakerStat>();
    for (const u of call.utterances) {
      const name = u?.speaker ?? "Unknown";
      const existing = map.get(name);
      if (existing) {
        existing.count++;
        existing.totalMs += (u.end_ms ?? 0) - (u.start_ms ?? 0);
      } else {
        order.push(name);
        map.set(name, {
          speaker: name,
          count: 1,
          totalMs: (u.end_ms ?? 0) - (u.start_ms ?? 0),
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
  // pick from a muted palette tuned for the warm dark surface. Must tolerate
  // null/undefined speaker strings — one bad row otherwise took down the whole
  // call-detail render tree when `[...null]` threw at template time.
  // Called from {@html ...}. Escapes HTML, then wraps each matching
  // speaker name (longest-first so full names beat prefixes) in a
  // colored span. Shares behavior with the portal.
  function renderWithSpeakers(text: string | null | undefined): string {
    if (!text) return "";
    const esc = (s: string) =>
      s
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    let out = esc(text);
    const names = speakers
      .map((s) => ({ name: s.speaker, color: speakerColor(s.speaker) }))
      .sort((a, b) => b.name.length - a.name.length);
    for (const { name, color } of names) {
      if (!name) continue;
      const escName = esc(name);
      const reName = escName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      const re = new RegExp(
        `(^|[^A-Za-z0-9_])(${reName})(?=[^A-Za-z0-9_]|$)`,
        "g",
      );
      out = out.replace(
        re,
        (_m, lead: string, match: string) =>
          `${lead}<span class="spk" style="color:${color}">${match}</span>`,
      );
    }
    return out;
  }

  function speakerColor(speaker: string | null | undefined): string {
    if (!speaker) return "var(--bone-2)";
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

  // Kept in lock-step with Waveform.kindBand() so the chip row reads as the
  // same color as the band above it. Each value is the "edge" color from the
  // waveform — saturated enough to pop on the warm-ink background.
  function kindAccent(kind: string): string {
    switch (kind) {
      case "decision":
        return "#56b8ae"; // bright teal
      case "follow_up":
        return "#f0c86e"; // warm gold
      case "question":
        return "#aac3e1"; // slate
      case "action":
        return "#afd28c"; // olive
      case "bookmark":
      default:
        return "#ffaa87"; // coral
    }
  }

  function kindLabel(kind: string): string {
    return (
      HIGHLIGHT_KINDS.find((k) => k.value === kind)?.label ??
      kind.replace(/_/g, " ")
    );
  }
</script>

<!-- reveal class stripped on this route while we isolate the blank-on-click
     crash — the CSS animation is the last untested variable. -->
<main class="page">
  {#if loading}
    <p class="state" style="--i: 0">Loading call…</p>
  {:else if error}
    <p class="state err" style="--i: 0">{error}</p>
  {:else if call}
    <header class="head" style="--i: 0">
      <a class="back" href="/calls">
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="M10 3 L5 8 L10 13" stroke="currentColor" stroke-width="1.75" fill="none" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
        <span>Calls</span>
      </a>
      <div class="head-row">
        <div class="head-main">
          <p class="dateline">{fmtDateTitle(call.recorded_at)}</p>
          <h1>{call.title ?? "(untitled)"}</h1>
          <p class="chip-row">
            {#if call.matched_client}
              <span class="chip chip-accent">{call.matched_client}</span>
            {/if}
            {#if prettyApp(call.source_app)}
              <span class="chip" title={sourceKindLabel(call.source_kind)}>
                <span class="src-dot" aria-hidden="true"></span>
                {prettyApp(call.source_app)}
              </span>
            {:else if call.source_kind}
              <span class="chip">{sourceKindLabel(call.source_kind)}</span>
            {/if}
          </p>
        </div>
        <button class="delete" disabled={deleting} onclick={askDeleteCall}>
          {deleting ? "Deleting…" : "Delete"}
        </button>
      </div>
    </header>

    <!-- ── Player ───────────────────────────────────────────────────────── -->
    <section class="player" style="--i: 1">
      <div class="wave-host">
        <Waveform
          {peaks}
          audio={audioEl}
          bind:currentMs
          durationMs={call.duration_ms}
          {highlights}
          onseek={(ms) => seekTo(ms)}
          onmark={(r) => createHighlight(r)}
        />
      </div>

      <div class="transport">
        <div class="transport-left">
          <button
            class="t-btn"
            onclick={() => skip(-10)}
            aria-label="Back 10 seconds"
            disabled={!audioSrc}
            title="Back 10s"
          >
            <svg viewBox="0 0 20 20" width="14" height="14" aria-hidden="true">
              <path d="M5 10 L11 5 L11 15 Z" fill="currentColor" />
              <rect x="12.5" y="5" width="1.5" height="10" rx="0.5" fill="currentColor" />
              <text x="10" y="19" text-anchor="middle" font-size="4.5" fill="currentColor" font-family="inherit" font-weight="600">10</text>
            </svg>
          </button>
          <button
            class="t-btn play"
            class:playing
            onclick={togglePlay}
            aria-label={playing ? "Pause" : "Play"}
            disabled={!audioSrc}
          >
            {#if playing}
              <svg viewBox="0 0 20 20" width="15" height="15" aria-hidden="true">
                <rect x="5" y="3.5" width="3.6" height="13" rx="0.8" fill="currentColor" />
                <rect x="11.4" y="3.5" width="3.6" height="13" rx="0.8" fill="currentColor" />
              </svg>
            {:else}
              <svg viewBox="0 0 20 20" width="15" height="15" aria-hidden="true">
                <path d="M6 3.5 L17 10 L6 16.5 Z" fill="currentColor" />
              </svg>
            {/if}
          </button>
          <button
            class="t-btn"
            onclick={() => skip(10)}
            aria-label="Forward 10 seconds"
            disabled={!audioSrc}
            title="Forward 10s"
          >
            <svg viewBox="0 0 20 20" width="14" height="14" aria-hidden="true">
              <path d="M15 10 L9 5 L9 15 Z" fill="currentColor" />
              <rect x="6" y="5" width="1.5" height="10" rx="0.5" fill="currentColor" />
              <text x="10" y="19" text-anchor="middle" font-size="4.5" fill="currentColor" font-family="inherit" font-weight="600">10</text>
            </svg>
          </button>
        </div>

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

      <p class="hint">
        <kbd>Shift</kbd>+drag on the waveform to mark a highlight.
      </p>
    </section>

    <section class="block" style="--i: 1.5">
      <div class="block-head">
        <h2>Highlights</h2>
        <div class="hl-head-actions">
          {#if autoDetectResult}<span class="hl-flash">{autoDetectResult}</span>{/if}
          <button
            class="copy-btn"
            disabled={autoDetecting}
            onclick={autoDetect}
            title="Run an LLM pass over the transcript to auto-mark decisions, objections, commitments, etc."
          >
            {autoDetecting ? "Detecting…" : "Auto-detect"}
          </button>
        </div>
      </div>
      {#if highlights.length === 0}
        <p class="hl-empty">
          None yet. <kbd>Shift</kbd>+drag on the waveform to mark one,
          or tap <strong>Auto-detect</strong> to have the AI scan the call.
        </p>
      {:else}
        <ul class="highlights">
          {#each highlights as h (h.id)}
            <li class="hl" style="--c: {kindAccent(h.kind)}">
              <button
                type="button"
                class="hl-main"
                onclick={() => seekTo(h.start_ms)}
                title="Jump to {fmtTime(h.start_ms)}"
              >
                <span class="hl-dot"></span>
                <span class="hl-time">{fmtTime(h.start_ms)} – {fmtTime(h.end_ms)}</span>
                {#if editingHighlight === h.id}
                  <select
                    class="hl-kind"
                    bind:value={highlightKindEdit}
                    onclick={(e) => e.stopPropagation()}
                  >
                    {#each HIGHLIGHT_KINDS as k (k.value)}
                      <option value={k.value}>{k.label}</option>
                    {/each}
                  </select>
                  <input
                    class="hl-label-input"
                    placeholder="Label (optional)"
                    bind:value={highlightLabelEdit}
                    onclick={(e) => e.stopPropagation()}
                    onkeydown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        saveEditHighlight();
                      }
                      if (e.key === "Escape") editingHighlight = null;
                    }}
                  />
                {:else}
                  <span class="hl-kind-chip">{kindLabel(h.kind)}</span>
                  {#if h.label}
                    <span class="hl-label">{h.label}</span>
                  {:else}
                    <span class="hl-label hl-placeholder">Untitled</span>
                  {/if}
                  {#if h.source === "ai"}
                    <span class="hl-ai" title="Detected by AI">AI</span>
                  {/if}
                {/if}
              </button>
              {#if editingHighlight === h.id}
                <button
                  class="hl-action"
                  aria-label="Save"
                  onclick={saveEditHighlight}
                >Save</button>
                <button
                  class="hl-action"
                  aria-label="Cancel"
                  onclick={() => (editingHighlight = null)}
                >Cancel</button>
              {:else}
                <button
                  class="hl-action"
                  aria-label="Edit highlight"
                  onclick={() => startEditHighlight(h)}
                  title="Edit"
                >Edit</button>
                <button
                  class="hl-action hl-delete"
                  aria-label="Delete highlight"
                  onclick={() => deleteHighlight(h.id)}
                  title="Delete"
                >
                  <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
                    <path d="M4 4 L12 12 M12 4 L4 12" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
                  </svg>
                </button>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    {#if speakers.length > 0}
      <section class="block" style="--i: 2">
        <div class="block-head">
          <h2>Participants</h2>
          <span class="block-hint">Renaming here rewrites transcript, summary, and action items.</span>
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

    {#if call.summary_text}
      <section class="block" style="--i: 3">
        <div class="block-head">
          <h2>Summary</h2>
          <button
            class="copy-btn"
            onclick={() => copy(call!.summary_text ?? "", "summary")}
          >
            {copiedLabel === "summary" ? "Copied" : "Copy"}
          </button>
        </div>
        <p class="summary">{@html renderWithSpeakers(call.summary_text)}</p>
      </section>
    {/if}

    {#if Array.isArray(call.action_items) && call.action_items.length > 0}
      <section class="block" style="--i: 4">
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
              <span>{@html renderWithSpeakers(item)}</span>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <section class="block" style="--i: 5">
      <div class="block-head">
        <h2>Transcript</h2>
        <button class="copy-btn" onclick={copyTranscript}>
          {copiedLabel === "transcript" ? "Copied" : "Copy"}
        </button>
      </div>
      <div class="transcript">
        {#each (call.utterances ?? []) as u (u.idx)}
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

{#if confirmingDelete && call}
  <div
    class="modal-backdrop"
    role="button"
    tabindex="-1"
    onclick={() => (confirmingDelete = false)}
    onkeydown={(e) => {
      if (e.key === "Escape") confirmingDelete = false;
    }}
  >
    <div
      class="modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="confirm-del-title"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      tabindex="-1"
    >
      <h3 id="confirm-del-title">Delete this call?</h3>
      <p class="modal-body">
        <strong>{call.title ?? "Untitled call"}</strong> will be removed from
        the portal and your call list. The audio files stay on disk under
        <code>{call.session_id}</code>.
      </p>
      <div class="modal-actions">
        <button
          class="btn-ghost"
          onclick={() => (confirmingDelete = false)}
          disabled={deleting}
        >
          Cancel
        </button>
        <button
          class="btn-danger"
          onclick={confirmDeleteCall}
          disabled={deleting}
        >
          {deleting ? "Deleting…" : "Delete"}
        </button>
      </div>
    </div>
  </div>
{/if}

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
    gap: 0.45rem;
    font-size: 0.88rem;
    font-weight: 500;
    color: var(--bone-2);
    padding: 0.4rem 0.8rem 0.4rem 0.55rem;
    margin: 0 0 1.1rem -0.55rem;
    border-radius: var(--radius);
    border: 1px solid transparent;
    transition: all 0.15s;
  }
  .back svg {
    width: 13px;
    height: 13px;
    transition: transform 0.15s;
  }
  .back:hover {
    color: var(--bone-0);
    background: var(--ink-2);
    border-color: var(--hairline);
  }
  .back:hover svg {
    transform: translateX(-2px);
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

  .chip-row {
    margin: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    align-items: center;
  }

  .src-dot {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--bone-3);
    display: inline-block;
    margin-right: 0.3rem;
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

  .transport-left {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }

  .t-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 34px;
    height: 34px;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    transition: all 0.15s;
  }

  .t-btn:hover:not(:disabled) {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }

  .t-btn.play {
    width: 42px;
    height: 42px;
    border-radius: 50%;
    border-color: var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    box-shadow: 0 8px 22px -12px var(--accent-glow);
  }
  .t-btn.play:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
    color: var(--ink-0);
  }
  .t-btn.play.playing {
    background: var(--ink-2);
    color: var(--accent);
    border-color: var(--accent);
    box-shadow: none;
  }
  .t-btn:disabled {
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

  .player .hint {
    margin: 0.8rem 0 0;
    font-size: 0.75rem;
    color: var(--bone-3);
    letter-spacing: 0.005em;
  }

  kbd {
    display: inline-block;
    font-family: var(--font-mono);
    font-size: 0.7rem;
    padding: 0.08rem 0.38rem;
    border: 1px solid var(--hairline);
    border-radius: 4px;
    background: var(--ink-2);
    color: var(--bone-2);
    letter-spacing: 0.02em;
    margin-right: 0.2rem;
  }

  /* ── Highlights panel ──────────────────────────────────────────────── */
  .hl-head-actions {
    display: flex;
    align-items: center;
    gap: 0.7rem;
  }

  .hl-flash {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    letter-spacing: 0.02em;
    color: var(--olive);
  }

  .hl-empty {
    margin: 0.4rem 0 0;
    padding: 0.9rem 1rem;
    border: 1px dashed var(--hairline);
    border-radius: var(--radius);
    color: var(--bone-3);
    font-size: 0.85rem;
    background: var(--ink-1);
  }

  .hl-empty strong {
    color: var(--bone-1);
    font-weight: 500;
  }

  .highlights {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    border-top: 1px solid var(--hairline);
  }

  .hl {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.5rem 0.1rem;
    border-bottom: 1px solid var(--hairline);
  }

  .hl-main {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.2rem 0.4rem;
    border-radius: 6px;
    background: none;
    color: inherit;
    text-align: left;
    transition: background 0.12s;
  }
  .hl-main:hover {
    background: var(--ink-1);
  }

  .hl-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--c);
    flex-shrink: 0;
  }

  .hl-time {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--bone-3);
    letter-spacing: 0.02em;
    flex-shrink: 0;
  }

  .hl-kind-chip {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--c);
    padding: 0.1rem 0.45rem;
    border: 1px solid currentColor;
    border-radius: 4px;
    flex-shrink: 0;
    opacity: 0.85;
  }

  .hl-label {
    color: var(--bone-1);
    font-size: 0.88rem;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hl-placeholder {
    color: var(--bone-4);
    font-style: italic;
  }

  .hl-ai {
    font-family: var(--font-mono);
    font-size: 0.62rem;
    letter-spacing: 0.12em;
    color: var(--sig);
    border: 1px solid rgba(201, 162, 74, 0.4);
    padding: 0.05rem 0.35rem;
    border-radius: 3px;
    margin-left: auto;
  }

  .hl-kind {
    padding: 0.3rem 0.5rem;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font-size: 0.82rem;
  }

  .hl-label-input {
    flex: 1;
    padding: 0.35rem 0.55rem;
    border-radius: 6px;
    border: 1px solid var(--hairline-hi);
    background: var(--ink-0);
    color: var(--bone-0);
    font-size: 0.88rem;
    min-width: 0;
  }
  .hl-label-input:focus {
    outline: none;
    border-color: var(--accent);
  }

  .hl-action {
    padding: 0.25rem 0.6rem;
    font-size: 0.72rem;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    background: transparent;
    color: var(--bone-2);
    transition: all 0.15s;
  }
  .hl-action:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .hl-action.hl-delete:hover {
    border-color: var(--live);
    color: var(--live);
  }
  .hl-action.hl-delete {
    display: flex;
    align-items: center;
    padding: 0.25rem 0.45rem;
  }

  /* ── Block common ──────────────────────────────────────────────────── */
  .block {
    margin-bottom: 2rem;
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
    max-width: 74ch;
  }
  :global(.spk) {
    font-weight: 500;
  }
  .block-hint {
    font-size: 0.72rem;
    color: var(--bone-3);
    letter-spacing: normal;
    text-transform: none;
    font-weight: 400;
  }

  /* ── Actions ───────────────────────────────────────────────────────── */
  .actions {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 0;
    max-width: 74ch;
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

  /* ── Delete confirm modal ──────────────────────────────────────────── */
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(3px);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 50;
    padding: 1rem;
    cursor: default;
  }

  .modal {
    max-width: 440px;
    width: 100%;
    padding: 1.4rem 1.5rem 1.2rem;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    box-shadow: 0 22px 40px -12px rgba(0, 0, 0, 0.55);
    cursor: auto;
  }

  .modal h3 {
    margin: 0 0 0.6rem;
    font-size: 1.05rem;
    color: var(--bone-0);
    font-weight: 600;
  }

  .modal-body {
    margin: 0 0 1.1rem;
    color: var(--bone-1);
    font-size: 0.9rem;
    line-height: 1.55;
  }

  .modal-body code {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--bone-2);
    background: var(--ink-2);
    padding: 0.08rem 0.35rem;
    border-radius: 4px;
  }

  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
  }

  .btn-ghost,
  .btn-danger {
    padding: 0.5rem 1rem;
    font-size: 0.85rem;
    font-weight: 500;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    transition: all 0.15s;
  }

  .btn-ghost {
    background: transparent;
    color: var(--bone-2);
  }
  .btn-ghost:hover:not(:disabled) {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }

  .btn-danger {
    background: var(--live);
    border-color: var(--live);
    color: #fff;
  }
  .btn-danger:hover:not(:disabled) {
    filter: brightness(1.1);
  }

  .btn-ghost:disabled,
  .btn-danger:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
</style>
