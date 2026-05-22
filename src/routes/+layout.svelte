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
  import { onDestroy, onMount, tick } from "svelte";
  import { notifyAutoDetect } from "$lib/notify";
  import { detectPlatform, playStartCueIfEnabled } from "$lib/compliance";
  import { confirmAutoStart } from "$lib/autoStart";
  import Avatar from "@aftercalls/shared/ui/Avatar.svelte";
  import OfflineBanner from "@aftercalls/shared/ui/OfflineBanner.svelte";
  import ReportIssueDialog from "$lib/ReportIssueDialog.svelte";
  import RecordingFloatingControl from "$lib/RecordingFloatingControl.svelte";
  import ToastHost from "@aftercalls/shared/ui/ToastHost.svelte";
  import ShortcutsOverlay from "@aftercalls/shared/ui/ShortcutsOverlay.svelte";
  import CommandPalette from "@aftercalls/shared/ui/CommandPalette.svelte";
  import AgentOnboardingDeck from "$lib/AgentOnboardingDeck.svelte";
  import { initSentry } from "$lib/sentry";
  import { getAppPrefs } from "$lib/stores/appPrefs.svelte";
  import {
    initShortcutListener,
    openCommandPalette,
    registerShortcuts,
    toggleHelp,
  } from "@aftercalls/shared/shortcuts";
  import { createAgentCommandPaletteSections } from "$lib/commandPalette";
  import { onlineStatus } from "$lib/stores/onlineStatus.svelte";
  import { saveQueue } from "$lib/stores/saveQueue.svelte";
  import { callRecording } from "$lib/stores/callRecording.svelte";
  import { autoRecordStore } from "$lib/stores/autoRecord.svelte";
  import { autoRecord, type AutoRecordPendingPayload } from "$lib/api";
  import { toast } from "@aftercalls/shared/stores/toast.svelte";
  import { portalErrorToText } from "$lib/portalError";
  import "../app.css";

  let { children } = $props();

  type PendingTos = {
    id: string;
    kind: "terms" | "privacy" | string;
    slug: string;
    effective_at: string;
  };

  type Me = {
    user_id: string;
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
    /// #215 — per-org feature flags. Optional so older auth.json
    /// files deserialize cleanly; missing keys default to false (the
    /// gate hides Zoho UI on those, matching the per-org-disabled
    /// state). Backend re-checks via 404 — this is purely UX.
    features?: { zoho?: boolean };
    /// #320 — outstanding ToS / privacy versions. The layout routes
    /// the user to /accept-terms when this is non-empty so they can't
    /// reach any recording surface without confirming the legal
    /// gates. Optional so older auth.json files (written before this
    /// field landed on AuthFile) decode cleanly; missing → empty
    /// → no gate, which matches the portal's defensive read.
    pending_tos?: PendingTos[];
  };

  let me = $state<Me | null>(null);
  let authResolved = $state(false);

  // #634 — unread-call count for the sidebar Calls pill + tray
  // tooltip. Polled every 60s from /v1/auth/me (same pattern the
  // portal uses for the Support staff badge); also bumped manually
  // when a user opens a call (the detail page dispatches an
  // `unread-count-changed` event to avoid the next 60s lag) or when
  // the user clicks "Mark read" / "Mark all read" on /calls (those
  // pages dispatch the same event after their own optimistic
  // updates). Persists at the layout level so SvelteKit client
  // navigation between /calls and /calls/[id] doesn't reset it.
  let unreadCallsCount = $state(0);
  let unreadPollTimer: ReturnType<typeof setInterval> | null = null;
  const UNREAD_POLL_MS = 60_000;
  // Last value pushed to the Tauri tray. Avoids a needless
  // `set_unread_badge` IPC every poll tick when the count hasn't
  // moved (most calls). `-1` is the "never pushed" sentinel; first
  // tick always lands.
  let lastTrayBadgePushed = -1;

  let recording = $state(false);
  let pipelineStage = $state("");
  // #646 Layer A/C — true when an in-process retry (Layer A attempt
  // 2–4) is currently waiting between attempts, OR when Layer C's
  // sweeper is re-driving the pipeline from an orphan. Drives the
  // topstrip label swap from the stage name (e.g. "Transcribing")
  // to "Retrying…" without changing the pip — that stays
  // `.pip.working` per ui.md §Topstrip indicator. Cleared back to
  // false when the next stage transition arrives or the retry
  // ladder bubbles a terminal failure. We accept three wire shapes
  // from the Rust side (any one suffices to flip the flag):
  //   1. `pipeline` event with an explicit `retrying: true` field;
  //   2. `pipeline` event with `stage === "retrying"`;
  //   3. a dedicated `pipeline-retry` event with `{ active: bool }`.
  // The pipeline listener below treats all three as equivalent so
  // the Rust builder doesn't have to commit to one shape before this
  // ships.
  let pipelineRetrying = $state(false);
  // #142 · v0.4.5 — track which mode the in-flight recording is in
  // so the note-to-self overlay can surface only during a self-note
  // session. `"call"` defaults in a fallback where the backend's
  // event shape lacks the mode field.
  // #327: still tracked at the layout level for the existing
  // top-strip indicator + auto-detect chime gating; the floater reads
  // the same value off the `callRecording` store, which the listeners
  // below seed.
  let recordingMode = $state<"call" | "self_note" | null>(null);
  // #327 — call-recording floater fade-out timer. Held at the layout
  // level so consecutive recordings (back-to-back start) cancel a
  // pending fade from the previous session before the floater
  // collapses on the new live state.
  let callFloaterDoneTimer: ReturnType<typeof setTimeout> | null = null;
  let unlistenState: UnlistenFn | null = null;
  let unlistenPipeline: UnlistenFn | null = null;
  let unlistenTray: UnlistenFn | null = null;
  let unlistenUpdatePoll: (() => void) | null = null;
  let unlistenAutoDetect: UnlistenFn | null = null;
  // #313 — close-window advisory listener. Wired in onMount; receives
  // the Rust-side `close-advisory` emit when a user with
  // close_to_tray=false clicks X (and isn't busy, so the window
  // really closes). Writes a localStorage flag the next launch
  // reads to render the one-time tray-explainer note.
  let unlistenCloseAdvisory: UnlistenFn | null = null;
  // #313 — render-flag for the one-time advisory note. Read from
  // localStorage on mount: when `pending=1` AND `seen` isn't set yet,
  // we surface the note in the rail / page. Dismiss flips `seen=1`
  // permanently and clears `pending`. The two-flag dance keeps the
  // note from appearing on every launch — only the launch following a
  // close-without-tray.
  let closeAdvisoryVisible = $state(false);
  const CLOSE_ADVISORY_PENDING_KEY = "aftercalls:close-advisory-pending";
  const CLOSE_ADVISORY_SEEN_KEY = "aftercalls:close-advisory-seen";
  // #89 — separate listener for the OS-native actionable notification
  // (mic-detect "Record this call?" toast). Wired alongside the
  // existing auto-detect slide-out so both surfaces drive the same
  // autoRecordClick / autoDismiss handlers — no parallel state.
  let unlistenNotifyAction: UnlistenFn | null = null;
  // #596 — auto-record event listeners. Three from the agent
  // (`auto-record-pending`, `auto-record-fired`, `auto-record-cancelled`)
  // drive the 5s cancel toast; one (`observed-apps-updated`) bumps the
  // settings store so a freshly-observed app appears in the list
  // without the user manually navigating away and back.
  let unlistenAutoRecordPending: UnlistenFn | null = null;
  let unlistenAutoRecordFired: UnlistenFn | null = null;
  let unlistenAutoRecordCancelled: UnlistenFn | null = null;
  let unlistenObservedAppsUpdated: UnlistenFn | null = null;
  // #646 Layer A/C — optional separate retry event listener. Wired
  // only if the Rust side emits a dedicated `pipeline-retry` channel
  // (vs. reusing the existing `pipeline` channel with a `retrying`
  // flag). Either path flips `pipelineRetrying` the same way.
  let unlistenPipelineRetry: UnlistenFn | null = null;
  // #646 Layer C — `visibilitychange` handler so the sweeper can stay
  // paused while the user has the window hidden (battery + noise
  // mitigation, plus avoids polling the backend for an agent the
  // user isn't actively touching).
  let onVisibilityChange: (() => void) | null = null;
  let pipelineReadyNotifiedFor = "";
  // Map of pending_id → toast id so the matching `auto-record-fired` /
  // `auto-record-cancelled` event can dismiss the in-flight toast
  // instead of letting the 5s auto-dismiss tick down on a recording
  // that's already running.
  const autoRecordToasts = new Map<string, number>();
  // Reverse lookup: bundle_id → pending_id, so the `fired` /
  // `cancelled` payloads (which only carry bundle_id) can find their
  // matching toast.
  const autoRecordPendingByBundle = new Map<string, string>();

  // #327 — call-recording floater bridge. The floater reads its own
  // `callRecording` store; this layout owns the `recording-state` +
  // `pipeline` event listeners (single source of truth for those
  // events on the agent), so it pushes into the store from those
  // handlers below. The store also carries a `stopRequestId` that
  // bumps when the user clicks Stop on the floater; an effect here
  // observes the bump and invokes `stop_recording` so the floater
  // stays decoupled from the Tauri command vocabulary.
  let lastStopRequestId = 0;
  $effect(() => {
    const id = callRecording.stopRequestId;
    if (id !== lastStopRequestId) {
      lastStopRequestId = id;
      if (callRecording.state === "recording") {
        // The recording-state listener below transitions the store to
        // "stopping" once the backend acks the stop. We optimistically
        // do it here so the Stop button reads "Wrapping up…"
        // immediately even before the IPC roundtrip completes.
        callRecording.markStopping();
        invoke<string>("stop_recording").catch((e) => {
          // If the backend stop fails (e.g. recorder already torn
          // down), surface as a failed pipeline so the user sees the
          // floater's retry pointer. The actual error text is
          // sanitized by portalErrorToText.
          callRecording.setError(portalErrorToText(e));
          callRecording.notePipelineStage("failed");
        });
      }
    }
  });

  // Auto-detect slide-out state (#59). Both `prompt_start` and
  // `prompt_end` surface here so the prompt is reachable from any
  // route, not just /record — a user who started a call from /record
  // and then navigated to /calls used to miss the idle-mic prompt
  // entirely (the +page banner only renders while /record is mounted).
  // The slide-out anchors to the rail's Record item; the Record page
  // suppresses both copies via `isRecordPage` to avoid duplicate UI.
  let autoPrompt = $state<{ app: string } | null>(null);
  let autoEndPrompt = $state<{ app: string } | null>(null);

  // Orphan-session recovery (#63 + #646 Layer D). One-shot check at
  // startup for session_dirs left behind by a crashed / force-quit
  // previous session. Non-blocking pill in the top-strip, expandable
  // via "Review…" into a per-session list. Auto-clean for dirs older
  // than 7 days happens Rust-side inside list_orphan_sessions.
  //
  // #646 Layer D extends each row with two fields the Rust side now
  // ships:
  //   - `kind` discriminates "never_created" (session_dir exists but
  //     the backend never saw a call row — typical of an upload that
  //     died before create_call) from "stuck_pipeline" (backend has a
  //     call row stuck mid-pipeline, e.g. transcribe never completed).
  //     The UI renders distinct badges so the user can tell which
  //     class they're looking at.
  //   - `auto_resume_count` is the number of silent Layer C resume
  //     attempts the agent has already made for this session. When it
  //     hits the 3-attempt ceiling, this row escaped auto-recovery
  //     and surfaces a "Couldn't resume automatically." line plus the
  //     orphan-pill's pip flips from sig to live red.
  type OrphanKind = "never_created" | "stuck_pipeline";
  type OrphanSession = {
    session_id: string;
    recorded_at: string;
    age_minutes: number;
    kind?: OrphanKind;
    auto_resume_count?: number;
  };
  // #646 Layer C — return shape from the `auto_resume_orphans` IPC.
  // Surface-only field — the UI never renders this; the side effect
  // (pipelines re-running) is what matters. Logged at debug level
  // for development visibility.
  type AutoResumeResult = {
    session_id: string;
    resumed: boolean;
    reason?: string;
  };
  let orphans = $state<OrphanSession[]>([]);
  let orphanReview = $state(false);
  // #646 Layer C — sweeper interval handle. Distinct from
  // `unreadPollTimer` so we can clean it up independently on logout.
  let autoResumeSweeperTimer: ReturnType<typeof setInterval> | null = null;
  // 5-min sweeper cadence per plan.md §"Sweeper interval" decision.
  const AUTO_RESUME_SWEEPER_MS = 5 * 60 * 1000;
  // #646 Layer D — derived flag: at least one orphan row hit the
  // 3-attempt auto-resume ceiling. Drives the orphan pill's pip
  // color flip (sig → live red) without changing label or button.
  let hasExhaustedAutoResume = $derived(
    orphans.some((o) => (o.auto_resume_count ?? 0) >= 3),
  );
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

  // #327 — strip internal vendor names from a free-form pipeline-
  // error string before it reaches the floater. Mirrors the helper
  // in +page.svelte (see CLAUDE.md hard rule #2 — public-facing copy
  // stays vendor-opaque). Kept inline rather than shared so the
  // redaction list stays close to the strings the user sees from
  // this layout's surfaces.
  function redactVendorNames(s: string): string {
    if (!s) return s;
    return s
      .replace(/\bAssemblyAI\b/gi, "the transcription provider")
      .replace(/\bOpenAI\b/gi, "the summarization provider")
      .replace(/\bPostmark\b/gi, "the email provider")
      .replace(/\bDigitalOcean\b/gi, "the storage provider")
      .replace(/\bSpaces\b/g, "object storage");
  }

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

  // #177 — how many MINOR versions `local` is behind `manifest`. We
  // diff on (major*1000 + minor) so a major bump (e.g. 0.x → 1.0)
  // counts as a single step on the minor axis (1000), not as zero.
  // Patch differences don't move the result. Returns 0 when local is
  // ahead of or equal to manifest on the minor axis (caller still
  // checks `semverGt(manifest, local)` to be sure).
  function minorVersionsBehind(local: string, manifest: string): number {
    const pl = local.split(".").map((x) => parseInt(x, 10) || 0);
    const pm = manifest.split(".").map((x) => parseInt(x, 10) || 0);
    const lk = (pl[0] ?? 0) * 1000 + (pl[1] ?? 0);
    const mk = (pm[0] ?? 0) * 1000 + (pm[1] ?? 0);
    return Math.max(0, mk - lk);
  }

  // #179 — emit `updater::manifest_fetch` only ONCE per session on the
  // happy path, plus EVERY error. The manifest URL is hit on every
  // hourly poll + every refreshStaleNudge tick; logging each one would
  // bloat agent_logs without diagnostic value (the relevant signal is
  // "we successfully reached the manifest at all" + "every failure").
  let manifestFetchEmittedThisSession = false;

  // #179 — fire-and-forget telemetry helper. Mirrors the inline
  // sendToTelemetry helper in onMount so the updater + nudge code paths
  // can emit structured events without the layout-error capture
  // closure. Telemetry-gated server-side; no-op when the user has
  // telemetry off.
  function logEvent(
    level: "info" | "warn" | "error",
    module: string,
    message: string,
    meta?: Record<string, unknown>,
  ) {
    invoke("log_event", {
      input: { level, module, message, meta },
    }).catch(() => {});
  }

  // #177 — independent manifest fetch used by both the Linux fallback
  // (inside pollForUpdate below) and the stale-version nudge poll.
  // The nudge MUST surface even when `@tauri-apps/plugin-updater`'s
  // `check()` is silently broken — that's the entire failure mode #177
  // exists for — so this helper deliberately does not go through any
  // Tauri plugin code path. Returns the manifest's `version` string
  // on success, or null on any failure (network, non-2xx, malformed
  // JSON). Callers must NOT clear cached state on null — the previous
  // poll's verdict stands until a fresh successful fetch refutes it.
  //
  // #179 — emits `updater::manifest_fetch` first-of-session AND on
  // every error. The flag flips on the first SUCCESS so subsequent
  // happy-path polls stay quiet; failures always emit so a transient
  // outage is observable.
  async function pollManifestVersion(): Promise<string | null> {
    try {
      const resp = await fetch(UPDATE_MANIFEST_URL, { cache: "no-store" });
      if (!resp.ok) {
        logEvent("info", "updater::manifest_fetch", "manifest fetch non-2xx", {
          status: resp.status,
          last_modified: resp.headers.get("last-modified") ?? undefined,
        });
        return null;
      }
      const doc = (await resp.json()) as { version?: string };
      const v = doc.version ?? null;
      if (!manifestFetchEmittedThisSession) {
        manifestFetchEmittedThisSession = true;
        logEvent("info", "updater::manifest_fetch", "manifest fetch ok", {
          status: resp.status,
          last_modified: resp.headers.get("last-modified") ?? undefined,
          manifest_version: v ?? undefined,
        });
      }
      return v;
    } catch (e) {
      console.warn("manifest fetch failed", e);
      logEvent("info", "updater::manifest_fetch", "manifest fetch threw", {
        status: "network_error",
        error: String((e as any)?.message ?? e),
      });
      return null;
    }
  }

  // #177 — refresh the stale-version nudge state from the manifest.
  // Independent of pollForUpdate: even when checkForUpdate() throws
  // or returns null silently, this still fires and surfaces the
  // ≥2-minor-behind nudge. On any failure we leave staleNudge as it
  // was (do NOT false-clear) so a transient network blip doesn't
  // hide the nudge for users who genuinely need it.
  async function refreshStaleNudge() {
    const manifestVersion = await pollManifestVersion();
    if (!manifestVersion || !version) return;
    if (
      semverGt(manifestVersion, version) &&
      minorVersionsBehind(version, manifestVersion) >= 2
    ) {
      if (
        !staleNudge ||
        semverGt(manifestVersion, staleNudge.manifestVersion)
      ) {
        staleNudge = { manifestVersion };
      }
    } else {
      staleNudge = null;
    }
  }

  // #179 — `trigger` discriminates which call site initiated the poll
  // ("mount" / "interval" / "user" / "post-pipeline"). Threaded into
  // the `updater::check_start` + `updater::check_result` telemetry
  // events so log queries can ask "did the hourly tick fire on Nick's
  // machine?" without correlating timestamps. `userInitiated` keeps
  // its existing semantics (bypass the busy-gate) so the manual
  // "Check for updates" menu item still surfaces the answer mid-call.
  async function pollForUpdate(opts: {
    trigger: "mount" | "interval" | "user" | "post-pipeline";
    userInitiated?: boolean;
  }) {
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
    let deferredForBusy = false;
    if (!opts.userInitiated) {
      try {
        const busy = await invoke<boolean>("is_processing");
        if (busy) {
          updateCheckDeferred = true;
          deferredForBusy = true;
        }
      } catch (e) {
        // If the probe fails for any reason, fall through to the
        // normal poll rather than suppressing updates silently.
        console.warn("is_processing probe failed", e);
      }
    }
    // #179 — emit `updater::check_start` BEFORE the early-return so a
    // deferred-for-busy poll is observable (today the busy-gate
    // silently drops, which made debugging the original Nick incident
    // harder than it should have been).
    logEvent("info", "updater::check_start", "checkForUpdate begin", {
      trigger: opts.trigger,
      deferred_for_busy: deferredForBusy,
    });
    if (deferredForBusy) {
      return;
    }
    // Primary path: Tauri's updater plugin. Works for Windows, macOS,
    // and AppImage Linux. Returns null for non-AppImage Linux installs.
    try {
      const u = await checkForUpdate();
      if (u) {
        // #428 — skip re-surfacing if the user clicked "Later" on this
        // version within the snooze window (4 h). A newer version on
        // the manifest always surfaces because its key is unset.
        if (!opts.userInitiated && isUpdateSnoozed(u.version)) {
          logEvent("info", "updater::check_result", "checkForUpdate -> snoozed", {
            outcome: "snoozed",
            manifest_version: u.version,
          });
          return;
        }
        // Replace the cached object only if the manifest has moved
        // forward. Keeps the pill steady when nothing changed; flips
        // it to the newer version when 0.3.19 → 0.3.20 happens while
        // the pill is open.
        if (!updateAvailable || semverGt(u.version, updateAvailable.version)) {
          updateAvailable = u;
        }
        // #179 — `update` outcome. The version stamp is the manifest's
        // verdict; `manifest_version` makes the diff between local +
        // remote inspectable from the row alone.
        logEvent("info", "updater::check_result", "checkForUpdate -> update", {
          outcome: "update",
          manifest_version: u.version,
        });
        return;
      }
      // `check()` returned null: there is no update. If we had a
      // cached Update from a previous poll (edge case: manifest
      // rolled back), clear it so the pill doesn't lie.
      updateAvailable = null;
      // #179 — the `null` outcome is the one the original Nick
      // incident was blind to. Stamping the manifest's view (via
      // pollManifestVersion) lets staff distinguish "running latest"
      // from "updater plugin silently broken".
      const manifestVersion = await pollManifestVersion();
      logEvent("info", "updater::check_result", "checkForUpdate -> null", {
        outcome: "null",
        manifest_version: manifestVersion ?? undefined,
      });
    } catch (e) {
      // Network blip or non-AppImage Linux — retry next tick, fall
      // through to the Linux manifest-fetch fallback below.
      console.warn("update check failed", e);
      logEvent("info", "updater::check_result", "checkForUpdate threw", {
        outcome: "error",
        error: String((e as any)?.message ?? e),
      });
    }
    // Linux-only fallback for .deb/.rpm/tarball users (updater returned
    // null or threw because the running binary isn't an AppImage).
    // #177 — fetch is now delegated to `pollManifestVersion()` so the
    // stale-nudge poll and this fallback share the same code path.
    if (isLinux) {
      const manifestVersion = await pollManifestVersion();
      if (!manifestVersion || !version) return;
      if (semverGt(manifestVersion, version)) {
        // #428 — same snooze gate as the AppImage path.
        if (opts.userInitiated || !isUpdateSnoozed(manifestVersion)) {
          // Same refresh logic: only replace when newer.
          if (!linuxUpdateAvailable || semverGt(manifestVersion, linuxUpdateAvailable)) {
            linuxUpdateAvailable = manifestVersion;
          }
        }
      } else {
        linuxUpdateAvailable = null;
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
    if (linuxUpdateAvailable) snoozeUpdate(linuxUpdateAvailable);
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
  // #177 — stale-version nudge. Fires when the running binary is ≥ 2
  // minor versions behind the manifest's `latest.json`. Independent of
  // `updateAvailable` (the Tauri-updater pill) so it surfaces even
  // when `check()` is silently broken — the failure mode that landed
  // two real customers months behind. Non-dismissible by design;
  // takes precedence over `updateAvailable` in the topstrip render
  // (a stale install means the regular updater path is unreliable,
  // so we want the manual download nudge to win).
  let staleNudge = $state<{ manifestVersion: string } | null>(null);

  // Post-update welcome. On each auth session we compare the running
  // binary's version against the last one we showed release notes for
  // (localStorage). If it's new, we pop the modal once the user's name
  // is in hand so the greeting can be personal. No key => first install;
  // we still show a welcome with the current version's notes.
  const LAST_SEEN_VERSION_KEY = "aftercalls.lastSeenVersion";
  const AGENT_ONBOARDING_KEY_PREFIX = "aftercalls:agent-onboarding:v1:";
  let agentOnboardingOpen = $state(false);
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
  // #492 — focus trap: ref for the secondary action so Tab wraps
  // between the two buttons inside the modal.
  let rnLinkBtn = $state<HTMLButtonElement | null>(null);
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


  // #282 — shared keyboard shortcuts. The window-level handler is
  // initialized once and lives for the session; the global context
  // owns the `?` overlay toggle. Per-page contexts (calls list, call
  // detail, actions) register on mount and tear down on destroy.
  let teardownGlobalShortcuts: (() => void) | null = null;

  // #634 — single tick of the unread-call-count poll. Fail-soft: a
  // transient backend hiccup keeps the last-known count visible
  // until the next successful tick. Skips when `document.hidden`
  // matches the portal's Support badge poll posture — a backgrounded
  // webview shouldn't generate pointless `/auth/me` traffic. The
  // tray-badge IPC fires only when the count actually changes so a
  // foregrounded poll on a steady count is a single round-trip.
  async function tickUnreadCount() {
    if (typeof document !== "undefined" && document.hidden) return;
    if (!me) return;
    try {
      const count = await invoke<number>("me_unread_count");
      const next = Math.max(0, Math.floor(count) | 0);
      if (next !== unreadCallsCount) {
        unreadCallsCount = next;
      }
      pushTrayBadge(next);
    } catch {
      // Swallow — keep the last-known value visible. The next tick
      // catches up. Logging would spam telemetry on offline windows.
    }
  }

  /** Update the Tauri tray tooltip with the live count. Idempotent
   *  per-value (gates on `lastTrayBadgePushed`) so a steady-state
   *  poll doesn't churn the IPC channel. */
  function pushTrayBadge(count: number) {
    if (count === lastTrayBadgePushed) return;
    lastTrayBadgePushed = count;
    invoke("set_unread_badge", { count }).catch(() => {
      // Best-effort. Tauri commands rarely fail in steady state; a
      // failure here doesn't change the sidebar pill (the same
      // value is already in `unreadCallsCount` driving the chip).
    });
  }

  /** Webview-side dispatcher for the `unread-count-changed` window
   *  event. Pages that just performed an optimistic mark-as-read
   *  fire this so the sidebar pill ticks down without waiting for
   *  the next 60s poll. The detail page's auto-on-open fire is the
   *  most common caller; the list page's "Mark read" / "Mark all
   *  read" flows fire it too. Listener lives at the layout level
   *  so the same handler covers every dispatcher. */
  function onUnreadCountChanged(ev: Event) {
    const e = ev as CustomEvent<{ delta?: number; absolute?: number }>;
    const detail = e?.detail ?? {};
    if (typeof detail.absolute === "number") {
      unreadCallsCount = Math.max(0, detail.absolute | 0);
    } else if (typeof detail.delta === "number") {
      unreadCallsCount = Math.max(0, unreadCallsCount + (detail.delta | 0));
    } else {
      // No payload → re-fetch live. Keeps callers honest if the
      // local state was wrong (e.g. user hit "Mark all read" but
      // optimistically the page had stale rows).
      void tickUnreadCount();
      return;
    }
    pushTrayBadge(unreadCallsCount);
  }

  onMount(async () => {
    const appPrefs = await getAppPrefs();
    initSentry(appPrefs?.telemetry_enabled === true);
    // #282 — start the shared shortcut listener + register the
    // always-on global bindings.
    initShortcutListener();
    teardownGlobalShortcuts = registerShortcuts(
      "global",
      "Global",
      {
        "?": () => toggleHelp(),
        "meta+k": () => {
          if (!commandPaletteDisabled) openCommandPalette();
        },
        "ctrl+k": () => {
          if (!commandPaletteDisabled) openCommandPalette();
        },
      },
      [
        { keys: "?", label: "Show this shortcuts panel" },
        { keys: "meta+k ctrl+k", label: "Open command palette" },
      ],
    );
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

    // #107 — start polling backend reachability so the OfflineBanner
    // reflects /health roundtrips. Reading `saveQueue.items` ensures
    // the singleton initialises its online-flip subscriber even when
    // no save-site has imported it yet (replay needs to be wired
    // before the user recovers connectivity).
    onlineStatus.start();
    void saveQueue.items;

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
    // #320 — ToS gate. When the cached `pending_tos` is non-empty,
    // route the user to /accept-terms before any recording surface
    // becomes reachable. Mirrors the portal's `applyRouteGates`
    // shape: redirect on entry to any non-public route, and bounce
    // away from /accept-terms when the gate is already clear.
    applyTosGate();

    unlistenState = await listen<{
      recording: boolean;
      mode?: "call" | "self_note" | null;
    }>(
      "recording-state",
      (evt) => {
        const wasRecording = recording;
        recording = evt.payload.recording;
        // #142 · v0.4.5 — mode rides on the event so the overlay
        // only surfaces for self-notes. Missing field (fallback
        // from an old backend) → treat as "call".
        const mode = evt.payload.mode ?? "call";
        recordingMode = recording ? mode : null;
        // #327 — drive the persistent floater from the same event.
        // On start we also clear any pending fade-out from a previous
        // session so back-to-back recordings don't ghost the new pill.
        // The transition event itself doesn't carry started_at_ms, so
        // we seed the timer to "now"; the layout's mount-time
        // is_recording probe handles the re-mount-mid-recording case.
        if (recording) {
          if (callFloaterDoneTimer) {
            clearTimeout(callFloaterDoneTimer);
            callFloaterDoneTimer = null;
          }
          callRecording.markRecording(mode, Date.now());
        } else if (wasRecording) {
          // Backend transitioned us out of recording. The Stop click
          // path already moved the store to "stopping" optimistically;
          // for stops triggered elsewhere (Record page button, hotkey,
          // tray) we haven't, so do it here. The pipeline listener
          // below carries the floater the rest of the way.
          if (callRecording.state === "recording") {
            callRecording.markStopping();
          }
        }
        // #79: a very short recording may stop before the pipeline
        // ever engages (auto-detect false start, cancelled session).
        // If we deferred a poll and no pipeline event comes along to
        // clear the flag, the stop transition does it here. If the
        // pipeline listener beats us to it the flag is already
        // cleared so this is a no-op.
        if (!recording && updateCheckDeferred) {
          updateCheckDeferred = false;
          // #177 — fire both checks in parallel; same URL, browser
          // HTTP cache deduplicates within the process.
          // #179 — trigger="post-pipeline" because the recording-stop
          // path is conceptually the same "work just finished, re-poll"
          // signal as the pipeline-done path below.
          void Promise.all([
            pollForUpdate({ trigger: "post-pipeline" }),
            refreshStaleNudge(),
          ]);
        }
      },
    );
    unlistenPipeline = await listen<{
      stage: string;
      call_id?: string;
      error?: string;
      // #646 Layer A — Rust may extend the event with a `retrying`
      // flag during a Layer A retry wait so the topstrip can swap
      // the stage label for "Retrying…". Optional so older Rust
      // builds still type-match cleanly.
      retrying?: boolean;
    }>("pipeline", (evt) => {
      // #646 Layer A — three accepted wire shapes for "a retry wait
      // is currently in flight", any one flips the topstrip label
      // to "Retrying…":
      //   1. payload `retrying: true` on a regular stage event
      //   2. payload `stage === "retrying"` (dedicated stage value)
      //   3. a separate `pipeline-retry` event (handled below)
      // Cases 1 + 2 are handled here. The `pipelineStage` itself is
      // left at the prior in-flight stage (e.g. "transcribing") so
      // the underlying pip class doesn't shift — only the rendered
      // label swaps, per ui.md §Topstrip indicator.
      const rawStage = evt.payload.stage ?? "";
      if (rawStage === "retrying" || evt.payload.retrying === true) {
        pipelineRetrying = true;
        // Don't overwrite pipelineStage — keep the prior stage so
        // the pip class is stable across the retry wait.
        if (rawStage !== "retrying" && rawStage) {
          pipelineStage = rawStage;
        }
      } else {
        pipelineStage = rawStage;
        // Any non-retrying stage transition clears the flag — a
        // successful retry resumes the normal stage label
        // seamlessly per ui.md §Topstrip indicator → "Retry
        // succeeded".
        pipelineRetrying = false;
      }
      // #327 — feed the floater so it shows post-stop progress
      // (transcribing → summarizing → done) and a retry pointer on
      // failure. The store collapses the wider stage vocabulary into
      // a small status set; see callRecording.svelte.ts.
      if (pipelineStage === "failed" && evt.payload.error) {
        callRecording.setError(redactVendorNames(evt.payload.error));
      }
      callRecording.notePipelineStage(pipelineStage, evt.payload.call_id);
      if (pipelineStage === "done") {
        if (
          evt.payload.call_id &&
          evt.payload.call_id !== pipelineReadyNotifiedFor &&
          document.hidden
        ) {
          pipelineReadyNotifiedFor = evt.payload.call_id;
          invoke("notify_call_ready", {
            title: "",
            body: "Your saved call has finished processing.",
          }).catch((e) => console.warn("notify_call_ready failed", e));
        }
        // Auto-fade the floater after a short hold. Failure holds
        // indefinitely (user must dismiss via the Open pointer).
        if (callFloaterDoneTimer) clearTimeout(callFloaterDoneTimer);
        callFloaterDoneTimer = setTimeout(() => {
          callFloaterDoneTimer = null;
          if (callRecording.state === "done") callRecording.reset();
        }, 3500);
      }
      if (pipelineStage === "done" || pipelineStage === "failed") {
        // #79: pipeline finished → if we deferred an update poll
        // while work was in flight, re-run it now so the pill
        // surfaces within ~1s of completion instead of waiting up
        // to 60min for the next hourly tick.
        if (updateCheckDeferred) {
          updateCheckDeferred = false;
          // #177 — fire both checks in parallel; the nudge is
          // independent of pollForUpdate's plugin path.
          void Promise.all([
            pollForUpdate({ trigger: "post-pipeline" }),
            refreshStaleNudge(),
          ]);
        }
        setTimeout(() => {
          if (pipelineStage === "done" || pipelineStage === "failed")
            pipelineStage = "";
        }, 4000);
      }
    });
    // #646 Layer A — optional dedicated retry channel. Fires when
    // the Rust side prefers a separate event over an in-band
    // `retrying` flag on the `pipeline` event. Either path flips the
    // same `pipelineRetrying` state, so the topstrip render is
    // identical regardless of which the Rust builder chooses.
    try {
      unlistenPipelineRetry = await listen<{ active: boolean }>(
        "pipeline-retry",
        (evt) => {
          pipelineRetrying = !!evt.payload?.active;
        },
      );
    } catch (e) {
      // Channel not registered on the Rust side — fine, the
      // in-band `retrying` flag path covers us.
      console.debug("pipeline-retry listener wiring noop", e);
    }
    // #327 — `recording-state` only fires on transitions, so a fresh
    // mount mid-recording (route nav, tray show, hydration) wouldn't
    // know the truth. Probe the backend once so the floater picks up
    // an in-flight session immediately. Best-effort; a failure is
    // silent and the next transition event corrects course.
    try {
      const status = await invoke<{
        recording: boolean;
        mode?: "call" | "self_note" | null;
        started_at_ms: number | null;
      }>("is_recording");
      if (status?.recording) {
        recording = true;
        const mode = status.mode ?? "call";
        recordingMode = mode;
        callRecording.markRecording(
          mode,
          status.started_at_ms ?? Date.now(),
        );
      }
    } catch {}

    // Tray menu items that need to route: open Settings directly.
    unlistenTray = await listen<string>("tray-open", (evt) => {
      if (evt.payload === "settings") goto("/settings");
    });

    // Auto-detect → slide-out (#59). Both `prompt_start` (call
    // detected, prompt to record) and `prompt_end` (mic idle mid-
    // recording, prompt to stop) surface here so the user can answer
    // the prompt from any route — the Record page suppresses both
    // copies via `isRecordPage` so its own inline banner remains the
    // single source of truth when the user is on /record.
    // `cleared` drops both slide-outs and any open ack modal.
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
      } else if (evt.payload.kind === "prompt_end") {
        // Same record-page guard as prompt_start: the inline banner
        // on /record already handles this case. Off-route, surface
        // a slide-out anchored to the rail so the user can save +
        // transcribe (or keep recording) without route-changing.
        if (isRecordPage) {
          autoEndPrompt = null;
          return;
        }
        if (!autoEndPrompt) notifyAutoDetect();
        autoEndPrompt = { app: evt.payload.app };
      } else if (evt.payload.kind === "cleared") {
        autoPrompt = null;
        autoEndPrompt = null;
        autoAckOpen = false;
      }
    });

    // #89 — OS-native actionable notification (mic-detect "Record
    // this call?"). Routes the user's button click back through the
    // same handlers the in-app slide-out uses, so the PIPEDA-ack flow,
    // start cue, and detector-side state transitions all run identical
    // paths whether the user clicked the slide-out or the toast.
    // `auto_dismissed=true` carries the 30s timeout case; v1 routes
    // it to autoDismiss exactly like an explicit "Not now" click.
    unlistenNotifyAction = await listen<{
      action_id: string;
      auto_dismissed: boolean;
    }>("notify-action", (evt) => {
      // Belt-and-braces: when the user's already on /record the inline
      // banner there owns the prompt — mirrors the autoPrompt guard
      // above. Without this a stale toast click could race the in-page
      // banner.
      if (isRecordPage) return;
      // #317 — the OS toast carries a 30-s timeout (notify_actions.rs
      // hard-codes it for the auto-detect "Record this call?" prompt).
      // When the slim ack modal is open the user is mid-consent — if
      // the toast auto-dismisses while they're reading, treating that
      // as a "Not now" click would yank the in-flight ack out from
      // under them. Suppress auto-dismissed events while the modal
      // is up; an explicit user click on "Not now" still routes
      // through (auto_dismissed=false). This is the practical
      // equivalent of the timer-extend the plan calls for — the toast
      // wait-task is fire-and-forget once spawned, so frontend-side
      // suppression is the cleanest available knob without a deeper
      // notify_actions refactor.
      if (autoAckOpen && evt.payload.auto_dismissed) return;
      if (evt.payload.action_id === "record") {
        void autoRecordClick();
      } else {
        void autoDismiss();
      }
    });

    // #596 — auto-record cancel toast. The agent fires
    // `auto-record-pending` when an enabled app's mic transition is
    // about to land; the user has 5s to click Cancel before the
    // matching `auto-record-fired` lands and the recording starts.
    // Inline action on the toast routes to the cancel IPC, which wipes
    // the pending slot Rust-side and emits `auto-record-cancelled`
    // (reason="user"). Empty `friendly_name` falls back to bundle_id
    // for the rare app that didn't surface application.name.
    unlistenAutoRecordPending = await listen<AutoRecordPendingPayload>(
      "auto-record-pending",
      (evt) => {
        const { pending_id, bundle_id, friendly_name } = evt.payload;
        const display = friendly_name?.trim() || bundle_id;
        // Belt-and-braces: if a pending toast is already up for this
        // bundle (shouldn't happen — Rust drops a second pending
        // mid-flight), dismiss the old one before pushing a new.
        const stale = autoRecordPendingByBundle.get(bundle_id);
        if (stale) {
          const tid = autoRecordToasts.get(stale);
          if (tid !== undefined) toast.dismiss(tid);
          autoRecordToasts.delete(stale);
          autoRecordPendingByBundle.delete(bundle_id);
        }
        const toastId = toast.info(`Auto-recording ${display} in 5s`, {
          duration: 5000,
          action: {
            label: "Cancel",
            onClick: () => {
              // Fire-and-forget; the matching `auto-record-cancelled`
              // event will dismiss the toast for us. We pre-emptively
              // dismiss here too so a flaky IPC round-trip doesn't
              // leave the toast hanging.
              autoRecord.cancelPending(pending_id).catch((e) => {
                console.warn("confirm_auto_record_cancel failed", e);
              });
              const tid = autoRecordToasts.get(pending_id);
              if (tid !== undefined) toast.dismiss(tid);
              autoRecordToasts.delete(pending_id);
              autoRecordPendingByBundle.delete(bundle_id);
            },
          },
        });
        autoRecordToasts.set(pending_id, toastId);
        autoRecordPendingByBundle.set(bundle_id, pending_id);
      },
    );

    // #596 — auto-record-fired: 5s elapsed, recording is running.
    // Dismiss the toast cleanly so the user doesn't see "Cancel" on
    // an action that's no longer cancellable. The `recording-state`
    // event listener above takes over from here.
    unlistenAutoRecordFired = await listen<{ bundle_id: string }>(
      "auto-record-fired",
      (evt) => {
        const pending = autoRecordPendingByBundle.get(evt.payload.bundle_id);
        if (pending) {
          const tid = autoRecordToasts.get(pending);
          if (tid !== undefined) toast.dismiss(tid);
          autoRecordToasts.delete(pending);
          autoRecordPendingByBundle.delete(evt.payload.bundle_id);
        }
      },
    );

    // #596 — auto-record-cancelled: pending was aborted before firing.
    // `reason` carries the cause (user / app_stopped / error). Dismiss
    // the toast unconditionally; only show a follow-up note for
    // surprising cases (errors). app_stopped is the user opening then
    // closing the app within 5s — non-noteworthy.
    unlistenAutoRecordCancelled = await listen<{
      bundle_id: string;
      reason: "user" | "app_stopped" | "error";
    }>("auto-record-cancelled", (evt) => {
      const pending = autoRecordPendingByBundle.get(evt.payload.bundle_id);
      if (pending) {
        const tid = autoRecordToasts.get(pending);
        if (tid !== undefined) toast.dismiss(tid);
        autoRecordToasts.delete(pending);
        autoRecordPendingByBundle.delete(evt.payload.bundle_id);
      }
      if (evt.payload.reason === "error") {
        toast.warning(
          "Couldn't auto-start the recording. Try the Record button.",
        );
      }
    });

    // #596 — observed-apps-updated: a new row was inserted or an
    // existing row's last_seen_at advanced. Bump the settings store so
    // a Settings page mounted right now sees the change without a
    // route navigation. Throttled server-side (2s) so a chatty
    // consumer doesn't spam the IPC bus.
    unlistenObservedAppsUpdated = await listen("observed-apps-updated", () => {
      void autoRecordStore.refresh();
    });

    // #313 — wire the close-window advisory listener. Rust emits this
    // when `close_to_tray=false` AND the agent isn't busy, i.e. the
    // user clicked X and the window is genuinely closing. Stash a
    // pending flag so the next launch shows the tray-explainer note;
    // we only have a tick or two before the webview tears down so
    // localStorage (synchronous) is the right tool.
    unlistenCloseAdvisory = await listen("close-advisory", () => {
      try {
        if (localStorage.getItem(CLOSE_ADVISORY_SEEN_KEY) !== "1") {
          localStorage.setItem(CLOSE_ADVISORY_PENDING_KEY, "1");
        }
      } catch {}
    });

    // #313 — render the one-time advisory if the previous session
    // queued one. The note dismisses itself; once dismissed the seen
    // flag is permanent.
    try {
      if (
        localStorage.getItem(CLOSE_ADVISORY_PENDING_KEY) === "1" &&
        localStorage.getItem(CLOSE_ADVISORY_SEEN_KEY) !== "1"
      ) {
        closeAdvisoryVisible = true;
      }
    } catch {}

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
    // #177 — refreshStaleNudge() runs alongside on every trigger so
    // the nudge surfaces even when checkForUpdate() is broken.
    // #179 — trigger="mount" on first poll, "interval" on subsequent
    // hourly ticks; both flow into `updater::check_start` meta.
    await Promise.all([
      pollForUpdate({ trigger: "mount" }),
      refreshStaleNudge(),
    ]);
    const updateTimer = window.setInterval(
      () => {
        void Promise.all([
          pollForUpdate({ trigger: "interval" }),
          refreshStaleNudge(),
        ]);
      },
      60 * 60 * 1000,
    );
    unlistenUpdatePoll = () => window.clearInterval(updateTimer);

    // First-run education wins over release notes on a fresh install.
    // ToS/privacy has already been evaluated above, so onboarding only
    // opens for a fully usable authenticated session.
    maybeShowAgentOnboarding();
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
    window.addEventListener("aftercalls-auth-refresh", handleLoginEvent);

    // #634 — start the unread-count poll. Only meaningful when
    // signed in; the tickUnreadCount guard short-circuits when
    // `me` is null so a logged-out webview doesn't churn /auth/me.
    // Listener for `unread-count-changed` is wired regardless so a
    // page that fires while the layout is mid-mount still lands
    // (the listener falls back to a refetch when the payload is
    // empty, which the cached count protects from a 401 on the
    // logged-out leg).
    window.addEventListener(
      "unread-count-changed",
      onUnreadCountChanged as EventListener,
    );
    if (me) {
      void tickUnreadCount();
      unreadPollTimer = setInterval(tickUnreadCount, UNREAD_POLL_MS);
    }

    // #646 Layer C — 5-min orphan-resume sweeper. Fires a Tauri
    // command that re-classifies recent orphan sessions and silently
    // re-drives the ones with transient failure classes. Gated on
    // `authed` (the IPC talks to the backend) AND
    // `document.visibilityState !== "hidden"` (battery + noise — a
    // hidden window doesn't need to keep polling). The handler
    // re-evaluates the visibility guard at every tick so a tab that
    // gets hidden mid-interval doesn't fire. The IPC return shape
    // (`AutoResumeResult[]`) is logged at debug level only — all
    // user-visible state changes happen via the existing `pipeline`
    // + orphan-list channels.
    if (me) {
      onVisibilityChange = () => {
        // Refresh the orphan list when the window comes back so the
        // panel reflects whatever Layer C did while we were hidden.
        if (
          document.visibilityState !== "hidden" &&
          autoResumeSweeperTimer !== null
        ) {
          void loadOrphans();
        }
      };
      document.addEventListener("visibilitychange", onVisibilityChange);
      autoResumeSweeperTimer = setInterval(
        runAutoResumeSweeper,
        AUTO_RESUME_SWEEPER_MS,
      );
    }
  });

  // #646 Layer C — one tick of the sweeper. Best-effort: any failure
  // is logged at debug level and silently dropped so transient
  // errors (network blip, backend hiccup, command not yet registered
  // on older Rust builds) don't surface a user-visible state change.
  // Re-runs `loadOrphans()` afterwards so the orphan panel UI picks
  // up: (a) sessions that auto-resumed (now removed from the list),
  // (b) sessions whose `auto_resume_count` bumped past the ceiling
  // (which flips the topstrip pip color via `hasExhaustedAutoResume`).
  async function runAutoResumeSweeper() {
    if (!me) return;
    if (typeof document !== "undefined" && document.visibilityState === "hidden") {
      return;
    }
    try {
      const results = await invoke<AutoResumeResult[]>("auto_resume_orphans");
      console.debug("auto_resume_orphans tick", results);
    } catch (e) {
      console.debug("auto_resume_orphans failed (likely unregistered)", e);
    }
    // Pick up the side-effects: sessions either resumed (removed) or
    // bumped their auto_resume_count (visual treatment changes).
    void loadOrphans();
  }

  async function handleLoginEvent() {
    try {
      me = await invoke<Me | null>("current_user");
    } catch {}
    // #320 — re-evaluate the ToS gate after a fresh sign-in. Login
    // navigates to "/" itself, which lands the user on /calls; if the
    // user has outstanding ToS we want to bounce them to
    // /accept-terms before maybeShowReleaseNotes / loadOrphans run.
    applyTosGate();
    maybeShowAgentOnboarding();
    await maybeShowReleaseNotes();
    // Fresh login → check for leftover sessions from a previous run.
    // Same call as the onMount path; idempotent.
    await loadOrphans();
    // #634 — start the unread-count poll on the post-login leg too.
    // Login arrives via `aftercalls-login`, after which `me` is
    // populated; the onMount-time poll didn't start because `me`
    // was null. Idempotent: don't double-schedule if the timer is
    // already alive (handleLoginEvent also fires on token-refresh).
    if (me && unreadPollTimer === null) {
      void tickUnreadCount();
      unreadPollTimer = setInterval(tickUnreadCount, UNREAD_POLL_MS);
    }
    // #646 Layer C — same idempotent kickoff for the auto-resume
    // sweeper. On the post-login leg `me` is fresh, so this is the
    // first opportunity to start polling. Guarded behind a null
    // check so a token-refresh re-firing handleLoginEvent doesn't
    // double-schedule.
    if (me && autoResumeSweeperTimer === null) {
      onVisibilityChange = () => {
        if (
          document.visibilityState !== "hidden" &&
          autoResumeSweeperTimer !== null
        ) {
          void loadOrphans();
        }
      };
      document.addEventListener("visibilitychange", onVisibilityChange);
      autoResumeSweeperTimer = setInterval(
        runAutoResumeSweeper,
        AUTO_RESUME_SWEEPER_MS,
      );
    }
  }

  // #320 — Single point of truth for the agent-side ToS gate. Mirrors
  // the portal's `applyRouteGates` ToS branch: when the cached `me`
  // has outstanding rows AND the current path isn't already
  // /accept-terms (or /login, which sits ahead of the gate), redirect.
  // When the user is on /accept-terms with no pending rows, bounce to
  // /calls so the page isn't a manual-navigate dead-end. The login
  // route is intentionally exempt — a user with pending_tos who hasn't
  // signed in yet still sees the login form first; the gate kicks in
  // once `me` is populated.
  function applyTosGate() {
    if (!me) return;
    const path = page.url.pathname;
    const pending = me.pending_tos ?? [];
    if (pending.length > 0 && path !== "/accept-terms" && path !== "/login") {
      goto("/accept-terms");
      return;
    }
    if (pending.length === 0 && path === "/accept-terms") {
      goto("/calls");
    }
  }

  function agentOnboardingKey(userId: string): string {
    return `${AGENT_ONBOARDING_KEY_PREFIX}${userId}`;
  }

  function hasPendingTos(): boolean {
    return (me?.pending_tos?.length ?? 0) > 0;
  }

  function shouldShowAgentOnboarding(): boolean {
    if (!me || hasPendingTos()) return false;
    if (page.url.pathname === "/login" || page.url.pathname === "/accept-terms") {
      return false;
    }
    try {
      return localStorage.getItem(agentOnboardingKey(me.user_id)) !== "1";
    } catch {
      return false;
    }
  }

  function maybeShowAgentOnboarding(): boolean {
    if (agentOnboardingOpen) return true;
    if (!shouldShowAgentOnboarding()) return false;
    releaseNotes = null;
    agentOnboardingOpen = true;
    return true;
  }

  async function closeAgentOnboarding(destination: "record" | "settings") {
    if (me) {
      try {
        localStorage.setItem(agentOnboardingKey(me.user_id), "1");
      } catch (e) {
        console.warn("agent onboarding persistence failed", e);
      }
    }
    agentOnboardingOpen = false;
    if (destination === "settings") {
      await goto("/settings");
    } else if (page.url.pathname !== "/") {
      await goto("/");
    }
    await maybeShowReleaseNotes();
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
      // #646 — surface the failure as a transient toast (was a
      // silent console.warn). Vendor-opaque copy per CLAUDE.md hard
      // rule. 8 s duration matches ui.md §Orphan row "Error" state.
      console.warn("resume_orphan_session failed", e);
      toast.error("Couldn't resume this recording.", { duration: 8_000 });
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
    if (hasPendingTos() || page.url.pathname === "/accept-terms") return;
    if (agentOnboardingOpen || shouldShowAgentOnboarding()) return;
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
    // #179 — `install_start` fires once per install attempt, BEFORE
    // downloadAndInstall. The progress + done + error events follow.
    logEvent("info", "updater::install_start", "downloadAndInstall begin", {
      target_version: target.version,
    });
    updateState = "downloading";
    updateError = "";
    // #179 — throttle `install_progress` to ≥10% delta OR ≥2s
    // interval, whichever comes first. A slow Windows download could
    // otherwise emit dozens of events per second and burn the 200-row
    // batch budget.
    let lastEmittedPercent = -1;
    let lastEmittedAt = 0;
    try {
      await target.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          updateTotal = ev.data.contentLength ?? 0;
          updateDownloaded = 0;
        } else if (ev.event === "Progress") {
          updateDownloaded += ev.data.chunkLength;
          if (updateTotal > 0) {
            const percent = Math.min(
              100,
              Math.round((updateDownloaded / updateTotal) * 100),
            );
            const now = Date.now();
            if (
              percent - lastEmittedPercent >= 10 ||
              now - lastEmittedAt >= 2000
            ) {
              lastEmittedPercent = percent;
              lastEmittedAt = now;
              logEvent(
                "info",
                "updater::install_progress",
                "downloadAndInstall progress",
                {
                  percent,
                  downloaded_bytes: updateDownloaded,
                  total_bytes: updateTotal,
                },
              );
            }
          }
        } else if (ev.event === "Finished") {
          updateState = "ready";
        }
      });
      // downloadAndInstall returns once the new version is applied. Tell the
      // user explicitly before we relaunch so the app doesn't just vanish.
      // #179 — `install_done` fires AFTER downloadAndInstall returns
      // and BEFORE relaunch, so the event lands in the buffer + has a
      // shot at being flushed before the process exits. (relaunch is
      // best-effort flushed via the post-relaunch process; this event
      // is the one we'll see if relaunch itself races the flush.)
      logEvent("info", "updater::install_done", "downloadAndInstall ok", {
        target_version: target.version,
        total_bytes: updateTotal,
      });
      await relaunch();
    } catch (e) {
      // #179 — capture the phase from `updateState` BEFORE we overwrite
      // it with "error" (otherwise every install_error would map to
      // "apply"). "downloading" → download (the in-flight path),
      // "ready" → relaunch (post-Finished, before relaunch resolves),
      // anything else → apply.
      const phase: "download" | "verify" | "apply" | "relaunch" =
        updateState === "downloading"
          ? "download"
          : updateState === "ready"
            ? "relaunch"
            : "apply";
      updateError = portalErrorToText(e);
      updateState = "error";
      logEvent("error", "updater::install_error", "downloadAndInstall threw", {
        phase,
        target_version: target.version,
        error: String((e as any)?.message ?? e),
      });
    }
  }

  // #428 — per-version snooze. "Later" hides the pill for 4 hours so
  // the hourly poll doesn't resurface the exact same version on the
  // very next tick.  The key includes the version string so a newer
  // release on the manifest always surfaces fresh (its key is unset).
  const UPDATE_SNOOZE_MS = 4 * 60 * 60 * 1000; // 4 hours
  function updateSnoozeKey(ver: string) {
    return `aftercalls:update-snooze:${ver}`;
  }
  function isUpdateSnoozed(ver: string): boolean {
    try {
      const raw = localStorage.getItem(updateSnoozeKey(ver));
      if (!raw) return false;
      return Date.now() < parseInt(raw, 10);
    } catch {
      return false;
    }
  }
  function snoozeUpdate(ver: string) {
    try {
      localStorage.setItem(
        updateSnoozeKey(ver),
        String(Date.now() + UPDATE_SNOOZE_MS),
      );
    } catch {}
  }

  function dismissUpdate() {
    if (updateAvailable) snoozeUpdate(updateAvailable.version);
    updateAvailable = null;
    updateState = "idle";
  }

  onDestroy(() => {
    unlistenState?.();
    unlistenPipeline?.();
    unlistenTray?.();
    unlistenUpdatePoll?.();
    unlistenAutoDetect?.();
    unlistenNotifyAction?.();
    unlistenCloseAdvisory?.();
    unlistenAutoRecordPending?.();
    unlistenAutoRecordFired?.();
    unlistenAutoRecordCancelled?.();
    unlistenObservedAppsUpdated?.();
    unlistenPipelineRetry?.();
    if (callFloaterDoneTimer) {
      clearTimeout(callFloaterDoneTimer);
      callFloaterDoneTimer = null;
    }
    // Reset the floater store so a remount doesn't pick up stale
    // post-stop UI from the previous session.
    callRecording.reset();
    window.removeEventListener("aftercalls-login", handleLoginEvent);
    window.removeEventListener("aftercalls-auth-refresh", handleLoginEvent);
    // #634 — stop the unread-count poll + drop the listener.
    window.removeEventListener(
      "unread-count-changed",
      onUnreadCountChanged as EventListener,
    );
    if (unreadPollTimer !== null) {
      clearInterval(unreadPollTimer);
      unreadPollTimer = null;
    }
    // #646 Layer C — stop the auto-resume sweeper + drop the
    // visibility-change listener. Same lifecycle shape as the
    // unread poll above.
    if (autoResumeSweeperTimer !== null) {
      clearInterval(autoResumeSweeperTimer);
      autoResumeSweeperTimer = null;
    }
    if (onVisibilityChange) {
      document.removeEventListener("visibilitychange", onVisibilityChange);
      onVisibilityChange = null;
    }
    teardownGlobalShortcuts?.();
    teardownGlobalShortcuts = null;
  });

  // #313 — dismiss the one-time tray-explainer note. Flips the seen
  // flag permanently and clears the pending one so the note never
  // resurfaces. Called by both the × and the "Open Settings" link
  // because both are dispositive — the user has acknowledged the note.
  function dismissCloseAdvisory() {
    closeAdvisoryVisible = false;
    try {
      localStorage.setItem(CLOSE_ADVISORY_SEEN_KEY, "1");
      localStorage.removeItem(CLOSE_ADVISORY_PENDING_KEY);
    } catch {}
  }

  function openSettingsFromAdvisory() {
    dismissCloseAdvisory();
    goto("/settings");
  }

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
      // Shared helper plays the start cue then calls confirm_auto_start.
      // See $lib/autoStart.ts — same mechanics used by the inline banner
      // on +page.svelte (#465).
      await confirmAutoStart();
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

  // Mid-recording idle-mic slide-out handlers. Mirrors the inline
  // banner on +page.svelte (#60) so the user gets the same two
  // affordances regardless of route: confirm_auto_end stops the
  // recording and runs the post-call pipeline; keep_auto_recording
  // tells the detector to stop nagging until the consumer comes
  // back. Both clear the slide-out optimistically — the detector's
  // own `cleared` event arrives shortly after either decision.
  async function confirmAutoEnd() {
    try {
      await invoke("confirm_auto_end");
    } catch (e) {
      console.warn("confirm_auto_end failed", e);
    }
    autoEndPrompt = null;
  }

  async function keepAutoRecording() {
    try {
      await invoke("keep_auto_recording");
    } catch (e) {
      console.warn("keep_auto_recording failed", e);
    }
    autoEndPrompt = null;
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
      autoAckError = portalErrorToText(e).replace(/^Error:\s*/, "");
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

  // #317 — route the slim ack modal's "Read full policy" link to the
  // in-app Settings → Recording explainer (anchor #recording-policy).
  // Closing the modal AND cancelling the auto-detect prompt is
  // intentional: the user has left the consent flow, so we shouldn't
  // leave a phantom ack waiting in the slide-out behind them. They
  // can re-trigger the prompt by recording manually or by waiting
  // for the next auto-detection cycle.
  async function openRecordingPolicy() {
    autoAckOpen = false;
    autoAckChecked = false;
    autoAckError = "";
    try {
      await invoke("dismiss_auto_start");
    } catch {}
    autoPrompt = null;
    await goto("/settings#recording-policy");
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
    /// #634 — numeric count chip rendered to the right of the label.
    /// Falsy / undefined / 0 omits the chip entirely (matches the
    /// portal's `badge: count > 0` pattern). Capped at 99 in the
    /// rendered text; raw value rides on `aria-label` for SR users.
    badge?: number;
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
          // #634 — unread-count chip mirror of the portal Support
          // badge pattern (`portal/+layout.svelte:616`). Hidden when
          // zero by the render-side `badge > 0` guard; capped at 99
          // in the rendered text per ui.md.
          badge: unreadCallsCount,
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
      const adminItems: NavItem[] = [
        { href: "/admin/users", label: "Team", match: nope, icon: ICON_TEAM, external: true },
        { href: "/admin", label: "Org", match: nope, icon: ICON_ORG, external: true },
        { href: "/admin/vocab", label: "Vocab", match: nope, icon: ICON_VOCAB, external: true },
      ];
      // #215: per-org Zoho gate. Hide the rail entry when
      // `features.zoho` is false; the backend 404s every Zoho route
      // for those orgs anyway, so a click would land on the portal's
      // /admin/zoho page which redirects back. Skipping the rail
      // entry just keeps the surface clean.
      if (me?.features?.zoho) {
        // #186: CRM (Zoho) connect page — admin opens it in the
        // browser via openPortalLink. The agent has no local
        // routes for admin surfaces.
        adminItems.push({ href: "/admin/zoho", label: "CRM", match: nope, icon: ICON_CRM, external: true });
      }
      sections.push({
        id: "admin",
        label: "Admin",
        items: adminItems,
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
  async function startRecordingFromCommandPalette() {
    await goto("/");
    await tick();
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    const startButton = Array.from(
      document.querySelectorAll<HTMLButtonElement>('button[aria-label="Start recording"]'),
    ).find((button) => !button.disabled);
    startButton?.click();
  }
  async function manualUpdateCheck() {
    closeUserMenu();
    // #79: explicit user action bypasses the processing gate. If
    // they asked, surface the answer even mid-call.
    // #177 — refresh the stale-version nudge alongside; same URL,
    // same trigger semantics.
    // #179 — trigger="user" so the menu-driven poll is distinguishable
    // from the hourly interval in agent_logs queries.
    await Promise.all([
      pollForUpdate({ trigger: "user", userInitiated: true }),
      refreshStaleNudge(),
    ]);
  }
  async function signOut() {
    closeUserMenu();
    // #646 Layer C — stop the sweeper before the auth token goes
    // away so a stale `me` reference doesn't drive a background
    // IPC against a now-logged-out session.
    if (autoResumeSweeperTimer !== null) {
      clearInterval(autoResumeSweeperTimer);
      autoResumeSweeperTimer = null;
    }
    if (onVisibilityChange) {
      document.removeEventListener("visibilitychange", onVisibilityChange);
      onVisibilityChange = null;
    }
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

  const commandPaletteDisabled = $derived(
    !me ||
      hasPendingTos() ||
      isLoginPage ||
      page.url.pathname === "/accept-terms" ||
      agentOnboardingOpen ||
      releaseNotes !== null ||
      autoAckOpen ||
      reportDialogOpen,
  );
  const commandPaletteSections = $derived(
    createAgentCommandPaletteSections(
      me,
      goto,
      openPortalLink,
      startRecordingFromCommandPalette,
    ),
  );
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
              <div class="nav-row" class:has-slideout={it.href === "/" && (autoPrompt || autoEndPrompt)}>
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
                    {#if typeof it.badge === "number" && it.badge > 0}
                      <!-- #634 — unread-count chip on the Calls rail
                           entry. Cap visible text at 99; raw count
                           rides on aria-label so screen-readers
                           announce the full number alongside the
                           item label. Per ui.md: chip is absent at
                           zero, font weight + variant-numeric keep
                           digits aligned. -->
                      <span
                        class="nav-badge count"
                        aria-label="{it.badge} unread"
                      >{it.badge >= 100 ? "99+" : it.badge}</span>
                    {/if}
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
          {:else if it.href === "/" && autoEndPrompt && !isRecordPage}
            <!-- Mid-recording idle-mic slide-out. The detector fires
                 `prompt_end` after the chosen consumer has been gone
                 for CONSUMER_GONE_BEFORE_END_PROMPT seconds. Off-route
                 users used to miss this entirely — the Record page's
                 inline banner only renders while /record is mounted.
                 Reuses the `auto-slideout` shell from the start
                 prompt for visual consistency. -->
            <div class="auto-slideout" role="dialog" aria-label="Idle microphone">
              <div class="auto-head">
                <span class="auto-pip" aria-hidden="true"></span>
                <span class="auto-title">Idle mic</span>
              </div>
              <p class="auto-body">
                The microphone has been quiet for a while. End the recording?
              </p>
              <div class="auto-actions">
                <button
                  type="button"
                  class="auto-primary"
                  onclick={confirmAutoEnd}
                >Save + transcribe</button>
                <button
                  type="button"
                  class="auto-secondary"
                  onclick={keepAutoRecording}
                >Keep recording</button>
              </div>
            </div>
          {/if}
              </div>
            {/each}
          </nav>
        </section>
      {/each}
      <!-- #634 — visually-hidden live region. SR users hear "{N}
           unread calls" whenever the count moves so the chip's
           visual transition has an accessible counterpart. The chip
           itself is decorative on a nav link (not focusable); this
           region carries the announcement load. -->
      <span class="sr-only-live" aria-live="polite" aria-atomic="true"
        >{unreadCallsCount} unread calls</span
      >
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
      <!-- Rail foot meta row: version + shortcuts hint (#416) side-by-side. -->
      <div class="rail-foot-meta">
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
        <!-- #416 — shortcuts-panel discovery affordance. Small "?" button
             in the rail foot so users can discover the keyboard-shortcut
             panel without already knowing the ? hotkey. Tooltip reinforces
             the hotkey so users learn it. -->
        <button
          type="button"
          class="shortcuts-hint"
          title="Keyboard shortcuts (press ?)"
          aria-label="Show keyboard shortcuts"
          onclick={toggleHelp}
        >?</button>
      </div>

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
        <!-- #107 — offline indicator. Sits at the head of the
             strip-right pill cluster so it shares the existing wrap
             behaviour (.strip-right is `flex-wrap`); reads from the
             shared `onlineStatus` store. Renders nothing while the
             agent's `backend_health` Tauri command is happy. -->
        <OfflineBanner online={onlineStatus.online} />
        {#if orphans.length > 0}
          <!-- Orphan recovery pill (#63 + #646 Layer D). Collapsed to
               a short label + single Review button — bulk actions
               (Resume all / Discard all) live in the orphan review
               pane that Review toggles. Full description in tooltip
               so it still appears in the narrower topstrip cluster.
               (#432)
               #646: when any orphan row hit the 3-attempt
               auto-resume ceiling, the pip flips from sig (heads-up
               gold) to failed (live red). Label + button are
               unchanged — color is the only signal that auto-recovery
               actually tried and failed. See ui.md §"Pill (topstrip,
               existing .update cluster)". -->
          <div class="update" data-tauri-drag-region>
            <span
              class="pip {hasExhaustedAutoResume ? 'failed' : 'sig'}"
              data-tauri-drag-region
            ></span>
            <span
              class="update-label"
              data-tauri-drag-region
              title={`${orphans.length} unfinished call${orphans.length === 1 ? "" : "s"} from before the last restart`}
            >
              {orphans.length} unfinished
            </span>
            <button
              class="update-install"
              onclick={() => (orphanReview = !orphanReview)}
              disabled={orphanBulkBusy}
            >{orphanReview ? "Hide" : "Review"}</button>
          </div>
        {/if}
        {#if staleNudge}
          <!-- #177 — stale-version nudge. Fires when the running
               binary is ≥ 2 minor versions behind the manifest. Owns
               its own manifest fetch, so it surfaces even when the
               Tauri updater plugin's check() is silently broken (the
               failure mode that put two real customers months
               behind). Non-dismissible by design — pure information,
               renders during recording, takes precedence over the
               regular install pill below. -->
          <div class="update nudge" data-tauri-drag-region>
            <span class="pip nudge-pip" data-tauri-drag-region></span>
            <span
              class="update-label"
              data-tauri-drag-region
              title={`Update available — v${staleNudge.manifestVersion}. You're running v${version}.`}
            >
              Update available — v{staleNudge.manifestVersion}.
              You're running v{version}.
            </span>
            <button class="update-install" onclick={openDownloadsPage}>
              Get it ↗
            </button>
          </div>
        {:else if updateAvailable}
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
            <!-- #646 Layer A — when a retry wait is in flight the pip
                 shifts to .working (accent blink) and the label
                 swaps to "Retrying…". Underlying pipelineStage is
                 preserved so the transition out of the retry
                 ladder reads as a normal stage label swap, not a
                 stage change. -->
            <span
              class="pip {pipelineRetrying ? 'working' : pipelineStage}"
              data-tauri-drag-region
            ></span>
            <span class="ind-label" data-tauri-drag-region>
              {pipelineRetrying
                ? "Retrying…"
                : (stageLabel[pipelineStage] ?? pipelineStage)}
            </span>
          {:else if recording}
            <span class="pip live" data-tauri-drag-region></span>
            <span class="ind-label" data-tauri-drag-region>Recording</span>
          {:else if pipelineStage && pipelineStage !== "done"}
            <span
              class="pip {pipelineRetrying ? 'working' : pipelineStage}"
              data-tauri-drag-region
            ></span>
            <span class="ind-label" data-tauri-drag-region>
              {pipelineRetrying
                ? "Retrying…"
                : (stageLabel[pipelineStage] ?? pipelineStage)}
            </span>
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
            {@const exhausted = (o.auto_resume_count ?? 0) >= 3}
            <!-- #646 Layer D — orphan-row picks up two new visual
                 elements:
                 * `.orphan-kind-badge` distinguishes "never_created"
                   from "stuck_pipeline" (see ui.md
                   §Orphan review panel).
                 * "Couldn't resume automatically." line below
                   .orphan-meta when auto_resume_count >= 3, i.e. the
                   3-attempt Layer C ceiling tripped.
                 `.orphan-meta` carries aria-live="polite" so a row
                 whose ceiling trips while the panel is already open
                 announces the failure-line text. -->
            <li class="orphan-row" role="listitem">
              <div class="orphan-meta" aria-live="polite">
                <div class="orphan-meta-main">
                  <span class="orphan-time">
                    {new Date(o.recorded_at).toLocaleString(undefined, {
                      dateStyle: "medium",
                      timeStyle: "short",
                    })}
                  </span>
                  <span class="orphan-age">· {ageLabel(o.age_minutes)}</span>
                  {#if o.kind === "never_created"}
                    <span
                      class="orphan-kind-badge orphan-kind-never"
                      aria-hidden="false"
                    >Never uploaded</span>
                  {:else if o.kind === "stuck_pipeline"}
                    <span
                      class="orphan-kind-badge orphan-kind-stuck"
                      aria-hidden="false"
                    >Stuck mid-pipeline</span>
                  {/if}
                </div>
                {#if exhausted}
                  <span class="orphan-auto-failed">
                    Couldn't resume automatically.
                  </span>
                {/if}
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
      <!-- #313 — one-time tray-explainer note. Surfaces on the launch
           after the user clicks X with close-to-tray off (and not
           busy, so the window really closed). Reuses the existing
           `.hotkey-note` pattern from +page.svelte so we don't
           introduce a third advisory shape; styles are scoped below. -->
      {#if closeAdvisoryVisible}
        <div class="hotkey-note close-advisory" role="note">
          <p class="hotkey-note-text">
            X closes the window — aftercalls keeps running in the tray.
            <button
              type="button"
              class="hotkey-note-link"
              onclick={openSettingsFromAdvisory}
            >
              Open Settings
            </button>
            to change.
          </p>
          <button
            type="button"
            class="hotkey-note-dismiss"
            onclick={dismissCloseAdvisory}
            aria-label="Dismiss"
            title="Dismiss"
          >
            ×
          </button>
        </div>
      {/if}
      {@render children()}
    </div>
  </div>
</div>
{/if}

<!-- #142 · v0.4.5 — note-to-self overlay folded into the persistent
     live-recording floater (#327). The floater (`<RecordingFloatingControl />`
     mounted below) now surfaces during BOTH call recording and
     self-note capture, with a `mode === "self_note"` variant that
     stays prominent. Removing the parallel overlay here means one
     truth for "recording is in flight" instead of two drifting
     surfaces. -->


{#if agentOnboardingOpen}
  <AgentOnboardingDeck
    onFinish={() => closeAgentOnboarding("record")}
    onSkip={() => closeAgentOnboarding("record")}
    onOpenSettings={() => closeAgentOnboarding("settings")}
  />
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
        // #492 — hand-rolled focus trap. Cycle Tab between the two
        // action buttons (rnLinkBtn ↔ rnDismissBtn) so the body's
        // scroll region doesn't bleed keyboard focus out of the modal.
        if (e.key === "Tab") {
          const first = rnLinkBtn;
          const last = rnDismissBtn;
          if (!first || !last) return;
          if (e.shiftKey) {
            if (document.activeElement === first) {
              e.preventDefault();
              last.focus();
            }
          } else {
            if (document.activeElement === last) {
              e.preventDefault();
              first.focus();
            }
          }
        }
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
          bind:this={rnLinkBtn}
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
  <!-- #317 — slim PIPEDA ack modal for the auto-detect start path.
       The auto-detect prompt has a 30-s OS-toast deadline (see
       detector.rs / notify_actions.rs) so the wall-of-text version
       reliably blew past the timeout when a first-time user tried to
       read it — the late ack would land after the detector cleared
       and the recording wouldn't capture. The full PIPEDA explainer
       is now relocated verbatim to Settings → Recording (anchor
       `#recording-policy`); this slim modal collects the same
       affirmative ack with a single sentence and a single checkbox
       so the median user clears it in well under 30 s. The audit
       row is identical: same `post_recording_ack` Tauri command,
       same backend endpoint, same { agent_version, platform }
       payload as the full Record-page modal that runs on the
       manual-Start path. -->
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
      class="rn-modal auto-ack-slim"
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
        <p class="auto-ack-slim-body">
          By recording, you confirm consent from all participants in this call. Your audio will be transcribed and summarized by automated services to generate notes for you.
        </p>
        <label class="auto-ack-check">
          <input
            type="checkbox"
            bind:checked={autoAckChecked}
            disabled={autoAckSubmitting}
          />
          <span>I confirm and have read the recording policy.</span>
        </label>
        {#if autoAckError}
          <p class="auto-ack-err">{autoAckError}</p>
        {/if}
      </div>
      <div class="rn-actions">
        <!-- #317 — "Read full policy" routes the user in-app to the
             relocated explainer at Settings → Recording. Closes the
             ack modal AND cancels the in-flight auto-detect prompt
             (the user is leaving the path; we don't want a phantom
             ack waiting on them mid-route). -->
        <button
          type="button"
          class="rn-link"
          onclick={openRecordingPolicy}
        >Read full policy</button>
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

<!-- #282 — Keyboard-shortcuts help overlay. Renders nothing until
     the user presses `?`. Registered shortcut contexts (per-page)
     drive what's listed inside. -->
<ShortcutsOverlay />
<CommandPalette
  sections={commandPaletteSections}
  disabled={commandPaletteDisabled}
/>

<!-- #216 / #327 — Persistent live-recording floater. Mounted at the
     layout root so it stays visible while the main window is
     minimized to tray, while the report dialog is hidden, or while
     the user navigates between routes. Surfaces during BOTH screen
     recording (issue dialog flow) and call / self-note recording.
     Renders nothing when no recording is in flight. -->
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
    /* Stacking-context bump (#89 follow-up): the auto-detect slide-out
       is anchored inside .nav-row and pokes out into the main grid
       column via `left: calc(100% + 0.6rem)`. Without an explicit
       z-index here the rail's stacking context loses source-order to
       the main grid column, so the slide-out renders behind the page
       content (Discord-call-detected popup bleed-through). */
    z-index: 10;
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

  /* #416 — rail-foot meta row: version + shortcuts hint side-by-side. */
  .rail-foot-meta {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  /* Shortcuts hint "?" button — minimal chrome, matches the version
     button's visual weight so neither competes with the who-btn. */
  .shortcuts-hint {
    appearance: none;
    background: transparent;
    border: 1px solid var(--hairline);
    border-radius: 6px;
    font: inherit;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-4);
    cursor: pointer;
    padding: 0.1rem 0.45rem;
    line-height: 1.4;
    transition: color 0.15s, border-color 0.15s, background 0.15s;
    margin-right: 0.4rem;
  }
  .shortcuts-hint:hover {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
    background: var(--ink-2);
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

  /* #634 — numeric count chip on the Calls rail entry. Mirror of the
     portal's component-scoped `.nav-badge.count` rule (see
     `portal/src/routes/+layout.svelte` ~line 1101 — the pattern lives
     scoped per-surface so it never bleeds into `app.css`, which keeps
     the byte-identical-mirror invariant intact). Sits at the right
     end of the nav-item via `margin-left: auto`; tabular-nums keeps
     the digits aligned as the count climbs. */
  .nav-badge.count {
    margin-left: auto;
    min-width: 18px;
    padding: 0 5px;
    border-radius: 9px;
    background: var(--ink-2);
    color: var(--bone-1);
    font-size: 0.7rem;
    font-weight: 600;
    line-height: 16px;
    text-align: center;
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
    box-shadow: inset 0 0 0 1px var(--hairline);
  }
  .nav-item.active .nav-badge.count,
  .nav-item:hover .nav-badge.count {
    /* Lift the chip's contrast on the active / hovered nav row so
       it stays legible against the brighter `--ink-2` background
       (which the row itself now occupies). */
    background: var(--ink-1);
  }
  /* Visually-hidden live region (#634, ui.md). Announces count
     transitions to screen-readers without rendering anything on
     screen. Same `.sr-only` shape used by `ShortcutsOverlay` —
     keep this identical so accessibility tooling treats them the
     same way. */
  .sr-only-live {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
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

  /* ── Stale-version nudge (#177) ───────────────────────────────────
     Reuses the .update pill shape (border, padding, font, gap) so
     visual mass matches the existing chrome. Distinct from the
     install pill via a warm `--sig` border + a slightly emphasized
     pip — reads as "you really should act on this" without shouting
     in red. Non-dismissible (no .update-dismiss button rendered),
     so the rule set is intentionally minimal. */
  .update.nudge {
    border-color: var(--sig);
    background: color-mix(in srgb, var(--sig) 8%, var(--ink-1));
  }
  .pip.nudge-pip {
    background: var(--sig);
    box-shadow: 0 0 6px rgba(201, 162, 74, 0.7);
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
    /* #646 Layer D — stacks the time/age/badge row above the
       optional "Couldn't resume automatically." line. Column
       layout keeps the aria-live region a single container so
       AT users hear the failure line when it appears. */
    flex-direction: column;
    align-items: flex-start;
    gap: 0.15rem;
    font-size: 0.82rem;
    color: var(--bone-1);
    min-width: 0;
  }
  /* #646 — the inner horizontal row that holds time, age, and the
     orphan-kind badge. ellipsis clipping moved here from
     .orphan-meta so the auto-failed line below isn't truncated. */
  .orphan-meta-main {
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
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
  /* #646 Layer D — orphan-kind badge. Component-scoped (NOT in
     app.css per the mirror-pair invariant). Two variants share
     shape + radius + border + padding; only color + background
     differ. See ui.md §"Orphan review panel — failure ceiling". */
  .orphan-kind-badge {
    display: inline-flex;
    align-items: center;
    padding: 0.2rem 0.5rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm);
    font-size: 0.72rem;
    font-weight: 500;
    line-height: 1;
    flex-shrink: 0;
  }
  /* "Never uploaded" — neutral bone/ink scheme. Reads as
     informational ("we never saw this") rather than alarming. */
  .orphan-kind-never {
    color: var(--bone-3);
    background: var(--ink-2);
  }
  /* "Stuck mid-pipeline" — sig gold. Reads as "recoverable but
     needs attention" per design.md §Semantic color rules. */
  .orphan-kind-stuck {
    color: var(--sig);
    background: color-mix(in srgb, var(--sig) 14%, transparent);
  }
  /* #646 Layer D — failure-ceiling line surfaced when
     auto_resume_count >= 3. Live-red signal that auto-recovery
     was attempted and exhausted — first and only time the user
     learns the agent tried on its own. */
  .orphan-auto-failed {
    font-size: 0.78rem;
    color: var(--live);
    line-height: 1.3;
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

  /* #317 — slim variant of the auto-detect ack modal. The body is one
     short sentence + one checkbox, so the modal is noticeably tighter
     than the full Record-page version. The `.auto-ack-slim` rn-modal
     selector keeps the trim scoped to the slim variant — the
     full-text modal on the Record page stays at its current
     proportions. */
  .auto-ack-slim {
    max-width: 440px;
  }
  .auto-ack-slim-body {
    margin: 0 0 0.4rem;
    font-size: 0.92rem;
    line-height: 1.55;
    color: var(--bone-1);
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

  /* #370 — collapse pill cluster on narrow windows. Below 480 px the
     combined widths of an update pill + orphan pill + indicator can
     overflow the topstrip even with flex-wrap. Collapse each .update
     pill to pip-only (hide the label + action buttons) so the strip
     stays in a single row. The tooltip/title on the wrapping element
     still carries the full text for pointer users; screen readers
     never see the span text anyway (they traverse the DOM). The
     indicator drops its ind-label too, keeping just the pip + the
     separator dot. */
  @media (max-width: 480px) {
    .update-label,
    .update-install,
    .update-dismiss {
      display: none;
    }
    .update {
      padding: 0.28rem 0.55rem;
      gap: 0;
    }
    .ind-label,
    .ind-sep {
      display: none;
    }
    .indicator {
      gap: 0.4rem;
    }
  }

  /* #327 — note-to-self overlay styles removed. The persistent
     live-recording floater (`RecordingFloatingControl.svelte`)
     subsumes the self-note overlay along with screen + call
     recording, all rendered from one component-scoped <style>
     block. */

  /* #313 — close-window advisory. Mirror of `.hotkey-note` from
     +page.svelte (Linux Wayland hotkey hint) so the visual vocabulary
     for "muted, dismissible info bar" stays consistent across the app
     surface. The close-advisory variant adds top spacing so it
     doesn't crowd the page header. design.md §Pattern library
     documents the shared shape. */
  .hotkey-note {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.55rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
  }
  .close-advisory {
    margin: 1rem 0 0.4rem;
  }
  .hotkey-note-text {
    flex: 1;
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--bone-2);
  }
  .hotkey-note-link {
    color: var(--accent);
    font: inherit;
    padding: 0;
    background: transparent;
    border: 0;
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .hotkey-note-link:hover {
    color: var(--accent-hi);
  }
  .hotkey-note-dismiss {
    flex-shrink: 0;
    width: 1.4rem;
    height: 1.4rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: 0;
    padding: 0;
    background: transparent;
    color: var(--bone-3);
    font-size: 1.1rem;
    line-height: 1;
    border-radius: 4px;
    cursor: pointer;
    transition: color 0.15s, background 0.15s;
  }
  .hotkey-note-dismiss:hover {
    color: var(--bone-0);
    background: var(--ink-2);
  }
</style>
