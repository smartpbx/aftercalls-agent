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
    // The panel's own name ("DELL U2723QE") when the desktop reports one.
    description: string | null;
  };
  type WindowInfo = { title: string };
  type RegionPicked = { request_token: string; geometry: string };
  type RegionCancelled = { request_token: string };
  type CaptureStatus = {
    capturing: boolean;
    source_kind: string | null;
    producing: boolean;
  };
  type Mode =
    | "hidden"
    | "choosing"
    | "screen-list"
    | "window-list"
    | "resolving"
    | "waiting-window"
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
  // Every request/close advances this token. Async list/picker/listener work
  // captures it and must still match after each await before touching UI or
  // invoking capture, preventing a stopped/restarted call from resurrecting
  // an old chooser or region overlay.
  let operationGeneration = 0;
  let requestWasRecording = false;
  let requestArmingTimer: ReturnType<typeof setTimeout> | null = null;

  let unlistenRequest: UnlistenFn | null = null;
  let unlistenPicked: UnlistenFn | null = null;
  let unlistenCancelled: UnlistenFn | null = null;
  let componentDestroyed = false;

  const hasKind = (k: string) => sources.includes(k);

  onMount(async () => {
    try {
      const unlisten = await listen<{
        session_dir: string;
        sources: string[];
      }>("screen-source-request", (e) => {
        operationGeneration += 1;
        clearRequestArmingTimer();
        cleanupRegionListeners();
        // A replacement request owns a new session. Ensure an older area
        // overlay cannot remain above it.
        void invoke("close_region_select").catch(() => {});
        sessionDir = e.payload.session_dir;
        sources = e.payload.sources ?? [];
        displays = [];
        windowsList = [];
        errorLine = "";
        busy = false;
        requestWasRecording = isActiveSession(sessionDir);
        // Nothing to offer → stay hidden (audio-only).
        mode = sources.length > 0 ? "choosing" : "hidden";
        // Backward-compatible safety net if events ever arrive out of order:
        // give recording-state one turn to correlate, then discard a request
        // that never became the active session.
        if (mode !== "hidden" && !requestWasRecording) {
          const generation = operationGeneration;
          requestArmingTimer = setTimeout(() => {
            requestArmingTimer = null;
            if (
              generation === operationGeneration &&
              mode !== "hidden" &&
              !isActiveSession(sessionDir)
            ) {
              close();
            }
          }, 500);
        }
      });
      if (componentDestroyed) {
        unlisten();
        return;
      }
      unlistenRequest = unlisten;
    } catch {
      return;
    }
    try {
      const detected = await invoke<string>("platform_os");
      if (!componentDestroyed) platform = detected;
    } catch {
      if (!componentDestroyed) platform = "";
    }
  });

  onDestroy(() => {
    componentDestroyed = true;
    unlistenRequest?.();
    clearRequestArmingTimer();
    operationGeneration += 1;
    cleanupRegionListeners();
  });

  // Force-close if the call leaves the recording state while the chooser is
  // still open (stop / restart mid-choice) — audio-only, no orphaned card.
  $effect(() => {
    const state = callRecording.state;
    const activeDir = callRecording.sessionDir;
    if (mode === "hidden") return;
    if (state === "recording" && activeDir === sessionDir) {
      requestWasRecording = true;
      clearRequestArmingTimer();
      return;
    }
    // A different active session is always stale. Once this request has been
    // observed recording, any non-recording state is a real stop. During the
    // narrow legacy out-of-order start window (`idle`, never armed), the
    // bounded arming timer above decides instead of instantly dismissing.
    if (
      (state === "recording" && activeDir !== sessionDir) ||
      requestWasRecording ||
      state !== "idle"
    ) {
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

  function clearRequestArmingTimer() {
    if (requestArmingTimer) {
      clearTimeout(requestArmingTimer);
      requestArmingTimer = null;
    }
  }

  function isActiveSession(expectedSession: string): boolean {
    return (
      callRecording.state === "recording" &&
      callRecording.sessionDir === expectedSession
    );
  }

  function isCurrent(
    generation: number,
    expectedSession: string,
  ): boolean {
    return (
      generation === operationGeneration &&
      mode !== "hidden" &&
      sessionDir === expectedSession &&
      isActiveSession(expectedSession)
    );
  }

  async function resolvePlatform(
    generation: number,
    expectedSession: string,
  ): Promise<string | null> {
    if (platform) return platform;
    try {
      const detected = await invoke<string>("platform_os");
      if (!isCurrent(generation, expectedSession)) return null;
      platform = detected;
      return detected;
    } catch {
      if (isCurrent(generation, expectedSession)) {
        errorLine = "Couldn't determine which screen picker is available.";
        mode = "error";
      }
      return null;
    }
  }

  function close() {
    operationGeneration += 1;
    clearRequestArmingTimer();
    mode = "hidden";
    sessionDir = "";
    sources = [];
    displays = [];
    windowsList = [];
    errorLine = "";
    resolvingLabel = "";
    busy = false;
    requestWasRecording = false;
    cleanupRegionListeners();
  }

  // Esc anywhere while the chooser is up = "Just audio".
  function onKeydown(e: KeyboardEvent) {
    if (mode !== "hidden" && mode !== "resolving" && e.key === "Escape") {
      justAudio();
    }
  }

  function justAudio() {
    void invoke("close_region_select").catch(() => {});
    // A pending window handoff has a live capture behind it that has to be
    // told to stop; every other state has nothing to unwind.
    if (mode === "waiting-window") {
      void abandonWindowWait();
      return;
    }
    close();
  }

  // Ask Rust to start capture. Returns "started" only when the capture
  // producer survived its startup check — every other outcome has already
  // been turned into a closed card or an error line here.
  //
  // `hold` keeps the card open on success, for the one source whose work
  // isn't finished when capture starts: a window handoff is still waiting
  // on the desktop's picker at that point.
  async function startSource(
    kind: string,
    target: string | null,
    generation = operationGeneration,
    expectedSession = sessionDir,
    hold = false,
  ): Promise<string> {
    if (busy || !isCurrent(generation, expectedSession)) return "stale";
    busy = true;
    try {
      const res = await invoke<string>("start_screen_source", {
        sessionDir: expectedSession,
        kind,
        target,
      });
      if (!isCurrent(generation, expectedSession)) return "stale";
      if (res === "started") {
        if (!hold) close();
      } else if (res === "unavailable") {
        errorLine = "Screen recording couldn't start, so this call is audio only.";
        mode = "error";
      } else {
        // cancelled (call already stopped / stale) → audio-only.
        close();
      }
      return res;
    } catch {
      if (!isCurrent(generation, expectedSession)) return "stale";
      errorLine = "Screen recording couldn't start, so this call is audio only.";
      mode = "error";
      return "error";
    } finally {
      if (isCurrent(generation, expectedSession)) busy = false;
    }
  }

  // Abandon a capture that started but never produced video. The audio call
  // keeps running; without this a handoff that hangs would leave a capture
  // process alive and invisible until the user hits Stop.
  async function cancelPendingCapture(expectedSession: string) {
    try {
      await invoke("cancel_screen_source", { sessionDir: expectedSession });
    } catch {
      // Best-effort — Stop finalizes whatever is left either way.
    }
  }

  function delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  // ── Screen ──────────────────────────────────────────────────────────
  async function chooseScreen() {
    // The latest chooser interaction wins if two pointer events arrive before
    // Svelte has painted the next mode.
    const generation = ++operationGeneration;
    const expectedSession = sessionDir;
    if (!isCurrent(generation, expectedSession)) return;
    let next: DisplayInfo[] = [];
    try {
      next = await invoke<DisplayInfo[]>("list_displays");
    } catch {
      next = [];
    }
    if (!isCurrent(generation, expectedSession)) return;
    displays = next;
    if (displays.length > 1) {
      mode = "screen-list";
    } else {
      await startSource(
        "screen",
        displays[0]?.name ?? null,
        generation,
        expectedSession,
      );
    }
  }

  // Naming a screen. A connector name — "DP-1", "HDMI-A-1", "\\.\DISPLAY2" —
  // names a socket on the graphics card, so a list of them asks the user to
  // guess which is which. Lead with whatever actually identifies the panel:
  // its model when the desktop reports one, otherwise its place on the desk.
  function screenTitle(d: DisplayInfo, i: number): string {
    if (d.description) return d.description;
    if (d.name && !d.name.startsWith("\\\\")) return d.name;
    return `Screen ${i + 1}`;
  }

  // Where each screen sits. The list arrives already ordered left to right,
  // so the hint just names each row's place in it — which is what actually
  // separates three identical panels on one desk.
  //
  // Real desks are not tidy grids (a portrait panel nudged down to centre it,
  // an ultrawide on a lower shelf), so this deliberately does not require a
  // clean row: distinct horizontal positions are enough. It falls back to a
  // vertical reading for a stacked pair, and says nothing at all when the
  // desktop reported no coordinates — a wrong "left" is worse than no hint.
  let positionHints = $derived.by<(string | null)[]>(() => {
    const count = displays.length;
    const none = displays.map(() => null);
    if (count < 2) return none;

    if (new Set(displays.map((d) => d.x)).size === count) {
      if (count === 2) return ["left", "right"];
      return displays.map((_, i) => {
        if (i === 0) return "leftmost";
        if (i === count - 1) return "rightmost";
        return `${ordinal(i + 1)} from left`;
      });
    }
    if (new Set(displays.map((d) => d.y)).size === count) {
      if (count === 2) return ["top", "bottom"];
      return displays.map((_, i) => {
        if (i === 0) return "topmost";
        if (i === count - 1) return "bottom";
        return `${ordinal(i + 1)} from top`;
      });
    }
    return none;
  });

  function ordinal(n: number): string {
    const tens = n % 100;
    if (tens >= 11 && tens <= 13) return `${n}th`;
    switch (n % 10) {
      case 1:
        return `${n}st`;
      case 2:
        return `${n}nd`;
      case 3:
        return `${n}rd`;
      default:
        return `${n}th`;
    }
  }

  // The supporting line: the connector name (only when it isn't already the
  // title), the resolution, where it sits, and whether it's the main screen.
  function screenMeta(d: DisplayInfo, i: number): string {
    const parts: string[] = [];
    if (d.description && d.name && !d.name.startsWith("\\\\")) parts.push(d.name);
    if (d.width) parts.push(`${d.width}×${d.height}`);
    const hint = positionHints[i];
    if (hint) parts.push(hint);
    if (d.is_primary) parts.push("main");
    return parts.join(" · ");
  }

  // ── Window ──────────────────────────────────────────────────────────
  async function chooseWindow() {
    const generation = ++operationGeneration;
    const expectedSession = sessionDir;
    if (!isCurrent(generation, expectedSession)) return;
    const currentPlatform = await resolvePlatform(generation, expectedSession);
    if (!currentPlatform || !isCurrent(generation, expectedSession)) return;
    if (currentPlatform === "windows") {
      let next: WindowInfo[] = [];
      try {
        next = await invoke<WindowInfo[]>("list_windows");
      } catch {
        next = [];
      }
      if (!isCurrent(generation, expectedSession)) return;
      windowsList = next;
      mode = "window-list";
    } else {
      // Linux hands off to the desktop's own window picker. Capture starts
      // immediately but records nothing until that picker comes back with a
      // window, so the card stays up through the wait instead of closing on
      // a "started" that hasn't produced a frame.
      resolvingLabel = "Starting window recording…";
      mode = "resolving";
      const res = await startSource(
        "window",
        null,
        generation,
        expectedSession,
        true,
      );
      if (res !== "started" || !isCurrent(generation, expectedSession)) return;
      await awaitWindowPick(generation, expectedSession);
    }
  }

  // How long to wait for the desktop's window picker before calling it dead.
  // Generous — the user may be hunting through a long window list — but
  // finite, because the alternative is a call that silently records no video.
  const WINDOW_PICK_TIMEOUT_MS = 90_000;

  // Watch the capture until it is really recording. Three ways out: frames
  // start arriving (done), the producer exits without a stream (the picker
  // never opened, or the user dismissed it), or nobody picks anything.
  async function awaitWindowPick(generation: number, expectedSession: string) {
    mode = "waiting-window";
    const deadline = Date.now() + WINDOW_PICK_TIMEOUT_MS;
    while (isCurrent(generation, expectedSession)) {
      let status: CaptureStatus | null = null;
      try {
        status = await invoke<CaptureStatus>("screen_capture_local_status");
      } catch {
        status = null;
      }
      if (!isCurrent(generation, expectedSession)) return;
      if (status) {
        if (status.producing) {
          close();
          return;
        }
        if (!status.capturing) {
          errorLine =
            "No window was shared, so this call is recording audio only.";
          mode = "error";
          return;
        }
      }
      if (Date.now() >= deadline) {
        await cancelPendingCapture(expectedSession);
        if (!isCurrent(generation, expectedSession)) return;
        errorLine =
          "Your desktop never opened a window picker, so this call is recording audio only.";
        mode = "error";
        return;
      }
      await delay(600);
    }
  }

  // "Just audio" while the window handoff is still pending: stop the capture
  // that is waiting on a picker, and leave the call recording.
  async function abandonWindowWait() {
    const expectedSession = sessionDir;
    // Retire this operation first so the polling loop above stands down.
    operationGeneration += 1;
    await cancelPendingCapture(expectedSession);
    close();
  }

  // ── Region (area) ───────────────────────────────────────────────────
  async function chooseRegion() {
    const generation = ++operationGeneration;
    const expectedSession = sessionDir;
    if (!isCurrent(generation, expectedSession)) return;
    const currentPlatform = await resolvePlatform(generation, expectedSession);
    if (!currentPlatform || !isCurrent(generation, expectedSession)) return;
    if (currentPlatform === "windows") {
      await openRegionOverlay(generation, expectedSession);
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
      if (!isCurrent(generation, expectedSession)) return;
      if (geo) {
        await startSource("region", geo, generation, expectedSession);
      } else {
        // Cancelled the drag-select → back to the choices.
        mode = "choosing";
      }
    }
  }

  async function openRegionOverlay(
    generation: number,
    expectedSession: string,
  ) {
    // Size/position the transparent overlay to the primary monitor rect so
    // client coords map cleanly to screen coords (page adds the origin).
    let d: DisplayInfo | undefined;
    try {
      const list = await invoke<DisplayInfo[]>("list_displays");
      d = list.find((x) => x.is_primary) ?? list[0];
    } catch {
      d = undefined;
    }
    if (!isCurrent(generation, expectedSession)) return;
    if (!d || d.width === 0 || d.height === 0) {
      errorLine = "Couldn't find a screen for the area selector.";
      mode = "error";
      return;
    }
    const { x, y, width, height } = d;

    resolvingLabel = "Select an area on your screen…";
    mode = "resolving";
    let requestToken = "";
    try {
      requestToken = crypto.randomUUID();
    } catch {
      if (isCurrent(generation, expectedSession)) {
        errorLine = "Couldn't open the area selector.";
        mode = "error";
      }
      return;
    }

    cleanupRegionListeners();
    try {
      const picked = await listen<RegionPicked>("region-picked", async (e) => {
        if (
          e.payload.request_token !== requestToken ||
          !isCurrent(generation, expectedSession)
        )
          return;
        cleanupRegionListeners();
        await startSource(
          "region",
          e.payload.geometry,
          generation,
          expectedSession,
        );
      });
      if (!isCurrent(generation, expectedSession)) {
        picked();
        return;
      }
      unlistenPicked = picked;

      const cancelled = await listen<RegionCancelled>(
        "region-cancelled",
        (e) => {
          if (
            e.payload.request_token !== requestToken ||
            !isCurrent(generation, expectedSession)
          )
            return;
          cleanupRegionListeners();
          mode = "choosing";
        },
      );
      if (!isCurrent(generation, expectedSession)) {
        cancelled();
        cleanupRegionListeners();
        return;
      }
      unlistenCancelled = cancelled;

      // Rust creates the overlay window (mirrors the co-pilot overlay's
      // open_overlay command), so no webview-create capability is needed. The
      // window submits its result via `submit_region_selection`, which closes
      // it and re-emits `region-picked` / `region-cancelled`.
      await invoke("open_region_select", {
        sessionDir: expectedSession,
        requestToken,
        x,
        y,
        width,
        height,
      });
      if (!isCurrent(generation, expectedSession)) {
        void invoke("close_region_select").catch(() => {});
      }
    } catch {
      if (!isCurrent(generation, expectedSession)) return;
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
          <button type="button" class="src-row src-row-stacked" onclick={() => startSource("screen", d.name)} disabled={busy}>
            <span class="src-row-title">{screenTitle(d, i)}</span>
            {#if screenMeta(d, i)}
              <span class="src-row-meta">{screenMeta(d, i)}</span>
            {/if}
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
    {:else if mode === "waiting-window"}
      <!-- The desktop owns the window picker, so all this card can do is say
           what to look for and stay reachable while the user looks. It holds
           until frames actually arrive — closing on "capture started" is what
           made a picker that never opened look like a working recording. -->
      <p class="src-title">Waiting for you to pick a window</p>
      <p class="src-sub">
        Your desktop should be showing a picker. Nothing is recorded until you
        choose.
      </p>
      <button type="button" class="src-dismiss" onclick={justAudio}>Just audio</button>
    {:else if mode === "error"}
      <p class="src-title src-title-warn">{errorLine}</p>
      <div class="src-error-actions">
        <button type="button" class="src-dismiss" onclick={() => (mode = "choosing")}>Try another way</button>
        <button type="button" class="src-dismiss" onclick={justAudio}>Continue with audio</button>
      </div>
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
  /* A screen row carries two facts: which panel it is, and how to tell it
     apart from the others. The second is support text, not a peer. */
  .src-row-stacked {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    white-space: normal;
  }
  .src-row-title {
    color: var(--bone-0);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .src-row-meta {
    font-size: 0.74rem;
    color: var(--bone-2);
    font-family: var(--font-mono);
  }

  .src-error-actions {
    display: flex;
    gap: 0.75rem;
    flex-wrap: wrap;
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
