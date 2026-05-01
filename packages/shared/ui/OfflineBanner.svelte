<script lang="ts">
  // Offline indicator pill (#107). Renders nothing while online; once
  // the caller's `onlineStatus` store flips to offline, surface a small
  // top-strip pill so the user understands why background saves aren't
  // landing. The copy keeps the failure language about THEM, not us —
  // see issue #107 spec + repo CLAUDE.md "vendor-opaque" rule.
  //
  // Shared UI only depends on the public online flag. Each surface
  // keeps its intentionally drifted onlineStatus implementation local.

  let { online }: { online: boolean } = $props();
</script>

{#if !online}
  <div class="offline-pill" role="status" aria-live="polite">
    <span class="pip" aria-hidden="true"></span>
    <span class="label">You're offline — changes will save when you're back.</span>
  </div>
{/if}

<style>
  /* Top-strip pill sized to match the existing `.update` chip in
     +layout.svelte (mono 0.7rem, 1px hairline, pill border-radius).
     Stays component-scoped so we don't pollute the mirrored app.css.
     `margin-left: auto` floats the pill to the right of the topstrip
     so it sits between the crumb and any window-control / avatar
     chrome — mirrors how the agent's `.update` pills already lay out
     in `.strip-right`. */
  .offline-pill {
    display: inline-flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.28rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    letter-spacing: 0.02em;
    color: var(--bone-1);
    min-width: 0;
    max-width: 100%;
    margin-left: auto;
  }

  /* On narrow viewports the topstrip is already pinched between a
     hamburger, the page title, and the avatar — keep the pill from
     squeezing the title to one character by collapsing the label and
     surfacing only the pip. The aria-live region in the markup still
     announces the full copy to assistive tech. */
  @media (max-width: 640px) {
    .offline-pill {
      padding: 0.28rem 0.45rem;
    }
    .label {
      display: none;
    }
  }

  .pip {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--bone-3);
    flex-shrink: 0;
    /* Subtle pulse so the pill reads as a live status without
       shouting; the offline state is a polite informational signal,
       not an alarm (the issue calls it "polite + unobtrusive"). */
    animation: offline-pulse 2s ease-in-out infinite;
  }

  @keyframes offline-pulse {
    0%, 100% { opacity: 0.6; }
    50% { opacity: 1; }
  }

  .label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
