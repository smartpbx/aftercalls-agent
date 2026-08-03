# Third-party notices

aftercalls bundles and depends on open-source components. Each of the
following retains its own upstream license. The list is not
exhaustive — the `Cargo.toml`, `package.json`, and `pnpm-lock.yaml`
files in the repo are the authoritative dependency manifest.

If you believe a component is missing from this notice, email
**privacy@aftercalls.io** and we will update it.

---

## FFmpeg sidecar (LGPL v3)

The desktop release and backend container are provisioned to bundle a build of
[FFmpeg](https://ffmpeg.org/). The desktop exposes it as the
`ffmpeg-aftercalls` sidecar; the backend exposes the identical Linux `ffmpeg`
and `ffprobe` executables on its internal `PATH`. CI downloads them at build
time from the following immutable monthly BtbN release. Both platforms use the
same FFmpeg source revision and LGPL feature set:

| Platform | Build | Archive SHA-256 |
|---|---|---|
| Linux x86_64 | [`ffmpeg-n7.1.5-12-g1fdbca85aa-linux64-lgpl-7.1.tar.xz`](https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n7.1.5-12-g1fdbca85aa-linux64-lgpl-7.1.tar.xz) | `58057a52db17bd2fefa87f271956f04aa2277d55efc13f288594cc2c65c59479` |
| Windows x86_64 | [`ffmpeg-n7.1.5-12-g1fdbca85aa-win64-lgpl-7.1.zip`](https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n7.1.5-12-g1fdbca85aa-win64-lgpl-7.1.zip) | `b7c1c846dacca68ee4ebf5c390742c973b3d5d14a6d44b061f500d8e4ac74fc0` |

The pin is enforced in CI: each download is sha256-verified before
extraction. CI then checks the exact `ffmpeg -version` identity,
requires libopus, and rejects a configuration containing
`--enable-gpl` or `--enable-nonfree`.

We distribute the executable unmodified from the upstream archives.
These builds enable version-3 components and are therefore conveyed
under **LGPL v3**. The exact `LICENSE.txt` from the selected platform
archive and this notice are included with every installer. We do not
currently publish a macOS installer or macOS sidecar.

### Corresponding source

The binary identifies FFmpeg commit
[`1fdbca85aaea513c9cc6c14d347f76543346d3da`](https://github.com/FFmpeg/FFmpeg/commit/1fdbca85aaea513c9cc6c14d347f76543346d3da).
The complete build configuration and dependency recipes are pinned at
BtbN/FFmpeg-Builds commit
[`a99e8230eae00d1cee38f23076a7a1f55cd984e2`](https://github.com/BtbN/FFmpeg-Builds/tree/a99e8230eae00d1cee38f23076a7a1f55cd984e2).
Every release built from this pinned configuration must attach
checksum-verified archives of both source snapshots next to the installers:

- `ffmpeg-source-1fdbca85aaea513c9cc6c14d347f76543346d3da.tar.gz`
  (SHA-256 `1312ecd4b87383182530278450c204c27e6787d033b190a28072a149cca59ed3`)
- `ffmpeg-build-scripts-a99e8230eae00d1cee38f23076a7a1f55cd984e2.tar.gz`
  (SHA-256 `7deac4a5190b2be84d4d548db2885d05152f9e3d77069d0e34841a46efd95e2b`)

For a copy of any corresponding dependency source or build artifact,
email **privacy@aftercalls.io**.

The backend container (`backend/Dockerfile`) uses the checksum-identical Linux
sidecar and includes this notice plus the archive's exact LGPL v3 license under
`/usr/share/doc/aftercalls/`. It does not install Debian's FFmpeg package or
its codec dependency tree.

### AAC-bearing imports (m4a, mp4)

The agent's import-audio path decodes user-supplied media through
the bundled ffmpeg, which includes AAC-carrying containers such as
m4a and mp4. AAC is still subject to patent assertions in some
jurisdictions by the MPEG-LA successor pool. At our current scale
and consistent with the posture taken by other desktop apps that
import user media via bundled ffmpeg, we accept this risk rather
than transcoding server-side or rejecting AAC-bearing inputs at the
picker. Revisit on material scale or distribution-model change.
Reference: issue #70.

## Tauri (Apache-2.0 / MIT)

[Tauri 2](https://tauri.app) — the desktop app framework. Used in
the agent. Apache-2.0 / MIT dual-licensed.

## Svelte / SvelteKit (MIT)

[Svelte](https://svelte.dev) / [SvelteKit](https://kit.svelte.dev) —
the UI framework used in the agent and the web portal.

## Rust crates (mix: Apache-2.0 / MIT / ISC)

The backend (`backend/Cargo.toml`) and the agent
(`agent/src-tauri/Cargo.toml`) depend on a number of Rust crates
including but not limited to: axum, tokio, sqlx, serde, reqwest,
anyhow, uuid, chrono, aws-sdk-s3, tauri-plugin-*. These are all
Apache-2.0 / MIT / ISC — no copyleft obligations.

## windows (Apache-2.0 / MIT)

The [windows](https://github.com/microsoft/windows-rs) crate is
used on Windows-only builds of the agent to enumerate active
WASAPI capture sessions (PIPEDA auto-detect). Microsoft's official
Rust bindings, Apache-2.0 / MIT.

## Geist + Geist Mono (SIL Open Font License 1.1)

[Geist](https://vercel.com/font) from Vercel. Self-hosted in
`site/fonts/`, `portal/static/fonts/`, and `agent/static/fonts/` to
avoid third-party CDN calls. OFL v1.1 license text is included in
each font directory as `OFL.txt`.

## marked (MIT)

The web portal uses [marked](https://github.com/markedjs/marked)
for rendering markdown in the TOS-acceptance flow. MIT-licensed.

## Sentry SDKs (MIT)

The backend, web portal, and desktop agent can use
[sentry-rust](https://github.com/getsentry/sentry-rust) and
[@sentry/browser](https://github.com/getsentry/sentry-javascript)
for DSN-gated diagnostic error reporting when configured.

## Services (sub-processors, not bundled)

The operational sub-processors aftercalls uses (cloud hosting,
transcription, summarization, email) aren't listed here — they
aren't licensed components we redistribute. See the current
[Privacy Policy](https://aftercalls.io/privacy) for how call data
flows through them at the category level.

---

Last updated: 2026-08-01.
