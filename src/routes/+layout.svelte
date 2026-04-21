<script lang="ts">
  import { page } from "$app/state";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import "../app.css";

  let { children } = $props();

  let recording = $state(false);
  let pipelineStage = $state("");
  let unlistenState: UnlistenFn | null = null;
  let unlistenPipeline: UnlistenFn | null = null;

  onMount(async () => {
    unlistenState = await listen<{ recording: boolean }>(
      "recording-state",
      (evt) => (recording = evt.payload.recording),
    );
    unlistenPipeline = await listen<{ stage: string }>("pipeline", (evt) => {
      pipelineStage = evt.payload.stage ?? "";
      if (pipelineStage === "done" || pipelineStage === "failed") {
        setTimeout(() => {
          if (pipelineStage === "done" || pipelineStage === "failed")
            pipelineStage = "";
        }, 4000);
      }
    });
  });

  onDestroy(() => {
    unlistenState?.();
    unlistenPipeline?.();
  });

  const items: {
    href: string;
    label: string;
    match: (p: string) => boolean;
    icon: string;
  }[] = [
    {
      href: "/",
      label: "Record",
      match: (p) => p === "/",
      // simple inline SVGs — microphone, list, gear
      icon: `<svg viewBox="0 0 20 20" width="16" height="16"><path d="M10 2a3 3 0 0 0-3 3v5a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" fill="currentColor"/><path d="M5 9v1a5 5 0 0 0 10 0V9M10 15v3M7 18h6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/></svg>`,
    },
    {
      href: "/calls",
      label: "Calls",
      match: (p) => p.startsWith("/calls"),
      icon: `<svg viewBox="0 0 20 20" width="16" height="16"><path d="M3 5h14M3 10h14M3 15h14" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>`,
    },
    {
      href: "/settings",
      label: "Settings",
      match: (p) => p.startsWith("/settings"),
      icon: `<svg viewBox="0 0 20 20" width="16" height="16"><circle cx="10" cy="10" r="2.4" stroke="currentColor" stroke-width="1.4" fill="none"/><path d="M10 2v2.2M10 15.8V18M2 10h2.2M15.8 10H18M4.3 4.3l1.6 1.6M14.1 14.1l1.6 1.6M4.3 15.7l1.6-1.6M14.1 5.9l1.6-1.6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>`,
    },
  ];

  const stageLabel: Record<string, string> = {
    started: "Processing",
    transcribing: "Transcribing",
    summarizing: "Summarizing",
    writing_note: "Writing note",
    uploading: "Syncing",
    done: "Saved",
    failed: "Failed",
  };

  // Figure out the page title for the top strip — functional, not narrative.
  let pageTitle = $derived.by(() => {
    const p = page.url.pathname;
    if (p === "/") return "Record";
    if (p === "/calls") return "Calls";
    if (p.startsWith("/calls/")) return "Call";
    if (p.startsWith("/settings")) return "Settings";
    return "callscribe";
  });
</script>

<div class="shell">
  <aside class="rail">
    <a href="/" class="brand">
      <span class="onair" class:live={recording}></span>
      <span class="wordmark">callscribe</span>
    </a>

    <nav>
      {#each items as it (it.href)}
        {@const active = it.match(page.url.pathname)}
        <a
          href={it.href}
          class="nav-item"
          class:active
          aria-current={active ? "page" : undefined}
        >
          <span class="glyph">{@html it.icon}</span>
          <span class="label">{it.label}</span>
        </a>
      {/each}
    </nav>

  </aside>

  <div class="main">
    <header class="topstrip">
      <div class="crumbs">
        <span class="crumb">{pageTitle}</span>
      </div>

      <div class="indicator">
        {#if recording}
          <span class="pip live"></span>
          <span class="ind-label">Recording</span>
        {:else if pipelineStage && pipelineStage !== "done"}
          <span class="pip {pipelineStage}"></span>
          <span class="ind-label">{stageLabel[pipelineStage] ?? pipelineStage}</span>
        {:else if pipelineStage === "done"}
          <span class="pip done"></span>
          <span class="ind-label">Saved</span>
        {:else}
          <span class="pip idle"></span>
          <span class="ind-label">Idle</span>
        {/if}
      </div>
    </header>

    <div class="page">
      {@render children()}
    </div>
  </div>
</div>

<style>
  .shell {
    position: relative;
    z-index: 1;
    display: grid;
    grid-template-columns: var(--rail-w) 1fr;
    min-height: 100vh;
  }

  /* ── Left rail ─────────────────────────────────────────────────────── */
  .rail {
    position: sticky;
    top: 0;
    height: 100vh;
    display: flex;
    flex-direction: column;
    padding: 1.1rem 0.9rem 0.9rem;
    border-right: 1px solid var(--hairline);
    background: var(--ink-0);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.25rem 0.45rem 1.1rem;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    padding: 0.5rem 0.65rem;
    border-radius: var(--radius);
    color: var(--bone-2);
    font-size: 0.88rem;
    font-weight: 500;
    letter-spacing: -0.005em;
    transition:
      color 0.15s,
      background-color 0.15s;
  }

  .nav-item:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }

  .nav-item.active {
    color: var(--bone-0);
    background: var(--ink-2);
    box-shadow: inset 0 0 0 1px var(--hairline);
  }

  .nav-item .glyph {
    display: flex;
    align-items: center;
    color: var(--bone-3);
    transition: color 0.15s;
  }

  .nav-item:hover .glyph,
  .nav-item.active .glyph {
    color: var(--accent);
  }

  /* ── Main area ─────────────────────────────────────────────────────── */
  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .topstrip {
    position: sticky;
    top: 0;
    z-index: 5;
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: var(--topbar-h);
    padding: 0 1.5rem;
    border-bottom: 1px solid var(--hairline);
    background: rgba(14, 13, 12, 0.85);
    backdrop-filter: saturate(140%) blur(10px);
    -webkit-backdrop-filter: saturate(140%) blur(10px);
  }

  .crumbs {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .crumb {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--bone-1);
    letter-spacing: -0.005em;
  }

  .indicator {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    letter-spacing: 0.04em;
    color: var(--bone-2);
  }

  .pip {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--bone-4);
  }

  .pip.live {
    background: var(--live);
    box-shadow: 0 0 8px var(--live);
    animation: pip-pulse 1.2s ease-in-out infinite;
  }
  .pip.started,
  .pip.transcribing,
  .pip.summarizing,
  .pip.writing_note,
  .pip.uploading {
    background: var(--sig);
    box-shadow: 0 0 5px rgba(201, 162, 74, 0.6);
    animation: pip-blink 1s ease-in-out infinite;
  }
  .pip.done {
    background: var(--olive);
    box-shadow: 0 0 5px rgba(143, 175, 114, 0.5);
  }

  @keyframes pip-pulse {
    0%,
    100% {
      transform: scale(1);
      opacity: 1;
    }
    50% {
      transform: scale(1.3);
      opacity: 0.85;
    }
  }
  @keyframes pip-blink {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.45;
    }
  }

  .page {
    flex: 1;
    min-height: 0;
  }
</style>
