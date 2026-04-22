# Third-party notices

The aftercalls desktop agent bundles and depends on open-source
components. Each of the following retains its own upstream license.
The list is not exhaustive — the `src-tauri/Cargo.toml`,
`package.json`, and `pnpm-lock.yaml` files in this repo are the
authoritative dependency manifest.

If you believe a component is missing from this notice, email
**privacy@aftercalls.io** and we will update it.

---

## ffmpeg (LGPL v2.1+)

The desktop agent bundles a build of [ffmpeg](https://ffmpeg.org/)
as a sidecar executable named `ffmpeg-aftercalls`. The sidecar is
downloaded by the CI release workflow
(`.github/workflows/release.yml`) from the following sources:

- **Linux**: statically-linked LGPL build from
  [John Van Sickle](https://johnvansickle.com/ffmpeg/). Upstream
  sources + build scripts are published on the same site.
- **Windows**: "essentials" LGPL build from
  [gyan.dev](https://www.gyan.dev/ffmpeg/builds/). Sources + build
  recipes are published at
  [github.com/GyanD/codexffmpeg](https://github.com/GyanD/codexffmpeg).

We distribute ffmpeg unmodified from those upstream releases. The
LGPL v2.1 license text is bundled with the downstream build and
available upstream at https://ffmpeg.org/legal.html.

**Corresponding source**: the exact unmodified upstream sources for
the ffmpeg build shipped with the version of aftercalls you received
are available on request at **privacy@aftercalls.io** for at least
three years from the date of that release, per LGPL v2.1 §6. You can
alternatively fetch the matching release directly from the publisher
sites linked above.

## Tauri (Apache-2.0 / MIT)

[Tauri 2](https://tauri.app) — the desktop app framework. Apache-2.0
/ MIT dual-licensed.

## Svelte / SvelteKit (MIT)

[Svelte](https://svelte.dev) / [SvelteKit](https://kit.svelte.dev) —
the UI framework.

## Rust crates (mix: Apache-2.0 / MIT / ISC)

`src-tauri/Cargo.toml` depends on a number of Rust crates including
but not limited to: tauri, tauri-plugin-*, serde, tokio, reqwest,
anyhow, chrono, cpal, hound, toml, dirs, sysinfo. These are all
Apache-2.0 / MIT / ISC — no copyleft obligations.

## windows (Apache-2.0 / MIT)

The [windows](https://github.com/microsoft/windows-rs) crate is used
on Windows-only builds of the agent to enumerate active WASAPI
capture sessions (PIPEDA auto-detect). Microsoft's official Rust
bindings, Apache-2.0 / MIT.

## Geist + Geist Mono (SIL Open Font License 1.1)

[Geist](https://vercel.com/font) from Vercel. Self-hosted in
`static/fonts/` to avoid third-party CDN calls. OFL v1.1 license
text is included in that directory as `OFL.txt`.

---

Last updated: 2026-04-22.
