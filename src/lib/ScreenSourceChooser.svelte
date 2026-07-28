<script lang="ts">
  // #302 follow-up — per-call screen-source chooser.
  //
  // At Call-mode record-start, if screen recording is on + consented +
  // available AND "ask me each call" is set, Rust emits `screen-source-request`
  // (audio is ALREADY recording — this never delays it). This global,
  // non-modal card offers the source kinds the platform advertises — a
  // screen, a window, or an area — and starts capture on the pick via
  // `start_screen_source`. Cancel / "Just audio" / Escape = audio-only, no
  // screen row.
  //
  // Non-modal by design: it must NOT trap focus — the live-call Stop floater
  // stays reachable. It sits clear of `RecordingFloatingControl` (bottom-
  // right pill) and force-closes when the call leaves the recording state.
  //
  // Vendor-opaque: nothing here names a capture tool or backend. We say
  // "screen" / "window" / "area" only.

  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onMount, onDestroy } from "svelte";
  import { callRecording } from "$lib/stores/callRecording.svelte";

  type DisplayInfo = {
    name: string;
    width: number;
    height: number;
    is_primary: boolean;
    x: number;
    y: number;
  };
  type WindowInfo = { title: string };
  type Mode =
    | "hidden"
    | "choosing"
    | "screen-list"
    | "window-list"
    | "resolving"
    | "error";

  let mode = $state<Mode>("hidden");
  let sessionDir = $state("");
  let sources = $state<string[]>([]);
  let displays = $state<DisplayInfo[]>([]);
  let windowsList = $state<WindowInfo[]>([]);
  let resolvingLabel = $state("");
  let errorLine = $state("");
  let platform = $state("");
  let busy = $state(false);

  let unlistenRequest: UnlistenFn | null = null;
  let unlistenPicked: UnlistenFn | null = null;
  let unlistenCancelled: UnlistenFn | null = null;

  const hasKind = (k: string) => sources.includes(k);

  onMount(async () => {
    try {
      platform = await invoke<string>("platform_os");
    } catch {
      platform = "";
    }
    unlistenRequest = await listen<{ session_dir: string; sources: string[] }>(
      "screen-source-request",
      (e) => {
        sessionDir = e.payload.session_dir;
        sources = e.payload.sources ?? [];
        displays = [];
        windowsList = [];
        errorLine = "";
        busy = false;
        // Nothing to offer → stay hidden (audio-only).
        mode = sources.length > 0 ? "choosing" : "hidden";
      },
    );
  });

  onDestroy(() => {
    unlistenRequest?.();
    cleanupRegionListeners();
  });

  // Force-close if the call leaves the recording state while the chooser is
  // still open (stop / restart mid-choice) — audio-only, no orphaned card.
  $effect(() => {
    if (callRecording.state !== "recording" && mode !== "hidden") {
      // If a region-select overlay is mid-drag, force it closed too, else a
      // stuck fullscreen always-on-top window survives the call ending.
      // Mirrors +layout.svelte's scheduleOverlayClose() → close_overlay.
      void invoke("close_region_select").catch(() => {});
      close();
    }
  });

  function cleanupRegionListeners() {
    unlistenPicked?.();
    unlistenPicked = null;
    unlistenCancelled?.();
    unlistenCancelled = null;
  }

  function close() {
    mode = "hidden";
    sources = [];
    displays = [];
    windowsList = [];
    errorLine = "";
    resolvingLabel = "";
    busy = false;
    cleanupRegionListeners();
  }

  // Esc anywhere while the chooser is up = "Just audio".
  function onKeydown(e: KeyboardEvent) {
    if (mode !== "hidden" && mode !== "resolving" && e.key === "Escape") {
      justAudio();
    }
  }

  function justAudio() {
    close();
  }

  async function startSource(kind: string, target: string | null) {
    if (busy) return;
    busy = true;
    try {
      const res = await invoke<string>("start_screen_source", {
        sessionDir,
        kind,
        target,
      });
      if (res === "started") {
        close();
      } else if (res === "unavailable") {
        errorLine = "Screen recording isn't available right now.";
        mode = "error";
      } else {
        // cancelled (call already stopped / stale) → audio-only.
        close();
      }
    } catch {
      errorLine = "Couldn't start screen recording.";
      mode = "error";
    } finally {
      busy = false;
    }
  }

  // ── Screen ──────────────────────────────────────────────────────────
  async function chooseScreen() {
    try {
      displays = await invoke<DisplayInfo[]>("list_displays");
    } catch {
      displays = [];
    }
    if (displays.length > 1) {
      mode = "screen-list";
    } else {
      await startSource("screen", displays[0]?.name ?? null);
    }
  }

  // A friendly label for a monitor (Windows device names like \\.\DISPLAY1
  // read poorly; fall back to positional numbering).
  function screenLabel(d: DisplayInfo, i: number): string {
    const dims = d.width ? ` · ${d.width}×${d.height}` : "";
    const name = d.name && !d.name.startsWith("\\\\") ? d.name : `Screen ${i + 1}`;
    return `${name}${dims}${d.is_primary ? " · main" : ""}`;
  }

  // ── Window ──────────────────────────────────────────────────────────
  async function chooseWindow() {
    if (platform === "windows") {
      try {
        windowsList = await invoke<WindowInfo[]>("list_windows");
      } catch {
        windowsList = [];
      }
      mode = "window-list";
    } else {
      // Linux hands off to the compositor's native window picker.
      resolvingLabel = "Choose a window to record…";
      mode = "resolving";
      await startSource("window", null);
    }
  }

  // ── Region (area) ───────────────────────────────────────────────────
  async function chooseRegion() {
    if (platform === "windows") {
      await openRegionOverlay();
    } else {
      // Linux drives the native drag-select tool.
      resolvingLabel = "Select an area on your screen…";
      mode = "resolving";
      let geo: string | null = null;
      try {
        geo = await invoke<string | null>("pick_region");
      } catch {
        geo = null;
      }
      if (geo) {
        await startSource("region", geo);
      } else {
        // Cancelled the drag-select → back to the choices.
        mode = "choosing";
      }
    }
  }

  async function openRegionOverlay() {
    // Size/position the transparent overlay to the primary monitor rect so
    // client coords map cleanly to screen coords (page adds the origin).
    let d: DisplayInfo | undefined;
    try {
      const list = await invoke<DisplayInfo[]>("list_displays");
      d = list.find((x) => x.is_primary) ?? list[0];
    } catch {
      d = undefined;
    }
    const x = d?.x ?? 0;
    const y = d?.y ?? 0;
    const width = d?.width ?? 1920;
    const height = d?.height ?? 1080;

    resolvingLabel = "Select an area on your screen…";
    mode = "resolving";

    cleanupRegionListeners();
    unlistenPicked = await listen<string>("region-picked", async (e) => {
      cleanupRegionListeners();
      await startSource("region", e.payload);
    });
    unlistenCancelled = await listen("region-cancelled", () => {
      cleanupRegionListeners();
      mode = "choosing";
    });

    // Rust creates the overlay window (mirrors the co-pilot overlay's
    // open_overlay command), so no webview-create capability is needed. The
    // window submits its result via `submit_region_selection`, which closes
    // it and re-emits `region-picked` / `region-cancelled`.
    try {
      await invoke("open_region_select", { x, y, width, height });
    } catch {
      cleanupRegionListeners();
      errorLine = "Couldn't open the area selector.";
      mode = "error";
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if mode !== "hidden"}
  <div class="src-chooser" role="dialog" aria-label="Choose what to record">
    {#if mode === "choosing"}
      <p class="src-title">Record a screen, a window, or an area?</p>
      <p class="src-sub">Audio is already recording. Pick what to add.</p>
      <div class="src-actions">
        {#if hasKind("screen")}
          <button type="button" class="src-btn" onclick={chooseScreen} disabled={busy}>
            <svg class="src-glyph" viewBox="0 0 20 20" width="16" height="16" aria-hidden="true">
              <rect x="2.2" y="3.5" width="15.6" height="10" rx="1.4" fill="none" stroke="currentColor" stroke-width="1.4" />
              <path d="M7.5 16.5 h5 M10 13.5 v3" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none" />
            </svg>
            Record a screen
          </button>
        {/if}
        {#if hasKind("window")}
          <button type="button" class="src-btn" onclick={chooseWindow} disabled={busy}>
            <svg class="src-glyph" viewBox="0 0 20 20" width="16" height="16" aria-hidden="true">
              <rect x="2.5" y="4" width="15" height="12" rx="1.4" fill="none" stroke="currentColor" stroke-width="1.4" />
              <path d="M2.5 7 h15" stroke="currentColor" stroke-width="1.4" fill="none" />
            </svg>
            Record a window
          </button>
        {/if}
        {#if hasKind("region")}
          <button type="button" class="src-btn" onclick={chooseRegion} disabled={busy}>
            <svg class="src-glyph" viewBox="0 0 20 20" width="16" height="16" aria-hidden="true">
              <path d="M3 6 V3 h3 M14 3 h3 v3 M17 14 v3 h-3 M6 17 H3 v-3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
            </svg>
            Record an area
          </button>
        {/if}
      </div>
      <button type="button" class="src-dismiss" onclick={justAudio}>Just audio</button>
    {:else if mode === "screen-list"}
      <p class="src-title">Which screen?</p>
      <div class="src-list">
        {#each displays as d, i (d.name)}
          <button type="button" class="src-row" onclick={() => startSource("screen", d.name)} disabled={busy}>
            {screenLabel(d, i)}
          </button>
        {/each}
      </div>
      <button type="button" class="src-dismiss" onclick={() => (mode = "choosing")}>Back</button>
    {:else if mode === "window-list"}
      <p class="src-title">Which window?</p>
      {#if windowsList.length === 0}
        <p class="src-sub">No open windows found.</p>
      {:else}
        <div class="src-list src-list-scroll">
          {#each windowsList as w, i (w.title + i)}
            <button type="button" class="src-row" onclick={() => startSource("window", w.title)} disabled={busy} title={w.title}>
              {w.title}
            </button>
          {/each}
        </div>
      {/if}
      <button type="button" class="src-dismiss" onclick={() => (mode = "choosing")}>Back</button>
    {:else if mode === "resolving"}
      <p class="src-title">{resolvingLabel}</p>
      <p class="src-sub">Follow the prompt on your screen.</p>
    {:else if mode === "error"}
      <p class="src-title src-title-warn">{errorLine}</p>
      <button type="button" class="src-dismiss" onclick={justAudio}>Continue with audio</button>
    {/if}
  </div>
{/if}

<style>
  /* Non-modal card, bottom-right, ABOVE the Stop floater so the live-call
   * Stop pill stays reachable (the floater sits at right:1rem bottom:1rem).
   * No backdrop → never traps focus or blocks the floater. */
  .src-chooser {
    position: fixed;
    right: 1rem;
    bottom: 5.25rem;
    z-index: 82;
    width: 280px;
    max-width: calc(100vw - 2rem);
    padding: 0.9rem 1rem 0.8rem;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-lg);
    box-shadow: 0 14px 34px -12px rgba(0, 0, 0, 0.6);
    color: var(--bone-1);
    font-family: var(--font-sans);
    animation: src-in 0.16s ease-out;
  }
  @keyframes src-in {
    from {
      opacity: 0;
      transform: translateY(8px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }

  .src-title {
    margin: 0 0 0.15rem;
    font-size: 0.9rem;
    font-weight: 600;
    color: var(--bone-0);
    letter-spacing: -0.01em;
  }
  .src-title-warn {
    color: var(--bone-0);
  }
  .src-sub {
    margin: 0 0 0.7rem;
    font-size: 0.78rem;
    color: var(--bone-2);
  }

  .src-actions {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    margin-bottom: 0.6rem;
  }
  .src-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.55rem;
    width: 100%;
    padding: 0.55rem 0.7rem;
    border: 1px solid var(--hairline-hi);
    background: var(--ink-2);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.85rem;
    font-weight: 500;
    border-radius: var(--radius);
    cursor: pointer;
    text-align: left;
    transition:
      border-color 0.15s,
      background 0.15s,
      color 0.15s;
  }
  .src-btn:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent-hi);
  }
  .src-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .src-glyph {
    color: var(--accent);
    flex-shrink: 0;
  }

  .src-list {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    margin-bottom: 0.6rem;
  }
  .src-list-scroll {
    max-height: 220px;
    overflow-y: auto;
  }
  .src-row {
    width: 100%;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    font: inherit;
    font-size: 0.82rem;
    border-radius: var(--radius-sm);
    cursor: pointer;
    text-align: left;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    transition:
      border-color 0.15s,
      color 0.15s;
  }
  .src-row:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--bone-0);
  }
  .src-row:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .src-dismiss {
    appearance: none;
    background: transparent;
    border: none;
    padding: 0.2rem 0;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
    transition: color 0.15s;
  }
  .src-dismiss:hover {
    color: var(--bone-0);
  }

  @media (prefers-reduced-motion: reduce) {
    .src-chooser {
      animation: none;
    }
  }
</style>
