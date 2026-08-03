<script lang="ts" module>
  import type {
    ScreenPlaybackUrl,
    ScreenRecording,
  } from "@aftercalls/shared/types";

  export type { ScreenPlaybackUrl, ScreenRecording };

  export type ScreenRecordingPlayerProps = {
    /** Call id — passed straight to `fetchMeta`. */
    callId: string;
    /** The AUDIO master clock, in ms (owned by the parent's `<audio>`
     *  via `ontimeupdate` + every seek path). The video is a follower:
     *  it never writes this back. */
    currentMs: number;
    /** Whether the audio element is currently playing. The muted video
     *  mirrors this (play/pause) once it's past `start_offset_ms`. */
    playing: boolean;
    /** Playback speed of the audio master. The muted video mirrors it so
     *  accelerated playback does not continuously drift and seek. */
    playbackRate?: number;
    /** Call/audio duration in ms, for clamping the follower. Falls back
     *  to the recording's own `duration_ms` when absent. */
    durationMs?: number;
    /** Surface-specific metadata fetch. Agent → `get_screen_recording`
     *  Tauri command; portal → `api.calls.screenRecording`. Resolves to
     *  `null` on a 404 (org flag off OR no row) → the player renders
     *  nothing. */
    fetchMeta: (callId: string) => Promise<ScreenRecording | null>;
    /** Mint a short-lived playback credential. Called only after the user
     *  expands a ready recording. The returned immutable generation id is
     *  correlated against the metadata before `<video src>` is updated. */
    fetchPlaybackUrl: (callId: string) => Promise<ScreenPlaybackUrl>;
  };
</script>

<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import {
    classifyScreenPlaybackCandidate,
    screenPlaybackRefreshDelayMs,
  } from "@aftercalls/shared/screenPlayback";

  let {
    callId,
    currentMs,
    playing,
    playbackRate = 1,
    durationMs,
    fetchMeta,
    fetchPlaybackUrl,
  }: ScreenRecordingPlayerProps = $props();

  // `undefined` = still loading (render nothing); `null` = absent
  // (flag off OR no row → render nothing, byte-identical to an
  // audio-only call); otherwise the fetched row drives the state table.
  let meta = $state<ScreenRecording | null | undefined>(undefined);
  let expanded = $state(false);
  let videoEl = $state<HTMLVideoElement | undefined>(undefined);
  let buffering = $state(false);
  let playbackError = $state(false);
  let playbackLoading = $state(false);
  let videoSrc = $state("");
  let playbackGenerationId = $state<string | null>(null);
  let playbackExpiresAt = $state<string | null>(null);
  // `failed` is dismissable inline — it must never block the page.
  let failedDismissed = $state(false);

  let pollHandle: ReturnType<typeof setTimeout> | null = null;
  let playbackRefreshHandle: ReturnType<typeof setTimeout> | null = null;
  let destroyed = false;
  let loadSequence = 0;
  let playbackSequence = 0;
  let transientFailures = 0;
  const TRANSIENT_RETRY_DELAYS_MS = [
    1_000, 2_000, 5_000, 10_000, 20_000, 30_000, 30_000, 30_000,
  ] as const;

  function isProcessing(m: ScreenRecording | null | undefined): boolean {
    return m?.status === "recording" || m?.status === "uploading";
  }

  function stopPlaybackRefresh() {
    if (playbackRefreshHandle) {
      clearTimeout(playbackRefreshHandle);
      playbackRefreshHandle = null;
    }
  }

  function clearPlaybackCredential(cancelRequest = true) {
    if (cancelRequest) playbackSequence += 1;
    stopPlaybackRefresh();
    videoSrc = "";
    playbackGenerationId = null;
    playbackExpiresAt = null;
    playbackLoading = false;
    buffering = false;
  }

  function applyMetadata(next: ScreenRecording | null) {
    meta = next;
    if (
      playbackGenerationId &&
      (next?.status !== "ready" ||
        playbackGenerationId !== next.generation_id)
    ) {
      // A credential is scoped to one immutable generation. A metadata
      // refresh that points elsewhere invalidates it immediately.
      clearPlaybackCredential(false);
    }
  }

  async function load(requestedCallId = callId) {
    stopPoll();
    const sequence = ++loadSequence;
    try {
      const next = await fetchMeta(requestedCallId);
      if (
        destroyed ||
        sequence !== loadSequence ||
        requestedCallId !== callId
      )
        return;
      transientFailures = 0;
      applyMetadata(next);
      // Keep polling while the row is still finalizing; stop otherwise.
      if (isProcessing(next)) schedulePoll(5_000);
      else stopPoll();
    } catch {
      // A transient fetch error shouldn't fabricate an error surface on
      // the page — treat it as "not known yet" and let a later poll (or
      // a page reload) settle it. Absent-until-known keeps the common
      // audio-only call untouched.
      if (
        destroyed ||
        sequence !== loadSequence ||
        requestedCallId !== callId
      )
        return;
      if (meta === undefined) meta = null;
      // Initial network failures used to permanently collapse the player
      // until a full page reload. Retry with bounded exponential backoff;
      // terminal `null` responses still stop immediately on the success path.
      if (transientFailures < TRANSIENT_RETRY_DELAYS_MS.length) {
        const delay = TRANSIENT_RETRY_DELAYS_MS[transientFailures];
        transientFailures += 1;
        schedulePoll(delay);
      }
    }
  }

  function schedulePoll(delayMs: number) {
    stopPoll();
    pollHandle = setTimeout(() => {
      pollHandle = null;
      void load();
    }, delayMs);
  }
  function stopPoll() {
    if (pollHandle) {
      clearTimeout(pollHandle);
      pollHandle = null;
    }
  }

  // SvelteKit reuses the same `[id]` route component when navigating directly
  // between calls. Reset/reload on the id itself so metadata, errors, expansion,
  // and retry timers from call N never bleed into call N+1.
  $effect(() => {
    const activeCallId = callId;
    untrack(() => {
      loadSequence += 1;
      stopPoll();
      transientFailures = 0;
      meta = undefined;
      expanded = false;
      playbackError = false;
      failedDismissed = false;
      clearPlaybackCredential();
      void load(activeCallId);
    });
    return () => {
      untrack(() => {
        loadSequence += 1;
        stopPoll();
        clearPlaybackCredential();
      });
    };
  });

  onDestroy(() => {
    destroyed = true;
    loadSequence += 1;
    stopPoll();
    clearPlaybackCredential();
  });

  function playbackRequestIsCurrent(
    request: number,
    requestedCallId: string,
  ): boolean {
    return (
      !destroyed &&
      request === playbackSequence &&
      requestedCallId === callId &&
      expanded
    );
  }

  function schedulePlaybackRefresh(
    requestedCallId: string,
    generationId: string,
    delayMs: number,
  ) {
    stopPlaybackRefresh();
    playbackRefreshHandle = setTimeout(() => {
      playbackRefreshHandle = null;
      if (
        destroyed ||
        !expanded ||
        requestedCallId !== callId ||
        meta?.status !== "ready" ||
        meta.generation_id !== generationId
      ) {
        return;
      }
      void mintPlayback(requestedCallId);
    }, Math.max(0, delayMs));
  }

  function currentCredentialIsUsable(nowMs = Date.now()): boolean {
    if (
      !videoSrc ||
      !playbackExpiresAt ||
      meta?.status !== "ready" ||
      playbackGenerationId !== meta.generation_id
    ) {
      return false;
    }
    const expiresAtMs = Date.parse(playbackExpiresAt);
    return Number.isFinite(expiresAtMs) && expiresAtMs > nowMs;
  }

  function installPlayback(
    requestedCallId: string,
    recording: ScreenRecording,
    candidate: ScreenPlaybackUrl,
  ) {
    videoSrc = candidate.playback_url;
    playbackGenerationId = candidate.generation_id;
    playbackExpiresAt = candidate.playback_url_expires_at;
    playbackLoading = false;
    playbackError = false;
    const refreshDelay = screenPlaybackRefreshDelayMs(
      candidate.playback_url_expires_at,
    );
    if (refreshDelay !== null) {
      schedulePlaybackRefresh(
        requestedCallId,
        recording.generation_id,
        refreshDelay,
      );
    }
  }

  async function mintPlayback(requestedCallId = callId) {
    const startingMeta = meta;
    if (
      destroyed ||
      !expanded ||
      requestedCallId !== callId ||
      startingMeta?.status !== "ready"
    ) {
      return;
    }

    stopPlaybackRefresh();
    const request = ++playbackSequence;
    playbackLoading = true;
    playbackError = false;
    let authorityMismatchSeen = false;

    try {
      // A newly published generation can race the metadata read. At most one
      // metadata refresh + second mint is needed to converge on a stable pair.
      for (let attempt = 0; attempt < 2; attempt += 1) {
        const candidate = await fetchPlaybackUrl(requestedCallId);
        if (!playbackRequestIsCurrent(request, requestedCallId)) return;

        let recording = meta;
        let classification = classifyScreenPlaybackCandidate(
          recording,
          candidate,
        );
        if (classification === "generation_mismatch") {
          authorityMismatchSeen = true;
          const refreshed = await fetchMeta(requestedCallId);
          if (!playbackRequestIsCurrent(request, requestedCallId)) return;
          if (refreshed === null) {
            // The playback endpoint just proved a current generation exists;
            // a contradictory metadata 404 is transient. Keep the ready row
            // visible so Retry can converge instead of hiding the player.
            throw new Error("screen metadata disappeared during playback mint");
          }
          applyMetadata(refreshed);
          recording = refreshed;
          classification = classifyScreenPlaybackCandidate(
            recording,
            candidate,
          );
        }

        if (classification === "ready" && recording) {
          installPlayback(requestedCallId, recording, candidate);
          return;
        }
        if (classification === "recording_not_ready") {
          clearPlaybackCredential(false);
          playbackError = false;
          return;
        }
        if (classification !== "generation_mismatch" || attempt === 1) {
          throw new Error(`screen playback credential ${classification}`);
        }
      }
    } catch {
      if (!playbackRequestIsCurrent(request, requestedCallId)) return;
      playbackLoading = false;
      if (!authorityMismatchSeen && currentCredentialIsUsable()) {
        // A proactive refresh failure must not discard a still-valid source.
        // Retry quietly while the current credential can continue playback.
        const remainingMs =
          Date.parse(playbackExpiresAt ?? "") - Date.now();
        schedulePlaybackRefresh(
          requestedCallId,
          playbackGenerationId ?? startingMeta.generation_id,
          Math.max(1_000, Math.min(5_000, remainingMs - 1_000)),
        );
      } else {
        clearPlaybackCredential(false);
        playbackError = true;
      }
    }
  }

  function ensurePlayback() {
    const recording = meta;
    if (recording?.status !== "ready" || !expanded) return;
    if (currentCredentialIsUsable()) {
      const refreshDelay = screenPlaybackRefreshDelayMs(playbackExpiresAt!);
      if (refreshDelay !== null) {
        schedulePlaybackRefresh(callId, recording.generation_id, refreshDelay);
      }
      return;
    }
    clearPlaybackCredential();
    void mintPlayback(callId);
  }

  let effectiveDurationMs = $derived(durationMs ?? meta?.duration_ms ?? 0);

  // Turn precede is when the transcript/playhead sits BEFORE the capture
  // started — the video has no frame yet, so we park on frame 0 with a
  // calm caption instead of a black flash.
  let precede = $derived(
    !!meta && currentMs < (meta.start_offset_ms ?? 0),
  );

  // ── The follower: keep the muted video aligned to the audio clock ──
  //
  // Runs whenever the audio clock or play state moves. We only *correct*
  // the frame when it has drifted past a threshold so we don't fight the
  // video's own playback every timeupdate tick, and we mirror play/pause
  // exactly once per transition.
  function syncVideoToAudio(force = false) {
    const v = videoEl;
    const m = meta;
    const clockMs = currentMs;
    const isPlaying = playing;
    const requestedRate = playbackRate;
    if (!v || !m || m.status !== "ready" || !expanded || !videoSrc) return;

    const followerRate =
      Number.isFinite(requestedRate) && requestedRate > 0
        ? requestedRate
        : 1;
    if (v.playbackRate !== followerRate) {
      v.playbackRate = followerRate;
    }

    const offset = m.start_offset_ms ?? 0;
    if (clockMs < offset) {
      // Before capture began — hold frame 0, keep it paused.
      if (v.currentTime > 0.05) v.currentTime = 0;
      if (!v.paused) v.pause();
      return;
    }

    const durSec = effectiveDurationMs > 0 ? (effectiveDurationMs - offset) / 1000 : Infinity;
    const target = Math.max(0, Math.min((clockMs - offset) / 1000, durSec));
    // Correct only on meaningful drift (seek, skip, transcript click).
    if (Math.abs(v.currentTime - target) > (force ? 0.05 : 0.35)) {
      v.currentTime = target;
    }
    // Mirror transport play/pause. Guarded so we don't spam play().
    if (isPlaying && v.paused) {
      void v.play().catch(() => {});
    } else if (!isPlaying && !v.paused) {
      v.pause();
    }
  }

  $effect(() => {
    // Read the reactive deps before entering the helper so the effect follows
    // the audio clock/play state even though video itself is imperative.
    currentMs;
    playing;
    effectiveDurationMs;
    expanded;
    videoEl;
    meta;
    syncVideoToAudio(false);
  });

  let transportRepairQueued = false;
  function enforceAudioMaster() {
    if (transportRepairQueued) return;
    transportRepairQueued = true;
    queueMicrotask(() => {
      transportRepairQueued = false;
      syncVideoToAudio(true);
    });
  }

  function toggleExpand() {
    if (expanded) {
      expanded = false;
      playbackSequence += 1;
      playbackLoading = false;
      stopPlaybackRefresh();
      if (videoEl && !videoEl.paused) videoEl.pause();
      return;
    }
    expanded = true;
    playbackError = false;
    ensurePlayback();
  }

  function onVideoWaiting() {
    buffering = true;
  }
  function onVideoPlaying() {
    buffering = false;
    playbackError = false;
    enforceAudioMaster();
  }
  function onVideoPaused() {
    enforceAudioMaster();
  }
  function onVideoSeeking() {
    enforceAudioMaster();
  }
  function onVideoError() {
    buffering = false;
    if (!videoSrc) return;
    // The retry action always re-mints instead of reusing this source, which
    // covers expiry and Range failures without an automatic error loop for a
    // genuinely corrupt object.
    playbackError = true;
  }

  function retryPlayback() {
    playbackError = false;
    clearPlaybackCredential();
    void mintPlayback(callId);
  }

  function fmtDuration(ms: number | undefined): string {
    if (!ms || ms <= 0) return "";
    const total = Math.floor(ms / 1000);
    const mm = Math.floor(total / 60);
    const ss = total % 60;
    return `${mm}:${ss.toString().padStart(2, "0")}`;
  }
</script>

{#snippet monitorGlyph(size: number)}
  <svg
    class="scr-glyph"
    viewBox="0 0 20 20"
    width={size}
    height={size}
    aria-hidden="true"
  >
    <rect
      x="2.2"
      y="3.5"
      width="15.6"
      height="10"
      rx="1.4"
      fill="none"
      stroke="currentColor"
      stroke-width="1.4"
    />
    <path
      d="M7.5 16.5 h5 M10 13.5 v3"
      stroke="currentColor"
      stroke-width="1.4"
      stroke-linecap="round"
      fill="none"
    />
  </svg>
{/snippet}

{#if meta === null || meta === undefined}
  <!-- absent / loading → nothing (byte-identical to an audio-only call) -->
{:else if isProcessing(meta)}
  <div class="scr-stage scr-bar scr-processing" role="note">
    <span class="scr-pip scr-pip-proc" aria-hidden="true"></span>
    {@render monitorGlyph(15)}
    <span class="scr-bar-title">Screen recording still uploading…</span>
  </div>
{:else if meta.status === "expired"}
  <div class="scr-stage scr-expired" role="note">
    {@render monitorGlyph(15)}
    <span class="scr-expired-text">
      Screen recording expired — it's no longer available. The call,
      transcript, and summary are kept.
    </span>
  </div>
{:else if meta.status === "failed" && !failedDismissed}
  <div class="scr-stage scr-bar scr-failed" role="note">
    <span class="scr-pip scr-pip-failed" aria-hidden="true"></span>
    {@render monitorGlyph(15)}
    <span class="scr-bar-title">
      Screen recording didn't finish. Audio and transcript are unaffected.
    </span>
    <button
      type="button"
      class="scr-dismiss"
      onclick={() => (failedDismissed = true)}
      aria-label="Dismiss screen-recording notice"
    >
      Dismiss
    </button>
  </div>
{:else if meta.status === "ready"}
  <div class="scr-stage">
    <button
      type="button"
      class="scr-bar scr-bar-btn"
      aria-expanded={expanded}
      onclick={toggleExpand}
    >
      <span class="scr-pip scr-pip-ready" aria-hidden="true"></span>
      {@render monitorGlyph(15)}
      <span class="scr-bar-title">Screen recording</span>
      {#if fmtDuration(meta.duration_ms)}
        <span class="scr-bar-dur">{fmtDuration(meta.duration_ms)}</span>
      {/if}
      <svg
        class="scr-chevron"
        class:scr-chevron-open={expanded}
        viewBox="0 0 20 20"
        width="14"
        height="14"
        aria-hidden="true"
      >
        <path
          d="M6 8 L10 12 L14 8"
          stroke="currentColor"
          stroke-width="1.6"
          stroke-linecap="round"
          stroke-linejoin="round"
          fill="none"
        />
      </svg>
    </button>

    {#if expanded}
      <div
        class="scr-video-wrap"
        class:scr-buffering={buffering || playbackLoading}
      >
        <!-- svelte-ignore a11y_media_has_caption -->
        <video
          bind:this={videoEl}
          src={videoSrc}
          class="scr-video"
          muted
          playsinline
          preload="metadata"
          tabindex="-1"
          aria-label="Screen recording for this call"
          onwaiting={onVideoWaiting}
          onplay={enforceAudioMaster}
          onpause={onVideoPaused}
          onseeking={onVideoSeeking}
          onplaying={onVideoPlaying}
          oncanplay={onVideoPlaying}
          onerror={onVideoError}
        ></video>
        {#if precede}
          <p class="scr-precede">Screen capture started a few seconds into the call.</p>
        {/if}
        {#if playbackError}
          <div class="scr-playback-err" role="alert" aria-live="polite">
            <span>Couldn't load the screen recording.</span>
            <button type="button" class="scr-retry" onclick={retryPlayback}>
              Retry
            </button>
          </div>
        {/if}
      </div>
    {/if}
  </div>
{/if}

<style>
  /* All chrome is component-scoped (the byte-identical app.css rule
   * holds). Only design tokens (`--live`/`--sig`/`--olive`/`--ink-*`/
   * `--bone-*`/`--hairline`/`--radius`/`--font-mono`) are borrowed from
   * the global :root. Colour law mirrors design.md §Screen recording
   * player: live=`--live`, processing=`--sig`, ready=`--olive`,
   * expired/failed-calm=bone/hairline. No fifth hue. */
  .scr-stage {
    margin: 0 0 0.75rem;
  }

  .scr-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.5rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
    color: var(--bone-1);
    font-family: var(--font-sans);
    font-size: 0.85rem;
    text-align: left;
  }
  .scr-bar-btn {
    appearance: none;
    cursor: pointer;
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .scr-bar-btn:hover {
    border-color: var(--hairline-hi);
    background: var(--ink-2);
  }
  .scr-bar-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .scr-glyph {
    flex-shrink: 0;
    color: var(--bone-2);
  }

  .scr-bar-title {
    color: var(--bone-0);
    font-weight: 500;
    /* Push the trailing duration + chevron cluster to the right edge. */
    margin-right: auto;
  }
  .scr-bar-dur {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--bone-2);
  }
  .scr-chevron {
    color: var(--bone-3);
    transition: transform 0.15s ease;
  }
  .scr-chevron-open {
    transform: rotate(180deg);
  }

  /* Leading pip — recreates the app.css `.pip` vocabulary locally
   * (that class is component-scoped in the layout, not global). */
  .scr-pip {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .scr-pip-ready {
    background: var(--olive);
    box-shadow: 0 0 5px var(--olive-soft);
  }
  .scr-pip-proc {
    background: var(--sig);
    animation: scr-blink 1s ease-in-out infinite;
  }
  .scr-pip-failed {
    background: var(--live);
  }
  @keyframes scr-blink {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.45;
    }
  }

  /* Processing bar reads amber via its pip; failed bar keeps the calm
   * neutral surface (never `--live` ground) but flags with a red pip +
   * a dismiss. */
  .scr-processing .scr-bar-title {
    color: var(--bone-1);
  }
  .scr-failed {
    border-color: var(--hairline);
  }
  .scr-dismiss {
    appearance: none;
    margin-left: auto;
    padding: 0.2rem 0.55rem;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
    transition:
      color 0.15s,
      border-color 0.15s;
  }
  .scr-dismiss:hover {
    color: var(--bone-0);
    border-color: var(--bone-3);
  }

  /* Expired — the design.md track-quality-note recipe: bone text on a
   * hairline surface, monitor glyph dimmed. Calm, not an error. */
  .scr-expired {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.6rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
    color: var(--bone-2);
    font-size: 0.82rem;
    line-height: 1.45;
  }
  .scr-expired .scr-glyph {
    opacity: 0.6;
  }
  .scr-expired-text {
    color: var(--bone-2);
  }

  .scr-video-wrap {
    position: relative;
    margin-top: 0.5rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    overflow: hidden;
    background: var(--ink-0);
  }
  .scr-video {
    display: block;
    width: 100%;
    max-width: 100%;
    aspect-ratio: 16 / 9;
    background: var(--ink-0);
    object-fit: contain;
  }

  /* Buffering — a thin `--sig` shimmer along the video's bottom edge. */
  .scr-buffering::after {
    content: "";
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    height: 2px;
    background: linear-gradient(
      90deg,
      transparent,
      var(--sig),
      transparent
    );
    background-size: 40% 100%;
    animation: scr-shimmer 1.1s linear infinite;
  }
  @keyframes scr-shimmer {
    from {
      background-position: -40% 0;
    }
    to {
      background-position: 140% 0;
    }
  }

  .scr-precede {
    margin: 0;
    padding: 0.4rem 0.6rem;
    font-size: 0.78rem;
    color: var(--bone-2);
    background: var(--ink-1);
    border-top: 1px solid var(--hairline);
  }

  .scr-playback-err {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.5rem 0.7rem;
    font-size: 0.82rem;
    color: var(--bone-1);
    background: var(--ink-1);
    border-top: 1px solid var(--hairline);
  }
  .scr-retry {
    appearance: none;
    padding: 0.25rem 0.7rem;
    border: 1px solid var(--accent);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--accent);
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
    transition:
      background 0.15s,
      color 0.15s;
  }
  .scr-retry:hover {
    background: var(--accent-soft);
    color: var(--accent-hi);
  }

  @media (max-width: 640px) {
    .scr-bar {
      flex-wrap: wrap;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .scr-pip-proc,
    .scr-buffering::after,
    .scr-chevron {
      animation: none;
      transition: none;
    }
  }
</style>
