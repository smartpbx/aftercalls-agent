<script lang="ts">
  // #302 follow-up — Windows region drag-select overlay.
  //
  // Rendered in a SECOND, transparent, full-screen Tauri webview (label
  // "region-select"), created on demand from `ScreenSourceChooser` only on
  // Windows (Linux uses the native drag-select tool instead). The user drags
  // a rectangle; we compute the screen-space geometry (window is positioned
  // at the target monitor's origin, passed as ?x=&y=, so screen = client +
  // origin) and hand it back through the `submit_region_selection` app
  // command, which closes this window and re-emits the result to the main
  // window's chooser. Esc or a too-small drag cancels.
  //
  // This route renders BARE — +layout.svelte detects the "/region-select"
  // pathname and skips the app chrome + auth/ToS gates.
  //
  // Vendor-opaque: no tool / backend names anywhere.

  import { invoke } from "@tauri-apps/api/core";
  import { page } from "$app/state";
  import { onMount } from "svelte";

  let originX = 0;
  let originY = 0;
  let submitted = false;

  let dragging = $state(false);
  let sx = $state(0);
  let sy = $state(0);
  let cx = $state(0);
  let cy = $state(0);

  onMount(() => {
    const params = new URLSearchParams(page.url.search);
    originX = parseInt(params.get("x") ?? "0", 10) || 0;
    originY = parseInt(params.get("y") ?? "0", 10) || 0;
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") void cancel();
  }

  function onDown(e: PointerEvent) {
    dragging = true;
    sx = e.clientX;
    sy = e.clientY;
    cx = e.clientX;
    cy = e.clientY;
  }
  function onMove(e: PointerEvent) {
    if (!dragging) return;
    cx = e.clientX;
    cy = e.clientY;
  }
  async function onUp() {
    if (!dragging) return;
    dragging = false;
    const x = Math.min(sx, cx);
    const y = Math.min(sy, cy);
    const w = Math.abs(cx - sx);
    const h = Math.abs(cy - sy);
    // Too small to be a real selection → treat as a cancel click.
    if (w < 8 || h < 8) {
      await cancel();
      return;
    }
    const geometry = `${w}x${h}+${originX + x}+${originY + y}`;
    await submit(geometry);
  }

  async function submit(geometry: string | null) {
    if (submitted) return;
    submitted = true;
    try {
      await invoke("submit_region_selection", { geometry });
    } catch {
      // The window closes from the Rust side regardless; nothing to do.
    }
  }
  async function cancel() {
    await submit(null);
  }

  let rectStyle = $derived.by(() => {
    const x = Math.min(sx, cx);
    const y = Math.min(sy, cy);
    const w = Math.abs(cx - sx);
    const h = Math.abs(cy - sy);
    return `left:${x}px; top:${y}px; width:${w}px; height:${h}px;`;
  });
</script>

<div
  class="region-root"
  role="presentation"
  onpointerdown={onDown}
  onpointermove={onMove}
  onpointerup={onUp}
>
  <div class="region-hint">
    Drag to select an area to record · <kbd>Esc</kbd> to cancel
  </div>
  {#if dragging}
    <div class="region-rect" style={rectStyle}></div>
  {/if}
</div>

<style>
  /* Let the OS-level window transparency show — the app's solid page
   * background must not paint here. Scoped :global so it reaches html/body
   * without touching app.css. */
  :global(html),
  :global(body) {
    background: transparent !important;
    margin: 0;
    overflow: hidden;
  }

  .region-root {
    position: fixed;
    inset: 0;
    cursor: crosshair;
    /* A faint dim so the user sees the overlay is live, while the selection
     * rectangle reads as a "hole" via the accent border + soft fill. */
    background: rgba(14, 13, 12, 0.28);
    user-select: none;
    -webkit-user-select: none;
  }

  .region-hint {
    position: fixed;
    top: 14px;
    left: 50%;
    transform: translateX(-50%);
    padding: 0.4rem 0.8rem;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: 999px;
    color: var(--bone-1);
    font-family: var(--font-sans);
    font-size: 0.82rem;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.4);
    pointer-events: none;
  }
  .region-hint kbd {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    padding: 0.05rem 0.3rem;
    border: 1px solid var(--hairline-hi);
    border-radius: 4px;
    color: var(--bone-0);
  }

  .region-rect {
    position: fixed;
    border: 1.5px solid var(--accent);
    background: var(--accent-soft);
    pointer-events: none;
  }
</style>
