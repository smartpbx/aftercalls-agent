# aftercalls — desktop agent

Tauri 2 + SvelteKit desktop app that records, transcribes, and files your
calls. Runs on Linux, Windows, and macOS.

This is the **public** source for the desktop agent. The backend, web
portal, and deployment live in a separate private repo. Users who just
want to install the app should go to
[app.aftercalls.io/downloads](https://app.aftercalls.io/downloads).

## Build

Requires Rust stable, Node 20, pnpm 10, and the platform's Tauri
prerequisites ([docs](https://tauri.app/start/prerequisites/)).

```
pnpm install
pnpm tauri build
```

On Linux, `pnpm tauri build --bundles deb` skips the AppImage step
(which is slow and bundles a webkit fork that is unstable on some
distros — the AppImage is known to crash on Arch + NixOS).

## Release

Pushing a `v*` tag triggers `.github/workflows/release.yml`:

1. Builds signed installers + updater manifests on Linux + Windows via
   [tauri-action](https://github.com/tauri-apps/tauri-action).
2. Packages a system-linked Linux tarball (agent binary + .desktop +
   icon) for distros where the AppImage's bundled webkit crashes.
3. Uploads everything to a draft GitHub Release.
4. Mirrors the assets + rewritten `latest.json` to the DigitalOcean
   Spaces bucket that running apps poll for updates. Linux entries are
   stripped from `latest.json` so the auto-updater doesn't clobber
   manually-installed tarballs with the crashy AppImage.

Per-push master + PR builds run through `.github/workflows/build-agent.yml`
(Linux-only, no signing) for compile validation.

## License

Proprietary. Source is public so users can audit the app that runs on
their machine and so CI can build without eating private-repo minutes.
