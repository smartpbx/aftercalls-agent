<script lang="ts">
  // #302 Slice C — screen-capture consent modal.
  //
  // The DISTINCT, heavier consent gate for recording the screen as
  // video (separate from the audio recording-ack: separate copy,
  // separate endpoint `screen_capture_acks`, so audit clarity keeps the
  // two consents apart). Fired from the Settings toggle's first enable;
  // the toggle does NOT persist `enabled=true` until `onaccept` succeeds.
  //
  // Reuses the shared `.rn-*` modal shell (app.css) + the page-local
  // `.ack-*` vocabulary (copied component-scoped from the Record-page
  // recording-ack modal — those classes are not global). Vendor-opaque:
  // no capture-backend or storage-provider names; geography stays.

  import { openUrl } from "@tauri-apps/plugin-opener";

  let {
    submitting = false,
    error = "",
    onaccept,
    oncancel,
  }: {
    submitting?: boolean;
    error?: string;
    onaccept: () => void;
    oncancel: () => void;
  } = $props();

  let checked = $state(false);
  let primaryBtn = $state<HTMLButtonElement | undefined>(undefined);

  // Focus the primary action on open (matches the release-notes /
  // recording-ack modal discipline in design.md §Release-notes modal).
  $effect(() => {
    primaryBtn?.focus();
  });

  async function openGuide() {
    try {
      await openUrl("https://aftercalls.io/help#screen-recording");
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }
</script>

<div
  class="rn-backdrop"
  role="presentation"
  onclick={() => !submitting && oncancel()}
  onkeydown={(e) => {
    if (e.key === "Escape" && !submitting) oncancel();
  }}
>
  <div
    class="rn-modal ack-modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby="scr-consent-title"
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.stopPropagation()}
    tabindex="-1"
  >
    <div class="rn-head">
      <h2 id="scr-consent-title">
        <svg
          class="scr-consent-glyph"
          viewBox="0 0 20 20"
          width="18"
          height="18"
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
        Turn on screen recording
      </h2>
    </div>

    <div class="rn-body">
      <p class="ack-body ack-emph">
        While a call is recording, aftercalls records your chosen display
        as video — so whatever is on that screen (other apps, windows,
        messages, or anything sensitive) is captured.
      </p>
      <p class="ack-body">
        Only the one display you pick is recorded. It records only while a
        call is recording, and you can turn it off any time in Settings.
      </p>
      <p class="ack-body">
        Recordings are stored securely and kept for a limited time, then
        deleted automatically. Your recordings may be stored and processed
        outside your country.
      </p>
      <p class="ack-body ack-emph">
        Your screen recording is never sent to the AI — it's saved for you
        to review and nothing else.
      </p>
      <p class="ack-body">
        Tell everyone on the call their screen-share may be recorded and
        get their consent.
      </p>

      <label class="ack-check">
        <input type="checkbox" bind:checked disabled={submitting} />
        <span>
          I understand my screen will be recorded as video and I'll get
          consent from everyone on the call.
        </span>
      </label>

      <button type="button" class="ack-guide" onclick={openGuide}>
        Read full guide <span aria-hidden="true">↗</span>
      </button>

      {#if error}
        <p class="ack-err" role="alert">{error}</p>
      {/if}
    </div>

    <div class="rn-actions ack-actions">
      <button
        type="button"
        class="ack-cancel"
        onclick={oncancel}
        disabled={submitting}
      >
        Cancel
      </button>
      <button
        type="button"
        class="rn-dismiss"
        bind:this={primaryBtn}
        onclick={onaccept}
        disabled={!checked || submitting}
      >
        {submitting ? "Saving…" : "Turn on screen recording"}
      </button>
    </div>
  </div>
</div>

<style>
  /* Head glyph — the shared "this is the screen thing" monitor marker. */
  .scr-consent-glyph {
    color: var(--bone-2);
    margin-right: 0.4rem;
    vertical-align: -0.15em;
  }

  /* `.ack-*` copied component-scoped from the Record-page recording-ack
   * modal (design.md §Recording-ack modal). The `.rn-*` shell classes
   * come from app.css (global shared modal vocabulary). */
  .ack-body {
    margin: 0 0 0.85rem;
    font-size: 0.9rem;
    line-height: 1.55;
    color: var(--bone-1);
  }
  .ack-body.ack-emph {
    color: var(--bone-0);
    font-weight: 500;
  }
  .ack-check {
    display: flex;
    align-items: flex-start;
    gap: 0.55rem;
    margin: 0.4rem 0 0.35rem;
    padding: 0.6rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    font-size: 0.9rem;
    color: var(--bone-0);
    cursor: pointer;
  }
  .ack-check input[type="checkbox"] {
    margin-top: 0.2rem;
    width: 14px;
    height: 14px;
    accent-color: var(--accent);
    cursor: pointer;
  }
  .ack-guide {
    appearance: none;
    background: transparent;
    border: none;
    padding: 0.2rem 0 0.6rem;
    color: var(--bone-2);
    font: inherit;
    font-size: 0.82rem;
    cursor: pointer;
    text-align: left;
    transition: color 0.15s;
  }
  .ack-guide:hover {
    color: var(--accent-hi);
  }
  .ack-err {
    margin: 0.5rem 0 0;
    color: var(--live);
    font-size: 0.85rem;
  }
  .ack-actions {
    margin-top: 0.4rem;
  }
  .ack-cancel {
    padding: 0.55rem 1rem;
    border: 1px solid var(--hairline-hi);
    background: transparent;
    color: var(--bone-1);
    font-size: 0.88rem;
    font-weight: 500;
    border-radius: 8px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .ack-cancel:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--bone-3);
  }
  .ack-cancel:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
</style>
