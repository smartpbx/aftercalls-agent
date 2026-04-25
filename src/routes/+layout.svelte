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
  import Avatar from "$lib/Avatar.svelte";
  import ReportIssueDialog from "$lib/ReportIssueDialog.svelte";
  import RecordingFloatingControl from "$lib/RecordingFloatingControl.svelte";
  import ToastHost from "$lib/ToastHost.svelte";
  import "../app.css";

  let { children } = $props();

  type Me = {
    email: string;
    // #96: structured first/last ride alongside display_name. Optional
    // because older auth.json files may be missing them (serde
    // default on the Rust side); release-notes greeting prefers
    // first_name but falls back to a split of display_name.
    first_name?: string;
    last_name?: string;
    display_name: string;
    role: string;
    /// #86 — aftercalls-staff capability flag, orthogonal to role.
    /// Optional so older auth.json files deserialize cleanly; defaults
    /// to false when absent.
    is_platform_staff?: boolean;
    org_display_name: string;
  };

  let me = $state<Me | null>(null);
  let authResolved = $state(false);

  let recording = $state(false);
  let pipelineStage = $state("");
  // #142 · v0.4.5 — track which mode the in-flight recording is in
  // so the note-to-self overlay can surface only during a self-note
  // session. `"call"` defaults in a fallback where the backend's
  // event shape lacks the mode field.
  let recordingMode = $state<"call" | "self_note" | null>(null);
  // Live-elapsed label for the overlay. Populated on mount via
  // is_recording (so a reopened webview picks up an in-flight
  // session) and ticked every 500ms while the overlay is visible.
  let noteElapsedLabel = $state("0:00");
  let noteStartedAtMs = $state<number | null>(null);
  let noteTimerHandle: ReturnType<typeof setInterval> | null = null;
  let stoppingNote = $state(false);
  let noteError = $state<string | null>(null);
  let unlistenState: UnlistenFn | null = null;
  let unlistenPipeline: UnlistenFn | null = null;
  let unlistenTray: UnlistenFn | null = null;
  let unlistenUpdatePoll: (() => void) | null = null;
  let unlistenAutoDetect: UnlistenFn | null = null;

  // #142 · v0.4.5 — note-to-self overlay helpers. Kept close to the
  // overlay state declaration so additions/removals stay localised.
  function fmtElapsed(ms: number): string {
    const total = Math.max(0, Math.floor(ms / 1000));
    const m = Math.floor(total / 60);
    const s = total % 60;
    return `${m}:${String(s).padStart(2, "0")}`;
  }
  function startNoteTimer() {
    stopNoteTimer();
    noteTimerHandle = setInterval(() => {
      if (!noteStartedAtMs) return;
      noteElapsedLabel = fmtElapsed(Date.now() - noteStartedAtMs);
    }, 500);
  }
  function stopNoteTimer() {
    if (noteTimerHandle) {
      clearInterval(noteTimerHandle);
      noteTimerHandle = null;
    }
  }
  async function primeNoteOverlay() {
    try {
      const status = await invoke<{
        recording: boolean;
        started_at_ms: number | null;
      }>("is_recording");
      if (status?.started_at_ms) {
        noteStartedAtMs = status.started_at_ms;
        noteElapsedLabel = fmtElapsed(Date.now() - noteStartedAtMs);
      } else {
        noteStartedAtMs = Date.now();
        noteElapsedLabel = "0:00";
      }
    } catch {
      noteStartedAtMs = Date.now();
      noteElapsedLabel = "0:00";
    }
    startNoteTimer();
  }
  async function stopNoteToSelf() {
    if (stoppingNote) return;
    stoppingNote = true;
    noteError = null;
    try {
      await invoke<string>("stop_recording");
    } catch (e) {
      noteError = typeof e === "string" ? e : String(e);
      stoppingNote = false;
    }
  }
  function dismissNoteError() {
    noteError = null;
  }
  let noteOverlayLive = $derived(recording && recordingMode === "self_note");

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

  async function pollForUpdate(opts?: { userInitiated?: boolean }) {
    // Don't clobber an in-flight install — the Update object is the
    // one being downloaded right now and swapping it out would break
    // the progress callback. We DO still re-check in the
    // at-rest / error states so a newer version on the manifest
    // replaces a stale cached object (#58: user on 0.3.18 seeing a
    // "0.3.19 available" pill that never updates to 0.3.20).
    if (updateState === "downloading") return;
    // #79: gate the prompt while the agent is busy (recording or the
    // post-recording pipeline is still running). We do NOT touch
    // updateAvailable / linuxUpdateAvailable when deferring — if a
    // pill was already on screen from an earlier poll, it stays.
    // Manual "Check for updates" from the user menu bypasses via
    // userInitiated — the user asked, surface the answer.
    if (!opts?.userInitiated) {
      try {
        const busy = await invoke<boolean>("is_processing");
        if (busy) {
          updateCheckDeferred = true;
          return;
        }
      } catch (e) {
        // If the probe fails for any reason, fall through to the
        // normal poll rather than suppressing updates silently.
        console.warn("is_processing probe failed", e);
      }
    }
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
  // True when the current route is the Record page (`/`). Used to
  // suppress the layout-level auto-detect slide-out (#74 dup-toast),
  // since the Record page renders its own inline DETECTED banner and
  // we don't want both firing. We check BOTH pathname and route.id
  // so an early-launch paint where pathname is briefly empty (bare
  // tauri://localhost/ load before SvelteKit's client router has
  // normalized) still matches — route.id is `/` for the root page
  // from the first render.
  let isRecordPage = $derived(
    page.url.pathname === "/" || page.route?.id === "/",
  );

  // Auto-update state. Sits in the top strip as an unobtrusive nudge, then
  // flips into a progress row while downloading, then into a restart prompt.
  let updateAvailable = $state<Update | null>(null);
  let updateState = $state<"idle" | "downloading" | "ready" | "error">("idle");
  let updateError = $state("");
  let updateDownloaded = $state(0);
  let updateTotal = $state(0);
  let version = $state("");
  // #79: when an auto-poll lands mid-call / mid-pipeline, skip the
  // prompt and remember we owe the user a re-check. The pipeline /
  // recording-state listeners clear this and re-poll the moment work
  // finishes so latency is ~0s instead of up to 60min.
  let updateCheckDeferred = false;
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
  // Refs for initial focus so keyboard cycles start on the primary
  // dismiss button. The `.rn-modal` carries `overflow: hidden` with a
  // `.rn-body` scroll region, so without this the first Tab lands on
  // the scroll container itself and the footer link — neither is the
  // obvious "commit" target.
  let rnDismissBtn = $state<HTMLButtonElement | null>(null);
  let autoAckDismissBtn = $state<HTMLButtonElement | null>(null);
  $effect(() => {
    if (releaseNotes && rnDismissBtn) {
      rnDismissBtn.focus();
    }
  });
  $effect(() => {
    if (autoAckOpen && autoAckDismissBtn) {
      autoAckDismissBtn.focus();
    }
  });

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
    // they land in /staff/agent-logs alongside panics + pipeline failures.
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

    unlistenState = await listen<{
      recording: boolean;
      mode?: "call" | "self_note" | null;
    }>(
      "recording-state",
      (evt) => {
        recording = evt.payload.recording;
        // #142 · v0.4.5 — mode rides on the event so the overlay
        // only surfaces for self-notes. Missing field (fallback
        // from an old backend) → treat as "call".
        const mode = evt.payload.mode ?? "call";
        recordingMode = recording ? mode : null;
        if (recording && mode === "self_note") {
          void primeNoteOverlay();
        } else {
          stopNoteTimer();
          noteStartedAtMs = null;
          noteElapsedLabel = "0:00";
          stoppingNote = false;
        }
        // #79: a very short recording may stop before the pipeline
        // ever engages (auto-detect false start, cancelled session).
        // If we deferred a poll and no pipeline event comes along to
        // clear the flag, the stop transition does it here. If the
        // pipeline listener beats us to it the flag is already
        // cleared so this is a no-op.
        if (!recording && updateCheckDeferred) {
          updateCheckDeferred = false;
          pollForUpdate();
        }
      },
    );
    unlistenPipeline = await listen<{ stage: string }>("pipeline", (evt) => {
      pipelineStage = evt.payload.stage ?? "";
      if (pipelineStage === "done" || pipelineStage === "failed") {
        // #79: pipeline finished → if we deferred an update poll
        // while work was in flight, re-run it now so the pill
        // surfaces within ~1s of completion instead of waiting up
        // to 60min for the next hourly tick.
        if (updateCheckDeferred) {
          updateCheckDeferred = false;
          pollForUpdate();
        }
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
        // Belt-and-braces on top of the `isRecordPage` render guard
        // below (#74): when the user is already on the Record page,
        // don't even populate `autoPrompt`. The Record page's own
        // inline banner covers the prompt, its own listener plays
        // the chime, and keeping the state empty here means no
        // double-chime, no phantom slide-out if the render guard
        // ever misses a tick during initial hydration.
        if (isRecordPage) {
          autoPrompt = null;
          return;
        }
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
      // #96: prefer the structured first_name; fall back to splitting
      // display_name so a stale auth.json (pre-migration) still
      // yields a greeting.
      const firstName =
        me?.first_name ||
        (me?.display_name ?? "").split(/\s+/)[0] ||
        "";
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
    stopNoteTimer();
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
      // Play the start cue BEFORE invoking the recorder so the
      // system loopback doesn't capture the beep. Await blocks for
      // ~350ms when sounds are enabled; no-op when off. Failure
      // here is swallowed — we'd rather start the recording than
      // miss it because of a flaky audio subsystem.
      try {
        await playStartCueIfEnabled();
      } catch (e) {
        console.warn("start cue failed", e);
      }
      await invoke("confirm_auto_start");
      autoPrompt = null;
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

  type NavItem = {
    href: string;
    label: string;
    match: (p: string) => boolean;
    icon: string;
    /// When true, clicking opens the portal in the default browser
    /// instead of navigating inside the agent webview. Agent has no
    /// local /admin or /staff routes — those surfaces live on
    /// app.aftercalls.io.
    external?: boolean;
  };
  type NavSection = {
    id: "workspace" | "admin" | "staff";
    label: string;
    items: NavItem[];
  };

  // #86 — three-section rail: WORKSPACE / ADMIN / STAFF. See ui.md.
  // Back-compat branch on `superadmin` is dropped in the CHECK-
  // constraint-flip commit (already flipped in the backend by step 1,
  // but the claim can still say superadmin for ≤1h after deploy).
  const isAdmin = $derived(
    !!me &&
      (me.role === "admin" ||
        me.role === "owner" ||
        (me.role as string) === "superadmin"),
  );
  const isStaff = $derived(
    !!me &&
      ((me.is_platform_staff === true) ||
        (me.role as string) === "superadmin"),
  );

  // Icons — simple inline SVGs, 16px stroke vocabulary.
  const ICON_RECORD = `<svg viewBox="0 0 20 20" width="16" height="16"><path d="M10 2a3 3 0 0 0-3 3v5a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" fill="currentColor"/><path d="M5 9v1a5 5 0 0 0 10 0V9M10 15v3M7 18h6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/></svg>`;
  const ICON_CALLS = `<svg viewBox="0 0 20 20" width="16" height="16"><path d="M3 5h14M3 10h14M3 15h14" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>`;
  const ICON_ACTIONS = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="10" cy="10" r="7"/><path d="m7 10 2 2 4-4.5"/></svg>`;
  const ICON_TEAM = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="7" cy="8" r="3"/><circle cx="14" cy="9" r="2.2"/><path d="M2.5 17c.8-2.6 2.7-4 4.5-4s3.7 1.4 4.5 4M12 17c.6-1.8 1.8-3 3-3s2.4 1.2 3 3"/></svg>`;
  const ICON_ORG = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="14" height="13" rx="1.5"/><path d="M7 4v13M13 4v13M3 8h14M3 12h14"/></svg>`;
  const ICON_VOCAB = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4 4h9a3 3 0 0 1 3 3v10H6a2 2 0 0 1-2-2V4Z"/><path d="M4 15h12"/></svg>`;
  // #186 CRM (Zoho) icon — share / external-link glyph evokes the
  // outbound nature of the integration. Same 16×16 stroke shape as
  // the other admin icons.
  const ICON_CRM = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="6" r="2"/><circle cx="14" cy="6" r="2"/><circle cx="10" cy="14" r="2"/><path d="M7.5 7.3l1 5.4M12.5 7.3l-1 5.4M8 6h4"/></svg>`;
  const ICON_TERMS = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M5 3h8l3 3v11H5z"/><path d="M13 3v3h3M7.5 10h5M7.5 13h5"/></svg>`;
  const ICON_LOGS = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4 4h12v12H4z"/><path d="M7 8h6M7 11h6M7 14h4"/></svg>`;

  const navSections = $derived.by<NavSection[]>(() => {
    if (!me) return [];
    const sections: NavSection[] = [];
    sections.push({
      id: "workspace",
      label: "Workspace",
      items: [
        {
          href: "/",
          label: "Record",
          match: (p) => p === "/",
          icon: ICON_RECORD,
        },
        {
          href: "/calls",
          label: "Calls",
          match: (p) => p.startsWith("/calls"),
          icon: ICON_CALLS,
        },
        // v0.4.0 Phase 4 (#105): /actions portfolio view.
        {
          href: "/actions",
          label: "Action items",
          match: (p) => p.startsWith("/actions"),
          icon: ICON_ACTIONS,
        },
        // Settings stays in the user-menu dropdown — account-level
        // knobs per #33.
      ],
    });
    if (isAdmin) {
      // ADMIN surfaces live on the portal; agent has no local routes.
      // External=true routes clicks through openPortalLink().
      const nope = (_p: string) => false;
      sections.push({
        id: "admin",
        label: "Admin",
        items: [
          { href: "/admin/users", label: "Team", match: nope, icon: ICON_TEAM, external: true },
          { href: "/admin", label: "Org", match: nope, icon: ICON_ORG, external: true },
          { href: "/admin/vocab", label: "Vocab", match: nope, icon: ICON_VOCAB, external: true },
          // #186: CRM (Zoho) connect page — admin opens it in the
          // browser via openPortalLink. The agent has no local
          // routes for admin surfaces.
          { href: "/admin/zoho", label: "CRM", match: nope, icon: ICON_CRM, external: true },
        ],
      });
    }
    if (isStaff) {
      const nope = (_p: string) => false;
      sections.push({
        id: "staff",
        label: "Staff",
        items: [
          { href: "/staff/tos", label: "Terms", match: nope, icon: ICON_TERMS, external: true },
          { href: "/staff/agent-logs", label: "Agent logs", match: nope, icon: ICON_LOGS, external: true },
        ],
      });
    }
    return sections;
  });

  // ── User menu (rail foot) ────────────────────────────────────────
  let userMenuOpen = $state(false);
  // #183 — Report-issue dialog state. Mounts lazily on first open and
  // stays mounted thereafter (state survives across opens; cheaper
  // than a fresh component every time).
  let reportDialogOpen = $state(false);
  function toggleUserMenu() {
    userMenuOpen = !userMenuOpen;
  }
  function closeUserMenu() {
    userMenuOpen = false;
  }
  function openReportDialog() {
    closeUserMenu();
    reportDialogOpen = true;
  }
  function closeReportDialog() {
    reportDialogOpen = false;
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
  // #34 — Open web app, but logged-in. The agent already authed this
  // user against the backend; calling `mint_handoff_url` gets a
  // single-use, 60s URL on the portal's `/handoff` page that exchanges
  // the token for a normal access+refresh bundle and lands on /calls.
  // No second password prompt for the system browser.
  //
  // Falls back to the un-authed link on any error so the worst case is
  // "the user lands on /login" rather than "the menu item silently
  // does nothing." Common failure modes: backend unreachable (offline
  // recording), refresh token expired (user reopens after a long idle).
  async function openWebApp() {
    closeUserMenu();
    try {
      const url = await invoke<string>("mint_handoff_url");
      await openUrl(url);
    } catch (e) {
      console.warn("mint_handoff_url failed; falling back to /calls", e);
      try {
        await openUrl("https://app.aftercalls.io/calls");
      } catch (e2) {
        console.warn("openUrl fallback failed", e2);
      }
    }
  }
  async function manualUpdateCheck() {
    closeUserMenu();
    // #79: explicit user action bypasses the processing gate. If
    // they asked, surface the answer even mid-call.
    await pollForUpdate({ userInitiated: true });
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
    if (p.startsWith("/actions")) return "Action items";
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

    <div class="rail-scroll">
      {#each navSections as sec (sec.id)}
        <section class="rail-section" aria-labelledby={`nav-sec-${sec.id}`}>
          {#if navSections.length > 1}
            <h2 id={`nav-sec-${sec.id}`} class="rail-section-head">
              {sec.label}
            </h2>
          {/if}
          <nav aria-labelledby={`nav-sec-${sec.id}`}>
            {#each sec.items as it (it.href)}
              {@const active = it.match(page.url.pathname)}
              <div class="nav-row" class:has-slideout={it.href === "/" && autoPrompt}>
                {#if it.external}
                  <button
                    type="button"
                    class="nav-item"
                    onclick={() => openPortalLink(it.href)}
                  >
                    <span class="glyph">{@html it.icon}</span>
                    <span class="label">{it.label}</span>
                    <span class="ext" aria-hidden="true">↗</span>
                  </button>
                {:else}
                  <a
                    href={it.href}
                    class="nav-item"
                    class:active
                    aria-current={active ? "page" : undefined}
                  >
                    <span class="glyph">{@html it.icon}</span>
                    <span class="label">{it.label}</span>
                  </a>
                {/if}
                {#if it.href === "/" && autoPrompt && !isRecordPage}
            <!-- Auto-detect slide-out (#59, #60, #74). Shown only when
                 the user is on a route OTHER than the Record page —
                 the Record page has its own inline DETECTED banner.
                 Anchored to the Record nav item so it's visible
                 whether the user is on /calls, /settings, etc. No
                 focus-steal, no route change. Using the derived
                 `isRecordPage` (pathname OR route.id) so an early-
                 launch paint where pathname is empty before SvelteKit
                 client-route normalization can't show a phantom
                 slide-out (#74). -->
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
        </section>
      {/each}
    </div>

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
          <span class="who-avatar">
            <Avatar name={me.display_name} size={28} />
          </span>
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
            onclick={openWebApp}
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
          <!-- #183 — Report an issue. Opens an in-agent modal; not
               an external URL, so no `um-ext` arrow. Sits between
               Help (also support-leaning) and the separator that
               divides per-account actions from the maintenance row
               below. -->
          <button
            class="um-item"
            role="menuitem"
            onclick={openReportDialog}
          >
            Report an issue
          </button>
          <!-- #86 — ADMIN + STAFF items moved from the user menu to
               the rail's ADMIN / STAFF sections so the same identity
               sees the same rail on agent + portal. User menu shrinks
               back to per-account actions. -->
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
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <!-- The Windows-only dblclick mirrors the native title-bar
         maximize gesture. Keyboard + assistive-tech users have
         an accessible alternative: the explicit Maximize/Restore
         button rendered below in .win-controls (with aria-label).
         Adding a role here would mis-classify the topstrip; this
         is window chrome, not interactive content. -->
    <header
      class="topstrip"
      class:has-win-controls={isWindows}
      data-tauri-drag-region
      ondblclick={isWindows ? toggleMaximize : undefined}
    >
      <div class="crumbs" data-tauri-drag-region>
        <span class="crumb" data-tauri-drag-region>{pageTitle}</span>
      </div>

      <div class="strip-right" data-tauri-drag-region>
        {#if orphans.length > 0}
          <!-- Orphan recovery pill (#63). Same shape as the update
               pill: a thin rounded chip with status pip + label +
               primary + dismiss action. Sig-yellow pip so it reads
               as an interrupt without the warning-red weight. -->
          <div class="update" data-tauri-drag-region>
            <span class="pip sig" data-tauri-drag-region></span>
            <span
              class="update-label"
              data-tauri-drag-region
              title={`${orphans.length} unfinished call${orphans.length === 1 ? "" : "s"} from before the last restart`}
            >
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
          <div class="update" data-tauri-drag-region>
            {#if updateState === "downloading"}
              <span class="pip working" data-tauri-drag-region></span>
              <span
                class="update-label"
                data-tauri-drag-region
                title={`Updating to v${updateAvailable.version}…${updateTotal > 0 ? ` ${Math.min(100, Math.round((updateDownloaded / updateTotal) * 100))}%` : ""}`}
              >
                Updating to v{updateAvailable.version}…
                {#if updateTotal > 0}
                  {Math.min(100, Math.round((updateDownloaded / updateTotal) * 100))}%
                {/if}
              </span>
            {:else if updateState === "ready"}
              <span class="pip done" data-tauri-drag-region></span>
              <span class="update-label" data-tauri-drag-region title="Restarting…">Restarting…</span>
            {:else if updateState === "error"}
              <span class="pip failed" data-tauri-drag-region></span>
              <span class="update-label" data-tauri-drag-region title={updateError}>Update failed</span>
              <button class="update-dismiss" onclick={dismissUpdate}>Dismiss</button>
            {:else}
              <span class="pip sig" data-tauri-drag-region></span>
              <span
                class="update-label"
                data-tauri-drag-region
                title={`v${updateAvailable.version} available`}
              >
                v{updateAvailable.version} available
              </span>
              <button class="update-install" onclick={installUpdate}>Install</button>
              <button class="update-dismiss" onclick={dismissUpdate}>Later</button>
            {/if}
          </div>
        {:else if linuxUpdateAvailable}
          <div class="update" data-tauri-drag-region>
            <span class="pip sig" data-tauri-drag-region></span>
            <span
              class="update-label"
              data-tauri-drag-region
              title={`v${linuxUpdateAvailable} available`}
            >
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

        <div class="indicator" data-tauri-drag-region>
          {#if recording && pipelineStage && pipelineStage !== "done"}
            <!-- Back-to-back case: user's recording a new call while the
                 previous one is still processing. Keep both visible so
                 the pipeline progress isn't lost behind the live pill. -->
            <span class="pip live" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>Recording</span>
            <span class="ind-sep" data-tauri-drag-region>·</span>
            <span class="pip {pipelineStage}" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>{stageLabel[pipelineStage] ?? pipelineStage}</span>
          {:else if recording}
            <span class="pip live" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>Recording</span>
          {:else if pipelineStage && pipelineStage !== "done"}
            <span class="pip {pipelineStage}" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>{stageLabel[pipelineStage] ?? pipelineStage}</span>
          {:else if pipelineStage === "done"}
            <span class="pip done" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>Saved</span>
          {:else}
            <span class="pip idle" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>Idle</span>
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

<!-- #142 · v0.4.5 — note-to-self overlay. Modeless bottom-right
     (bottom-centre on narrow viewports). Reshapes to an error
     surface when `noteError` is set. Only renders during an
     active self-note session. -->
{#if noteOverlayLive && !noteError}
  <div
    class="note-overlay"
    role="dialog"
    aria-live="polite"
    aria-label="Recording note to self"
  >
    <div class="note-overlay-head">
      <span class="note-pip" aria-hidden="true"></span>
      <span class="note-title">Recording note</span>
      <span class="note-elapsed" aria-hidden="true">{noteElapsedLabel}</span>
    </div>
    <div class="note-overlay-actions">
      <button
        type="button"
        class="note-stop"
        onclick={stopNoteToSelf}
        disabled={stoppingNote}
      >
        {stoppingNote ? "Stopping…" : "Stop"}
      </button>
    </div>
  </div>
{/if}
{#if noteError}
  <div class="note-overlay note-overlay-error" role="alert">
    <div class="note-overlay-head">
      <span class="note-pip note-pip-failed" aria-hidden="true"></span>
      <span class="note-title">Note not recorded</span>
    </div>
    <p class="note-error-body">{noteError}</p>
    <div class="note-overlay-actions">
      <button
        type="button"
        class="note-retry"
        onclick={async () => {
          noteError = null;
          try {
            await invoke<string>("start_self_note");
          } catch (e) {
            noteError = typeof e === "string" ? e : String(e);
          }
        }}
      >
        Try again
      </button>
      <button type="button" class="note-dismiss" onclick={dismissNoteError}>
        Dismiss
      </button>
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
      onkeydown={(e) => {
        // Esc must bubble out so the modal dismisses even when focus
        // is trapped inside the scroll container or on the dismiss
        // button; previously the plain stopPropagation() here silently
        // swallowed it. All other keys stay contained so they don't
        // reach the rail's route-switch / shortcut handlers underneath.
        if (e.key === "Escape") { dismissReleaseNotes(); return; }
        e.stopPropagation();
      }}
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
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <div class="rn-body" tabindex="0" role="region" aria-label="Release notes">
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
      </div>
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
        <button class="rn-dismiss" bind:this={rnDismissBtn} onclick={dismissReleaseNotes}>
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
      onkeydown={(e) => {
        // Esc cancels the ack prompt (equivalent to clicking backdrop
        // or the Cancel button). All other keys stay contained so the
        // checkbox space-toggle doesn't leak into rail shortcuts.
        if (e.key === "Escape") { cancelAutoAck(); return; }
        e.stopPropagation();
      }}
      tabindex="-1"
    >
      <div class="rn-head">
        <h2 id="auto-ack-title">Before you record</h2>
      </div>
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <div class="rn-body" tabindex="0" role="region" aria-label="Recording acknowledgment">
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
      </div>
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
            bind:this={autoAckDismissBtn}
            onclick={submitAutoAck}
            disabled={!autoAckChecked || autoAckSubmitting}
          >{autoAckSubmitting ? "Saving…" : "I understand — start recording"}</button>
        </div>
      </div>
    </div>
  </div>
{/if}

<!-- #183 — Report-issue dialog. Mounted at the layout root (not
     inside `.shell`) so it correctly overlays everything including
     the rail. Only mounts when open; teardown happens via the
     dialog's onClose. -->
{#if reportDialogOpen}
  <ReportIssueDialog onClose={closeReportDialog} />
{/if}

<!-- #119 — Toast / transient-notification host. Mounted once at the
     layout root so notices surface above every page (including the
     auth + welcome bare layouts). The host renders nothing until the
     store has live items. -->
<ToastHost />

<!-- #216 — Persistent screen-recording control. Mounted at the
     layout root so it stays visible while the report dialog is
     hidden. Renders nothing while no recording is in flight. -->
<RecordingFloatingControl />

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
    grid-template-columns: auto 1fr auto;
    grid-template-rows: auto auto;
    align-items: center;
    gap: 0.1rem 0.55rem;
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
  .who-btn .who-avatar {
    grid-row: 1 / span 2;
    grid-column: 1;
    align-self: center;
    display: inline-flex;
  }
  .who-btn .who-chevron {
    grid-row: 1 / span 2;
    grid-column: 3;
    align-self: center;
    color: var(--bone-4);
    font-size: 0.72rem;
  }

  .who-name {
    grid-column: 2;
    grid-row: 1;
    font-size: 0.82rem;
    color: var(--bone-1);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }

  .who-org {
    grid-column: 2;
    grid-row: 2;
    font-size: 0.7rem;
    color: var(--bone-3);
    letter-spacing: 0.02em;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
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
    /* Was `height: var(--topbar-h)`; when `.strip-right` wraps on narrow
       windows the wrapped row needs vertical room, so use min-height to
       let the strip grow. The 40 px floor is preserved for the common
       no-wrap case (closes #88). */
    min-height: var(--topbar-h);
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
       edge-to-edge like a native titlebar. The 138px right-side
       reservation (3 × 46px buttons, see .wc-btn width) keeps the
       pills + indicator from rendering underneath the absolute-
       positioned .win-controls (see block below). Windows-only —
       non-Windows topstrip has no .has-win-controls class and keeps
       its normal `padding: 0 1.5rem` + inline layout. */
    padding: 0 calc(46px * 3) 0 1.5rem;
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
    /* #108 — slightly wider inter-chip gap + more row-gap when the
       strip wraps on narrow windows, so the wrapped pill clears the
       one above it with air instead of butting its bottom edge. */
    gap: 1rem;
    /* When both the orphan-recovery pill and the update pill fire
       at once, the combined width overflows the topstrip on narrow
       windows. flex-wrap lets the second pill drop to a new row so
       neither gets truncated or spills past the window-control
       buttons on the right. */
    flex-wrap: wrap;
    justify-content: flex-end;
    row-gap: 0.55rem;
    /* Allow the strip to grow vertically when wrapping; the
       container is sticky-positioned so this just means the
       content underneath pushes down by one pill's height. */
    min-height: var(--topbar-h);
    /* Let this flex item shrink below its content's intrinsic
       width so pill children (with their own min-width:0 +
       truncation) can ellipsize cleanly on narrow windows (#88). */
    min-width: 0;
    flex: 1 1 auto;
  }

  .update {
    display: flex;
    align-items: center;
    /* #108 — slightly more breathing room between the pip, the
       label, and the install/dismiss buttons. Paired with the
       .update .pip + .update-label negative-margin rule below
       so the pip-to-label distance stays tight. */
    gap: 0.55rem;
    padding: 0.28rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-1);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    letter-spacing: 0.02em;
    color: var(--bone-1);
    /* Let the pill shrink in its flex parent so long labels ellipsize
       instead of forcing the pill to max-content width (#88). */
    min-width: 0;
    max-width: 100%;
  }
  /* #108 — pull the update-pill label tight to its pip so the two
     read as a paired chrome unit. Net distance between pip and
     label comes out ~0.3rem (0.55rem gap − 0.25rem margin). */
  .update .pip + .update-label {
    margin-left: -0.25rem;
  }

  .update-label {
    color: var(--bone-1);
    /* Truncate instead of forcing the pill to its intrinsic width.
       The parent <span title="…"> carries the full text for tooltip
       + screen-reader access on overflow (#88). */
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
    /* #108 — slightly wider gap between clusters (pip+label pairs
       and the separator dot between them) so the two logical
       groups ("Recording · Processing") read as distinct units.
       The pip-to-label tight pair inside each cluster is handled
       by the negative-margin rule below. */
    gap: 0.65rem;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    letter-spacing: 0.04em;
    color: var(--bone-2);
  }
  /* #108 — pair the pip with its immediate label tight (net
     distance ~0.35rem). Without this, the uniform 0.65rem flex-gap
     makes the pip look unattached from its label. */
  .indicator .pip + .ind-label {
    margin-left: -0.3rem;
  }
  /* #108 — give the "·" separator explicit styling so it reads as
     a separator, not as part of the adjacent label. Dimmer than
     the labels (bone-3 at 0.7 opacity) with minimal horizontal
     breathing room. */
  .indicator .ind-sep {
    margin: 0 0.1rem;
    color: var(--bone-3);
    opacity: 0.7;
  }

  /* ── Windows custom titlebar controls ──────────────────────────── */
  /* Scoped under .topstrip.has-win-controls so the absolute pin only
     applies when the Windows titlebar branch is mounted. Non-Windows
     never has the .has-win-controls class (nor a .win-controls element),
     so its topstrip layout is unaffected (#88). The 40 px topstrip
     is position:sticky which establishes a containing block; the
     buttons anchor to its top-right corner and sit out of the
     .strip-right flex-wrap container entirely, so they stay pinned
     even when the pills wrap to a second row. */
  .topstrip.has-win-controls .win-controls {
    position: absolute;
    top: 0;
    right: 0;
    z-index: 1;
  }
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

  /* ── Note-to-self overlay (#142 / v0.4.5) ────────────────────────
     Modeless live-capture widget on the agent. Fixed bottom-right
     (bottom-centre on narrow viewports), `--ink-1` surface, same
     shadow vocabulary as the user menu + update pill. z-index 50 —
     above the chip-menu popover (30) but the overlay doesn't trap
     focus so it doesn't compete semantically. */
  .note-overlay {
    position: fixed;
    right: 1.5rem;
    bottom: 1.5rem;
    z-index: 50;
    min-width: 240px;
    max-width: 320px;
    padding: 0.75rem 0.9rem;
    background: var(--ink-1);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius);
    box-shadow:
      0 18px 36px -12px rgba(0, 0, 0, 0.55),
      0 2px 6px -2px rgba(0, 0, 0, 0.35);
    font-size: 0.85rem;
    color: var(--bone-1);
    animation: note-overlay-in 180ms ease-out both;
  }
  @media (max-width: 520px) {
    .note-overlay {
      left: 50%;
      right: auto;
      bottom: 1rem;
      transform: translateX(-50%);
      max-width: calc(100vw - 2rem);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .note-overlay {
      animation: none;
    }
  }
  @keyframes note-overlay-in {
    from {
      opacity: 0;
      transform: translateY(8px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  .note-overlay-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.45rem;
  }
  .note-pip {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--live);
    animation: note-pip-pulse 1.2s ease-in-out infinite;
  }
  @media (prefers-reduced-motion: reduce) {
    .note-pip {
      animation: none;
    }
  }
  @keyframes note-pip-pulse {
    0%, 100% {
      opacity: 1;
      box-shadow: 0 0 0 0 var(--live-soft);
    }
    50% {
      opacity: 0.85;
      box-shadow: 0 0 0 4px transparent;
    }
  }
  .note-pip-failed {
    animation: none;
  }
  .note-title {
    font-weight: 600;
    color: var(--bone-0);
    letter-spacing: -0.01em;
    flex: 1 1 auto;
  }
  .note-elapsed {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--bone-1);
    font-variant-numeric: tabular-nums;
  }
  .note-overlay-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.4rem;
  }
  .note-stop {
    padding: 0.38rem 0.9rem;
    border: 1px solid var(--live);
    background: var(--live-soft);
    color: var(--live);
    border-radius: 6px;
    font: inherit;
    font-size: 0.82rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 120ms linear, color 120ms linear;
  }
  .note-stop:hover:not(:disabled) {
    background: var(--live);
    color: var(--ink-0);
  }
  .note-stop:disabled {
    opacity: 0.65;
    cursor: not-allowed;
  }
  .note-stop:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .note-overlay-error {
    border-color: var(--live-soft);
  }
  .note-error-body {
    margin: 0 0 0.6rem;
    font-size: 0.82rem;
    color: var(--bone-1);
  }
  .note-retry {
    padding: 0.38rem 0.9rem;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    border-radius: 6px;
    font: inherit;
    font-size: 0.82rem;
    font-weight: 600;
    cursor: pointer;
  }
  .note-retry:hover {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .note-dismiss {
    padding: 0.38rem 0.9rem;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--bone-2);
    border-radius: 6px;
    font: inherit;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .note-dismiss:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
</style>
