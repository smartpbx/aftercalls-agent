# Bundled audio assets

Static audio shipped with the SvelteKit static adapter. Files in this
directory are served at the same path under the agent webview's root
(e.g. `static/audio/consent-notice.opus` → fetchable as
`/audio/consent-notice.opus`).

## consent-notice (#56)

`consent-notice.opus` is the spoken recording-start announcement that
plays after the chime when the org's `recording_notification_mode` is
`enforced`, or when a user opts into "Play a spoken reminder when
recording starts" in Settings (mode: `user`).

### Spec

- **Text:** "Please note — this call is being recorded."
- **Voice:** neutral, business-friendly. No accent strongly tied to
  one region; the agent ships globally.
- **Length target:** 2.5–3.5 seconds. Long enough to register over a
  busy soft-phone, short enough not to clip the start of the
  conversation.
- **Format:** opus, mono, 24 kHz, ~32 kbps target → ~12–16 KB on disk.
  - If `.opus` causes playback issues on a target webview, fall back
    to `.mp3` (mono, 24 kHz, 48 kbps → ~20 KB) and update
    `notify.ts:notifyConsentAnnouncement` to point at `.mp3`.
- **Vendor opacity** (CLAUDE.md hard rule #2): the bundled metadata
  must NOT name the TTS service or voice provider. Strip ID3 / Opus
  tags before committing — `ffmpeg -i in.opus -map_metadata -1 -c
  copy out.opus` is enough.
- **License:** if the TTS service requires attribution, add an
  `<article>` to `site/licenses/index.html` per the project's
  licenses-disclosure policy.

### Bake (manual step — not yet automated)

This is a one-shot bake; the agent doesn't synthesize at runtime
because Web Speech support is inconsistent across webkit2gtk on
Linux + WebView2 on Windows. Pick a TTS service / voice synthesizer
locally, render to wav, then transcode:

```bash
# Example using a local TTS that outputs wav, then ffmpeg to opus.
# Replace the first line with your synthesizer of choice.
your-tts --text "Please note — this call is being recorded." \
         --voice neutral-female --out raw.wav
ffmpeg -i raw.wav -ac 1 -ar 24000 -c:a libopus -b:a 32k \
       -map_metadata -1 \
       agent/static/audio/consent-notice.opus
```

Until a real bake lands, the agent's playback path is fault-tolerant:
a 404 on this asset is logged and swallowed (the chime still plays),
so missing audio doesn't break recording.

### Future

Issue #56 calls out per-org customizable wording (similar to
`recording_purpose`). That requires a backend column +
portal-admin-side bake-on-save flow and is intentionally out of
scope for the v1 ship of this feature.
