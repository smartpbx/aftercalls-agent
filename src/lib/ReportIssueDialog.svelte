<script lang="ts">
  /*
   * In-agent "Report an issue" modal (#183).
   *
   * Reuses the shared `.rn-*` modal shell from app.css (same pattern
   * as the release-notes modal, the impersonation start modal, the
   * PIPEDA recording-ack modal). All dialog-local styling lives in
   * this component's scoped style block — no app.css edit, mirror-pair
   * invariant intact.
   *
   * Vendor-opacity: the user-facing copy says "the aftercalls team."
   * No mention of GitHub, DigitalOcean, or any specific storage.
   *
   * Trigger flow:
   *   user-menu "Report an issue" → mounts this dialog → Empty state.
   * On submit: telemetry::flush_now → POST /v1/support/reports →
   * for each presigned PUT: upload bytes → POST .../attachments/finalize.
   * The whole orchestration lives Rust-side in `support::submit_support_report`
   * so the agent layer just calls one IPC and listens for progress.
   */

  import { onDestroy, onMount, tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { getVersion } from "@tauri-apps/api/app";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { screenRecording } from "$lib/stores/recording.svelte";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  // Form state ───────────────────────────────────────────────────
  let title = $state("");
  let body = $state("");
  let titleError = $state("");
  let bodyError = $state("");
  let submitting = $state(false);
  let succeeded = $state(false);
  let errorBanner = $state<string | null>(null);
  // Toggles to "Close without sending? Yes/Keep writing" inline prompt
  // when the user hits Esc or Cancel with content present.
  let confirmingClose = $state(false);
  let attemptCount = $state(0);
  // Toggle for "What's included with this report" disclosure block.
  let telemetryOpen = $state(false);

  // Attachments ──────────────────────────────────────────────────
  // "log" (#628) is a zip of the user's most recent recording session
  // + a session-meta.json with system context, produced on demand by
  // the Tauri command `bundle_latest_session`. Sized differently than
  // screenshots/video and goes to longer-retention storage server-side.
  type AttachmentKind = "screenshot" | "video" | "log";
  type Attachment = {
    /** Stable client-side id; server assigns its own UUID once submit lands. */
    localId: string;
    /**
     * Path for file-picker screenshots. For in-memory recordings, this
     * is populated at submit time by `stage_support_video` — until
     * then it stays empty and `blob` holds the bytes.
     */
    filePath: string;
    filename: string;
    mime: string;
    sizeBytes: number;
    /** dataURL / objectURL preview rendered as the chip thumbnail. */
    previewUrl: string | null;
    /** Set when the file fails local validation (size/type). Excluded from submit. */
    error: string | null;
    /** #203: screenshot or video. Drives per-kind caps + chip UX. */
    kind: AttachmentKind;
    /**
     * #203: in-memory webm blob produced by MediaRecorder. Staged to
     * a temp file by `stage_support_video` on submit. `null` for
     * file-picker screenshots.
     */
    blob: Blob | null;
  };
  let attachments = $state<Attachment[]>([]);
  const MAX_ATTACHMENTS = 5;
  const MAX_SCREENSHOT_BYTES = 25 * 1024 * 1024;
  const MAX_VIDEO_BYTES = 100 * 1024 * 1024;
  // #203: warn at 90 MB so the user can choose to stop recording
  // cleanly before auto-stop kicks at 100 MB.
  const VIDEO_WARN_BYTES = 90 * 1024 * 1024;
  const ALLOWED_MIME = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
  ];

  // Screen-recording state (#203) ───────────────────────────────
  let recorder: MediaRecorder | null = null;
  let recorderStream: MediaStream | null = null;
  let recorderChunks: Blob[] = [];
  let recorderBytes = $state(0);
  let recorderElapsedMs = $state(0);
  let recorderTickHandle: ReturnType<typeof setInterval> | null = null;
  let recordingStartedAt = 0;
  let recorderError = $state<string | null>(null);
  // Feature-detected once at mount; controls whether the Record
  // button shows up at all. Some webviews (older WebKitGTK, certain
  // Linux distros without the desktop portal) lack getDisplayMedia.
  let canRecord = $state(false);
  // #216: pre-record audio toggle. Default off — preserves the v0.7.1
  // PII posture (mic capture is a separate consent surface). User
  // opts in deliberately for narration use cases.
  let audioEnabled = $state(false);
  // #216: feature-detected separately from canRecord. Some webviews
  // expose getDisplayMedia but not getUserMedia; we hide the toggle
  // when the API isn't there.
  let canCaptureMic = $state(false);
  // Preview modal (tier-2) state.
  let previewingLocalId = $state<string | null>(null);

  // Derived + mimeType negotiation results. Renamed from `recording`
  // to `isRecording` (#216) so it doesn't read as the imported
  // screen-recording store.
  const isRecording = $derived(recorder !== null);

  // #216: floater → dialog signal. The floating control bumps
  // `screenRecording.stopRequestId` when the user clicks Stop; this
  // effect observes the bumped counter and routes through our local
  // stopRecording(). Keeps the OS-handle ownership inside the
  // dialog without exporting refs upward.
  let lastSeenStopRequest = 0;
  $effect(() => {
    const id = screenRecording.stopRequestId;
    if (id !== lastSeenStopRequest) {
      lastSeenStopRequest = id;
      if (recorder) stopRecording();
    }
  });

  // Modal scaffolding ────────────────────────────────────────────
  let modalEl = $state<HTMLDivElement | null>(null);
  let titleInput = $state<HTMLInputElement | null>(null);
  // Captured before mount so Escape/close can return focus to the
  // user-menu trigger that opened us. Mirrors the SpeakerRenamePicker
  // (#169) discipline so keyboard-only users don't lose context.
  let openerEl: HTMLElement | null = null;
  const titleId = `ri-title-${Math.random().toString(36).slice(2, 9)}`;
  // aria-live region for SR validation announcements.
  let liveAnnouncement = $state("");

  onMount(async () => {
    openerEl = (document.activeElement as HTMLElement | null) ?? null;
    await tick();
    titleInput?.focus();

    // #203 feature-detection. `navigator.mediaDevices.getDisplayMedia`
    // is absent on some older webviews (notably WebKitGTK < 2.38 or
    // distros without an xdg-desktop-portal). `MediaRecorder.isTypeSupported`
    // also varies — we need at least one webm flavour to work.
    try {
      const hasApi =
        typeof navigator !== "undefined" &&
        !!navigator.mediaDevices &&
        typeof navigator.mediaDevices.getDisplayMedia === "function" &&
        typeof MediaRecorder !== "undefined" &&
        typeof MediaRecorder.isTypeSupported === "function";
      const recorderReady =
        hasApi &&
        (MediaRecorder.isTypeSupported("video/webm;codecs=vp9,opus") ||
          MediaRecorder.isTypeSupported("video/webm;codecs=vp8,opus") ||
          MediaRecorder.isTypeSupported("video/webm"));
      canRecord = !!recorderReady;
      // #216: mic capture is a separate API. Some webviews expose
      // getDisplayMedia but not getUserMedia, or vice versa.
      canCaptureMic =
        hasApi &&
        typeof navigator.mediaDevices.getUserMedia === "function";
    } catch {
      canRecord = false;
      canCaptureMic = false;
    }
  });

  onDestroy(() => {
    // Tear down any live recorder so closing the dialog mid-record
    // doesn't leave a dangling OS capture session.
    if (recorder) {
      try { recorder.stop(); } catch { /* ignore */ }
    }
    if (recorderStream) {
      for (const t of recorderStream.getTracks()) {
        try { t.stop(); } catch { /* ignore */ }
      }
      recorderStream = null;
    }
    if (recorderTickHandle) {
      clearInterval(recorderTickHandle);
      recorderTickHandle = null;
    }
    // #216: reset the screen-recording store on dialog teardown so
    // the floating control disappears even if the user closes the
    // dialog mid-recording (the recorder.stop() above would also
    // fire onstop → markIdle, but it's async; reset here ensures
    // the floater hides immediately).
    screenRecording.markIdle();
    // Revoke all object URLs we minted so the webview doesn't leak
    // bytes for the lifetime of the page.
    for (const a of attachments) {
      if (a.previewUrl && a.previewUrl.startsWith("blob:")) {
        URL.revokeObjectURL(a.previewUrl);
      }
    }
    if (openerEl && document.contains(openerEl)) {
      try {
        openerEl.focus();
      } catch {
        /* opener detached — fall through */
      }
    }
  });

  // ── Recorder controls (#203) ───────────────────────────────────
  function pickMimeType(): string | null {
    const candidates = [
      "video/webm;codecs=vp9,opus",
      "video/webm;codecs=vp8,opus",
      "video/webm;codecs=vp9",
      "video/webm;codecs=vp8",
      "video/webm",
    ];
    for (const c of candidates) {
      try {
        if (MediaRecorder.isTypeSupported(c)) return c;
      } catch { /* ignore */ }
    }
    return null;
  }

  /** #216 defensive helper: stop every track on a stream, ignoring
   * per-track errors. Used on early-exit paths so PipeWire / portal
   * capture handles release immediately when MediaRecorder fails to
   * construct or the stream comes back empty. */
  function stopAllTracks(stream: MediaStream | null) {
    if (!stream) return;
    for (const t of stream.getTracks()) {
      try { t.stop(); } catch { /* ignore */ }
    }
  }

  async function startRecording() {
    if (submitting || isRecording) return;
    if (attachments.length >= MAX_ATTACHMENTS) return;
    recorderError = null;
    const mimeType = pickMimeType();
    if (!mimeType) {
      recorderError = "Screen recording isn't supported on this system.";
      return;
    }

    // #216: signal "starting" up-front so the floater doesn't briefly
    // flash the wrong state if anything async takes time.
    screenRecording.markStarting();

    let displayStream: MediaStream;
    try {
      // Screen capture: video only. Mic, when enabled, is layered
      // separately via getUserMedia below. Avoiding
      // `getDisplayMedia({audio:true})` deliberately — that captures
      // *system* audio (notifications, music, other tabs) which is a
      // much larger PII surface than user narration.
      displayStream = await navigator.mediaDevices.getDisplayMedia({
        video: true,
        audio: false,
      });
    } catch (e) {
      screenRecording.markIdleAfterAbort();
      // User cancelled picker OR permission denied. Distinguish by
      // error name — NotAllowedError is the user's choice (silent),
      // anything else gets a visible banner.
      const name = (e as { name?: string })?.name;
      if (name !== "NotAllowedError" && name !== "AbortError") {
        recorderError =
          "Couldn't start screen recording. Check system permissions and try again.";
      }
      return;
    }

    // #216 defensive (Linux WebKitGTK): some xdg-desktop-portal
    // builds resolve getDisplayMedia with no video tracks on certain
    // cancel paths. Constructing a MediaRecorder over a track-less
    // stream then crashes the webview. Bail cleanly instead.
    if (displayStream.getVideoTracks().length === 0) {
      stopAllTracks(displayStream);
      screenRecording.markIdleAfterAbort();
      recorderError =
        "Screen recording didn't return a video stream. Try again or pick a different window.";
      return;
    }

    // #216: layer mic in if the user opted in. Failure here is
    // non-fatal — proceed with video-only and surface a soft notice.
    let micStream: MediaStream | null = null;
    if (audioEnabled && canCaptureMic) {
      try {
        micStream = await navigator.mediaDevices.getUserMedia({
          audio: true,
          video: false,
        });
      } catch (e) {
        const name = (e as { name?: string })?.name;
        if (name === "NotAllowedError") {
          recorderError =
            "Microphone access was denied. Recording will continue without audio.";
        } else {
          recorderError =
            "Couldn't access the microphone. Recording will continue without audio.";
        }
        // Continue without mic — display capture is already live.
      }
    }

    // Combine video + mic into one MediaStream for MediaRecorder.
    // The webm container handles multi-track natively; opus encoding
    // is part of the negotiated mimeType when codecs allow it.
    const combined = new MediaStream();
    for (const t of displayStream.getVideoTracks()) combined.addTrack(t);
    if (micStream) {
      for (const t of micStream.getAudioTracks()) combined.addTrack(t);
    }

    recorderChunks = [];
    recorderBytes = 0;
    recorderElapsedMs = 0;
    recordingStartedAt = Date.now();
    recorderStream = combined;

    // #216 defensive (Linux): MediaRecorder construction can throw
    // synchronously on WebKitGTK even when isTypeSupported was
    // optimistic — capability check and constructor sometimes
    // disagree on PipeWire-backed capture. Wrap to surface a friendly
    // error and tear down the stream so PipeWire releases the handle.
    let rec: MediaRecorder;
    try {
      rec = new MediaRecorder(combined, { mimeType });
    } catch (e) {
      stopAllTracks(displayStream);
      stopAllTracks(micStream);
      stopAllTracks(combined);
      recorderStream = null;
      screenRecording.markIdleAfterAbort();
      const msg = (e as Error)?.message ?? String(e);
      recorderError = `Couldn't start the recorder (${msg}). Try again or report this if it keeps happening.`;
      return;
    }

    rec.ondataavailable = (ev) => {
      if (ev.data && ev.data.size > 0) {
        recorderChunks.push(ev.data);
        recorderBytes += ev.data.size;
        screenRecording.setProgress(recorderElapsedMs, recorderBytes);
        if (recorderBytes >= MAX_VIDEO_BYTES) {
          // Hard-cap: auto-stop so we never hold more bytes than the
          // backend accepts. A few hundred KB slack is fine because
          // `ondataavailable` is async-batched; server HEAD check
          // gates the final size regardless.
          liveAnnouncement = "Recording stopped automatically at 100 MB.";
          stopRecording();
        } else if (
          recorderBytes >= VIDEO_WARN_BYTES &&
          recorderBytes - ev.data.size < VIDEO_WARN_BYTES
        ) {
          // Crossed the warn threshold on THIS chunk — one-shot notice.
          liveAnnouncement =
            "Recording is about to reach the 100 MB limit.";
        }
      }
    };
    rec.onstop = () => {
      // Mark finalising on the store first so the floater shows
      // "Finishing…" while we assemble the blob.
      screenRecording.markFinalising();
      finaliseRecording();
    };
    rec.onerror = () => {
      // WebKitGTK occasionally fires onerror with a generic event;
      // treat any error as a fatal stop and surface a banner so the
      // user knows the recording is over.
      recorderError =
        "The recorder hit an error and stopped. Anything captured so far will be attached.";
      // Don't call stopRecording — recorder is already in the
      // erroring path. The browser will fire onstop next.
    };
    // #216 fix: assign `recorder` BEFORE wiring track-end handlers.
    // The previous code captured the closure over `recorder`, and on
    // a fast user-cancel the track ended *before* the assignment ran,
    // skipping cleanup.
    recorder = rec;
    // Browser chrome "Stop sharing" button → track ends → we stop.
    displayStream.getVideoTracks().forEach((t) => {
      t.onended = () => {
        if (recorder) stopRecording();
      };
    });
    // Timeslice = 1 s so `dataavailable` fires frequently and the
    // running-size accounting stays close to reality.
    rec.start(1000);

    recorderTickHandle = setInterval(() => {
      recorderElapsedMs = Date.now() - recordingStartedAt;
      screenRecording.setProgress(recorderElapsedMs, recorderBytes);
    }, 250);

    // #216: flip the store to "recording", which hides the dialog
    // (dialogHidden = true) and reveals the floating stop control.
    screenRecording.markRecording();
  }

  function stopRecording() {
    if (!recorder) return;
    try {
      if (recorder.state !== "inactive") recorder.stop();
    } catch {
      /* ignore */
    }
  }

  function finaliseRecording() {
    if (recorderTickHandle) {
      clearInterval(recorderTickHandle);
      recorderTickHandle = null;
    }
    if (recorderStream) {
      for (const t of recorderStream.getTracks()) {
        try { t.stop(); } catch { /* ignore */ }
      }
      recorderStream = null;
    }
    const rec = recorder;
    recorder = null;
    if (!rec || recorderChunks.length === 0) {
      // Edge case: stop fired with zero chunks (very-short recording
      // or PipeWire didn't push any data). Reset the store so the
      // dialog re-appears and the floater goes away.
      screenRecording.markIdle();
      return;
    }
    const type = rec.mimeType || "video/webm";
    // Server-side mime is always plain `video/webm` — stripped of
    // codec suffix because browsers sometimes emit
    // `video/webm;codecs=vp9,opus` in rec.mimeType. Backend allow-list
    // is strict-equal on the bare container type.
    const storedMime = type.split(";")[0]?.trim() || "video/webm";
    const blob = new Blob(recorderChunks, { type });
    const sizeBytes = blob.size;
    // Human-friendly filename; backend sanitiser normalises anyway.
    const stamp = new Date()
      .toISOString()
      .replace(/[:.]/g, "-")
      .slice(0, 19);
    const filename = `recording-${stamp}.webm`;
    const previewUrl = URL.createObjectURL(blob);
    const overCap = sizeBytes > MAX_VIDEO_BYTES;
    attachments = [
      ...attachments,
      {
        localId: crypto.randomUUID(),
        filePath: "",
        filename,
        mime: storedMime,
        sizeBytes,
        previewUrl,
        error: overCap ? "Too large (max 100 MB)" : null,
        kind: "video",
        blob,
      },
    ];
    recorderChunks = [];
    recorderBytes = 0;
    recorderElapsedMs = 0;
    // #216: clearing the store re-shows the dialog so the user can
    // finalise the report (now with the recording attached as a chip).
    screenRecording.markIdle();
  }

  // #216: fmtRecTime moved to RecordingFloatingControl — the dialog
  // no longer renders a live timer (the floater does).

  function openPreview(localId: string) {
    previewingLocalId = localId;
  }
  function closePreview() {
    previewingLocalId = null;
  }

  // Validation helpers ───────────────────────────────────────────
  function validate(): boolean {
    titleError = "";
    bodyError = "";
    let ok = true;
    const t = title.trim();
    const b = body.trim();
    if (t.length === 0) {
      titleError = "Please add a subject";
      ok = false;
    } else if (t.length > 200) {
      titleError = "Subject is too long (max 200 characters)";
      ok = false;
    }
    if (b.length === 0) {
      bodyError = "Please describe the issue (at least a few words)";
      ok = false;
    } else if (b.length > 8000) {
      bodyError = "Description is too long (max 8000 characters)";
      ok = false;
    }
    if (!ok) {
      liveAnnouncement = "Subject and description are required.";
    }
    return ok;
  }

  // File picker → chips ──────────────────────────────────────────
  async function addAttachments() {
    if (submitting) return;
    if (attachments.length >= MAX_ATTACHMENTS) return;
    try {
      const remaining = MAX_ATTACHMENTS - attachments.length;
      const selected = await invoke<
        Array<{
          path: string;
          filename: string;
          mime: string;
          size_bytes: number;
          preview_data_url: string | null;
        }>
      >("select_support_attachments", { maxFiles: remaining });
      for (const meta of selected) {
        stageSelectedAttachment(meta);
      }
    } catch (e) {
      console.warn("ReportIssueDialog: file picker error", e);
    }
  }

  /**
   * Pull metadata + a small preview for one path via the Tauri-side
   * helper. Bytes never travel through the JS bridge for the actual
   * upload — the Rust-side submit reads the file directly when it's
   * time to PUT to the presigned URL.
   */
  function stageSelectedAttachment(meta: {
    path: string;
    filename: string;
    mime: string;
    size_bytes: number;
    preview_data_url: string | null;
  }) {
    const sizeError =
      meta.size_bytes > MAX_SCREENSHOT_BYTES ? "Too large (max 25 MB)" : null;
    const mimeError = ALLOWED_MIME.includes(meta.mime)
      ? null
      : "Not an image — skipped";
    const error = sizeError ?? mimeError;

    attachments = [
      ...attachments,
      {
        localId: crypto.randomUUID(),
        filePath: meta.path,
        filename: meta.filename,
        mime: meta.mime,
        sizeBytes: meta.size_bytes,
        previewUrl: meta.preview_data_url,
        error,
        kind: "screenshot",
        blob: null,
      },
    ];
  }

  // #628: opt-in attach of the user's most recent recording session.
  // Tauri's `bundle_latest_session` finds the latest dir under
  // <app_data>/recordings/, zips it with a session-meta.json, and
  // returns a staged path that rides the existing path-based upload
  // pipeline. The chip appears alongside screenshots/videos and the
  // user can remove it pre-submit if they change their mind.
  let bundlingSession = $state(false);
  let bundleError = $state<string | null>(null);
  const sessionAttached = $derived(
    attachments.some((a) => a.kind === "log"),
  );

  async function attachLatestRecording() {
    if (submitting || bundlingSession || sessionAttached) return;
    if (attachments.length >= MAX_ATTACHMENTS) return;
    bundlingSession = true;
    bundleError = null;
    try {
      const meta = await invoke<{
        path: string;
        size_bytes: number;
      }>("bundle_latest_session");
      // Filename from the staged path so the chip matches the
      // backend-side stored filename.
      const filename =
        meta.path.split(/[\\/]/).pop() ?? "aftercalls-session.zip";
      attachments = [
        ...attachments,
        {
          localId: crypto.randomUUID(),
          filePath: meta.path,
          filename,
          mime: "application/zip",
          sizeBytes: meta.size_bytes,
          previewUrl: null,
          error: null,
          kind: "log",
          blob: null,
        },
      ];
    } catch (e) {
      bundleError =
        e instanceof Error
          ? e.message
          : typeof e === "string"
            ? e
            : "Could not bundle the latest recording.";
    } finally {
      bundlingSession = false;
    }
  }

  function removeAttachment(localId: string) {
    if (submitting) return;
    const target = attachments.find((a) => a.localId === localId);
    if (target?.previewUrl) {
      // data: URLs don't need revoke, but blob: URLs would.
      // Keep this defensive in case we swap implementations.
      try {
        URL.revokeObjectURL(target.previewUrl);
      } catch {
        /* ignore */
      }
    }
    attachments = attachments.filter((a) => a.localId !== localId);
  }

  function fmtBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
    return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  }

  // Submit orchestration ─────────────────────────────────────────
  async function submit() {
    if (submitting) return;
    if (!validate()) return;

    submitting = true;
    errorBanner = null;
    attemptCount += 1;

    const valid = attachments.filter((a) => a.error == null);

    // Snapshot diagnostics for the metadata blob. Plain-language
    // surfaces of these are documented in the "What's included"
    // disclosure above the submit button.
    let agentVersion = "unknown";
    try {
      agentVersion = await getVersion();
    } catch {}
    let windowSize: { w: number; h: number } | null = null;
    try {
      const w = getCurrentWindow();
      const inner = await w.innerSize();
      windowSize = { w: inner.width, h: inner.height };
    } catch {}
    const theme =
      typeof document !== "undefined"
        ? document.documentElement.getAttribute("data-theme") ?? "dark"
        : "dark";

    const metadata: Record<string, unknown> = {
      agent_version: agentVersion,
      platform: navigator.platform || "unknown",
      user_agent: navigator.userAgent,
      window: windowSize,
      theme,
      reported_at: new Date().toISOString(),
    };

    try {
      // #203: any in-memory video needs a disk path before the
      // backend pipeline can read it. Stage here — serial because
      // blobs are large and the Tauri bridge serialises one at a
      // time anyway. On stage failure we bail the whole submit.
      const staged: { att: Attachment; path: string }[] = [];
      for (const a of valid) {
        if (a.kind === "video" && a.blob) {
          const buf = new Uint8Array(await a.blob.arrayBuffer());
          const result = await invoke<{
            path: string;
            size_bytes: number;
          }>("stage_support_video", {
            // Tauri 2 serialises a `number[]` into Rust `Vec<u8>`.
            // Using Array.from over Uint8Array keeps the bridge happy
            // without a base64 round-trip.
            bytes: Array.from(buf),
            filename: a.filename,
          });
          staged.push({ att: a, path: result.path });
        }
      }

      await invoke("submit_support_report", {
        title: title.trim(),
        body: body.trim(),
        metadata,
        attachments: valid.map((a) => {
          if (a.kind === "video") {
            const hit = staged.find((s) => s.att.localId === a.localId);
            return {
              path: hit?.path ?? "",
              filename: a.filename,
              mime: a.mime,
              size_bytes: a.sizeBytes,
              kind: a.kind,
            };
          }
          return {
            path: a.filePath,
            filename: a.filename,
            mime: a.mime,
            size_bytes: a.sizeBytes,
            kind: a.kind,
          };
        }),
      });
      succeeded = true;
      submitting = false;
      // Auto-dismiss after 3s, but the explicit Close button is
      // available immediately. This matches the "submit flow"
      // expectation in the spec.
      setTimeout(() => {
        if (succeeded) close();
      }, 3000);
    } catch (e) {
      submitting = false;
      const msg = typeof e === "string" ? e : (e as Error)?.message ?? String(e);
      // Network / 5xx → full-failure banner; 413 / 400 → partial-failure
      // tone (we keep the typed text and chips so the user can retry).
      errorBanner =
        attemptCount >= 3
          ? `${msg}\n\nIf this keeps happening, email us at support@aftercalls.io.`
          : msg;
    }
  }

  // Close path ────────────────────────────────────────────────────
  function close() {
    onClose();
  }

  function attemptClose() {
    if (submitting) return;
    if (succeeded) {
      close();
      return;
    }
    // Empty state → close immediately. Populated → inline confirm.
    if (title.trim() === "" && body.trim() === "" && attachments.length === 0) {
      close();
      return;
    }
    confirmingClose = true;
  }

  function onBackdropKey(e: KeyboardEvent) {
    if (e.key === "Escape") attemptClose();
  }
  function onModalKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.stopPropagation();
      attemptClose();
    } else if ((e.metaKey || e.ctrlKey) && e.key === "Enter" && !submitting) {
      e.preventDefault();
      submit();
    }
  }

  // A browser drop exposes raw path strings without the native dialog's
  // authorization boundary. Treat it as a request to open the approved
  // picker instead of forwarding those paths to Rust.
  let dragHover = $state(false);
  function onDragOver(e: DragEvent) {
    if (submitting || succeeded) return;
    if (!e.dataTransfer) return;
    e.preventDefault();
    dragHover = true;
  }
  function onDragLeave() {
    dragHover = false;
  }
  async function onDrop(e: DragEvent) {
    dragHover = false;
    if (submitting || succeeded) return;
    e.preventDefault();
    if ((e.dataTransfer?.files.length ?? 0) > 0) await addAttachments();
  }

  const remainingSlots = $derived(MAX_ATTACHMENTS - attachments.length);
  const canSubmit = $derived(
    !submitting &&
      !succeeded &&
      !isRecording &&
      title.trim() !== "" &&
      body.trim() !== "",
  );
  const totalAttempted = $derived(attachments.length);
  const oversizedCount = $derived(
    attachments.filter((a) => a.error != null).length,
  );
  const previewAttachment = $derived(
    previewingLocalId
      ? attachments.find((a) => a.localId === previewingLocalId) ?? null
      : null,
  );
</script>

<!-- #216: while a recording is live, the dialog hides itself
     entirely so the user can navigate the rest of the app to
     demonstrate the bug. The component stays mounted — `title`,
     `body`, and `attachments` state survives across the hide/show.
     The floating control (`<RecordingFloatingControl />` mounted in
     +layout) provides the visible stop affordance. -->
{#if !screenRecording.dialogHidden}
<div
  class="rn-backdrop"
  role="button"
  tabindex="-1"
  aria-label="Close report dialog"
  onclick={attemptClose}
  onkeydown={onBackdropKey}
>
  <div
    class="rn-modal ri-modal"
    class:dragging={dragHover}
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    bind:this={modalEl}
    onclick={(e) => e.stopPropagation()}
    onkeydown={onModalKey}
    ondragover={onDragOver}
    ondragleave={onDragLeave}
    ondrop={onDrop}
    tabindex="-1"
  >
    <div class="rn-head">
      <h2 id={titleId}>Report an issue</h2>
    </div>

    <!-- aria-live for validation announcements (SR-only). -->
    <div class="sr-only" aria-live="assertive">{liveAnnouncement}</div>

    <div class="rn-body">
      {#if succeeded}
        <div class="ri-success">
          <div class="ri-success-disc" aria-hidden="true">✓</div>
          <div class="ri-success-heading">Report sent</div>
          <div class="ri-success-sub">
            We'll follow up by email if we need more.
          </div>
        </div>
      {:else}
        {#if errorBanner}
          <div class="ri-error-banner" role="alert">
            <div class="ri-error-text">{errorBanner}</div>
          </div>
        {/if}

        <label class="ri-field" class:has-error={titleError !== ""}>
          <span class="ri-label">Subject</span>
          <input
            bind:this={titleInput}
            bind:value={title}
            type="text"
            class="ri-input"
            maxlength="200"
            placeholder="Summarise the problem in one line"
            disabled={submitting}
            aria-invalid={titleError !== "" ? "true" : "false"}
            aria-describedby={titleError ? `${titleId}-err` : undefined}
          />
          {#if titleError}
            <span id={`${titleId}-err`} class="ri-error">{titleError}</span>
          {/if}
        </label>

        <label class="ri-field" class:has-error={bodyError !== ""}>
          <span class="ri-label">Description</span>
          <textarea
            bind:value={body}
            class="ri-textarea"
            rows="5"
            maxlength="8000"
            placeholder="Describe what happened — what you did, what you expected, what went wrong"
            disabled={submitting}
            aria-invalid={bodyError !== "" ? "true" : "false"}
            aria-describedby={bodyError ? `${titleId}-body-err` : undefined}
          ></textarea>
          {#if bodyError}
            <span id={`${titleId}-body-err`} class="ri-error">{bodyError}</span>
          {/if}
        </label>

        <div class="ri-attach-row">
          {#if attachments.length > 0}
            <ul class="ri-chip-list" aria-label="Attached files">
              {#each attachments as a (a.localId)}
                <li
                  class="ri-chip"
                  class:has-error={a.error != null}
                  class:chip-uploading={submitting && a.error == null}
                  data-mime={a.mime}
                >
                  {#if a.kind === "video"}
                    <span
                      class="ri-chip-thumb video-thumb"
                      aria-hidden="true"
                      title="Screen recording"
                    >▶</span>
                  {:else if a.kind === "log"}
                    <span
                      class="ri-chip-thumb video-thumb"
                      aria-hidden="true"
                      title="Recording session bundle"
                    >ZIP</span>
                  {:else if a.previewUrl}
                    <img
                      class="ri-chip-thumb"
                      src={a.previewUrl}
                      alt={a.filename}
                    />
                  {:else}
                    <span class="ri-chip-thumb placeholder" aria-hidden="true"
                    ></span>
                  {/if}
                  <span class="ri-chip-name" title={a.filename}>{a.filename}</span>
                  <span class="ri-chip-size">{fmtBytes(a.sizeBytes)}</span>
                  {#if a.error}
                    <span class="ri-chip-error">{a.error}</span>
                  {/if}
                  {#if a.kind === "video" && !a.error}
                    <button
                      type="button"
                      class="ri-chip-preview"
                      aria-label={`Preview ${a.filename}`}
                      onclick={() => openPreview(a.localId)}
                      disabled={submitting}
                    >Preview</button>
                  {/if}
                  <button
                    type="button"
                    class="ri-chip-remove"
                    aria-label={`Remove ${a.filename}`}
                    onclick={() => removeAttachment(a.localId)}
                    disabled={submitting}
                  >×</button>
                </li>
              {/each}
            </ul>
          {/if}
          {#if oversizedCount > 0}
            <div class="ri-attach-warning">
              {oversizedCount} file{oversizedCount === 1 ? "" : "s"} won't be
              sent.
            </div>
          {/if}
          {#if recorderError}
            <div class="ri-attach-warning" role="alert">{recorderError}</div>
          {/if}
          <!-- #216: pre-record audio toggle. Shown only when the
               webview supports both display + user media; default
               off, so a user opting in is a deliberate consent
               choice. The notice copy below shifts when this is on. -->
          {#if canRecord && canCaptureMic}
            <label class="ri-audio-toggle">
              <input
                type="checkbox"
                bind:checked={audioEnabled}
                disabled={submitting || isRecording}
              />
              <span class="ri-audio-label">
                Capture microphone audio (your narration)
              </span>
            </label>
          {/if}
          <div class="ri-attach-actions">
            <button
              type="button"
              class="ri-attach-btn"
              onclick={addAttachments}
              disabled={submitting || isRecording || remainingSlots <= 0}
            >
              {#if attachments.length === 0}
                Add screenshots
              {:else if remainingSlots <= 0}
                5 of 5 — limit reached
              {:else}
                Add more
              {/if}
            </button>
            {#if canRecord && !isRecording}
              <button
                type="button"
                class="ri-attach-btn"
                onclick={startRecording}
                disabled={submitting || remainingSlots <= 0}
              >Record screen</button>
            {/if}
            {#if !isRecording}
              <button
                type="button"
                class="ri-attach-btn"
                onclick={attachLatestRecording}
                disabled={submitting ||
                  bundlingSession ||
                  sessionAttached ||
                  remainingSlots <= 0}
              >
                {#if bundlingSession}
                  Bundling…
                {:else if sessionAttached}
                  Last recording attached
                {:else}
                  Attach last recording
                {/if}
              </button>
            {/if}
          </div>
          {#if canRecord && !isRecording}
            <!-- #216: brief hint about what happens when the user
                 clicks Record screen. Keeps the surprise low and
                 tells them where to look for the stop control. -->
            <p class="ri-recorder-hint">
              Recording stays live while you navigate the app to
              demonstrate the issue. A floating control in the
              bottom-right corner shows the timer and lets you stop.
            </p>
          {/if}
          {#if !isRecording}
            <p class="ri-recorder-hint">
              The "Attach last recording" option packages the audio +
              system info from your most recent recording so we can
              diagnose what went wrong. Remove it from the chip row
              before submitting if you'd rather not include it.
            </p>
          {/if}
          {#if bundleError}
            <div class="ri-attach-warning" role="alert">{bundleError}</div>
          {/if}
        </div>

        <div class="ri-telemetry-summary">
          <button
            type="button"
            class="ri-telemetry-toggle"
            aria-expanded={telemetryOpen}
            aria-controls={`${titleId}-telem`}
            onclick={() => (telemetryOpen = !telemetryOpen)}
          >
            <span class="ri-telemetry-arrow" class:open={telemetryOpen}
              >▶</span
            >
            What's included with this report
          </button>
          <!-- Always mounted (with `hidden`) so the toggle's
               aria-controls reference always resolves. Using `{#if}`
               here would unmount the <ul> when collapsed, leaving
               aria-controls dangling — RC-2 from the #183 review. -->
          <ul
            id={`${titleId}-telem`}
            class="ri-telemetry-list"
            hidden={!telemetryOpen}
          >
            <li>Your agent version</li>
            <li>Your operating system</li>
            <li>Recent diagnostic logs from this session</li>
            <li>Window size and display theme</li>
            <li>Any screenshots or screen recordings you added</li>
          </ul>
        </div>

        <p class="ri-notice">
          <strong
            >Your message, any screenshots, and any screen recordings
            go to the aftercalls team.</strong
          >
          Attachments may include information visible on your screen —
          <strong>including passwords, messages, and other sensitive
          content</strong>.
          {#if audioEnabled && canCaptureMic}
            <strong
              >Your microphone will be captured for the duration of
              the recording.</strong
            >
          {/if}
          Review each item before sending. Recent diagnostic logs from
          this device are included automatically. By clicking "Send
          report" you consent to this.
        </p>
      {/if}
    </div>

    <div class="rn-actions ri-actions">
      {#if succeeded}
        <span></span>
        <button type="button" class="rn-dismiss" onclick={close}>Close</button>
      {:else if confirmingClose}
        <span class="ri-confirm-text">Close without sending?</span>
        <span class="ri-confirm-buttons">
          <button
            type="button"
            class="rn-link"
            onclick={() => {
              confirmingClose = false;
              close();
            }}>Yes, close</button
          >
          <button
            type="button"
            class="rn-dismiss"
            onclick={() => (confirmingClose = false)}>Keep writing</button
          >
        </span>
      {:else}
        <button
          type="button"
          class="rn-link"
          onclick={attemptClose}
          disabled={submitting}>Cancel</button
        >
        <button
          type="button"
          class="rn-dismiss rn-primary"
          onclick={submit}
          disabled={!canSubmit}
          aria-busy={submitting}
        >
          {submitting ? "Sending…" : "Send report"}
        </button>
      {/if}
    </div>
  </div>
</div>
{/if}

{#if previewAttachment && previewAttachment.kind === "video" && previewAttachment.previewUrl}
  <!-- Tier-2 preview modal. Same backdrop pattern as the main
       dialog so keyboard-only users can dismiss with Esc. We
       intentionally do NOT focus-trap the outer dialog away — the
       user lands back in the report modal on close. -->
  <div
    class="rn-backdrop preview-backdrop"
    role="button"
    tabindex="-1"
    aria-label="Close recording preview"
    onclick={closePreview}
    onkeydown={(e) => {
      if (e.key === "Escape") closePreview();
    }}
  >
    <div
      class="rn-modal preview-modal"
      role="dialog"
      aria-modal="true"
      aria-label={`Preview of ${previewAttachment.filename}`}
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          closePreview();
        }
      }}
      tabindex="-1"
    >
      <div class="rn-head">
        <h2>Recording preview</h2>
      </div>
      <div class="rn-body preview-body">
        <video
          class="preview-video"
          src={previewAttachment.previewUrl}
          controls
          preload="metadata"
        >
          <track kind="captions" />
        </video>
        <p class="preview-caption">
          {previewAttachment.filename} · {fmtBytes(previewAttachment.sizeBytes)}
        </p>
      </div>
      <div class="rn-actions">
        <span></span>
        <button
          type="button"
          class="rn-dismiss"
          onclick={closePreview}
        >Close preview</button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* Locally-scoped — nothing in this file edits app.css. The shared
   * `.rn-*` shell + tokens carry the modal skeleton; the rules below
   * style the form, chips, telemetry block, notice, success state,
   * and error banner. All colors come from existing tokens. */

  .ri-modal {
    max-width: 480px;
  }

  .ri-modal.dragging {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .ri-field {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    margin-bottom: 0.9rem;
  }

  .ri-field.has-error .ri-input,
  .ri-field.has-error .ri-textarea {
    border-color: var(--live);
  }

  .ri-label {
    color: var(--bone-2);
    font-size: 0.78rem;
    font-weight: 500;
    letter-spacing: 0.01em;
  }

  .ri-input,
  .ri-textarea {
    appearance: none;
    background: var(--ink-2);
    border: 1px solid var(--hairline-hi);
    border-radius: var(--radius-sm, 6px);
    color: var(--bone-1);
    font-family: var(--font-sans);
    font-size: 0.85rem;
    padding: 0.55rem 0.7rem;
    transition: border-color 0.15s, box-shadow 0.15s;
  }
  .ri-input:focus,
  .ri-textarea:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 25%, transparent);
  }
  .ri-textarea {
    resize: vertical;
    min-height: 5em;
    line-height: 1.5;
  }

  .ri-error {
    color: var(--live);
    font-size: 0.78rem;
  }

  .ri-attach-row {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
    margin-bottom: 0.9rem;
  }

  .ri-chip-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.45rem;
  }

  .ri-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.25rem 0.55rem 0.25rem 0.3rem;
    background: var(--ink-3);
    border: 1px solid var(--hairline-hi);
    border-radius: 999px;
    font-size: 0.78rem;
    color: var(--bone-1);
    max-width: 100%;
  }
  .ri-chip.has-error {
    border-color: var(--live);
  }
  /* Indeterminate "upload in flight" cue while the parent dialog is
   * `submitting`. CSS-only — no per-byte progress channel. Subtle
   * brightness/opacity pulse so multi-file uploads (up to 5 × 25 MB,
   * 20–30 s) don't look frozen. */
  .ri-chip.chip-uploading {
    animation: ri-chip-pulse 1.2s ease-in-out infinite alternate;
  }
  @keyframes ri-chip-pulse {
    from {
      opacity: 0.6;
      border-color: var(--hairline-hi);
    }
    to {
      opacity: 1;
      border-color: var(--accent);
    }
  }
  .ri-chip-thumb {
    width: 24px;
    height: 24px;
    object-fit: cover;
    border-radius: 4px;
    background: var(--ink-2);
    flex-shrink: 0;
  }
  .ri-chip-thumb.placeholder {
    display: inline-block;
    background: var(--ink-2);
  }
  /* #203: video thumbnail uses a simple play-glyph tile — deliberately
   * not a first-frame poster, which would need a hidden <video> +
   * canvas draw just for a chip visual. The "Preview" button in the
   * same chip opens the full <video> if the user wants to check
   * the recording before submitting. */
  .ri-chip-thumb.video-thumb {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--accent);
    font-size: 0.82rem;
    font-weight: 600;
    background: color-mix(in srgb, var(--accent) 15%, transparent);
  }
  .ri-chip-preview {
    appearance: none;
    background: transparent;
    border: 1px solid var(--hairline-hi);
    color: var(--bone-2);
    font-size: 0.72rem;
    padding: 0.15rem 0.5rem;
    border-radius: 999px;
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
  }
  .ri-chip-preview:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--bone-0);
  }
  .ri-chip-preview:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .ri-chip-name {
    max-width: 18ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ri-chip-size {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-3);
  }
  .ri-chip-error {
    color: var(--live);
    font-size: 0.72rem;
  }
  .ri-chip-remove {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--bone-3);
    font-size: 1.05rem;
    line-height: 1;
    padding: 0 0.2rem;
    cursor: pointer;
    border-radius: 4px;
    transition: color 0.15s, background 0.15s;
  }
  .ri-chip-remove:hover:not(:disabled) {
    color: var(--live);
    background: color-mix(in srgb, var(--live) 15%, transparent);
  }
  .ri-chip-remove:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .ri-attach-btn {
    appearance: none;
    border: 1px solid var(--hairline-hi);
    background: transparent;
    color: var(--bone-1);
    font-size: 0.82rem;
    padding: 0.4rem 0.8rem;
    border-radius: var(--radius-sm, 6px);
    cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
  }
  .ri-attach-btn:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--bone-0);
  }
  .ri-attach-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
    color: var(--bone-4);
  }

  /* #203: side-by-side action row for "Add screenshots" + "Record
   * screen". Was a single button that self-aligned flex-start; now
   * wrapped so both buttons sit together and wrap naturally on
   * narrow widths. */
  .ri-attach-actions {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
  }

  /* #216: pre-record audio toggle. Reads as an inline option
   * directly above the two action buttons. Off by default; the
   * label is sized to be obvious without dominating the dialog. */
  .ri-audio-toggle {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    color: var(--bone-2);
    font-size: 0.78rem;
    cursor: pointer;
    user-select: none;
    align-self: flex-start;
  }
  .ri-audio-toggle input[type="checkbox"] {
    accent-color: var(--accent);
    cursor: pointer;
  }
  .ri-audio-toggle input[type="checkbox"]:disabled {
    cursor: not-allowed;
  }
  .ri-audio-toggle:has(input:disabled) {
    color: var(--bone-4);
    cursor: not-allowed;
  }
  .ri-audio-label {
    line-height: 1.3;
  }

  /* #216: the in-dialog live-recorder strip from #203 was removed.
   * The dialog now hides itself entirely while recording is live;
   * the floating control component carries the live UI. The hint
   * style below survives — used pre-record to set expectations. */
  .ri-recorder-hint {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.78rem;
    line-height: 1.4;
  }

  /* #203: tier-2 preview modal. Sits above the main report dialog
   * by virtue of being rendered after it in DOM order (both are
   * position-fixed z-index-default from `.rn-backdrop`), so the
   * second backdrop covers the first. */
  .preview-backdrop {
    /* Bump z-index over the base rn-backdrop so click-through goes
     * to the preview first. */
    z-index: 100;
  }
  .preview-modal {
    max-width: 640px;
  }
  .preview-body {
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  .preview-video {
    width: 100%;
    max-height: 420px;
    background: var(--ink-3);
    border-radius: var(--radius-sm, 6px);
  }
  .preview-caption {
    margin: 0;
    color: var(--bone-3);
    font-size: 0.78rem;
    text-align: center;
  }

  .ri-attach-warning {
    color: var(--live);
    font-size: 0.78rem;
  }

  .ri-telemetry-summary {
    margin-bottom: 0.9rem;
  }
  .ri-telemetry-toggle {
    appearance: none;
    background: none;
    border: none;
    padding: 0;
    color: var(--bone-3);
    font-size: 0.78rem;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
  }
  .ri-telemetry-toggle:hover {
    color: var(--bone-1);
  }
  .ri-telemetry-arrow {
    display: inline-block;
    transition: transform 0.15s;
    font-size: 0.7rem;
  }
  .ri-telemetry-arrow.open {
    transform: rotate(90deg);
  }
  .ri-telemetry-list {
    margin: 0.4rem 0 0;
    padding: 0.55rem 0.85rem 0.55rem 1.5rem;
    background: var(--ink-2);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-sm, 6px);
    color: var(--bone-3);
    font-size: 0.78rem;
    line-height: 1.7;
  }

  .ri-notice {
    color: var(--bone-3);
    font-size: 0.78rem;
    line-height: 1.5;
    margin: 0 0 0.5rem;
  }
  .ri-notice strong {
    color: var(--bone-1);
    font-weight: 600;
  }

  .ri-error-banner {
    border: 1px solid var(--live);
    border-left-width: 4px;
    background: color-mix(in srgb, var(--live) 10%, transparent);
    color: var(--bone-1);
    font-size: 0.85rem;
    padding: 0.6rem 0.85rem;
    border-radius: var(--radius-sm, 6px);
    margin-bottom: 0.9rem;
  }
  .ri-error-text {
    white-space: pre-line;
  }

  /* Success state replaces the body content. */
  .ri-success {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.6rem;
    padding: 1.2rem 0;
    text-align: center;
  }
  .ri-success-disc {
    width: 38px;
    height: 38px;
    border-radius: 50%;
    background: color-mix(in srgb, var(--olive) 22%, transparent);
    color: var(--olive);
    font-size: 1.4rem;
    line-height: 38px;
    text-align: center;
    font-weight: 600;
  }
  .ri-success-heading {
    color: var(--bone-0);
    font-size: 1rem;
    font-weight: 600;
  }
  .ri-success-sub {
    color: var(--bone-2);
    font-size: 0.85rem;
  }

  .ri-actions {
    align-items: center;
  }
  .ri-confirm-text {
    color: var(--bone-2);
    font-size: 0.82rem;
  }
  .ri-confirm-buttons {
    display: inline-flex;
    gap: 0.5rem;
  }

  /* Local Cancel-style ghost button matching the Cancel slots in
   * impersonation start + speaker-rename pickers. Component-scoped
   * so we don't add a new shared `.rn-link` rule to app.css. */
  .rn-link {
    appearance: none;
    background: transparent;
    border: 1px solid var(--hairline-hi);
    color: var(--bone-2);
    font-size: 0.85rem;
    padding: 0.5rem 0.95rem;
    border-radius: 8px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }
  .rn-link:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--bone-2);
  }
  .rn-link:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .sr-only {
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

  @media (prefers-reduced-motion: reduce) {
    .ri-telemetry-arrow,
    .ri-input,
    .ri-textarea,
    .ri-attach-btn,
    .ri-chip-remove {
      transition: none;
    }
    .ri-chip.chip-uploading {
      animation: none;
      opacity: 0.85;
    }
  }
</style>
