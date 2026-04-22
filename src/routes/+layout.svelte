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
  import { notifyAutoDetect } from "$lib/notify";
  import { detectPlatform, playStartCueIfEnabled } from "$lib/compliance";
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
  let unlistenAutoDetect: UnlistenFn | null = null;

  // Auto-detect slide-out state (#59). Only the `prompt_start` kind
  // surfaces here; `prompt_end` (mid-recording idle-mic prompt) stays
  // on the Record page since the user is almost always on /record
  // while a recording is live. This slide-out anchors to the rail's
  // Record item so it's visible regardless of current route.
  let autoPrompt = $state<{ app: string } | null>(null);

  // Orphan-session recovery (#63). One-shot check at startup for
  // session_dirs left behind by a crashed / force-quit previous
  // session. Non-blocking pill in the top-strip, expandable via
  // "Review…" into a per-session list. Auto-clean for dirs older
  // than 7 days happens Rust-side inside list_orphan_sessions.
  type OrphanSession = {
    session_id: string;
    recorded_at: string;
    age_minutes: number;
  };
  let orphans = $state<OrphanSession[]>([]);
  let orphanReview = $state(false);
  // Set of session_ids currently being resumed or discarded so the
  // per-row buttons can show disabled state without hiding the row
  // before the async work completes.
  let orphanBusy = $state<Set<string>>(new Set());
  let orphanBulkBusy = $state(false);
  // PIPEDA ack modal state for the auto-detect start path. The Record
  // page owns its own ack modal for the manual Start Recording
  // button; having a second one here keeps the auto flow self-
  // contained so a user on /calls never needs to route-change to
  // acknowledge. Cached ack flag flips globally so both copies
  // short-circuit once the user has accepted.
  let autoAckOpen = $state(false);
  let autoAckChecked = $state(false);
  let autoAckError = $state("");
  let autoAckSubmitting = $state(false);
  let autoAckCached = $state(false);

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
    // Don't clobber an in-flight install — the Update object is the
    // one being downloaded right now and swapping it out would break
    // the progress callback. We DO still re-check in the
    // at-rest / error states so a newer version on the manifest
    // replaces a stale cached object (#58: user on 0.3.18 seeing a
    // "0.3.19 available" pill that never updates to 0.3.20).
    if (updateState === "downloading") return;
    // Primary path: Tauri's updater plugin. Works for Windows, macOS,
    // and AppImage Linux. Returns null for non-AppImage Linux installs.
    try {
      const u = await checkForUpdate();
      if (u) {
        // Replace the cached object only if the manifest has moved
        // forward. Keeps the pill steady when nothing changed; flips
        // it to the newer version when 0.3.19 → 0.3.20 happens while
        // the pill is open.
        if (!updateAvailable || semverGt(u.version, updateAvailable.version)) {
          updateAvailable = u;
        }
        return;
      }
      // `check()` returned null: there is no update. If we had a
      // cached Update from a previous poll (edge case: manifest
      // rolled back), clear it so the pill doesn't lie.
      updateAvailable = null;
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
          // Same refresh logic: only replace when newer.
          if (!linuxUpdateAvailable || semverGt(doc.version, linuxUpdateAvailable)) {
            linuxUpdateAvailable = doc.version;
          }
        } else {
          linuxUpdateAvailable = null;
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
  // A single release-notes modal showing every version the user
  // jumped over since their last launch. `entries` is ordered newest-
  // first so the headline at the top is the version they're now
  // running. When only one version is in the set the modal looks
  // identical to the pre-aggregation behaviour.
  type ReleaseNotesEntry = {
    version: string;
    headline: string;
    changes: string[];
    footer?: string;
  };
  let releaseNotes = $state<{
    entries: ReleaseNotesEntry[];
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
    // Forward frontend errors into the Rust-side telemetry buffer so
    // they land in /admin/logs alongside panics + pipeline failures.
    // Fire-and-forget invoke — telemetry is best-effort and
    // log_event is a no-op when the user has telemetry off.
    const sendToTelemetry = (
      level: "error" | "warn",
      module: string,
      message: string,
      meta?: Record<string, unknown>,
    ) => {
      invoke("log_event", {
        input: { level, module, message, meta },
      }).catch(() => {});
    };
    window.addEventListener("unhandledrejection", (ev) => {
      const r: any = ev.reason;
      const msg = r?.stack ?? r?.message ?? String(r);
      console.error("[unhandledrejection]", msg);
      sendToTelemetry("error", "webview::unhandledrejection", msg, {
        url: window.location.href,
      });
      ev.preventDefault();
    });
    window.addEventListener("error", (ev) => {
      const msg = ev.error?.stack ?? ev.error?.message ?? ev.message;
      console.error("[error]", msg);
      sendToTelemetry("error", "webview::error", String(msg), {
        url: window.location.href,
        filename: ev.filename,
        line: ev.lineno,
      });
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

    // Auto-detect → slide-out (#59). We only handle `prompt_start`
    // here; mid-recording `prompt_end` prompts stay with the Record
    // page since the user is almost always on /record while live.
    // `cleared` drops both the slide-out and any open ack modal.
    unlistenAutoDetect = await listen<
      | { kind: "prompt_start"; app: string }
      | { kind: "prompt_end"; app: string }
      | { kind: "cleared" }
    >("auto-detect", (evt) => {
      if (evt.payload.kind === "prompt_start") {
        // Only chime when the prompt newly appears — re-emissions
        // (e.g. the detector re-confirming the same session) shouldn't
        // repeat the sound.
        if (!autoPrompt) notifyAutoDetect();
        autoPrompt = { app: evt.payload.app };
      } else if (evt.payload.kind === "cleared") {
        autoPrompt = null;
        autoAckOpen = false;
      }
    });

    // Warm the ack-cached flag from current_user so the slide-out's
    // "Record this call" click can short-circuit without a roundtrip
    // when the user has already acknowledged.
    try {
      const u = await invoke<{
        recording_acknowledged?: boolean;
      } | null>("current_user");
      autoAckCached = !!u?.recording_acknowledged;
    } catch {}

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

    // Orphan-recovery scan (#63). Only meaningful when the user is
    // authed — scan_orphans talks to the backend to distinguish
    // complete calls from half-processed ones. Skip on the login
    // page; we'll pick it up again post-login via handleLoginEvent.
    if (me) {
      await loadOrphans();
    }

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
    // Fresh login → check for leftover sessions from a previous run.
    // Same call as the onMount path; idempotent.
    await loadOrphans();
  }

  async function loadOrphans() {
    try {
      orphans = await invoke<OrphanSession[]>("list_orphan_sessions");
    } catch (e) {
      // Scan failures (missing recordings dir, backend hiccup) aren't
      // worth surfacing — under-report rather than nag.
      console.warn("list_orphan_sessions failed", e);
      orphans = [];
    }
  }

  function ageLabel(mins: number): string {
    if (mins < 60) return `${mins} min ago`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `${hrs} hr ago`;
    const days = Math.floor(hrs / 24);
    return `${days} day${days === 1 ? "" : "s"} ago`;
  }

  async function resumeOrphan(id: string) {
    if (orphanBusy.has(id)) return;
    orphanBusy = new Set([...orphanBusy, id]);
    try {
      await invoke("resume_orphan_session", { sessionId: id });
      orphans = orphans.filter((o) => o.session_id !== id);
    } catch (e) {
      console.warn("resume_orphan_session failed", e);
    } finally {
      const next = new Set(orphanBusy);
      next.delete(id);
      orphanBusy = next;
      if (orphans.length === 0) orphanReview = false;
    }
  }

  async function discardOrphan(id: string) {
    if (orphanBusy.has(id)) return;
    orphanBusy = new Set([...orphanBusy, id]);
    try {
      await invoke("discard_orphan_session", { sessionId: id });
      orphans = orphans.filter((o) => o.session_id !== id);
    } catch (e) {
      console.warn("discard_orphan_session failed", e);
    } finally {
      const next = new Set(orphanBusy);
      next.delete(id);
      orphanBusy = next;
      if (orphans.length === 0) orphanReview = false;
    }
  }

  async function resumeAllOrphans() {
    if (orphanBulkBusy) return;
    orphanBulkBusy = true;
    // Sequential so we don't hammer the backend with N parallel
    // transcribe + summarize jobs. Each call returns once the
    // pipeline has been spawned server-side (the Tauri command
    // fire-and-forgets the run), so "sequential" here really just
    // serializes the resume HTTP handshake, not the full pipeline.
    for (const o of [...orphans]) {
      try {
        await invoke("resume_orphan_session", { sessionId: o.session_id });
        orphans = orphans.filter((x) => x.session_id !== o.session_id);
      } catch (e) {
        console.warn("resume all: session failed", o.session_id, e);
      }
    }
    orphanBulkBusy = false;
    orphanReview = false;
  }

  async function discardAllOrphans() {
    if (orphanBulkBusy) return;
    orphanBulkBusy = true;
    for (const o of [...orphans]) {
      try {
        await invoke("discard_orphan_session", { sessionId: o.session_id });
        orphans = orphans.filter((x) => x.session_id !== o.session_id);
      } catch (e) {
        console.warn("discard all: session failed", o.session_id, e);
      }
    }
    orphanBulkBusy = false;
    orphanReview = false;
  }

  // Shows the release-notes modal at most once per "new" version per
  // device — regardless of whether we're hit at onMount (returning
  // user) or from the post-login event (fresh sign-in). If the user
  // jumped multiple versions (e.g. 0.3.18 → 0.3.23), aggregate every
  // versioned entry between lastSeen (exclusive) and current (inclusive)
  // so they see the full changelog in one modal instead of getting
  // shown only the latest headline.
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
      // Collect every entry with version > lastSeen AND <= current
      // running. When lastSeen is absent (first install), everything
      // <= current qualifies — but we cap at the newest 3 entries so
      // the first-ever launch doesn't dump the whole history at them.
      const FIRST_INSTALL_CAP = 3;
      const candidates = Object.keys(all)
        .filter((v) => !semverGt(v, version)) // v <= current
        .filter((v) => !lastSeen || semverGt(v, lastSeen)) // v > lastSeen
        // Newest first.
        .sort((a, b) => (semverGt(a, b) ? -1 : semverGt(b, a) ? 1 : 0));
      const slice = lastSeen ? candidates : candidates.slice(0, FIRST_INSTALL_CAP);
      const entries: ReleaseNotesEntry[] = slice.map((v) => ({
        version: v,
        headline: all[v].headline,
        changes: all[v].changes,
        footer: all[v].footer,
      }));
      if (entries.length === 0) {
        // Nothing to show. Silently bookmark so the modal doesn't
        // fire empty next launch.
        localStorage.setItem(LAST_SEEN_VERSION_KEY, version);
        return;
      }
      const firstName = (me?.display_name ?? "").split(/\s+/)[0] ?? "";
      releaseNotes = { entries, firstName };
    } catch (e) {
      console.warn("release notes load failed", e);
    }
  }

  function dismissReleaseNotes() {
    // Bookmark the newest version shown (entries[0]) so the user
    // doesn't see the same aggregate modal again on next launch.
    if (releaseNotes && releaseNotes.entries.length > 0) {
      try {
        localStorage.setItem(LAST_SEEN_VERSION_KEY, releaseNotes.entries[0].version);
      } catch {}
    }
    releaseNotes = null;
  }

  async function installUpdate() {
    if (!updateAvailable) return;
    // Re-check right before install so we install the newest version
    // currently on the manifest, not whatever was cached in the
    // Update object when the hourly poll last ran (#58). This
    // collapses the 0.3.18 → 0.3.19 → 0.3.20 multi-step into one
    // install whenever the manifest has moved forward between the
    // poll and the click.
    let target: Update = updateAvailable;
    try {
      const fresh = await checkForUpdate();
      if (fresh && semverGt(fresh.version, target.version)) {
        target = fresh;
        updateAvailable = fresh;
      }
    } catch (e) {
      // Network blip — proceed with the cached object. Better to
      // install something than nothing.
      console.warn("pre-install recheck failed, using cached target", e);
    }
    updateState = "downloading";
    updateError = "";
    try {
      await target.downloadAndInstall((ev) => {
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
    unlistenAutoDetect?.();
    window.removeEventListener("aftercalls-login", handleLoginEvent);
  });

  // ── Auto-detect slide-out handlers (#59) ─────────────────────────
  // "Record this call" path: gated on PIPEDA ack. Same gate logic
  // the Record page's manual Start button uses, mirrored here so
  // the auto-start flow never needs a route change.
  async function autoRecordClick() {
    if (autoAckCached) {
      await triggerAutoStart();
      return;
    }
    // Re-check in case the local cache is stale (e.g. user
    // acknowledged on another device).
    try {
      const r = await invoke<{ accepted_at: string } | null>("get_recording_ack");
      if (r) {
        autoAckCached = true;
        await triggerAutoStart();
        return;
      }
    } catch {}
    // Not acknowledged — show the ack modal. The slide-out stays
    // visible behind the modal so the user's "Record this call"
    // intent is preserved if they cancel.
    autoAckChecked = false;
    autoAckError = "";
    autoAckOpen = true;
  }

  async function triggerAutoStart() {
    try {
      await invoke("confirm_auto_start");
      autoPrompt = null;
      // Fire the start cue per the org's notification mode. Non-
      // blocking — if the audio subsystem is flaky we'd rather start
      // the recording than miss it.
      playStartCueIfEnabled().catch(() => {});
    } catch (e) {
      console.warn("confirm_auto_start failed", e);
    }
  }

  async function autoDismiss() {
    try {
      await invoke("dismiss_auto_start");
    } catch {}
    autoPrompt = null;
  }

  async function submitAutoAck() {
    if (!autoAckChecked || autoAckSubmitting) return;
    autoAckSubmitting = true;
    autoAckError = "";
    try {
      const agentVersion = await getVersion().catch(() => "unknown");
      await invoke("post_recording_ack", {
        agentVersion,
        platform: detectPlatform(),
      });
      autoAckCached = true;
      autoAckOpen = false;
      await triggerAutoStart();
    } catch (e) {
      autoAckError = String(e).replace(/^Error:\s*/, "");
    } finally {
      autoAckSubmitting = false;
    }
  }

  function cancelAutoAck() {
    if (autoAckSubmitting) return;
    autoAckOpen = false;
    autoAckChecked = false;
    autoAckError = "";
    // Leave the slide-out visible — the user can still decide to
    // dismiss outright, or come back to acknowledge later before
    // the detector's own timeout clears the prompt.
  }

  async function openConsentGuide() {
    try {
      await openUrl("https://aftercalls.io/help#privacy-consent");
    } catch {}
  }

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
    // Settings moved out of the primary nav and into the user-menu
    // dropdown in the rail foot — account-level knobs don't belong
    // next to Record and Calls (the main workflow). See #33.
  ];

  // ── User menu (rail foot) ────────────────────────────────────────
  let userMenuOpen = $state(false);
  function toggleUserMenu() {
    userMenuOpen = !userMenuOpen;
  }
  function closeUserMenu() {
    userMenuOpen = false;
  }
  async function openSettings() {
    closeUserMenu();
    await goto("/settings");
  }
  async function openPortalLink(path: string) {
    closeUserMenu();
    try {
      await openUrl(`https://app.aftercalls.io${path}`);
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }
  async function manualUpdateCheck() {
    closeUserMenu();
    await pollForUpdate();
  }
  async function signOut() {
    closeUserMenu();
    try {
      await invoke("logout");
    } catch {}
    await goto("/login");
  }

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
        <div class="nav-row" class:has-slideout={it.href === "/" && autoPrompt}>
          <a
            href={it.href}
            class="nav-item"
            class:active
            aria-current={active ? "page" : undefined}
          >
            <span class="glyph">{@html it.icon}</span>
            <span class="label">{it.label}</span>
          </a>
          {#if it.href === "/" && autoPrompt && page.url.pathname !== "/"}
            <!-- Auto-detect slide-out (#59, #60). Shown only when the
                 user is on a route OTHER than /record — the Record
                 page has its own inline banner. Anchored to the
                 Record nav item so it's visible whether the user is
                 on /calls, /settings, etc. No focus-steal, no route
                 change. -->
            <div class="auto-slideout" role="dialog" aria-label="Call detected">
              <div class="auto-head">
                <span class="auto-pip" aria-hidden="true"></span>
                <span class="auto-title">Call detected</span>
              </div>
              <p class="auto-body">
                <strong>{autoPrompt.app}</strong> is using your microphone. Record this call?
              </p>
              <div class="auto-actions">
                <button
                  type="button"
                  class="auto-primary"
                  onclick={autoRecordClick}
                >Record this call</button>
                <button
                  type="button"
                  class="auto-secondary"
                  onclick={autoDismiss}
                >Not now</button>
              </div>
              <p class="auto-hint">
                Change auto-detect in
                <a href="/settings" onclick={() => (autoPrompt = null)}>Settings</a>.
              </p>
            </div>
          {/if}
        </div>
      {/each}
    </nav>

    <!-- Rail foot: user name/org doubles as the trigger for a
         dropdown menu that carries every account-scoped action.
         Keeps the primary nav clean (Record + Calls only). -->
    <div class="rail-foot" class:menu-open={userMenuOpen}>
      {#if me}
        <button
          type="button"
          class="who-btn"
          aria-haspopup="menu"
          aria-expanded={userMenuOpen}
          onclick={toggleUserMenu}
        >
          <span class="who-name">{me.display_name}</span>
          <span class="who-org">{me.org_display_name}</span>
          <span class="who-chevron" aria-hidden="true">
            {userMenuOpen ? "▾" : "▸"}
          </span>
        </button>
      {/if}
      {#if version}
        <!-- Version doubles as the release-notes entry point — clicking
             opens the public per-version page in the default browser.
             Replaces the equivalent user-menu item (redundant). -->
        <button
          type="button"
          class="version"
          title="See release notes"
          onclick={async () => {
            try { await openUrl("https://aftercalls.io/releases"); } catch {}
          }}
        >v{version}</button>
      {/if}

      {#if userMenuOpen}
        <!-- Backdrop absorbs outside clicks to dismiss the menu.
             role="button" + keydown for a11y. -->
        <div
          class="user-menu-backdrop"
          role="button"
          tabindex="-1"
          aria-label="Close menu"
          onclick={closeUserMenu}
          onkeydown={(e) => { if (e.key === "Escape") closeUserMenu(); }}
        ></div>
        <div class="user-menu" role="menu">
          <button class="um-item" role="menuitem" onclick={openSettings}>
            Settings
          </button>
          <button
            class="um-item"
            role="menuitem"
            onclick={() => openPortalLink("/calls")}
          >
            Open web app <span class="um-ext" aria-hidden="true">↗</span>
          </button>
          <button
            class="um-item"
            role="menuitem"
            onclick={async () => {
              closeUserMenu();
              try { await openUrl("https://aftercalls.io/help"); } catch {}
            }}
          >
            Help <span class="um-ext" aria-hidden="true">↗</span>
          </button>
          <button
            class="um-item"
            role="menuitem"
            onclick={async () => {
              closeUserMenu();
              try { await openUrl("https://aftercalls.io/licenses"); } catch {}
            }}
          >
            Licenses <span class="um-ext" aria-hidden="true">↗</span>
          </button>
          {#if me && (me.role === "admin" || me.role === "superadmin")}
            <div class="um-sep"></div>
            <button
              class="um-item"
              role="menuitem"
              onclick={() => openPortalLink("/admin/users")}
            >
              Team <span class="um-ext" aria-hidden="true">↗</span>
            </button>
            <button
              class="um-item"
              role="menuitem"
              onclick={() => openPortalLink("/admin/vocab")}
            >
              Org vocab <span class="um-ext" aria-hidden="true">↗</span>
            </button>
            {#if me.role === "superadmin"}
              <button
                class="um-item"
                role="menuitem"
                onclick={() => openPortalLink("/admin/tos")}
              >
                Terms &amp; privacy <span class="um-ext" aria-hidden="true">↗</span>
              </button>
            {/if}
          {/if}
          <div class="um-sep"></div>
          <button
            class="um-item"
            role="menuitem"
            onclick={manualUpdateCheck}
          >
            Check for updates
          </button>
          <button class="um-item um-danger" role="menuitem" onclick={signOut}>
            Sign out
          </button>
        </div>
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
        {#if orphans.length > 0}
          <!-- Orphan recovery pill (#63). Same shape as the update
               pill: a thin rounded chip with status pip + label +
               primary + dismiss action. Sig-yellow pip so it reads
               as an interrupt without the warning-red weight. -->
          <div class="update">
            <span class="pip sig"></span>
            <span class="update-label">
              {orphans.length} unfinished call{orphans.length === 1 ? "" : "s"}
              from before the last restart
            </span>
            <button
              class="update-install"
              onclick={resumeAllOrphans}
              disabled={orphanBulkBusy}
            >
              {orphanBulkBusy ? "Working…" : "Resume all"}
            </button>
            <button
              class="update-dismiss"
              onclick={discardAllOrphans}
              disabled={orphanBulkBusy}
            >Discard all</button>
            <button
              class="update-dismiss"
              onclick={() => (orphanReview = !orphanReview)}
              disabled={orphanBulkBusy}
            >{orphanReview ? "Hide" : "Review…"}</button>
          </div>
        {/if}
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
          <!-- onmousedown|stopPropagation is the belt half of the
               belt-and-suspenders: Tauri 2's drag-region handler fires
               on ancestor mousedown, and without this the button's
               mousedown bubbles up to .topstrip, drag-region calls
               start_dragging, the window starts dragging, the
               button's click event never gets dispatched. pointer-
               events: none on the <svg> alone wasn't enough. -->
          <div class="win-controls">
            <button
              type="button"
              class="wc-btn"
              aria-label="Minimize"
              onmousedown={(e) => e.stopPropagation()}
              onclick={minimizeWindow}
            >
              <svg viewBox="0 0 12 12" width="12" height="12"><rect x="2" y="5.4" width="8" height="1.2" fill="currentColor"/></svg>
            </button>
            <button
              type="button"
              class="wc-btn"
              aria-label={winMaximized ? "Restore" : "Maximize"}
              onmousedown={(e) => e.stopPropagation()}
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
              onmousedown={(e) => e.stopPropagation()}
              onclick={closeWindow}
            >
              <svg viewBox="0 0 12 12" width="12" height="12"><path d="M3 3 L9 9 M9 3 L3 9" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>
            </button>
          </div>
        {/if}
      </div>
    </header>

    {#if orphans.length > 0 && orphanReview}
      <!-- Expanded per-session review. Docks below the top strip so
           the user can act on each session individually. Reuses the
           pill's bone/ink palette for consistency with the pill
           above it. -->
      <div class="orphan-review" role="region" aria-label="Unfinished recordings">
        <ul class="orphan-list">
          {#each orphans as o (o.session_id)}
            {@const busy = orphanBusy.has(o.session_id)}
            <li class="orphan-row">
              <div class="orphan-meta">
                <span class="orphan-time">
                  {new Date(o.recorded_at).toLocaleString(undefined, {
                    dateStyle: "medium",
                    timeStyle: "short",
                  })}
                </span>
                <span class="orphan-age">· {ageLabel(o.age_minutes)}</span>
              </div>
              <div class="orphan-actions">
                <button
                  class="update-install"
                  onclick={() => resumeOrphan(o.session_id)}
                  disabled={busy || orphanBulkBusy}
                >{busy ? "…" : "Resume"}</button>
                <button
                  class="update-dismiss"
                  onclick={() => discardOrphan(o.session_id)}
                  disabled={busy || orphanBulkBusy}
                >{busy ? "…" : "Discard"}</button>
              </div>
            </li>
          {/each}
        </ul>
      </div>
    {/if}

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
        <span class="rn-badge">v{releaseNotes.entries[0].version}</span>
        <h2 id="rn-title">
          {#if releaseNotes.firstName}
            {releaseNotes.firstName}, {releaseNotes.entries[0].headline}
          {:else}
            {releaseNotes.entries[0].headline}
          {/if}
        </h2>
      </div>
      {#if releaseNotes.entries.length > 1}
        <p class="rn-aggregate-caption">
          You jumped {releaseNotes.entries.length} versions — here's everything since your last launch.
        </p>
      {/if}
      {#each releaseNotes.entries as entry, i (entry.version)}
        {#if i > 0}
          <!-- Secondary versions get a small version header + their
               headline in-line so the caller can scan per-version. -->
          <div class="rn-entry-head">
            <span class="rn-badge rn-badge-dim">v{entry.version}</span>
            <span class="rn-entry-headline">{entry.headline}</span>
          </div>
        {/if}
        <ul class="rn-list">
          {#each entry.changes as line (line)}
            <li>{line}</li>
          {/each}
        </ul>
        {#if entry.footer}
          <p class="rn-footer">{entry.footer}</p>
        {/if}
      {/each}
      <div class="rn-actions">
        <button
          type="button"
          class="rn-link"
          onclick={async () => {
            try { await openUrl("https://aftercalls.io/releases"); } catch {}
          }}
        >
          See all release notes <span aria-hidden="true">↗</span>
        </button>
        <button class="rn-dismiss" onclick={dismissReleaseNotes}>
          Got it
        </button>
      </div>
    </div>
  </div>
{/if}

{#if autoAckOpen}
  <!-- PIPEDA ack modal for the auto-detect start path (#44 + #59).
       Mirror of the Record page's ack modal but layout-owned so the
       slide-out never needs a route change to collect the ack. -->
  <div
    class="rn-backdrop"
    role="button"
    tabindex="-1"
    onclick={cancelAutoAck}
    onkeydown={(e) => {
      if (e.key === "Escape") cancelAutoAck();
    }}
  >
    <div
      class="rn-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="auto-ack-title"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      tabindex="-1"
    >
      <div class="rn-head">
        <h2 id="auto-ack-title">Before you record</h2>
      </div>
      <p class="auto-ack-body">
        Under Canadian PIPEDA — and equivalent privacy laws in most
        jurisdictions — you are responsible for notifying every
        participant that the call is being recorded, obtaining their
        consent, and using the recording only for the purpose you
        disclosed.
      </p>
      <p class="auto-ack-body">
        aftercalls doesn't automate consent. You do.
      </p>
      <label class="auto-ack-check">
        <input
          type="checkbox"
          bind:checked={autoAckChecked}
          disabled={autoAckSubmitting}
        />
        <span>I understand and will get consent from everyone I record.</span>
      </label>
      {#if autoAckError}
        <p class="auto-ack-err">{autoAckError}</p>
      {/if}
      <div class="rn-actions">
        <button
          type="button"
          class="rn-link"
          onclick={openConsentGuide}
        >See our recording-consent guide <span aria-hidden="true">↗</span></button>
        <div class="auto-ack-buttons">
          <button
            type="button"
            class="auto-secondary"
            onclick={cancelAutoAck}
            disabled={autoAckSubmitting}
          >Cancel</button>
          <button
            type="button"
            class="rn-dismiss"
            onclick={submitAutoAck}
            disabled={!autoAckChecked || autoAckSubmitting}
          >{autoAckSubmitting ? "Saving…" : "I understand — start recording"}</button>
        </div>
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
    position: relative;
    margin-top: auto;
    padding-top: 0.8rem;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 0.5rem;
  }

  .who-btn {
    display: grid;
    grid-template-columns: 1fr auto;
    grid-template-rows: auto auto;
    align-items: center;
    gap: 0.1rem 0.4rem;
    padding: 0.4rem 0.55rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    cursor: pointer;
    color: inherit;
    font: inherit;
    text-align: left;
    transition: background 0.12s, border-color 0.12s;
  }
  .who-btn:hover,
  .rail-foot.menu-open .who-btn {
    background: var(--ink-2);
    border-color: var(--hairline);
  }
  .who-btn .who-chevron {
    grid-row: 1 / span 2;
    grid-column: 2;
    align-self: center;
    color: var(--bone-4);
    font-size: 0.72rem;
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

  /* ── User menu dropdown ──────────────────────────────────────────
     Anchored above the rail-foot so the popup opens UP from the
     button rather than overflowing the bottom of the rail. */
  .user-menu-backdrop {
    position: fixed;
    inset: 0;
    z-index: 40;
    cursor: default;
  }
  .user-menu {
    position: absolute;
    bottom: 100%;
    left: 0;
    right: 0;
    margin-bottom: 0.4rem;
    padding: 0.35rem;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    box-shadow: 0 14px 30px -10px rgba(0, 0, 0, 0.55);
    z-index: 45;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .um-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.55rem 0.7rem;
    background: transparent;
    border: none;
    border-radius: 6px;
    color: var(--bone-1);
    font: inherit;
    font-size: 0.85rem;
    text-align: left;
    cursor: pointer;
    transition: background 0.1s, color 0.1s;
  }
  .um-item:hover {
    background: var(--ink-2);
    color: var(--bone-0);
  }
  .um-item.um-danger {
    color: var(--live);
  }
  .um-item.um-danger:hover {
    background: var(--live-soft);
  }
  .um-ext {
    color: var(--bone-4);
    font-size: 0.75rem;
  }
  .um-sep {
    height: 1px;
    background: var(--hairline);
    margin: 0.2rem 0.3rem;
  }

  .version {
    /* It's a real <button> now (links to release notes) but we want
       it to look exactly like the static label it used to be —
       reset the browser-default button chrome and keep only the
       text styling. The cursor + hover color signal the affordance. */
    appearance: none;
    background: transparent;
    border: none;
    font: inherit;
    text-align: left;
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--bone-4);
    letter-spacing: 0.04em;
    padding: 0.15rem 0.55rem;
    transition: color 0.15s;
  }
  .version:hover {
    color: var(--bone-1);
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

  /* ── Orphan recovery (#63) ─────────────────────────────────────────
     Expanded review panel docks just under the top strip. Bone/ink
     palette to read as a system notification band rather than shouting
     for attention. */
  .orphan-review {
    border-bottom: 1px solid var(--hairline);
    background: var(--ink-1);
  }
  .orphan-list {
    list-style: none;
    margin: 0;
    padding: 0.5rem 1.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    max-height: 40vh;
    overflow-y: auto;
  }
  .orphan-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.8rem;
    padding: 0.45rem 0.6rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
  }
  .orphan-meta {
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
    font-size: 0.82rem;
    color: var(--bone-1);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .orphan-time {
    font-weight: 500;
    color: var(--bone-0);
  }
  .orphan-age {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-3);
  }
  .orphan-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
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
  /* Shared shell styles (rn-backdrop / rn-modal / rn-head / rn-actions
     / rn-dismiss) live in app.css so the ack modal on the Record page
     can reuse the same vocabulary. Below are only the bits specific
     to the release-notes variant — the version badge, bullet list,
     known-issues footer, and the "See all release notes" link. */
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
  /* Aggregated release-notes: dimmer badge + per-entry header for
     every version below the current one when the user jumped
     multiple releases. */
  .rn-aggregate-caption {
    margin: -0.4rem 0 1rem;
    color: var(--bone-3);
    font-size: 0.82rem;
    line-height: 1.45;
  }
  .rn-entry-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 1.4rem 0 0.55rem;
    padding-top: 0.9rem;
    border-top: 1px solid var(--hairline);
  }
  .rn-badge-dim {
    margin-top: 0;
    background: var(--ink-2);
    color: var(--bone-3);
  }
  .rn-entry-headline {
    font-size: 0.92rem;
    font-weight: 600;
    color: var(--bone-0);
    letter-spacing: -0.005em;
    line-height: 1.35;
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
  .rn-link {
    appearance: none;
    background: transparent;
    border: none;
    cursor: pointer;
    font: inherit;
    font-size: 0.82rem;
    color: var(--bone-2);
    padding: 0.3rem 0;
    transition: color 0.15s;
  }
  .rn-link:hover {
    color: var(--accent-hi);
  }

  /* ── Auto-detect slide-out (#59) ────────────────────────────────
     Anchored to the Record nav item in the rail, extends out to the
     right over the main content area. Doesn't steal focus, doesn't
     route-change — the user stays wherever they were. Kind-neutral
     bone/ink surface, sig-yellow accent on the "Call detected" pip
     so it reads as an interrupt without shouting. */
  .nav-row {
    position: relative;
  }
  .auto-slideout {
    position: absolute;
    top: 0;
    left: calc(100% + 0.6rem);
    width: 280px;
    padding: 0.85rem 0.9rem 0.8rem;
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    background: var(--ink-1);
    box-shadow: 0 14px 30px -10px rgba(0, 0, 0, 0.55);
    z-index: 50;
    animation: slide-in 180ms cubic-bezier(0.2, 0.6, 0.2, 1) both;
  }
  @keyframes slide-in {
    from {
      opacity: 0;
      transform: translateX(-6px);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }
  .auto-head {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    margin-bottom: 0.4rem;
  }
  .auto-pip {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--sig);
    box-shadow: 0 0 6px rgba(201, 162, 74, 0.7);
    animation: auto-pip-pulse 1.3s ease-in-out infinite;
  }
  @keyframes auto-pip-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.5;
    }
  }
  .auto-title {
    font-size: 0.78rem;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .auto-body {
    margin: 0 0 0.8rem;
    font-size: 0.85rem;
    line-height: 1.45;
    color: var(--bone-1);
  }
  .auto-body strong {
    color: var(--bone-0);
    font-weight: 600;
  }
  .auto-actions {
    display: flex;
    gap: 0.45rem;
    margin-bottom: 0.5rem;
  }
  .auto-primary {
    flex: 1;
    padding: 0.5rem 0.75rem;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 600;
    font-size: 0.82rem;
    border-radius: 8px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .auto-primary:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .auto-secondary {
    flex: 0 0 auto;
    padding: 0.5rem 0.75rem;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--bone-1);
    font-size: 0.82rem;
    border-radius: 8px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .auto-secondary:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
  .auto-secondary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .auto-hint {
    margin: 0;
    font-size: 0.72rem;
    color: var(--bone-3);
    line-height: 1.4;
  }
  .auto-hint a {
    color: var(--bone-2);
    text-decoration: underline;
  }
  .auto-hint a:hover {
    color: var(--bone-0);
  }

  /* Auto-detect PIPEDA ack modal — reuses .rn-backdrop + .rn-modal
     from the release-notes modal shared styles; only the body-copy
     and checkbox rules are auto-ack-specific. */
  .auto-ack-body {
    margin: 0 0 0.8rem;
    font-size: 0.9rem;
    line-height: 1.55;
    color: var(--bone-1);
  }
  .auto-ack-check {
    display: flex;
    align-items: flex-start;
    gap: 0.55rem;
    margin: 0.8rem 0 0.6rem;
    padding: 0.65rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    cursor: pointer;
    font-size: 0.88rem;
    line-height: 1.5;
    color: var(--bone-1);
  }
  .auto-ack-check input[type="checkbox"] {
    margin-top: 0.12rem;
    accent-color: var(--accent);
  }
  .auto-ack-err {
    margin: 0.4rem 0 0;
    padding: 0.55rem 0.7rem;
    border-left: 2px solid var(--live);
    background: var(--live-soft);
    color: var(--bone-0);
    font-size: 0.82rem;
    border-radius: 6px;
  }
  .auto-ack-buttons {
    display: flex;
    gap: 0.5rem;
  }

  @media (max-width: 640px) {
    /* On narrow screens the slide-out would overflow the viewport;
       collapse to a full-width drawer below the rail item instead. */
    .auto-slideout {
      position: fixed;
      top: auto;
      left: 1rem;
      right: 1rem;
      bottom: 1rem;
      width: auto;
    }
  }
</style>
