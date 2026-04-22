# aftercalls — desktop agent

Public source mirror for the aftercalls desktop agent (Tauri 2 +
SvelteKit). Runs on Linux and Windows.

Releases ship via this repo's CI (`.github/workflows/release.yml`).
The private backend, web portal, and site live at
[smartpbx/aftercalls](https://github.com/smartpbx/aftercalls) and are
not public.

End users who just want to install the app should go to
[app.aftercalls.io/downloads](https://app.aftercalls.io/downloads).

## Build

Requires Rust stable, Node 20, pnpm 10, and the platform's Tauri
prerequisites ([docs](https://tauri.app/start/prerequisites/)).

```
pnpm install
pnpm tauri build
```

## ffmpeg sidecar

The agent shells out to an ffmpeg sidecar, bundled as an
`externalBin` under the name `ffmpeg-aftercalls`. The sidecar is not
checked into this repo — it is downloaded in CI per `release.yml`
from the upstream LGPL builds (John Van Sickle for Linux, gyan.dev
for Windows) and bundled into the installer. See
[`THIRD_PARTY_NOTICES.md`](./THIRD_PARTY_NOTICES.md) for sourcing +
LGPL corresponding-source pointers.

## License

See [`LICENSE`](./LICENSE) and
[`THIRD_PARTY_NOTICES.md`](./THIRD_PARTY_NOTICES.md).
