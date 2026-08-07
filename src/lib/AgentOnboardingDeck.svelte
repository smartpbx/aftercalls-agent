<script lang="ts">
  import { onMount, tick } from "svelte";
  import { getAppPrefs, type AppPrefs } from "$lib/stores/appPrefs.svelte";

  let {
    onFinish,
    onSkip,
    onOpenSettings,
  } = $props<{
    onFinish: () => void | Promise<void>;
    onSkip: () => void | Promise<void>;
    onOpenSettings: () => void | Promise<void>;
  }>();

  type Slide = {
    step: string;
    title: string;
    body: string;
    chips?: string[];
    // `permissions` has no slide yet — it is the reserved seam for the
    // first conditional slide. Keep it; it is not dead code.
    kind:
      | "record"
      | "tray"
      | "shortcuts"
      | "detect"
      | "settings"
      | "permissions";
  };

  let index = $state(0);
  let maxReached = $state(0);
  let prefs = $state<AppPrefs | null>(null);
  let primaryBtn = $state<HTMLButtonElement | null>(null);

  const shortcutBody = $derived(
    prefs?.record_toggle_shortcut || prefs?.self_note_shortcut
      ? "Use your recording and note shortcuts when you want to stay in the flow. Press ? to see every in-app shortcut."
      : "Shortcuts are optional. You can set recording and note shortcuts in Settings, and press ? to see every in-app shortcut.",
  );

  // Slides are authored without a step label; `slides` below stamps
  // "Step N of M" from the live list. A slide that only appears on some
  // platforms or plans can then be spliced in or out here without
  // leaving every other label wrong.
  const baseSlides = $derived<Omit<Slide, "step">[]>([
    {
      title: "Start with Record.",
      body: "Capture a call, import an existing recording, or make a note to yourself from the Record screen.",
      chips: ["Call recording", "Import audio", "Note to self"],
      kind: "record",
    },
    {
      title: "The tray keeps aftercalls nearby.",
      body: "Closing the window can keep the agent available in the tray, depending on your Settings. The tray menu includes Start recording, Note to self, Settings, and Quit.",
      chips: ["Start recording", "Note to self", "Settings", "Quit"],
      kind: "tray",
    },
    {
      title: "Shortcuts work from the keyboard.",
      body: shortcutBody,
      kind: "shortcuts",
    },
    {
      title: "Call detection asks first.",
      body: "Auto-detect can notice likely calls and ask before recording. Auto-record is separate, opt-in, and only starts for apps you allow.",
      chips: ["Ask before recording", "Auto-record is opt-in", "Allowed apps only"],
      kind: "detect",
    },
    {
      title: "You control the agent in Settings.",
      body: "Choose your microphone, startup and close behavior, sounds, recording limits, telemetry, shortcuts, call detection, auto-record, and recording policy.",
      kind: "settings",
    },
  ]);

  const slides = $derived<Slide[]>(
    baseSlides.map((s, i) => ({
      ...s,
      step: `Step ${i + 1} of ${baseSlides.length}`,
    })),
  );

  const current = $derived(slides[index]);
  const isLast = $derived(index === slides.length - 1);
  const recordShortcut = $derived(prefs?.record_toggle_shortcut || "Set in Settings");
  const noteShortcut = $derived(prefs?.self_note_shortcut || "Set in Settings");

  onMount(() => {
    getAppPrefs().then((p) => (prefs = p));
  });

  $effect(() => {
    maxReached = Math.max(maxReached, index);
    void tick().then(() => primaryBtn?.focus());
  });

  function next() {
    if (isLast) {
      void onFinish();
      return;
    }
    index = Math.min(slides.length - 1, index + 1);
  }

  function back() {
    index = Math.max(0, index - 1);
  }

  function goTo(i: number) {
    if (i <= maxReached) index = i;
  }

  function focusables(): HTMLElement[] {
    return Array.from(
      document.querySelectorAll<HTMLElement>(
        ".agent-onboarding button:not([disabled]), .agent-onboarding [href], .agent-onboarding [tabindex]:not([tabindex='-1'])",
      ),
    ).filter((el) => el.offsetParent !== null || el === document.activeElement);
  }

  function onKeydown(e: KeyboardEvent) {
    const target = e.target as HTMLElement | null;
    const inTextInput =
      target?.tagName === "INPUT" ||
      target?.tagName === "TEXTAREA" ||
      target?.isContentEditable;
    e.stopPropagation();
    if (e.key === "Escape") {
      e.preventDefault();
      void onSkip();
      return;
    }
    if (!inTextInput && e.key === "ArrowRight") {
      e.preventDefault();
      next();
      return;
    }
    if (!inTextInput && e.key === "ArrowLeft") {
      e.preventDefault();
      back();
      return;
    }
    if (e.key === "Tab") {
      const items = focusables();
      if (items.length === 0) return;
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey) {
        if (active === first || !active || !items.includes(active)) {
          e.preventDefault();
          last.focus();
        }
      } else if (active === last) {
        e.preventDefault();
        first.focus();
      }
    }
  }
</script>

<div class="agent-onboarding-backdrop" role="presentation"></div>
<div
  class="agent-onboarding"
  role="dialog"
  aria-modal="true"
  aria-labelledby="agent-onboarding-title"
  onkeydown={onKeydown}
  tabindex="-1"
>
  <header class="ao-head">
    <div>
      <span class="ao-label">Agent setup</span>
      <h2 id="agent-onboarding-title">{current.title}</h2>
    </div>
    <button
      type="button"
      class="ao-skip"
      aria-label="Skip agent setup"
      onclick={() => onSkip()}
    >Skip</button>
  </header>

  <div class="ao-body">
    <div class="ao-art" class:record={current.kind === "record"} class:tray={current.kind === "tray"} class:shortcuts={current.kind === "shortcuts"} class:detect={current.kind === "detect"} class:settings={current.kind === "settings"} aria-hidden="true">
      <span></span>
      <span></span>
      <span></span>
    </div>
    <section class="ao-copy" aria-live="polite">
      <span class="ao-step">{current.step}</span>
      <p>{current.body}</p>
      {#if current.kind === "shortcuts"}
        <div class="ao-shortcuts">
          <div>
            <span>Record shortcut</span>
            <kbd class:unset={!prefs?.record_toggle_shortcut}>{recordShortcut}</kbd>
          </div>
          <div>
            <span>Note shortcut</span>
            <kbd class:unset={!prefs?.self_note_shortcut}>{noteShortcut}</kbd>
          </div>
          <div>
            <span>Shortcut help</span>
            <kbd>?</kbd>
          </div>
        </div>
      {:else if current.chips}
        <div class="ao-chips">
          {#each current.chips as chip (chip)}
            <span>{chip}</span>
          {/each}
        </div>
      {/if}
    </section>
  </div>

  <footer class="ao-actions">
    <button
      type="button"
      class="ao-secondary"
      onclick={back}
      disabled={index === 0}
    >Back</button>
    <div class="ao-dots" aria-label="Onboarding steps">
      {#each slides as slide, i (slide.title)}
        <button
          type="button"
          class="ao-dot"
          class:active={i === index}
          aria-label={`Go to slide ${i + 1}`}
          aria-current={i === index ? "step" : undefined}
          title={i > maxReached ? "Finish the current step first" : slide.title}
          disabled={i > maxReached}
          onclick={() => goTo(i)}
        ><span></span></button>
      {/each}
    </div>
    <div class="ao-primary-row">
      {#if isLast}
        <button type="button" class="ao-secondary" onclick={() => onOpenSettings()}>
          Open Settings
        </button>
      {/if}
      <button
        type="button"
        class="ao-primary"
        bind:this={primaryBtn}
        onclick={next}
      >{isLast ? "Finish" : "Next"}</button>
    </div>
  </footer>
</div>
<span class="ao-live" aria-live="polite">Slide {index + 1} of {slides.length}: {current.title}</span>

<style>
  .agent-onboarding-backdrop {
    position: fixed;
    inset: 0;
    z-index: 76;
    background: rgba(0, 0, 0, 0.54);
    backdrop-filter: blur(2px);
    -webkit-backdrop-filter: blur(2px);
  }

  .agent-onboarding {
    position: fixed;
    z-index: 77;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(680px, calc(100vw - 2rem));
    max-height: min(86vh, 720px);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    color: var(--bone-0);
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-lg, 14px);
    box-shadow: 0 24px 60px -14px rgba(0, 0, 0, 0.65);
    animation: ao-pop 150ms ease-out;
  }

  .ao-head,
  .ao-actions {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 1rem 1.1rem;
  }

  .ao-head {
    border-bottom: 1px solid var(--hairline);
  }

  .ao-label,
  .ao-step {
    display: block;
    color: var(--bone-3);
    font-size: 0.7rem;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }

  .ao-head h2 {
    margin: 0.18rem 0 0;
    color: var(--bone-0);
    font-size: 1.05rem;
    letter-spacing: 0;
    text-transform: none;
  }

  .ao-skip,
  .ao-secondary,
  .ao-primary {
    appearance: none;
    border-radius: 8px;
    cursor: pointer;
    font: inherit;
    font-size: 0.85rem;
    font-weight: 600;
    padding: 0.48rem 0.8rem;
  }

  .ao-skip,
  .ao-secondary {
    background: transparent;
    border: 1px solid var(--hairline);
    color: var(--bone-2);
  }

  .ao-primary {
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
  }

  .ao-skip:hover,
  .ao-secondary:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }

  .ao-primary:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }

  .ao-secondary:disabled {
    opacity: 0.42;
    cursor: default;
  }

  .ao-body {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    display: grid;
    grid-template-columns: minmax(160px, 0.8fr) minmax(0, 1.2fr);
    gap: 1.1rem;
    padding: 1.1rem;
  }

  .ao-art {
    min-height: 220px;
    border-radius: var(--radius);
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    position: relative;
    overflow: hidden;
  }

  .ao-art span {
    position: absolute;
    border: 1px solid var(--hairline-hi);
    background: var(--ink-2);
  }

  .ao-art.record span:nth-child(1) {
    width: 42px;
    height: 80px;
    border-radius: 22px;
    left: 50%;
    top: 38px;
    transform: translateX(-50%);
    background: var(--accent-soft);
    border-color: var(--accent);
  }

  .ao-art.record span:nth-child(2) {
    width: 92px;
    height: 44px;
    border-radius: 999px 999px 12px 12px;
    left: 50%;
    top: 110px;
    transform: translateX(-50%);
  }

  .ao-art.record span:nth-child(3) {
    width: 120px;
    height: 8px;
    border-radius: 999px;
    left: 50%;
    bottom: 44px;
    transform: translateX(-50%);
    background: var(--live);
    border: none;
  }

  .ao-art.tray span:nth-child(1) {
    width: 128px;
    height: 36px;
    border-radius: 10px;
    right: 28px;
    bottom: 36px;
  }

  .ao-art.tray span:nth-child(2),
  .ao-art.tray span:nth-child(3) {
    width: 22px;
    height: 22px;
    border-radius: 50%;
    bottom: 43px;
  }

  .ao-art.tray span:nth-child(2) {
    right: 112px;
    background: var(--accent-soft);
    border-color: var(--accent);
  }

  .ao-art.tray span:nth-child(3) {
    right: 76px;
  }

  .ao-art.shortcuts {
    display: grid;
    grid-template-columns: repeat(3, 42px);
    place-content: center;
    gap: 0.55rem;
  }

  .ao-art.shortcuts span {
    position: static;
    height: 34px;
    border-radius: 6px;
    box-shadow: inset 0 -3px 0 var(--hairline);
  }

  .ao-art.detect span:nth-child(1) {
    inset: 38px 38px auto;
    height: 76px;
    border-radius: 14px;
  }

  .ao-art.detect span:nth-child(2) {
    width: 11px;
    height: 11px;
    border-radius: 50%;
    left: 54px;
    top: 54px;
    background: var(--sig);
    box-shadow: 0 0 0 7px color-mix(in srgb, var(--sig) 22%, transparent);
  }

  .ao-art.detect span:nth-child(3) {
    width: 86px;
    height: 9px;
    border-radius: 999px;
    right: 55px;
    top: 56px;
  }

  .ao-art.settings span {
    width: 142px;
    height: 14px;
    border-radius: 999px;
    left: 50%;
    transform: translateX(-50%);
  }

  .ao-art.settings span:nth-child(1) { top: 58px; }
  .ao-art.settings span:nth-child(2) { top: 102px; }
  .ao-art.settings span:nth-child(3) { top: 146px; background: var(--accent-soft); border-color: var(--accent); }

  .ao-copy {
    min-width: 0;
    align-self: center;
  }

  .ao-copy p {
    margin: 0.55rem 0 0;
    color: var(--bone-1);
    font-size: 0.96rem;
    line-height: 1.55;
  }

  .ao-chips,
  .ao-shortcuts {
    margin-top: 1rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.45rem;
  }

  .ao-chips span {
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-2);
    color: var(--bone-1);
    font-size: 0.78rem;
    padding: 0.22rem 0.55rem;
  }

  .ao-shortcuts {
    flex-direction: column;
  }

  .ao-shortcuts div {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.8rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    padding: 0.45rem 0.6rem;
  }

  .ao-shortcuts span {
    color: var(--bone-2);
    font-size: 0.82rem;
  }

  .ao-shortcuts kbd {
    max-width: 55%;
    overflow-wrap: anywhere;
    border: 1px solid var(--hairline-hi);
    border-radius: 5px;
    background: var(--ink-2);
    color: var(--bone-0);
    font-family: var(--font-mono);
    font-size: 0.74rem;
    padding: 0.12rem 0.42rem;
    text-align: right;
  }

  .ao-shortcuts kbd.unset {
    color: var(--bone-3);
  }

  .ao-actions {
    border-top: 1px solid var(--hairline);
  }

  .ao-dots,
  .ao-primary-row {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
  }

  .ao-dot {
    appearance: none;
    display: inline-grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border: none;
    border-radius: 50%;
    background: transparent;
    cursor: pointer;
  }

  .ao-dot span {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--bone-4);
    transition: transform 150ms, background 150ms;
  }

  .ao-dot.active span {
    transform: scale(1.25);
    background: var(--accent);
  }

  .ao-dot:disabled {
    cursor: default;
    opacity: 0.45;
  }

  .ao-live {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
  }

  @keyframes ao-pop {
    from {
      opacity: 0;
      transform: translate(-50%, -50%) scale(0.97);
    }
    to {
      opacity: 1;
      transform: translate(-50%, -50%) scale(1);
    }
  }

  @media (max-width: 700px) {
    .agent-onboarding {
      width: calc(100vw - 1rem);
      max-height: calc(100dvh - 1rem);
    }

    .ao-body {
      grid-template-columns: 1fr;
      gap: 0.8rem;
    }

    .ao-art {
      min-height: 140px;
    }

    .ao-actions {
      align-items: stretch;
      flex-direction: column;
    }

    .ao-dots,
    .ao-primary-row {
      justify-content: center;
    }

    .ao-primary-row {
      flex-direction: column-reverse;
      align-items: stretch;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .agent-onboarding {
      animation: none;
    }

    .ao-dot span {
      transition: none;
    }
  }
</style>
