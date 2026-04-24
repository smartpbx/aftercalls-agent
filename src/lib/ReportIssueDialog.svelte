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
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { getVersion } from "@tauri-apps/api/app";
  import { getCurrentWindow } from "@tauri-apps/api/window";

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
  type Attachment = {
    /** Stable client-side id; server assigns its own UUID once submit lands. */
    localId: string;
    filePath: string;
    filename: string;
    mime: string;
    sizeBytes: number;
    /** dataURL preview rendered as the chip thumbnail. */
    previewUrl: string | null;
    /** Set when the file fails local validation (size/type). Excluded from submit. */
    error: string | null;
  };
  let attachments = $state<Attachment[]>([]);
  const MAX_ATTACHMENTS = 5;
  const MAX_ATTACHMENT_BYTES = 25 * 1024 * 1024;
  const ALLOWED_MIME = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
  ];

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
  });

  onDestroy(() => {
    // Revoke all object URLs we minted so the webview doesn't leak
    // bytes for the lifetime of the page.
    for (const a of attachments) {
      if (a.previewUrl) URL.revokeObjectURL(a.previewUrl);
    }
    if (openerEl && document.contains(openerEl)) {
      try {
        openerEl.focus();
      } catch {
        /* opener detached — fall through */
      }
    }
  });

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
      const result = await openDialog({
        multiple: true,
        filters: [
          {
            name: "Images",
            extensions: ["png", "jpg", "jpeg", "webp", "gif"],
          },
        ],
      });
      if (!result) return;
      const paths = Array.isArray(result) ? result : [result];
      const slice = paths.slice(0, remaining);
      for (const p of slice) {
        await stageFromPath(p);
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
  async function stageFromPath(path: string) {
    try {
      const meta = await invoke<{
        path: string;
        filename: string;
        mime: string;
        size_bytes: number;
        preview_data_url: string | null;
      }>("inspect_support_attachment", { path });

      const sizeError =
        meta.size_bytes > MAX_ATTACHMENT_BYTES
          ? "Too large (max 25 MB)"
          : null;
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
        },
      ];
    } catch (e) {
      console.warn("ReportIssueDialog: inspect failed for", path, e);
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
      await invoke("submit_support_report", {
        title: title.trim(),
        body: body.trim(),
        metadata,
        attachments: valid.map((a) => ({
          path: a.filePath,
          filename: a.filename,
          mime: a.mime,
          size_bytes: a.sizeBytes,
        })),
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

  // Drag-and-drop (v1-optional). Accepts dropped files exactly the
  // same way as the file picker — same validation, same chips.
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
    const files = e.dataTransfer?.files;
    if (!files || files.length === 0) return;
    const remaining = MAX_ATTACHMENTS - attachments.length;
    const slice = Array.from(files).slice(0, remaining);
    // Tauri exposes a path on the File via a non-standard property.
    // Fall back to skipping if we can't get one (the picker path is
    // still the primary entry point).
    for (const f of slice) {
      const p = (f as unknown as { path?: string }).path;
      if (p) await stageFromPath(p);
    }
  }

  const remainingSlots = $derived(MAX_ATTACHMENTS - attachments.length);
  const canSubmit = $derived(
    !submitting && !succeeded && title.trim() !== "" && body.trim() !== "",
  );
  const totalAttempted = $derived(attachments.length);
  const oversizedCount = $derived(
    attachments.filter((a) => a.error != null).length,
  );
</script>

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
            <ul class="ri-chip-list" aria-label="Attached screenshots">
              {#each attachments as a (a.localId)}
                <li
                  class="ri-chip"
                  class:has-error={a.error != null}
                  class:chip-uploading={submitting && a.error == null}
                  data-mime={a.mime}
                >
                  {#if a.previewUrl}
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
          <button
            type="button"
            class="ri-attach-btn"
            onclick={addAttachments}
            disabled={submitting || remainingSlots <= 0}
          >
            {#if attachments.length === 0}
              Add screenshots
            {:else if remainingSlots <= 0}
              5 of 5 — limit reached
            {:else}
              Add more
            {/if}
          </button>
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
          </ul>
        </div>

        <p class="ri-notice">
          <strong
            >Your message and any screenshots go to the aftercalls
            team.</strong
          >
          Attachments may include information from your account (call
          titles, transcripts, contact names) — only the team will see
          them. Recent diagnostic logs from this device are included
          automatically. By clicking "Send report" you consent to this.
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
    align-self: flex-start;
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
