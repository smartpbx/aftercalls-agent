<!--
  LiveAssistPanel — Phase 1 live-transcript draft (agent Record page).

  Renders a scrolling You/Them draft transcript beside an in-flight
  recording. Channel attribution only: mic → "You", system → "Them" (no
  live diarization). Segments arrive `provisional` (styled unstable) and
  settle when their final lands. This is a *disposable draft* — the
  post-call batch transcript is the source of truth and is never shown
  here as canonical.

  Gated by the caller (`+page.svelte`) on `me.features.live_transcript`;
  hidden entirely when the feature is off. Styling lives in the shared
  `app.css` (`.live-panel*` / `.live-seg*`) per CLAUDE.md hard rule #1.
  See design.md §Pattern library "Live assist panel (live transcript)".
-->
<script lang="ts">
  import type { LiveSegment } from "@aftercalls/shared/types";

  type LiveStatus = "idle" | "live" | "ended" | "error";

  let {
    segments = [],
    status = "idle",
  }: {
    segments?: LiveSegment[];
    status?: LiveStatus;
  } = $props();

  // Fold the raw ordered stream into a stable display list: finals in
  // arrival order, plus the current in-progress provisional per channel
  // appended at the tail. A provisional is replaced by later partials for
  // the same channel and cleared when that channel's final lands — so the
  // last row settles in place (its `provisional` class drops, CSS
  // transitions the opacity/colour) when the final arrives.
  let display = $derived.by(() => {
    const finals: LiveSegment[] = [];
    const prov: Record<string, LiveSegment> = {};
    for (const s of segments) {
      if (s.provisional) {
        prov[s.channel] = s;
      } else {
        finals.push(s);
        delete prov[s.channel];
      }
    }
    return [...finals, ...Object.values(prov)];
  });

  function labelFor(s: LiveSegment): string {
    if (s.speaker) return s.speaker;
    return s.channel === "mic" ? "You" : "Them";
  }

  // Auto-scroll to the newest line as segments stream in, but only when the
  // user is already near the bottom — don't yank them back if they've
  // scrolled up to re-read an earlier line.
  let streamEl: HTMLDivElement | undefined = $state();
  $effect(() => {
    // Read `display` so this effect re-runs on every stream update.
    void display.length;
    const el = streamEl;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
    if (nearBottom) el.scrollTop = el.scrollHeight;
  });
</script>

<section
  class="live-panel live-status-{status}"
  style="--i: 2.5"
  aria-label="Live transcript (draft)"
>
  <div class="live-panel-head">
    <span class="live-status-pip" aria-hidden="true"></span>
    <span class="live-panel-title">Live transcript</span>
    {#if status === "error"}
      <span class="live-status-note">Live unavailable</span>
    {:else if status === "ended"}
      <span class="live-status-note">Draft</span>
    {:else}
      <span class="live-status-note">Draft — finalized after the call</span>
    {/if}
  </div>

  {#if status === "error"}
    <p class="live-empty">
      The live draft isn't available right now. Your recording is
      unaffected — the full transcript is prepared after the call.
    </p>
  {:else if display.length === 0}
    <p class="live-empty">
      {status === "live" ? "Listening…" : "Waiting for audio…"}
    </p>
  {:else}
    <div
      class="live-stream"
      bind:this={streamEl}
      role="log"
      aria-live="polite"
    >
      {#each display as seg (seg.channel + ":" + seg.start_ms + ":" + (seg.provisional ? "p" : "f"))}
        <div
          class="live-seg live-seg-{seg.channel === 'mic' ? 'you' : 'them'}"
          class:live-seg-provisional={seg.provisional}
        >
          <span class="live-seg-label">{labelFor(seg)}</span>
          <span class="live-seg-text">{seg.text}</span>
        </div>
      {/each}
    </div>
  {/if}
</section>
