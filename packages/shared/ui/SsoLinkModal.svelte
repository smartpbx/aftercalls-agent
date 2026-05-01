<!--
  SSO account-link password prompt (#32).

  Shared package component shipped to both portal and agent.

  Used by:
    portal — `/login/sso/link?link_token=...` (full-page wrapper).
    agent  — `/login/sso/link` route (full-page) AND the
             "sso-link-required" Tauri event on the login screen
             (modal overlay).

  Inputs:
    `linkToken`  — single-use, 10-min token from the backend
                   pending-link row.
    `email`      — the matched aftercalls user's email; rendered as
                   "Sign in with your password to link <provider>
                   account: <email>".
    `provider`   — display label ("Google", "Microsoft", "Zoho").
    `onSubmit`   — async callback that POSTs to
                   `/v1/auth/sso/link/confirm`.
    `onCancel`   — optional async callback that POSTs to
                   `/v1/auth/sso/link/cancel`.
-->
<script lang="ts">
  type Props = {
    linkToken: string;
    email: string;
    provider: string;
    onSubmit: (linkToken: string, password: string) => Promise<void>;
    onCancel?: (linkToken: string) => Promise<void>;
  };

  let { linkToken, email, provider, onSubmit, onCancel }: Props = $props();

  let password = $state("");
  let submitting = $state(false);
  let error = $state("");

  async function submit(e: Event) {
    e.preventDefault();
    if (!password) return;
    error = "";
    submitting = true;
    try {
      await onSubmit(linkToken, password);
    } catch (err: any) {
      error = String(err?.message ?? err).replace(/^Error:\s*/, "");
    } finally {
      submitting = false;
    }
  }

  async function cancel() {
    if (!onCancel) return;
    try {
      await onCancel(linkToken);
    } catch {}
  }
</script>

<div class="sso-link">
  <h1>Link your {provider} account</h1>
  <p class="lead">
    Sign in with your aftercalls password to link your <strong>{provider}</strong>
    account to <strong>{email}</strong>. After linking, you can use either
    method to sign in.
  </p>

  <form onsubmit={submit}>
    <label>
      <span>Password</span>
      <input
        type="password"
        autocomplete="current-password"
        required
        bind:value={password}
        disabled={submitting}
      />
    </label>

    {#if error}<p class="error">{error}</p>{/if}

    <div class="actions">
      <button type="submit" class="primary" disabled={submitting || !password}>
        {submitting ? "Linking…" : "Link account"}
      </button>
      {#if onCancel}
        <button type="button" class="ghost" onclick={cancel} disabled={submitting}>
          Cancel
        </button>
      {/if}
    </div>
  </form>
</div>

<style>
  .sso-link {
    width: 100%;
    max-width: 420px;
    padding: 2rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
  }
  h1 {
    margin: 0 0 0.7rem;
    font-size: 1.15rem;
  }
  .lead {
    margin: 0 0 1rem;
    color: var(--bone-2);
    font-size: 0.92rem;
  }
  strong { color: var(--bone-0); }
  form {
    display: flex;
    flex-direction: column;
    gap: 0.85rem;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: 0.78rem;
    color: var(--bone-2);
  }
  input {
    padding: 0.55rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    color: var(--bone-0);
    font-family: inherit;
    font-size: 0.92rem;
  }
  input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .error {
    margin: 0;
    padding: 0.5rem 0.7rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.82rem;
  }
  .actions {
    display: flex;
    gap: 0.6rem;
    align-items: center;
  }
  .primary {
    padding: 0.6rem 1rem;
    border-radius: 8px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 600;
    cursor: pointer;
  }
  .primary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .ghost {
    padding: 0.6rem 0.85rem;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--bone-2);
    cursor: pointer;
  }
  .ghost:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--bone-0);
  }
</style>
