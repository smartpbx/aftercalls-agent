<script lang="ts">
  // #592 — agent surface parity with the portal's `/settings/privacy`.
  // Mirrors `portal/src/routes/settings/privacy/+page.svelte` byte-for-
  // byte UX-wise: same four sections backed by `myPrivacy.bundle()`,
  // same export action card on top, same status-pill states, same
  // access-log "Load more" pagination. The differences are purely
  // mechanical:
  //   - imports `$lib/api` which fronts Tauri commands instead of
  //     `fetch` (see the agent-side `api.ts` for the wire-shape parity
  //     contract).
  //   - the unauth bounce reads `PortalError`'s structured shape
  //     instead of regex-sniffing `String(e)` — matches the rest of
  //     the agent's `#124`-era error handling.
  //   - `openUrl` from the Tauri opener plugin opens external links
  //     so the system browser handles them rather than the webview.
  //
  // When the umbrella `packages/shared` lift (#355 / #446) lands, this
  // page and its portal counterpart should fold into a shared
  // component. Until then, keep them in sync section-for-section.

  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import * as api from "$lib/api";
  import { fmtDateTimeLocal } from "$lib/fmtDate";
  import { toast } from "$lib/stores/toast.svelte";
  import { isPortalError } from "$lib/portalError";

  let bundle = $state<api.MyPrivacyBundle | null>(null);
  let loading = $state(true);
  let loadErr = $state("");

  // Access-log pagination state. We start from the bundle's first
  // page; subsequent pages append to `accessRows` and update the
  // cursor. `loadingMore` covers the "Load more" CTA's spinner.
  let accessRows = $state<api.MyAccessLogRow[]>([]);
  let nextCursor = $state<string | null>(null);
  let loadingMore = $state(false);

  // ── Data export (#506) ─────────────────────────────────────────────
  // The list endpoint returns up to 50 rows newest-first; we render
  // the most recent (the head of the list) inline as the "current"
  // export, and let the rest collapse into a history accordion. This
  // keeps the section short on a fresh account (zero rows = explainer
  // only) and informative on a returning account (one row visible,
  // older rows tucked away).
  let exports = $state<api.DataExportRow[]>([]);
  let exportsLoaded = $state(false);
  let exportLoadErr = $state("");
  let requesting = $state(false);
  let showHistory = $state(false);

  // Latest row drives the request button's enabled-state + the status
  // pill. `null` means "no exports yet".
  let latest = $derived<api.DataExportRow | null>(exports[0] ?? null);
  let inFlight = $derived(
    !!latest && (latest.status === "pending" || latest.status === "running"),
  );
  // Cooldown is enforced server-side (24h). The agent never sees the
  // cooldown row directly; we infer "probably cooling down" from the
  // most recent successful request being < 24h old. This is purely a
  // UX hint — the backend is the authoritative gate, and a 400
  // response surfaces a precise retry-after.
  let cooldownActive = $derived(() => {
    if (!latest) return false;
    if (latest.status !== "ready" && latest.status !== "expired") return false;
    const requestedMs = Date.parse(latest.requested_at);
    if (Number.isNaN(requestedMs)) return false;
    return Date.now() - requestedMs < 24 * 60 * 60 * 1000;
  });

  function statusLabel(s: api.DataExportStatus): string {
    if (s === "pending") return "Queued";
    if (s === "running") return "Packaging";
    if (s === "ready") return "Ready";
    if (s === "failed") return "Failed";
    if (s === "expired") return "Expired";
    return s;
  }

  function formatBytes(b: number | null): string {
    if (b == null) return "";
    if (b < 1024) return `${b} B`;
    if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
    if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB`;
    return `${(b / 1024 / 1024 / 1024).toFixed(2)} GB`;
  }

  // PortalError → human string. The agent surface uses the structured
  // error shape (#124) rather than the portal's `String(e)`, so the
  // privacy page reads `kind` for the unauth bounce and falls back
  // to a flat text rendering for everything else. Bad-request bodies
  // (e.g. cooldown / in-flight collisions) still come through as a
  // string we pattern-match below.
  function errText(e: unknown): string {
    if (isPortalError(e)) {
      if (e.kind === "bad_request" || e.kind === "server" || e.kind === "other" || e.kind === "network") {
        return e.message ?? "";
      }
      if (e.kind === "cooldown") return `retry_after_seconds=${e.retry_after_seconds}`;
      return e.kind;
    }
    return String((e as { message?: unknown })?.message ?? e);
  }

  async function loadExports() {
    try {
      const res = await api.dataExports.list();
      exports = res.exports;
      exportLoadErr = "";
    } catch (e: unknown) {
      exportLoadErr = errText(e);
    } finally {
      exportsLoaded = true;
    }
  }

  async function requestExport() {
    if (requesting || inFlight) return;
    requesting = true;
    try {
      await api.dataExports.request();
      toast.success(
        "Export requested. We'll email you when your archive is ready (usually under a minute).",
      );
      await loadExports();
    } catch (e: unknown) {
      const msg = errText(e);
      // Backend surfaces rate-limit as 400 with `rate_limited:
      // retry_after_seconds=N`; show the seconds-as-hours hint so the
      // user doesn't have to do the math themselves.
      const m = msg.match(/retry_after_seconds=(\d+)/);
      if (m) {
        const hours = Math.max(1, Math.ceil(Number(m[1]) / 3600));
        toast.error(
          `You can request another export in about ${hours} hour${hours === 1 ? "" : "s"}.`,
        );
      } else if (/export_in_flight/i.test(msg)) {
        toast.error(
          "An export is already running. We'll email you when it's ready.",
        );
        await loadExports();
      } else {
        toast.error(`Couldn't request the export: ${msg}`);
      }
    } finally {
      requesting = false;
    }
  }

  /** Fetch a fresh row (with download_url re-presigned) and open it.
   *  We don't trust whatever URL the list page paint had — it might
   *  be older than the 24h presigned link's TTL, especially on a
   *  long-open tab. */
  async function downloadLatest() {
    if (!latest) return;
    try {
      const row = await api.dataExports.getStatus(latest.id);
      if (row.download_url) {
        // Use a hidden anchor so the click goes through the browser's
        // download path rather than navigating the SPA.
        const a = document.createElement("a");
        a.href = row.download_url;
        a.rel = "noopener";
        // Object-storage default disposition is `attachment`, so the
        // browser will save rather than try to render the .zip.
        a.click();
        // Keep the row in sync so the "expires" hint stays accurate.
        exports = exports.map((r) => (r.id === row.id ? row : r));
      } else {
        toast.error(
          "The download link couldn't be re-issued. Try again in a moment.",
        );
      }
    } catch (e: unknown) {
      toast.error(`Couldn't fetch the download link: ${errText(e)}`);
    }
  }

  onMount(async () => {
    try {
      bundle = await api.myPrivacy.bundle();
      accessRows = bundle.access_log.rows;
      nextCursor = bundle.access_log.next_cursor;
    } catch (e: unknown) {
      // Unauth → bounce to login (matches the rest of the agent's
      // PortalError handling). Anything else surfaces as an error
      // banner so the user knows the page didn't load rather than
      // seeing a silent empty render.
      if (isPortalError(e) && e.kind === "unauthorized") {
        await goto("/login");
        return;
      }
      loadErr = errText(e);
    } finally {
      loading = false;
    }
    // Load the export history independently of the bundle — a slow
    // bundle endpoint shouldn't gate the export action.
    await loadExports();
  });

  async function loadMore() {
    if (!nextCursor || loadingMore) return;
    loadingMore = true;
    try {
      const page = await api.myPrivacy.accessLog(nextCursor);
      accessRows = [...accessRows, ...page.rows];
      nextCursor = page.next_cursor;
    } catch (e: unknown) {
      loadErr = errText(e);
    } finally {
      loadingMore = false;
    }
  }

  // Pretty-print the TOS kind for the section's row label. Keeps the
  // rest of the codebase's "terms" / "privacy" lowercase tags
  // unchanged — this is a render-only transform.
  function tosKindLabel(kind: string): string {
    if (kind === "terms") return "Terms of Service";
    if (kind === "privacy") return "Privacy Policy";
    return kind;
  }

  // The access_kind enum carries two values today; future-proof the
  // render so a new kind doesn't fall through as a blank cell.
  function accessKindLabel(kind: string): string {
    if (kind === "view") return "Opened the call page";
    if (kind === "audio") return "Streamed the audio";
    return kind;
  }
</script>

<svelte:head>
  <title>Privacy summary · aftercalls</title>
</svelte:head>

<main class="page">
  <a class="back" href="/settings">← Back to Settings</a>
  <h1>Privacy summary</h1>
  <p class="lead">
    Everything aftercalls knows about your account, in one place.
  </p>

  <!-- ── Export my data (#506) ───────────────────────────────────
       Action card sits at the top so the read-only summary that
       follows reads as context for what's in the archive. Renders
       independently of the privacy bundle — a slow bundle endpoint
       shouldn't gate the export action. -->
  <section class="card">
    <h2>Export my data</h2>
    <p class="meta">
      Download a single archive containing your calls, transcripts,
      summaries, action items, highlights, your private notes, and
      the audio. The archive also includes an information packet
      explaining how aftercalls processes your data — your rights
      under PIPEDA, Quebec's Law 25, and GDPR Articles 15 and 20.
    </p>
    <p class="meta">
      We'll email you when it's ready (usually within a minute). The
      download link works for 24 hours; the archive itself stays
      available for 7 days, then we delete it from our storage.
      Exports include your most recent 200 calls — contact support if
      you need a full export of an older account.
    </p>
    {#if exportLoadErr}
      <p class="err" role="alert">
        Couldn't load your export history: {exportLoadErr}
      </p>
    {/if}
    {#if exportsLoaded && latest}
      <div class="export-current">
        <div class="export-row">
          <div class="export-row-main">
            <span
              class="status-pill status-pill-{latest.status}"
              aria-label="Status: {statusLabel(latest.status)}"
            >
              {statusLabel(latest.status)}
            </span>
            <span class="export-when">
              Requested {fmtDateTimeLocal(latest.requested_at)}
            </span>
          </div>
          {#if latest.status === "ready"}
            <p class="meta">
              {#if latest.bytes != null}
                {formatBytes(latest.bytes)}
              {/if}
              {#if latest.call_count != null}
                · {latest.call_count} call{latest.call_count === 1 ? "" : "s"}
              {/if}
              {#if latest.audio_count != null}
                · {latest.audio_count} audio file{latest.audio_count === 1 ? "" : "s"}
              {/if}
            </p>
            {#if latest.expires_at}
              <p class="meta">
                Expires {fmtDateTimeLocal(latest.expires_at)} — after that,
                request a new export.
              </p>
            {/if}
          {:else if latest.status === "failed"}
            <p class="err" role="alert">
              Something went wrong assembling your archive
              {#if latest.error_message && latest.error_message !== "orphaned_on_restart"}
                ({latest.error_message})
              {/if}.
              You can request a new export — the rate-limit doesn't
              count failed attempts.
            </p>
          {:else if latest.status === "expired"}
            <p class="meta">
              The download window for this archive has passed. Request
              a new export to download a fresh copy.
            </p>
          {:else if latest.status === "pending" || latest.status === "running"}
            <p class="meta">
              Your archive is being assembled. We'll email you the
              download link when it's ready.
            </p>
          {/if}
        </div>
      </div>
    {/if}
    <div class="export-actions">
      {#if latest && latest.status === "ready"}
        <button class="btn primary" onclick={downloadLatest}>
          Download archive
        </button>
      {/if}
      <button
        class="btn"
        onclick={requestExport}
        disabled={requesting || inFlight || (cooldownActive() && !!latest && latest.status === "ready")}
        aria-label={inFlight
          ? "Export in progress"
          : "Request a new data export"}
      >
        {#if requesting}
          Requesting…
        {:else if inFlight}
          Export in progress
        {:else if latest}
          Request a new export
        {:else}
          Request export
        {/if}
      </button>
    </div>
    {#if exportsLoaded && exports.length > 1}
      <div class="export-history">
        <button
          class="history-toggle"
          type="button"
          onclick={() => (showHistory = !showHistory)}
          aria-expanded={showHistory}
        >
          {showHistory ? "Hide" : "Show"} previous exports ({exports.length - 1})
        </button>
        {#if showHistory}
          <ul class="rows">
            {#each exports.slice(1) as row (row.id)}
              <li class="row">
                <div class="row-main">
                  <span class="status-pill status-pill-{row.status}">
                    {statusLabel(row.status)}
                  </span>
                  <span class="row-when">
                    {fmtDateTimeLocal(row.requested_at)}
                  </span>
                </div>
                {#if row.bytes != null || row.call_count != null}
                  <div class="row-aux">
                    {#if row.bytes != null}
                      <span>{formatBytes(row.bytes)}</span>
                    {/if}
                    {#if row.call_count != null}
                      <span>
                        {row.call_count} call{row.call_count === 1 ? "" : "s"}
                      </span>
                    {/if}
                  </div>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/if}
  </section>

  {#if loading}
    <p class="meta">Loading…</p>
  {:else if loadErr}
    <p class="err" role="alert">Couldn't load your privacy summary: {loadErr}</p>
  {:else if bundle}
    <!-- ── Account ─────────────────────────────────────────────── -->
    <section class="card">
      <h2>Account</h2>
      <dl class="kv">
        <div class="kv-row">
          <dt>Joined aftercalls</dt>
          <dd>{fmtDateTimeLocal(bundle.joined_at)}</dd>
        </div>
      </dl>
    </section>

    <!-- ── Terms accepted ──────────────────────────────────────── -->
    <section class="card">
      <h2>Terms accepted</h2>
      {#if bundle.tos_acceptances.length === 0}
        <p class="meta">
          You haven't accepted any version of the terms or privacy
          policy yet. You'll be prompted the next time one is
          required.
        </p>
      {:else}
        <p class="meta">
          The full audit trail, newest first. Each row is a separate
          acceptance — a re-acceptance after a new version is its own
          row.
        </p>
        <ul class="rows">
          {#each bundle.tos_acceptances as row (row.version_id)}
            <li class="row">
              <div class="row-main">
                <span class="row-title">{tosKindLabel(row.kind)}</span>
                <span class="row-meta mono">{row.slug}</span>
              </div>
              <div class="row-aux">
                Accepted {fmtDateTimeLocal(row.accepted_at)}
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <!-- ── Calls ───────────────────────────────────────────────── -->
    <section class="card">
      <h2>Calls</h2>
      <p class="meta">
        Calls you've recorded that are still in your library
        (excluding anything in the trash).
      </p>
      <p class="big-number mono">{bundle.calls_count.toLocaleString()}</p>
      <p class="meta">
        Trashed calls are kept for 30 days before they're permanently
        deleted from storage.
      </p>
    </section>

    <!-- ── Access log ──────────────────────────────────────────── -->
    <section class="card">
      <h2>Who has opened your calls</h2>
      <p class="meta">
        When an admin or another team member opens one of your
        recordings, we record who, when, and which call. You always
        see your own recordings, and those reads are not logged.
      </p>
      {#if accessRows.length === 0}
        <p class="meta empty">
          Nobody else has opened your calls yet.
        </p>
      {:else}
        <ul class="rows">
          {#each accessRows as row (row.id)}
            <li class="row">
              <div class="row-main">
                <span class="row-title">{row.viewer_display_name}</span>
                <span class="row-meta">
                  {accessKindLabel(row.access_kind)}
                </span>
              </div>
              <div class="row-aux">
                <span class="row-call">
                  {row.call_title ?? "Untitled call"}
                </span>
                <span class="row-when">
                  {fmtDateTimeLocal(row.accessed_at)}
                </span>
              </div>
            </li>
          {/each}
        </ul>
        {#if nextCursor}
          <button
            class="btn"
            onclick={loadMore}
            disabled={loadingMore}
          >
            {loadingMore ? "Loading…" : "Load more"}
          </button>
        {/if}
      {/if}
    </section>
  {/if}
</main>

<style>
  /* Mirrors the `.card` / typography pattern from `/settings`. All
     CSS is component-scoped — no `app.css` edits, so the byte-
     identical portal/agent invariant is trivially preserved.

     The portal counterpart (#514 + #506) is this file's source of
     truth; keep them in sync. */

  .page {
    max-width: 720px;
    margin: 0 auto;
    padding: 2rem;
    display: flex;
    flex-direction: column;
    gap: 1.2rem;
    position: relative;
    z-index: 2;
  }
  .back {
    color: var(--bone-2);
    text-decoration: none;
    font-size: 0.85rem;
    align-self: flex-start;
  }
  .back:hover {
    color: var(--bone-0);
    text-decoration: underline;
  }
  h1 {
    margin: 0 0 0.3rem;
  }
  .lead {
    margin: 0 0 0.6rem;
    color: var(--bone-2);
    font-size: 0.95rem;
    line-height: 1.5;
  }
  .card {
    padding: 1.3rem 1.4rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  h2 {
    margin: 0;
    font-size: 0.72rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .meta {
    margin: 0;
    font-size: 0.85rem;
    color: var(--bone-2);
    line-height: 1.5;
  }
  .meta.empty {
    color: var(--bone-3);
    font-style: italic;
  }
  .mono {
    font-family: var(--font-mono);
    font-size: 0.82rem;
    color: var(--bone-1);
  }
  .big-number {
    margin: 0;
    font-size: 2.2rem;
    line-height: 1.1;
    color: var(--bone-0);
    font-family: var(--font-mono);
    font-weight: 500;
  }
  /* Key-value list (used for the Account card today; safe shape for
     future single-row sections). */
  .kv {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .kv-row {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .kv dt {
    font-size: 0.78rem;
    color: var(--bone-2);
    font-weight: 500;
  }
  .kv dd {
    margin: 0;
    color: var(--bone-0);
    font-size: 0.95rem;
  }
  /* Generic row list — used by Terms accepted + Access log so the
     visual rhythm matches across both. */
  .rows {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .row {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    padding: 0.6rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
  }
  .row-main {
    display: flex;
    align-items: baseline;
    gap: 0.7rem;
    flex-wrap: wrap;
  }
  .row-title {
    font-weight: 500;
    color: var(--bone-0);
    font-size: 0.92rem;
  }
  .row-meta {
    color: var(--bone-2);
    font-size: 0.8rem;
  }
  .row-aux {
    display: flex;
    align-items: baseline;
    gap: 0.7rem;
    flex-wrap: wrap;
    color: var(--bone-2);
    font-size: 0.82rem;
  }
  .row-call {
    color: var(--bone-1);
  }
  .row-when {
    color: var(--bone-3);
    font-size: 0.78rem;
  }
  .btn {
    padding: 0.55rem 1rem;
    font-size: 0.85rem;
    font-weight: 500;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    cursor: pointer;
    transition: all 0.15s;
    align-self: flex-start;
  }
  .btn:hover:not(:disabled) {
    background: var(--ink-3);
    color: var(--bone-0);
  }
  .btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .err {
    color: var(--live);
    font-size: 0.85rem;
  }

  /* Mobile pass — match `/settings` (640px primary, 16px input
     font to dodge iOS focus-zoom). Component-scoped; no app.css
     touch. */
  @media (max-width: 640px) {
    .page {
      padding: 1.25rem 1rem;
      gap: 1rem;
    }
    .card {
      padding: 1.1rem 1rem;
      gap: 0.85rem;
    }
    .big-number {
      font-size: 1.8rem;
    }
  }

  /* #506 — Export-my-data section. Component-scoped (no app.css
     touch, so the byte-identical portal/agent invariant holds).
     Status pill colors reuse the existing semantic palette: olive
     (saved/complete), sig (in-flight pipeline), live (failure),
     bone-3 (terminal/inert). */
  .primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--ink-0);
    font-weight: 600;
  }
  .primary:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .export-current {
    margin-top: 0.2rem;
  }
  .export-row {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.7rem 0.85rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
  }
  .export-row-main {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    flex-wrap: wrap;
  }
  .export-when {
    color: var(--bone-3);
    font-size: 0.82rem;
  }
  .export-actions {
    display: flex;
    gap: 0.6rem;
    flex-wrap: wrap;
    margin-top: 0.3rem;
  }
  .status-pill {
    display: inline-block;
    padding: 0.1rem 0.55rem;
    border-radius: 999px;
    font-size: 0.7rem;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    border: 1px solid var(--hairline);
    color: var(--bone-2);
    background: var(--ink-2);
  }
  .status-pill-pending,
  .status-pill-running {
    color: var(--sig);
    border-color: color-mix(in srgb, var(--sig) 45%, transparent);
    background: color-mix(in srgb, var(--sig) 10%, transparent);
  }
  .status-pill-ready {
    color: var(--olive);
    border-color: color-mix(in srgb, var(--olive) 45%, transparent);
    background: color-mix(in srgb, var(--olive) 10%, transparent);
  }
  .status-pill-failed {
    color: var(--live);
    border-color: color-mix(in srgb, var(--live) 45%, transparent);
    background: color-mix(in srgb, var(--live) 10%, transparent);
  }
  .status-pill-expired {
    color: var(--bone-3);
  }
  .export-history {
    margin-top: 0.4rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .history-toggle {
    align-self: flex-start;
    background: transparent;
    border: none;
    padding: 0;
    color: var(--bone-2);
    font-size: 0.82rem;
    text-decoration: underline;
    cursor: pointer;
  }
  .history-toggle:hover {
    color: var(--bone-0);
  }

  /* Mobile: stack the two action buttons full-width so they hit the
     ≥44px tap target. */
  @media (max-width: 640px) {
    .export-actions .btn {
      width: 100%;
      min-height: 44px;
    }
  }
</style>
