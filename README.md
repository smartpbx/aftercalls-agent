# aftercalls — desktop agent

Tauri 2 + SvelteKit app that records a call on the user's machine,
uploads the per-channel tracks to the backend, and surfaces the
transcript + summary + action items the moment they land.

This directory is mirrored to the public repo at
[smartpbx/aftercalls-agent](https://github.com/smartpbx/aftercalls-agent)
for signed-release CI. Do not tag `v*` on this private repo —
releases are cut from the public mirror. See `memory/release_flow.md`
in the Claude session memory (or ask the maintainer) for the copy
procedure.

## Run locally

```bash
pnpm install
pnpm tauri:dev
```

The `tauri:dev` script sets `AFTERCALLS_PROFILE=dev`, so the agent
writes config to `~/.config/aftercalls-dev/` — isolated from an
installed production-profile copy on the same machine.

Linux: requires `libwebkit2gtk-4.1` and `libgtk-3` on the host. On
Arch: `pacman -S webkit2gtk-4.1 gtk3`. On Debian/Ubuntu:
`apt install libwebkit2gtk-4.1-0 libgtk-3-0`.

## ffmpeg sidecar

A self-contained ffmpeg binary is downloaded per-platform by CI and
bundled as a Tauri externalBin under the name `ffmpeg-aftercalls`.
For local dev, `pnpm tauri:dev` will fail at build time unless you
stage a placeholder at `src-tauri/binaries/ffmpeg-aftercalls-<triple>`.
The easiest stub is a symlink to your system `ffmpeg`:

```bash
ln -s "$(which ffmpeg)" \
  src-tauri/binaries/ffmpeg-aftercalls-x86_64-unknown-linux-gnu
```

The binary is gitignored (see `src-tauri/.gitignore`) so your
personal stub never ends up in a commit. CI downloads the real
LGPL-only build from John Van Sickle (Linux) or gyan.dev (Windows)
before `tauri-action` runs — see the release workflow in the public
mirror.

## Licensing

Proprietary — see the top-level `LICENSE`. ffmpeg and other bundled
third-party components retain their own licenses; see
`THIRD_PARTY_NOTICES.md` at the repo root.
