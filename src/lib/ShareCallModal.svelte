<script lang="ts">
  // #35 #240 #241 #242 Share-call modal. Three vertical sections:
  //   1. "What's included" — six per-link toggles (#240). manual_notes
  //      and allow_download default OFF (privacy-first); the others
  //      default ON so the v0.7.2 full-payload behaviour is preserved
  //      for users who don't touch the row.
  //   2. "Create new share" — pick expiry (7d / 30d / never), submit
  //      a wide primary button (#241), reveals URL with copy. Token
  //      is shown ONCE; once the modal closes (or the user creates a
  //      second share) the URL is gone forever — only the SHA256
  //      hash lives in the DB.
  //   3. "Manage existing shares" — list rows with view count, status
  //      pill (active / expired / revoked), per-link toggle chips
  //      (e.g. "Notes hidden", "Download allowed"), and a Revoke
  //      button.
  //
  // Shell: reuses the shared `.rn-*` classes from app.css (same as
  // ImpersonationStartModal / TosConfirmModal). All component-local
  // styling lives in the scoped style block below — no app.css edit,
  // mirror pair invariant intact.
  //
  // Vendor opacity: copy mentions "aftercalls" only. The body warns
  // the link gives full read access — no internal tools or vendor
  // names are exposed.

  import { onMount, tick } from "svelte";

  // The modal is surface-agnostic — both portal and agent inject the
  // share API funcs via these props. Same pattern as SendToZohoModal:
  // the component never imports `$lib/api` itself, so the agent-side
  // mirror copy can swap a Tauri-command shim in without diverging.
  type IncludedSections = {
    manual_notes: boolean;
    summary: boolean;
    action_items: boolean;
    transcript: boolean;
    audio: boolean;
    allow_download: boolean;
  };
  type ShareCreated = {
    id: string;
    token: string;
    url: string;
    expires_at: string | null;
    created_at: string;
    included_sections: IncludedSections;
  };
  type ShareSummary = {
    id: string;
    call_id: string;
    url: null;
    created_by: string | null;
    created_at: string;
    expires_at: string | null;
    revoked_at: string | null;
    view_count: number;
    status: "active" | "expired" | "revoked";
    included_sections: IncludedSections;
  };

  interface Props {
    callId: string;
    callTitle?: string | null;
    api: {
      createShare: (
        id: string,
        expiresInDays: number | null,
        includedSections?: IncludedSections,
      ) => Promise<ShareCreated>;
      listShares: (id: string) => Promise<ShareSummary[]>;
      revokeShare: (callId: string, shareId: string) => Promise<void>;
    };
    onClose: () => void;
  }

  let { callId, callTitle = null, api, onClose }: Props = $props();

  // Default toggle state — privacy-first on manual_notes +
  // allow_download (those should be opt-in for every link); the rest
  // default ON. Reset on every modal mount so a fresh open always
  // starts from the safe default.
  function defaultIncludedSections(): IncludedSections {
    return {
      manual_notes: false,
      summary: true,
      action_items: true,
      transcript: true,
      audio: true,
      allow_download: false,
    };
  }
  let included = $state<IncludedSections>(defaultIncludedSections());

  // 7d default — short enough that a forgotten link expires before
  // it accumulates a long view tail; long enough that real recipients
  // have time to open it. The issue body's recommendation; the modal
  // just surfaces it as the pre-selected radio.
  let expiryChoice = $state<"7" | "30" | "never">("7");
  let creating = $state(false);
  let createError = $state("");
  // Most recent created share — surfaced inline with a copy-to-
  // clipboard button. Cleared when the user creates another one (the
  // previous URL is forever-gone the moment we drop it from state).
  let lastCreated = $state<ShareCreated | null>(null);
  let copied = $state(false);
  let shares = $state<ShareSummary[]>([]);
  let listLoading = $state(true);
  let listError = $state("");
  let revokingId = $state<string | null>(null);
  let modalEl = $state<HTMLDivElement | null>(null);

  const titleId = `share-modal-title-${Math.random().toString(36).slice(2, 9)}`;

  onMount(async () => {
    await tick();
    // Focus the expiry fieldset's first radio so keyboard users land
    // on the create form, not on the close button.
    modalEl?.querySelector<HTMLInputElement>("input[type=radio]")?.focus();
    await refreshList();
  });

  async function refreshList() {
    listLoading = true;
    listError = "";
    try {
      shares = await api.listShares(callId);
    } catch (e) {
      listError = e instanceof Error ? e.message : "Couldn't load shares";
    } finally {
      listLoading = false;
    }
  }

  async function submitCreate() {
    if (creating) return;
    creating = true;
    createError = "";
    copied = false;
    try {
      const days =
        expiryChoice === "never" ? null : Number.parseInt(expiryChoice, 10);
      // Send a copy of the toggle state — the surface-agnostic
      // contract is "the modal owns its own state; the parent passes
      // through to the backend verbatim".
      const created = await api.createShare(callId, days, { ...included });
      lastCreated = created;
      // Reset toggles after a successful create so the next link
      // doesn't accidentally inherit a one-off "let manual notes
      // through" choice from the previous share.
      included = defaultIncludedSections();
      await refreshList();
    } catch (e) {
      createError = e instanceof Error ? e.message : "Couldn't create share";
    } finally {
      creating = false;
    }
  }

  async function copyUrl() {
    if (!lastCreated?.url) return;
    try {
      // The backend may return a relative `/c/<token>` URL when the
      // deploy doesn't have `PORTAL_BASE_URL` set. Resolve it against
      // the current origin so the copied URL is always paste-ready.
      const absolute = new URL(
        lastCreated.url,
        window.location.origin,
      ).toString();
      await navigator.clipboard.writeText(absolute);
      copied = true;
      window.setTimeout(() => {
        copied = false;
      }, 2000);
    } catch {
      // Clipboard API can reject in insecure contexts / older
      // browsers. Surface the URL field as still-selectable so the
      // user can copy manually.
      const input = modalEl?.querySelector<HTMLInputElement>(".share-url-input");
      input?.select();
    }
  }

  async function revoke(shareId: string) {
    if (revokingId) return;
    revokingId = shareId;
    try {
      await api.revokeShare(callId, shareId);
      await refreshList();
    } catch (e) {
      listError = e instanceof Error ? e.message : "Couldn't revoke share";
    } finally {
      revokingId = null;
    }
  }

  function fmtExpiry(s: ShareSummary): string {
    if (s.status === "revoked" && s.revoked_at) {
      return `Revoked ${fmtDate(s.revoked_at)}`;
    }
    if (s.expires_at === null) return "Never expires";
    if (s.status === "expired") return `Expired ${fmtDate(s.expires_at)}`;
    return `Expires ${fmtDate(s.expires_at)}`;
  }

  function fmtDate(iso: string): string {
    try {
      return new Date(iso).toLocaleDateString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
      });
    } catch {
      return iso;
    }
  }

  // Compact chips summarising what each existing share exposes /
  // hides. Only render the *interesting* deviations from default —
  // nothing for the everything-on, nothing-hidden case (would just be
  // visual noise when the row is the v0.7.2 shape). Nullish-safe for
  // legacy backends that don't return the field yet.
  function exposureChips(s: ShareSummary): { label: string; tone: "warn" | "info" }[] {
    const inc = s.included_sections;
    if (!inc) return [];
    const chips: { label: string; tone: "warn" | "info" }[] = [];
    if (!inc.summary) chips.push({ label: "Summary hidden", tone: "warn" });
    if (!inc.transcript) chips.push({ label: "Transcript hidden", tone: "warn" });
    if (!inc.action_items) chips.push({ label: "Actions hidden", tone: "warn" });
    if (!inc.audio) chips.push({ label: "Audio hidden", tone: "warn" });
    if (inc.manual_notes) chips.push({ label: "Notes shared", tone: "warn" });
    if (inc.allow_download) chips.push({ label: "Download allowed", tone: "info" });
    return chips;
  }
</script>

<div
  class="rn-backdrop"
  role="button"
  tabindex="-1"
  onclick={() => {
    if (!creating && !revokingId) onClose();
  }}
  onkeydown={(e) => {
    if (e.key === "Escape" && !creating && !revokingId) onClose();
  }}
>
  <div
    class="rn-modal share-modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    bind:this={modalEl}
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.stopPropagation()}
    tabindex="-1"
  >
    <div class="rn-head">
      <h2 id={titleId}>Share call</h2>
      {#if callTitle}
        <p class="head-sub">{callTitle}</p>
      {/if}
    </div>
    <div class="rn-body">
      <p class="warn">
        Anyone with the link can read whatever you include below. Use
        the toggles to control what's exposed. The link never reveals
        teammate emails or your other calls.
      </p>

      <!-- #240: per-link content toggles. manual_notes + allow_download
           default OFF (privacy-first); the rest default ON so the
           link's recipient gets the same shape as a v0.7.2 share when
           the creator doesn't fiddle with the row. -->
      <fieldset class="includes">
        <legend>What's included</legend>
        <label class="include-opt">
          <input
            type="checkbox"
            bind:checked={included.manual_notes}
            disabled={creating}
          />
          <span class="include-name">Manual notes</span>
          <span class="include-hint">Off by default — your scratchpad.</span>
        </label>
        <label class="include-opt">
          <input
            type="checkbox"
            bind:checked={included.summary}
            disabled={creating}
          />
          <span class="include-name">Summary</span>
        </label>
        <label class="include-opt">
          <input
            type="checkbox"
            bind:checked={included.action_items}
            disabled={creating}
          />
          <span class="include-name">Action items</span>
        </label>
        <label class="include-opt">
          <input
            type="checkbox"
            bind:checked={included.transcript}
            disabled={creating}
          />
          <span class="include-name">Transcript</span>
        </label>
        <label class="include-opt">
          <input
            type="checkbox"
            bind:checked={included.audio}
            disabled={creating}
          />
          <span class="include-name">Audio playback</span>
        </label>
        <label class="include-opt">
          <input
            type="checkbox"
            bind:checked={included.allow_download}
            disabled={creating || !included.audio}
          />
          <span class="include-name">Allow audio download</span>
          <span class="include-hint">Off by default — recipient can save the file.</span>
        </label>
      </fieldset>

      <fieldset class="expiry">
        <legend>Link expires</legend>
        <label class="expiry-opt">
          <input
            type="radio"
            name="share-expiry"
            value="7"
            bind:group={expiryChoice}
            disabled={creating}
          />
          <span>7 days</span>
        </label>
        <label class="expiry-opt">
          <input
            type="radio"
            name="share-expiry"
            value="30"
            bind:group={expiryChoice}
            disabled={creating}
          />
          <span>30 days</span>
        </label>
        <label class="expiry-opt">
          <input
            type="radio"
            name="share-expiry"
            value="never"
            bind:group={expiryChoice}
            disabled={creating}
          />
          <span>Never</span>
        </label>
      </fieldset>

      <!-- #241: promoted "Create link" button — wide primary teal,
           same visual weight as Send to CRM / Connect to Zoho CRM,
           sits below the toggle + expiry rows so the eye lands on
           it before the existing-links list. -->
      <div class="create-row">
        <button
          type="button"
          class="cta-btn create-link-btn"
          onclick={submitCreate}
          disabled={creating}
        >
          {creating ? "Creating…" : "Create link"}
        </button>
        {#if createError}
          <p class="error" role="alert">{createError}</p>
        {/if}
      </div>

      {#if lastCreated}
        <div class="created-card" role="region" aria-label="New share link">
          <p class="created-title">Link ready — copy it now</p>
          <p class="created-hint">
            We don't store the URL itself. After you close this dialog,
            you'll only see whether the link is active, expired, or
            revoked.
          </p>
          <div class="url-row">
            <input
              type="text"
              class="share-url-input"
              readonly
              value={lastCreated.url}
              onfocus={(e) => (e.currentTarget as HTMLInputElement).select()}
            />
            <button
              type="button"
              class="copy-btn"
              onclick={copyUrl}
              aria-live="polite"
            >
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
        </div>
      {/if}

      <div class="manage">
        <h3 class="manage-head">Existing links</h3>
        {#if listLoading}
          <p class="muted">Loading…</p>
        {:else if listError}
          <p class="error" role="alert">{listError}</p>
        {:else if shares.length === 0}
          <p class="muted">No links yet.</p>
        {:else}
          <ul class="share-list">
            {#each shares as s (s.id)}
              {@const chips = exposureChips(s)}
              <li class="share-row" class:revoked={s.status !== "active"}>
                <div class="share-row-main">
                  <span class="status-pill status-{s.status}">{s.status}</span>
                  <span class="share-meta">{fmtExpiry(s)}</span>
                </div>
                {#if chips.length > 0}
                  <div class="exposure-chips">
                    {#each chips as c (c.label)}
                      <span class="exposure-chip exposure-{c.tone}">{c.label}</span>
                    {/each}
                  </div>
                {/if}
                <div class="share-row-meta">
                  <span class="muted">
                    Created {fmtDate(s.created_at)} · {s.view_count}
                    {s.view_count === 1 ? "view" : "views"}
                  </span>
                  {#if s.status === "active"}
                    <button
                      type="button"
                      class="revoke-btn"
                      onclick={() => revoke(s.id)}
                      disabled={revokingId === s.id}
                    >
                      {revokingId === s.id ? "Revoking…" : "Revoke"}
                    </button>
                  {/if}
                </div>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    </div>
    <div class="rn-actions">
      <button
        type="button"
        class="rn-dismiss"
        onclick={onClose}
        disabled={creating || revokingId !== null}
      >
        Done
      </button>
    </div>
  </div>
</div>

<style>
  .share-modal {
    width: min(560px, 100%);
  }
  .head-sub {
    margin: 0.25rem 0 0;
    color: var(--bone-3);
    font-size: 0.82rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .warn {
    margin: 0 0 1rem;
    padding: 0.6rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-2);
    color: var(--bone-2);
    font-size: 0.82rem;
    line-height: 1.45;
  }

  /* #240 toggle row. Six checkboxes stacked vertically — easier to
     scan than a horizontal pill cluster and gives each option room
     for an inline hint on the privacy-default rows. */
  .includes {
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    padding: 0.65rem 0.8rem 0.6rem;
    margin: 0 0 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .includes legend {
    padding: 0 0.3rem;
    font-size: 0.72rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .include-opt {
    display: grid;
    grid-template-columns: 1.2rem 1fr;
    align-items: baseline;
    column-gap: 0.5rem;
    padding: 0.25rem 0;
    cursor: pointer;
    color: var(--bone-1);
    font-size: 0.86rem;
  }
  .include-opt input {
    margin: 0;
    grid-row: span 2;
    align-self: center;
  }
  .include-name {
    color: var(--bone-0);
  }
  .include-hint {
    grid-column: 2;
    color: var(--bone-3);
    font-size: 0.75rem;
    line-height: 1.35;
  }
  .include-opt input:disabled + .include-name {
    color: var(--bone-3);
  }

  .expiry {
    border: none;
    padding: 0;
    margin: 0 0 0.85rem;
    display: flex;
    flex-direction: row;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .expiry legend {
    padding: 0 0 0.4rem;
    font-size: 0.72rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .expiry-opt {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.45rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    cursor: pointer;
    font-size: 0.85rem;
    color: var(--bone-1);
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .expiry-opt:hover {
    border-color: var(--hairline-hi);
    background: var(--ink-2);
  }
  .expiry-opt input {
    margin: 0;
  }

  .create-row {
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 0.5rem;
    margin-bottom: 1rem;
  }
  /* #241: promoted "Create link" button — wide primary teal that
     matches the cta-btn class used by Send to CRM / Connect to Zoho
     CRM. Defined locally so the modal stays self-contained and the
     app.css mirror invariant isn't touched. */
  .create-link-btn {
    width: 100%;
    padding: 0.75rem 1rem;
    border: 1px solid var(--accent);
    border-radius: var(--radius);
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.92rem;
    font-weight: 600;
    letter-spacing: 0.02em;
    cursor: pointer;
    transition:
      background 0.15s,
      border-color 0.15s,
      box-shadow 0.15s,
      transform 0.06s;
  }
  .create-link-btn:hover:not(:disabled) {
    background: var(--accent-hover, var(--accent));
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .create-link-btn:active:not(:disabled) {
    transform: translateY(1px);
  }
  .create-link-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .created-card {
    margin: 0 0 1.25rem;
    padding: 0.85rem;
    border: 1px solid var(--accent);
    border-radius: var(--radius);
    background: var(--ink-2);
  }
  .created-title {
    margin: 0 0 0.25rem;
    color: var(--bone-0);
    font-size: 0.92rem;
    font-weight: 600;
  }
  .created-hint {
    margin: 0 0 0.7rem;
    color: var(--bone-3);
    font-size: 0.78rem;
    line-height: 1.45;
  }
  .url-row {
    display: flex;
    gap: 0.4rem;
  }
  .share-url-input {
    flex: 1 1 auto;
    min-width: 0;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    padding: 0.45rem 0.55rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
    color: var(--bone-0);
  }
  .copy-btn {
    flex: 0 0 auto;
    padding: 0.45rem 0.85rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-2);
    color: var(--bone-0);
    font-size: 0.82rem;
    cursor: pointer;
    transition:
      border-color 0.15s,
      background 0.15s,
      color 0.15s;
  }
  .copy-btn:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .manage-head {
    margin: 0.5rem 0 0.5rem;
    font-size: 0.78rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-2);
  }
  .share-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }
  .share-row {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    padding: 0.6rem 0.75rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-1);
  }
  .share-row.revoked {
    opacity: 0.65;
  }
  .share-row-main {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  /* #240 exposure chips on existing rows — small, dense, dashed
     borders so the creator can see at a glance whether each link
     deviates from the default everything-on shape. */
  .exposure-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin-top: 0.15rem;
  }
  .exposure-chip {
    display: inline-block;
    padding: 0.05rem 0.45rem;
    border-radius: 999px;
    font-size: 0.68rem;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    border: 1px dashed var(--hairline-hi);
    background: transparent;
  }
  .exposure-chip.exposure-warn {
    color: var(--bone-2);
    border-color: var(--hairline-hi);
  }
  .exposure-chip.exposure-info {
    color: var(--accent);
    border-color: var(--accent);
  }
  .share-meta {
    color: var(--bone-1);
    font-size: 0.85rem;
  }
  .share-row-meta {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    font-size: 0.78rem;
  }
  .muted {
    color: var(--bone-3);
  }
  .status-pill {
    display: inline-block;
    padding: 0.1rem 0.45rem;
    border-radius: 999px;
    font-size: 0.7rem;
    font-weight: 600;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    border: 1px solid var(--hairline);
    color: var(--bone-2);
    background: var(--ink-2);
  }
  .status-pill.status-active {
    border-color: var(--olive);
    color: var(--olive);
  }
  .status-pill.status-expired {
    border-color: var(--bone-3);
    color: var(--bone-3);
  }
  .status-pill.status-revoked {
    border-color: var(--live);
    color: var(--live);
  }
  .revoke-btn {
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--live);
    padding: 0.25rem 0.6rem;
    border-radius: var(--radius);
    font-size: 0.78rem;
    cursor: pointer;
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .revoke-btn:hover {
    border-color: var(--live);
    background: var(--ink-2);
  }
  .revoke-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .error {
    margin: 0;
    color: var(--live);
    font-size: 0.82rem;
  }
</style>
