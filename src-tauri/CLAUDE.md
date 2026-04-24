# CLAUDE.md — agent/src-tauri

Scoped notes for the Tauri backend. Inherits everything from the
repo-root `CLAUDE.md` and `.claude/context-map.md`.

## ffmpeg sidecar (gitignored; required for `cargo check` / builds)

Tauri ships ffmpeg as an `externalBin` sidecar, declared in
`tauri.conf.json` as `binaries/ffmpeg-aftercalls`. The actual binary
lives under `agent/src-tauri/binaries/` and is **gitignored** (see
`agent/src-tauri/.gitignore`) — CI downloads it per-platform at
release time.

Fresh checkouts, and fresh worktrees in particular, start without the
sidecar, so `cargo check` / `cargo build` inside `agent/src-tauri/`
fail to find `ffmpeg-aftercalls-<triple>`. Symlink it from the main
working tree instead of redownloading:

```bash
# From the repo root of a fresh worktree:
mkdir -p agent/src-tauri/binaries
ln -s \
  /home/cmannerow/Nextcloud/Documents/programming/callscribe/agent/src-tauri/binaries/ffmpeg-aftercalls-x86_64-unknown-linux-gnu \
  agent/src-tauri/binaries/ffmpeg-aftercalls-x86_64-unknown-linux-gnu
```

Adjust the triple for the host platform (e.g.
`x86_64-pc-windows-msvc.exe` on Windows). The main working tree path
above is the canonical local source; if you moved the repo, update
accordingly.

Background: three v0.5.2 builder worktrees independently tripped on
this. Filed as #174.
