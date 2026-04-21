<script lang="ts">
  import { page } from "$app/state";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { getVersion } from "@tauri-apps/api/app";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { check as checkForUpdate, type Update } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onDestroy, onMount } from "svelte";
  import "../app.css";

  let { children } = $props();

  type Me = {
    email: string;
    display_name: string;
    role: string;
    org_display_name: string;
  };

  let me = $state<Me | null>(null);
  let authResolved = $state(false);

  let recording = $state(false);
  let pipelineStage = $state("");
  let unlistenState: UnlistenFn | null = null;
  let unlistenPipeline: UnlistenFn | null = null;
  let unlistenTray: UnlistenFn | null = null;
  let unlistenUpdatePoll: (() => void) | null = null;

  // Linux has two update paths depending on how the app was installed:
  //   - AppImage → tauri-plugin-updater's `check()` returns an Update
  //     (APPIMAGE env var is set at launch; updater swaps in place).
  //     Same user flow as Windows: "Install" button kicks it off.
  //   - .deb / .rpm / tarball → `check()` returns null because the
  //     updater can't replace those installs. We fall back to a
  //     manifest fetch + semver compare, showing a "Get it ↗" pill
  //     that opens /downloads. No in-place upgrade.
  // The latest.json at the URL below always has a linux-x86_64
  // entry pointing at the slim system-webkit AppImage (#31).
  const UPDATE_MANIFEST_URL =
    "https://aftercalls-updates.tor1.digitaloceanspaces.com/latest.json";

  function semverGt(a: string, b: string): boolean {
    const pa = a.split(".").map((x) => parseInt(x, 10) || 0);
    const pb = b.split(".").map((x) => parseInt(x, 10) || 0);
    for (let i = 0; i < 3; i++) {
      const va = pa[i] ?? 0;
      const vb = pb[i] ?? 0;
      if (va !== vb) return va > vb;
    }
    return false;
  }

  async function pollForUpdate() {
    // Skip while one's already offered or being installed — don't
    // clobber the user's in-flight decision.
    if (updateAvailable || linuxUpdateAvailable || updateState === "downloading") return;
    // Primary path: Tauri's updater plugin. Works for Windows, macOS,
    // and AppImage Linux. Returns null for non-AppImage Linux installs.
    try {
      const u = await checkForUpdate();
      if (u) {
        updateAvailable = u;
        return;
      }
    } catch (e) {
      // Network blip or non-AppImage Linux — retry next tick, fall
      // through to the Linux manifest-fetch fallback below.
      console.warn("update check failed", e);
    }
    // Linux-only fallback for .deb/.rpm/tarball users (updater returned
    // null or threw because the running binary isn't an AppImage).
    if (isLinux) {
      try {
        const resp = await fetch(UPDATE_MANIFEST_URL, { cache: "no-store" });
        if (!resp.ok) return;
        const doc = (await resp.json()) as { version?: string };
        if (!doc.version || !version) return;
        if (semverGt(doc.version, version)) {
          linuxUpdateAvailable = doc.version;
        }
      } catch (e) {
        console.warn("linux manifest fetch failed", e);
      }
    }
  }

  async function openDownloadsPage() {
    try {
      await openUrl("https://app.aftercalls.io/downloads");
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }

  function dismissLinuxUpdate() {
    linuxUpdateAvailable = null;
  }

  let isLoginPage = $derived(page.url.pathname.startsWith("/login"));

  // Auto-update state. Sits in the top strip as an unobtrusive nudge, then
  // flips into a progress row while downloading, then into a restart prompt.
  let updateAvailable = $state<Update | null>(null);
  let updateState = $state<"idle" | "downloading" | "ready" | "error">("idle");
  let updateError = $state("");
  let updateDownloaded = $state(0);
  let updateTotal = $state(0);
  let version = $state("");
  // Linux has no in-place updater (see pollForUpdate). When we detect a
  // newer version on the manifest we stash the *new* version string here
  // and the topstrip renders a "v0.x.y out — get it" pill that opens
  // /downloads in the user's browser.
  let linuxUpdateAvailable = $state<string | null>(null);

  // Post-update welcome. On each auth session we compare the running
  // binary's version against the last one we showed release notes for
  // (localStorage). If it's new, we pop the modal once the user's name
  // is in hand so the greeting can be personal. No key => first install;
  // we still show a welcome with the current version's notes.
  const LAST_SEEN_VERSION_KEY = "aftercalls.lastSeenVersion";
  let releaseNotes = $state<{
    version: string;
    headline: string;
    changes: string[];
    footer?: string;
    firstName: string;
  } | null>(null);
  let releaseNotesChecked = false;

  // On Windows the backend drops native decorations so we draw our
  // own titlebar inside the webview. Detected once via UA; native
  // decorations stay on Linux + macOS.
  const isWindows =
    typeof navigator !== "undefined" &&
    /windows/i.test(navigator.userAgent);
  const isLinux =
    typeof navigator !== "undefined" &&
    /linux/i.test(navigator.userAgent) &&
    !/android/i.test(navigator.userAgent);
  let winMaximized = $state(false);

  async function minimizeWindow() {
    try {
      await getCurrentWindow().minimize();
    } catch {}
  }
  async function toggleMaximize() {
    const w = getCurrentWindow();
    try {
      await w.toggleMaximize();
      winMaximized = await w.isMaximized();
    } catch {}
  }
  async function closeWindow() {
    // Hide-on-close is handled Rust-side (prevents quit, keeps tray alive).
    try {
      await getCurrentWindow().close();
    } catch {}
  }


  onMount(async () => {
    // Safety net: on webkit2gtk, an unhandled promise rejection during a
    // client-side route transition can take the whole renderer down (blank
    // window + blank devtools). Swallow + log so the UI stays alive.
    // Using console.error (not warn) because some webkit builds filter warns
    // out of the devtools panel by default.
    window.addEventListener("unhandledrejection", (ev) => {
      const r: any = ev.reason;
      console.error(
        "[unhandledrejection]",
        r?.stack ?? r?.message ?? String(r),
      );
      ev.preventDefault();
    });
    window.addEventListener("error", (ev) => {
      console.error(
        "[error]",
        ev.error?.stack ?? ev.error?.message ?? ev.message,
      );
    });

    // Route guard: if we have no auth.json, send the user to /login. Do
    // this before subscribing to tray/pipeline events so background work
    // doesn't reference state from a signed-out session.
    try {
      me = await invoke<Me | null>("current_user");
    } catch (e) {
      console.warn("current_user failed", e);
    }
    authResolved = true;
    if (!me && !page.url.pathname.startsWith("/login")) {
      goto("/login");
      // NOTE: intentionally do NOT return here. onMount runs once for the
      // layout's lifetime, so if we bail now the recording-state / pipeline
      // listeners are never attached for this session. After the user logs
      // in the layout doesn't remount — the listeners we set up below are
      // the same ones that'll drive the status pill post-login.
    }

    unlistenState = await listen<{ recording: boolean }>(
      "recording-state",
      (evt) => (recording = evt.payload.recording),
    );
    unlistenPipeline = await listen<{ stage: string }>("pipeline", (evt) => {
      pipelineStage = evt.payload.stage ?? "";
      if (pipelineStage === "done" || pipelineStage === "failed") {
        setTimeout(() => {
          if (pipelineStage === "done" || pipelineStage === "failed")
            pipelineStage = "";
        }, 4000);
      }
    });
    // Tray menu items that need to route: open Settings directly.
    unlistenTray = await listen<string>("tray-open", (evt) => {
      if (evt.payload === "settings") goto("/settings");
    });

    // Read the running binary's version; displayed in the rail foot so a
    // user can tell which build they're on post-update.
    try {
      version = await getVersion();
    } catch (e) {
      console.warn("getVersion failed", e);
    }

    // Keep the maximize-button glyph in sync with actual window state.
    if (isWindows) {
      try {
        winMaximized = await getCurrentWindow().isMaximized();
        const unlisten = await getCurrentWindow().onResized(async () => {
          try {
            winMaximized = await getCurrentWindow().isMaximized();
          } catch {}
        });
        const prev = unlistenUpdatePoll;
        unlistenUpdatePoll = () => {
          prev?.();
          unlisten();
        };
      } catch {}
    }

    // Check for a new release on startup + on a slow periodic timer.
    // Users who leave the tray running for days were only getting
    // updates on next cold launch — now they'll see the "vX.Y.Z
    // available" pill within ~1h of a release landing.
    await pollForUpdate();
    const updateTimer = window.setInterval(
      () => {
        pollForUpdate();
      },
      60 * 60 * 1000,
    );
    unlistenUpdatePoll = () => window.clearInterval(updateTimer);

    // Fire the release-notes check if we already have an authed user
    // (returning session). Otherwise wait for the login event below.
    await maybeShowReleaseNotes();

    // The /login page fires this after a successful sign-in so we can
    // refresh the layout's user state + show the release-notes modal
    // personalized with the just-logged-in name.
    window.addEventListener("aftercalls-login", handleLoginEvent);
  });

  async function handleLoginEvent() {
    try {
      me = await invoke<Me | null>("current_user");
    } catch {}
    await maybeShowReleaseNotes();
  }

  // Shows the release-notes modal at most once per "new" version per
  // device — regardless of whether we're hit at onMount (returning
  // user) or from the post-login event (fresh sign-in).
  async function maybeShowReleaseNotes() {
    if (releaseNotesChecked) return;
    if (!version || !me) return;
    releaseNotesChecked = true;
    try {
      const lastSeen = localStorage.getItem(LAST_SEEN_VERSION_KEY);
      if (lastSeen === version) return;
      const resp = await fetch("/release-notes.json");
      if (!resp.ok) return;
      const all = (await resp.json()) as Record<
        string,
        { headline: string; changes: string[]; footer?: string }
      >;
      const entry = all[version];
      if (!entry) {
        // No entry for this version — silently bookmark so the modal
        // doesn't fire empty next launch.
        localStorage.setItem(LAST_SEEN_VERSION_KEY, version);
        return;
      }
      const firstName = (me?.display_name ?? "").split(/\s+/)[0] ?? "";
      releaseNotes = { version, firstName, ...entry };
    } catch (e) {
      console.warn("release notes load failed", e);
    }
  }

  function dismissReleaseNotes() {
    if (releaseNotes) {
      try {
        localStorage.setItem(LAST_SEEN_VERSION_KEY, releaseNotes.version);
      } catch {}
    }
    releaseNotes = null;
  }

  async function installUpdate() {
    if (!updateAvailable) return;
    updateState = "downloading";
    updateError = "";
    try {
      await updateAvailable.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          updateTotal = ev.data.contentLength ?? 0;
          updateDownloaded = 0;
        } else if (ev.event === "Progress") {
          updateDownloaded += ev.data.chunkLength;
        } else if (ev.event === "Finished") {
          updateState = "ready";
        }
      });
      // downloadAndInstall returns once the new version is applied. Tell the
      // user explicitly before we relaunch so the app doesn't just vanish.
      await relaunch();
    } catch (e) {
      updateError = String(e);
      updateState = "error";
    }
  }

  function dismissUpdate() {
    updateAvailable = null;
    updateState = "idle";
  }

  onDestroy(() => {
    unlistenState?.();
    unlistenPipeline?.();
    unlistenTray?.();
    unlistenUpdatePoll?.();
    window.removeEventListener("aftercalls-login", handleLoginEvent);
  });

  const items: {
    href: string;
    label: string;
    match: (p: string) => boolean;
    icon: string;
  }[] = [
    {
      href: "/",
      label: "Record",
      match: (p) => p === "/",
      // simple inline SVGs — microphone, list, gear
      icon: `<svg viewBox="0 0 20 20" width="16" height="16"><path d="M10 2a3 3 0 0 0-3 3v5a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" fill="currentColor"/><path d="M5 9v1a5 5 0 0 0 10 0V9M10 15v3M7 18h6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/></svg>`,
    },
    {
      href: "/calls",
      label: "Calls",
      match: (p) => p.startsWith("/calls"),
      icon: `<svg viewBox="0 0 20 20" width="16" height="16"><path d="M3 5h14M3 10h14M3 15h14" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>`,
    },
    {
      href: "/settings",
      label: "Settings",
      match: (p) => p.startsWith("/settings"),
      icon: `<svg viewBox="0 0 20 20" width="16" height="16"><circle cx="10" cy="10" r="2.4" stroke="currentColor" stroke-width="1.4" fill="none"/><path d="M10 2v2.2M10 15.8V18M2 10h2.2M15.8 10H18M4.3 4.3l1.6 1.6M14.1 14.1l1.6 1.6M4.3 15.7l1.6-1.6M14.1 5.9l1.6-1.6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>`,
    },
  ];

  const stageLabel: Record<string, string> = {
    started: "Processing",
    transcribing: "Transcribing",
    summarizing: "Summarizing",
    writing_note: "Writing note",
    uploading: "Syncing",
    done: "Saved",
    failed: "Failed",
  };

  // Figure out the page title for the top strip — functional, not narrative.
  let pageTitle = $derived.by(() => {
    const p = page.url.pathname;
    if (p === "/") return "Record";
    if (p === "/calls") return "Calls";
    if (p.startsWith("/calls/")) return "Call";
    if (p.startsWith("/settings")) return "Settings";
    return "aftercalls";
  });
</script>

<!-- Before auth resolves we render nothing so the login form doesn't flash
     behind the rail and vice versa. -->
{#if !authResolved}
  <div class="booting"></div>
{:else if isLoginPage}
  <div class="bare">
    {@render children()}
  </div>
{:else}
<div class="shell">
  <aside class="rail">
    <a href="/" class="brand">
      <span class="onair" class:live={recording}></span>
      <span class="wordmark">aftercalls</span>
    </a>

    <nav>
      {#each items as it (it.href)}
        {@const active = it.match(page.url.pathname)}
        <a
          href={it.href}
          class="nav-item"
          class:active
          aria-current={active ? "page" : undefined}
        >
          <span class="glyph">{@html it.icon}</span>
          <span class="label">{it.label}</span>
        </a>
      {/each}
    </nav>

    <!-- Anchored to the bottom of the rail: current user + running
         version, so which account + which build is one glance away. -->
    <div class="rail-foot">
      {#if me}
        <div class="who">
          <span class="who-name">{me.display_name}</span>
          <span class="who-org">{me.org_display_name}</span>
        </div>
      {/if}
      {#if version}
        <span class="version">v{version}</span>
      {/if}
    </div>
  </aside>

  <div class="main">
    <header
      class="topstrip"
      class:has-win-controls={isWindows}
      data-tauri-drag-region
      ondblclick={isWindows ? toggleMaximize : undefined}
    >
      <div class="crumbs" data-tauri-drag-region>
        <span class="crumb" data-tauri-drag-region>{pageTitle}</span>
      </div>

      <div class="strip-right">
        {#if updateAvailable}
          <div class="update">
            {#if updateState === "downloading"}
              <span class="pip working"></span>
              <span class="update-label">
                Updating to v{updateAvailable.version}…
                {#if updateTotal > 0}
                  {Math.min(100, Math.round((updateDownloaded / updateTotal) * 100))}%
                {/if}
              </span>
            {:else if updateState === "ready"}
              <span class="pip done"></span>
              <span class="update-label">Restarting…</span>
            {:else if updateState === "error"}
              <span class="pip failed"></span>
              <span class="update-label" title={updateError}>Update failed</span>
              <button class="update-dismiss" onclick={dismissUpdate}>Dismiss</button>
            {:else}
              <span class="pip sig"></span>
              <span class="update-label">
                v{updateAvailable.version} available
              </span>
              <button class="update-install" onclick={installUpdate}>Install</button>
              <button class="update-dismiss" onclick={dismissUpdate}>Later</button>
            {/if}
          </div>
        {:else if linuxUpdateAvailable}
          <div class="update">
            <span class="pip sig"></span>
            <span class="update-label">
              v{linuxUpdateAvailable} available
            </span>
            <button class="update-install" onclick={openDownloadsPage}>
              Get it ↗
            </button>
            <button class="update-dismiss" onclick={dismissLinuxUpdate}>
              Later
            </button>
          </div>
        {/if}

        <div class="indicator">
          {#if recording && pipelineStage && pipelineStage !== "done"}
            <!-- Back-to-back case: user's recording a new call while the
                 previous one is still processing. Keep both visible so
                 the pipeline progress isn't lost behind the live pill. -->
            <span class="pip live"></span>
            <span class="ind-label">Recording</span>
            <span class="ind-sep">·</span>
            <span class="pip {pipelineStage}"></span>
            <span class="ind-label">{stageLabel[pipelineStage] ?? pipelineStage}</span>
          {:else if recording}
            <span class="pip live"></span>
            <span class="ind-label">Recording</span>
          {:else if pipelineStage && pipelineStage !== "done"}
            <span class="pip {pipelineStage}"></span>
            <span class="ind-label">{stageLabel[pipelineStage] ?? pipelineStage}</span>
          {:else if pipelineStage === "done"}
            <span class="pip done"></span>
            <span class="ind-label">Saved</span>
          {:else}
            <span class="pip idle"></span>
            <span class="ind-label">Idle</span>
          {/if}
        </div>

        {#if isWindows}
          <!-- Windows-only window controls. The rest of the topstrip is
               the drag region; these buttons opt out so clicks don't
               also drag the window. -->
          <div class="win-controls">
            <button
              type="button"
              class="wc-btn"
              aria-label="Minimize"
              onclick={minimizeWindow}
            >
              <svg viewBox="0 0 12 12" width="12" height="12"><rect x="2" y="5.4" width="8" height="1.2" fill="currentColor"/></svg>
            </button>
            <button
              type="button"
              class="wc-btn"
              aria-label={winMaximized ? "Restore" : "Maximize"}
              onclick={toggleMaximize}
            >
              {#if winMaximized}
                <svg viewBox="0 0 12 12" width="12" height="12"><rect x="3.2" y="3.2" width="6" height="6" fill="none" stroke="currentColor" stroke-width="1.1"/><rect x="4.8" y="1.6" width="6" height="6" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              {:else}
                <svg viewBox="0 0 12 12" width="12" height="12"><rect x="2.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              {/if}
            </button>
            <button
              type="button"
              class="wc-btn wc-close"
              aria-label="Close"
              onclick={closeWindow}
            >
              <svg viewBox="0 0 12 12" width="12" height="12"><path d="M3 3 L9 9 M9 3 L3 9" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>
            </button>
          </div>
        {/if}
      </div>
    </header>

    <div class="page">
      {@render children()}
    </div>
  </div>
</div>
{/if}

{#if releaseNotes}
  <div
    class="rn-backdrop"
    role="button"
    tabindex="-1"
    onclick={dismissReleaseNotes}
    onkeydown={(e) => {
      if (e.key === "Escape") dismissReleaseNotes();
    }}
  >
    <div
      class="rn-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="rn-title"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      tabindex="-1"
    >
      <div class="rn-head">
        <span class="rn-badge">v{releaseNotes.version}</span>
        <h2 id="rn-title">
          {#if releaseNotes.firstName}
            {releaseNotes.firstName}, {releaseNotes.headline}
          {:else}
            {releaseNotes.headline}
          {/if}
        </h2>
      </div>
      <ul class="rn-list">
        {#each releaseNotes.changes as line (line)}
          <li>{line}</li>
        {/each}
      </ul>
      {#if releaseNotes.footer}
        <p class="rn-footer">{releaseNotes.footer}</p>
      {/if}
      <div class="rn-actions">
        <button class="rn-dismiss" onclick={dismissReleaseNotes}>
          Got it
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .booting {
    min-height: 100vh;
  }

  .bare {
    min-height: 100vh;
    display: flex;
    flex-direction: column;
  }

  .shell {
    position: relative;
    z-index: 1;
    display: grid;
    grid-template-columns: var(--rail-w) 1fr;
    min-height: 100vh;
  }

  /* ── Left rail ─────────────────────────────────────────────────────── */
  .rail {
    position: sticky;
    top: 0;
    height: 100vh;
    display: flex;
    flex-direction: column;
    padding: 1.1rem 0.9rem 0.9rem;
    border-right: 1px solid var(--hairline);
    background: var(--ink-0);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.25rem 0.45rem 0.25rem;
  }

  .rail-foot {
    margin-top: auto;
    padding-top: 0.8rem;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 0.5rem;
  }

  .who {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    padding: 0 0.55rem;
  }

  .who-name {
    font-size: 0.82rem;
    color: var(--bone-1);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .who-org {
    font-size: 0.7rem;
    color: var(--bone-3);
    letter-spacing: 0.02em;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .version {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--bone-4);
    letter-spacing: 0.04em;
    padding: 0.15rem 0.55rem;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    padding: 0.5rem 0.65rem;
    border-radius: var(--radius);
    color: var(--bone-2);
    font-size: 0.88rem;
    font-weight: 500;
    letter-spacing: -0.005em;
    transition:
      color 0.15s,
      background-color 0.15s;
  }

  .nav-item:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }

  .nav-item.active {
    color: var(--bone-0);
    background: var(--ink-2);
    box-shadow: inset 0 0 0 1px var(--hairline);
  }

  .nav-item .glyph {
    display: flex;
    align-items: center;
    color: var(--bone-3);
    transition: color 0.15s;
  }

  .nav-item:hover .glyph,
  .nav-item.active .glyph {
    color: var(--accent);
  }

  /* ── Main area ─────────────────────────────────────────────────────── */
  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .topstrip {
    position: sticky;
    top: 0;
    z-index: 5;
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: var(--topbar-h);
    padding: 0 1.5rem;
    border-bottom: 1px solid var(--hairline);
    /* Translucent background tracks the active theme — was hard-coded
       dark before so it looked wrong in light mode. The strip also
       doubles as the drag region on Windows (see #25); we handle that
       JS-side via onmousedown + startDragging because
       `-webkit-app-region: drag` eats mousedown events at the
       compositor level and the window-control buttons never see
       their clicks. */
    background: color-mix(in srgb, var(--ink-0) 85%, transparent);
    backdrop-filter: saturate(140%) blur(10px);
    -webkit-backdrop-filter: saturate(140%) blur(10px);
  }
  .topstrip.has-win-controls {
    /* No outer padding on the right so the window controls go
       edge-to-edge like a native titlebar. */
    padding: 0 0 0 1.5rem;
    -webkit-user-select: none;
    user-select: none;
  }

  .crumbs {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .crumb {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--bone-1);
    letter-spacing: -0.005em;
  }

  .strip-right {
    display: flex;
    align-items: center;
    gap: 0.9rem;
  }

  .update {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.22rem 0.6rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    letter-spacing: 0.02em;
    color: var(--bone-1);
  }

  .update-label {
    color: var(--bone-1);
  }

  .update-install,
  .update-dismiss {
    font-family: inherit;
    font-size: 0.7rem;
    padding: 0.15rem 0.55rem;
    border-radius: 4px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    transition: all 0.15s;
  }

  .update-install {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 600;
  }
  .update-install:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .update-dismiss:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }

  .pip.sig {
    background: var(--sig);
    box-shadow: 0 0 5px rgba(201, 162, 74, 0.6);
  }
  .pip.working {
    background: var(--accent);
    animation: pip-blink 0.8s ease-in-out infinite;
  }
  .pip.failed {
    background: var(--live);
  }

  .indicator {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    letter-spacing: 0.04em;
    color: var(--bone-2);
  }

  /* ── Windows custom titlebar controls ──────────────────────────── */
  .win-controls {
    display: flex;
    align-items: stretch;
    height: var(--topbar-h);
    margin-left: 0.3rem;
  }
  .wc-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 46px;
    height: 100%;
    background: transparent;
    border: none;
    color: var(--bone-2);
    cursor: pointer;
    transition: background 0.12s, color 0.12s;
    /* Native Windows titlebar buttons are square + flush with the
       top/right corners; mimic that. */
  }
  /* Tauri's drag-region handler checks `e.target.tagName` against an
     interactive allow-list (BUTTON, INPUT, A, SELECT, TEXTAREA). If a
     click lands on a nested <svg>, target.tagName === 'svg' is not on
     the list — the topstrip would start dragging and the button's
     onclick never fires. pointer-events: none on the svg promotes the
     target to the parent button, so the click hits target.tagName ===
     'BUTTON' and Tauri skips the drag. */
  .wc-btn svg {
    pointer-events: none;
  }
  .wc-btn:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .wc-btn:active {
    background: var(--ink-3);
  }
  .wc-btn.wc-close:hover {
    background: #e81123;
    color: #ffffff;
  }
  .wc-btn.wc-close:active {
    background: #b5101a;
  }

  .pip {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--bone-4);
  }

  .pip.live {
    background: var(--live);
    box-shadow: 0 0 8px var(--live);
    animation: pip-pulse 1.2s ease-in-out infinite;
  }
  .pip.started,
  .pip.transcribing,
  .pip.summarizing,
  .pip.writing_note,
  .pip.uploading {
    background: var(--sig);
    box-shadow: 0 0 5px rgba(201, 162, 74, 0.6);
    animation: pip-blink 1s ease-in-out infinite;
  }
  .pip.done {
    background: var(--olive);
    box-shadow: 0 0 5px rgba(143, 175, 114, 0.5);
  }

  @keyframes pip-pulse {
    0%,
    100% {
      transform: scale(1);
      opacity: 1;
    }
    50% {
      transform: scale(1.3);
      opacity: 0.85;
    }
  }
  @keyframes pip-blink {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.45;
    }
  }

  .page {
    flex: 1;
    min-height: 0;
  }

  /* ── Release notes modal ─────────────────────────────────────────── */
  .rn-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    backdrop-filter: blur(4px);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 60;
    padding: 1rem;
    cursor: default;
  }
  .rn-modal {
    max-width: 520px;
    width: 100%;
    padding: 1.6rem 1.7rem 1.3rem;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    box-shadow: 0 24px 48px -12px rgba(0, 0, 0, 0.6);
    cursor: auto;
  }
  .rn-head {
    display: flex;
    align-items: flex-start;
    gap: 0.7rem;
    margin-bottom: 0.9rem;
  }
  .rn-badge {
    display: inline-block;
    padding: 0.15rem 0.45rem;
    background: var(--accent-soft);
    color: var(--accent-hi);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    letter-spacing: 0.04em;
    border-radius: 4px;
    flex-shrink: 0;
    margin-top: 0.25rem;
  }
  .rn-head h2 {
    margin: 0;
    font-size: 1.1rem;
    line-height: 1.35;
    color: var(--bone-0);
    font-weight: 600;
  }
  .rn-list {
    margin: 0 0 1rem;
    padding-left: 1.05rem;
    color: var(--bone-1);
  }
  .rn-list li {
    margin: 0 0 0.55rem;
    font-size: 0.9rem;
    line-height: 1.5;
  }
  .rn-footer {
    margin: 0 0 1rem;
    padding: 0.7rem 0.85rem;
    border-left: 2px solid var(--sig);
    background: var(--ink-0);
    color: var(--bone-2);
    font-size: 0.82rem;
    line-height: 1.5;
    border-radius: 6px;
  }
  .rn-actions {
    display: flex;
    justify-content: flex-end;
  }
  .rn-dismiss {
    padding: 0.55rem 1.1rem;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.88rem;
    font-weight: 600;
    border-radius: 8px;
    cursor: pointer;
  }
  .rn-dismiss:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
</style>
