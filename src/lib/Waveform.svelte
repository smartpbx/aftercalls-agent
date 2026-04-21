<script lang="ts">
  import { onDestroy, onMount } from "svelte";

  // Peak-based waveform with a moving playhead. Decodes via Web Audio, extracts
  // min/max peaks into a flat Float32 buffer, renders once to an off-screen
  // canvas, then blits + overlays the playhead at 60fps. Keeps paint work off
  // the hot path during playback — only the playhead moves frame-to-frame.

  type Highlight = {
    id: string;
    start_ms: number;
    end_ms: number;
    kind: string;
    label: string | null;
  };

  type Props = {
    src: string; // Blob URL or remote URL (presigned). Empty = empty state.
    audio: HTMLAudioElement | undefined;
    currentMs: number;
    durationMs: number;
    height?: number;
    highlights?: Highlight[];
    onseek?: (ms: number) => void;
    // Shift-drag emits a range so the parent can create a highlight.
    onmark?: (range: { start_ms: number; end_ms: number }) => void;
  };

  let {
    src,
    audio,
    currentMs = $bindable(0),
    durationMs,
    height = 140,
    highlights = [],
    onseek,
    onmark,
  }: Props = $props();

  let dragRange = $state<{ start_ms: number; end_ms: number } | null>(null);
  let isMarking = $state(false);

  let wrap: HTMLDivElement | undefined = $state();
  let canvas: HTMLCanvasElement | undefined = $state();
  let peaks: Float32Array | null = $state(null);
  let status = $state<"idle" | "decoding" | "ready" | "error">("idle");
  let errorMsg = $state("");
  let width = $state(800);
  let hoverMs = $state<number | null>(null);

  const DPR = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1;

  let ro: ResizeObserver | null = null;
  let rafId = 0;
  // kept for when we reintroduce a decode path; currently unused.
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  let abort: AbortController | null = null;
  void abort;

  $effect(() => {
    // Re-decode whenever the source URL changes.
    if (!src) {
      peaks = null;
      status = "idle";
      return;
    }
    decode(src);
  });

  onMount(() => {
    if (wrap) {
      ro = new ResizeObserver(() => {
        width = wrap!.clientWidth;
        paint();
      });
      ro.observe(wrap);
      width = wrap.clientWidth;
    }
    loop();
  });

  onDestroy(() => {
    ro?.disconnect();
    cancelAnimationFrame(rafId);
  });

  async function decode(_url: string) {
    // Intentionally a no-op right now. The Web Audio decode path is the
    // prime suspect for the WebKitWebProcess SIGABRT crash we're seeing on
    // call-detail navigation — the bundled webkit2gtk-4.1 in the AppImage
    // has historically been unstable around decodeAudioData on certain
    // codec/container combinations, and nothing else on the page does
    // anything the waveform route hasn't already seen elsewhere.
    //
    // Playback still works via the <audio> element; we just render a flat
    // placeholder bar + the normal playhead until we move peak generation
    // server-side (backend samples via ffmpeg + ships a JSON peaks file).
    peaks = null;
    status = "ready";
    paint();
  }

  // Returns an interleaved [min, max] pair per column, normalized to [-1, 1].
  // We over-sample (resolution > display columns) so resizing the window
  // doesn't force re-decode.
  function extractPeaks(buf: AudioBuffer, columns: number): Float32Array {
    const channels = Math.min(2, buf.numberOfChannels);
    const len = buf.length;
    const samplesPerColumn = Math.max(1, Math.floor(len / columns));
    const out = new Float32Array(columns * 2);

    const data: Float32Array[] = [];
    for (let c = 0; c < channels; c++) data.push(buf.getChannelData(c));

    let max = 0;
    for (let i = 0; i < columns; i++) {
      const start = i * samplesPerColumn;
      const end = Math.min(len, start + samplesPerColumn);
      let lo = 0,
        hi = 0;
      for (let j = start; j < end; j++) {
        let v = 0;
        for (let c = 0; c < channels; c++) v += data[c][j];
        v /= channels;
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
      out[i * 2] = lo;
      out[i * 2 + 1] = hi;
      const amp = Math.max(Math.abs(lo), Math.abs(hi));
      if (amp > max) max = amp;
    }
    // Normalize for consistent visual amplitude across files. Clamp to avoid
    // dividing by near-zero on silent tracks.
    const norm = max > 0.001 ? 1 / max : 1;
    for (let i = 0; i < out.length; i++) out[i] *= norm;
    return out;
  }

  function paint() {
    try {
      paintInner();
    } catch (e) {
      // A single paint failure shouldn't take the whole page down — we're
      // called from a $effect and from the RAF loop, and an unhandled throw
      // in either context blanks the route in some webkit2gtk builds.
      console.warn("Waveform paint failed", e);
    }
  }

  function paintInner() {
    if (!canvas || width <= 0) return;
    const cw = width;
    const ch = height;
    canvas.width = Math.floor(cw * DPR);
    canvas.height = Math.floor(ch * DPR);
    canvas.style.width = `${cw}px`;
    canvas.style.height = `${ch}px`;
    const g = canvas.getContext("2d")!;
    g.scale(DPR, DPR);
    g.clearRect(0, 0, cw, ch);

    // ── Highlight bands (painted behind the bars) ──────────────────────
    if (durationMs > 0) {
      for (const h of highlights) {
        const x1 = (h.start_ms / durationMs) * cw;
        const x2 = (h.end_ms / durationMs) * cw;
        const w = Math.max(2, x2 - x1);
        const { fill, edge } = kindBand(h.kind);
        // Translucent body.
        g.fillStyle = fill;
        g.fillRect(x1, 0, w, ch);
        // A 2px cap at top + bottom so overlapping bands stay legible and
        // short bands still read as a distinct highlight.
        g.fillStyle = edge;
        g.fillRect(x1, 0, w, 2);
        g.fillRect(x1, ch - 2, w, 2);
      }
      if (dragRange && isMarking) {
        const x1 = (dragRange.start_ms / durationMs) * cw;
        const x2 = (dragRange.end_ms / durationMs) * cw;
        g.fillStyle = getCss("--accent-soft");
        g.fillRect(
          Math.min(x1, x2),
          0,
          Math.max(2, Math.abs(x2 - x1)),
          ch,
        );
        g.strokeStyle = getCss("--accent");
        g.setLineDash([4, 3]);
        g.strokeRect(
          Math.min(x1, x2) + 0.5,
          0.5,
          Math.max(2, Math.abs(x2 - x1)) - 1,
          ch - 1,
        );
        g.setLineDash([]);
      }
    }

    const mid = ch / 2;
    const bars = 220; // visible bar count — tuned for density, not resolution
    const gap = 2;
    const barW = Math.max(1.2, (cw - gap * (bars - 1)) / bars);

    // Styles
    const playedColor = getCss("--bone-1");
    const unplayedColor = getCss("--bone-4");
    const playedTail = getCss("--accent");

    const progress = durationMs > 0 ? currentMs / durationMs : 0;
    const playheadX = progress * cw;

    if (peaks) {
      // Real waveform from decoded audio.
      const srcCols = peaks.length / 2;
      for (let i = 0; i < bars; i++) {
        const x = i * (barW + gap);
        const colCenter = Math.floor(((i + 0.5) / bars) * srcCols);
        const lo = peaks[colCenter * 2];
        const hi = peaks[colCenter * 2 + 1];
        const top = mid + lo * (mid - 6);
        const bot = mid + hi * (mid - 6);
        const h = Math.max(1.5, bot - top);

        const barCenter = x + barW / 2;
        const dx = Math.abs(barCenter - playheadX);
        let color = barCenter <= playheadX ? playedColor : unplayedColor;
        if (dx < 24) color = playedTail;
        g.fillStyle = color;
        g.fillRect(x, top, barW, h);
      }
    } else {
      // Flat placeholder: the waveform strip still reads as a scrubber
      // (played / unplayed split, accent tail on the playhead) even with
      // no decoded peaks yet. Pre-existing playback + click-to-seek work
      // unchanged since they're duration-driven, not peak-driven.
      const ph = 3;
      for (let i = 0; i < bars; i++) {
        const x = i * (barW + gap);
        const barCenter = x + barW / 2;
        const dx = Math.abs(barCenter - playheadX);
        let color = barCenter <= playheadX ? playedColor : unplayedColor;
        if (dx < 24) color = playedTail;
        g.fillStyle = color;
        g.fillRect(x, mid - ph / 2, barW, ph);
      }
    }

    // Playhead — thin rule with a small cap glow
    g.fillStyle = getCss("--accent");
    g.fillRect(Math.max(0, playheadX - 0.5), 0, 1.2, ch);
    g.fillStyle = getCss("--accent-soft");
    g.fillRect(Math.max(0, playheadX - 6), 0, 12, ch);
  }

  function getCss(v: string): string {
    if (typeof window === "undefined") return "#000";
    return getComputedStyle(document.documentElement).getPropertyValue(v).trim();
  }

  // Translucent band fills + a brighter top/bottom edge per highlight kind.
  // The waveform bars are warm bone so the previous translucent bone bookmark
  // color was invisible against them — each kind now gets a saturated hue
  // distinct from the bars.
  function kindBand(kind: string): { fill: string; edge: string } {
    switch (kind) {
      case "decision":
        // Teal — matches the app accent.
        return { fill: "rgba(58, 155, 146, 0.38)", edge: "rgba(86, 184, 174, 0.95)" };
      case "follow_up":
        // Warm gold.
        return { fill: "rgba(224, 176, 80, 0.32)", edge: "rgba(240, 200, 110, 0.95)" };
      case "question":
        // Slate.
        return { fill: "rgba(138, 162, 192, 0.36)", edge: "rgba(170, 195, 225, 0.95)" };
      case "action":
        // Olive.
        return { fill: "rgba(143, 175, 114, 0.36)", edge: "rgba(175, 210, 140, 0.95)" };
      case "bookmark":
      default:
        // Coral — high-contrast against the bone bars, saves bone for text.
        return { fill: "rgba(245, 138, 103, 0.34)", edge: "rgba(255, 170, 135, 0.95)" };
    }
  }

  // Playhead RAF — only re-paints when the audio is actually playing (saves
  // idle CPU). Re-paint also runs after decode and on resize.
  function loop() {
    rafId = requestAnimationFrame(loop);
    if (audio && !audio.paused) {
      // currentMs is bound from outside; reading audio.currentTime directly
      // keeps the playhead smooth even when the outside event is throttled.
      currentMs = Math.floor(audio.currentTime * 1000);
      paint();
    }
  }

  function xToMs(clientX: number): number {
    if (!wrap || durationMs <= 0) return 0;
    const rect = wrap.getBoundingClientRect();
    const x = Math.max(0, Math.min(rect.width, clientX - rect.left));
    return Math.round((x / rect.width) * durationMs);
  }

  function onPointerDown(e: PointerEvent) {
    if (!wrap) return;
    (e.target as HTMLElement).setPointerCapture(e.pointerId);

    // Shift-drag marks a region; plain click-drag scrubs.
    if (e.shiftKey && onmark) {
      const start = xToMs(e.clientX);
      dragRange = { start_ms: start, end_ms: start };
      isMarking = true;
      paint();

      const move = (ev: PointerEvent) => {
        if (!dragRange) return;
        dragRange = { start_ms: dragRange.start_ms, end_ms: xToMs(ev.clientX) };
        paint();
      };
      const up = (ev: PointerEvent) => {
        (ev.target as HTMLElement).releasePointerCapture?.(ev.pointerId);
        wrap?.removeEventListener("pointermove", move);
        wrap?.removeEventListener("pointerup", up);
        wrap?.removeEventListener("pointercancel", up);
        if (dragRange) {
          const lo = Math.min(dragRange.start_ms, dragRange.end_ms);
          const hi = Math.max(dragRange.start_ms, dragRange.end_ms);
          // Require at least a tenth of a second so a stray click isn't treated
          // as a mark.
          if (hi - lo >= 100) onmark({ start_ms: lo, end_ms: hi });
        }
        dragRange = null;
        isMarking = false;
        paint();
      };
      wrap.addEventListener("pointermove", move);
      wrap.addEventListener("pointerup", up);
      wrap.addEventListener("pointercancel", up);
      return;
    }

    const ms = xToMs(e.clientX);
    onseek?.(ms);

    const move = (ev: PointerEvent) => onseek?.(xToMs(ev.clientX));
    const up = (ev: PointerEvent) => {
      (ev.target as HTMLElement).releasePointerCapture?.(ev.pointerId);
      wrap?.removeEventListener("pointermove", move);
      wrap?.removeEventListener("pointerup", up);
      wrap?.removeEventListener("pointercancel", up);
    };
    wrap.addEventListener("pointermove", move);
    wrap.addEventListener("pointerup", up);
    wrap.addEventListener("pointercancel", up);
  }

  function onPointerMoveHover(e: PointerEvent) {
    hoverMs = xToMs(e.clientX);
  }

  function onPointerLeave() {
    hoverMs = null;
  }

  function fmt(ms: number) {
    if (!isFinite(ms) || ms < 0) return "0:00";
    const s = Math.floor(ms / 1000);
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}:${String(r).padStart(2, "0")}`;
  }

  // Repaint reactively for currentMs/durationMs/highlights changes coming
  // from outside. Svelte 5 effect tracks through property reads.
  $effect(() => {
    currentMs;
    durationMs;
    highlights;
    paint();
  });
</script>

<div
  class="wave"
  bind:this={wrap}
  style="--h: {height}px"
  onpointerdown={onPointerDown}
  onpointermove={onPointerMoveHover}
  onpointerleave={onPointerLeave}
  role="slider"
  aria-label="Audio timeline"
  aria-valuemin="0"
  aria-valuemax={durationMs}
  aria-valuenow={currentMs}
  tabindex="0"
>
  {#if status === "decoding"}
    <div class="overlay">
      <div class="skeleton"></div>
      <span>Analyzing audio…</span>
    </div>
  {:else if status === "error"}
    <div class="overlay error">Could not decode audio: {errorMsg}</div>
  {:else if status === "idle"}
    <div class="overlay dim">No audio</div>
  {/if}

  <canvas bind:this={canvas}></canvas>

  {#if hoverMs !== null && status === "ready"}
    <div
      class="hover-line"
      style="left: {durationMs > 0 ? (hoverMs / durationMs) * 100 : 0}%"
    >
      <span class="hover-time">{fmt(hoverMs)}</span>
    </div>
  {/if}

  <div class="time-row">
    <span>{fmt(currentMs)}</span>
    <span>{fmt(durationMs)}</span>
  </div>
</div>

<style>
  .wave {
    position: relative;
    width: 100%;
    height: var(--h);
    cursor: crosshair;
    user-select: none;
    touch-action: none;
  }

  canvas {
    display: block;
    width: 100%;
    height: 100%;
  }

  .time-row {
    position: absolute;
    left: 2px;
    right: 2px;
    bottom: -22px;
    display: flex;
    justify-content: space-between;
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--bone-3);
    letter-spacing: 0.04em;
  }

  .overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.7rem;
    font-size: 0.8rem;
    color: var(--bone-3);
    font-family: var(--font-mono);
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .overlay.dim {
    color: var(--bone-4);
  }

  .overlay.error {
    color: var(--live);
  }

  .skeleton {
    width: 140px;
    height: 3px;
    border-radius: 2px;
    background: linear-gradient(
      90deg,
      transparent 0%,
      var(--accent) 50%,
      transparent 100%
    );
    background-size: 200% 100%;
    animation: slide 1.4s linear infinite;
  }
  @keyframes slide {
    0% {
      background-position: 200% 0;
    }
    100% {
      background-position: -200% 0;
    }
  }

  .hover-line {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 1px;
    background: var(--bone-2);
    pointer-events: none;
    opacity: 0.6;
  }

  .hover-time {
    position: absolute;
    top: -18px;
    transform: translateX(-50%);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--bone-1);
    background: var(--ink-3);
    padding: 1px 5px;
    border-radius: 3px;
    letter-spacing: 0.04em;
  }
</style>
