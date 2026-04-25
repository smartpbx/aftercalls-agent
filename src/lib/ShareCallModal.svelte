<script lang="ts">
  // #35 Share-call modal. Two halves stacked vertically:
  //   1. "Create new share" — pick expiry (7d / 30d / never), submit,
  //      reveals URL with copy-to-clipboard. Token is shown ONCE; once
  //      the modal closes (or the user creates a second share) the URL
  //      is gone forever — only the SHA256 hash lives in the DB.
  //   2. "Manage existing shares" — list rows with view count, status
  //      pill (active / expired / revoked), and a Revoke button.
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
  type ShareCreated = {
    id: string;
    token: string;
    url: string;
    expires_at: string | null;
    created_at: string;
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
  };

  interface Props {
    callId: string;
    callTitle?: string | null;
    api: {
      createShare: (id: string, expiresInDays: number | null) => Promise<ShareCreated>;
      listShares: (id: string) => Promise<ShareSummary[]>;
      revokeShare: (callId: string, shareId: string) => Promise<void>;
    };
    onClose: () => void;
  }

  let { callId, callTitle = null, api, onClose }: Props = $props();

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
      const created = await api.createShare(callId, days);
      lastCreated = created;
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
        Anyone with the link can read the transcript, summary, action
        items, and play the audio. The link doesn't expose teammate
        emails or your other calls.
      </p>

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

      <div class="create-row">
        <button
          type="button"
          class="rn-primary"
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
              <li class="share-row" class:revoked={s.status !== "active"}>
                <div class="share-row-main">
                  <span class="status-pill status-{s.status}">{s.status}</span>
                  <span class="share-meta">{fmtExpiry(s)}</span>
                </div>
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
    align-items: center;
    gap: 0.75rem;
    margin-bottom: 1rem;
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
