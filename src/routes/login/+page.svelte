<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { goto } from "$app/navigation";
  import { onMount } from "svelte";
  import { portalErrorToText } from "$lib/portalError";

  const REMEMBERED_EMAIL_KEY = "aftercalls.login.rememberedEmail";

  let email = $state("");
  let password = $state("");
  let rememberEmail = $state(false);
  let submitting = $state(false);
  let error = $state("");

  // #335 / #381 — SSO waiting-state. Set when the user clicks an SSO
  // button and the browser is launched. Shows a "Return to aftercalls"
  // hint plus a manual "Refresh sign-in state" button so the user has
  // a clear recovery path if auto-detection doesn't fire.
  let ssoWaiting = $state(false);
  let ssoRefreshing = $state(false);
  let ssoRefreshError = $state("");

  type PendingTos = {
    id: string;
    kind: string;
    slug: string;
    effective_at: string;
  };

  type Me = {
    email: string;
    display_name: string;
    role: string;
    org_display_name: string;
    /// #320 — outstanding ToS / privacy versions. The login flow
    /// routes the user to /accept-terms first when this is non-empty
    /// so they don't briefly mount the recording surface before the
    /// layout-level gate redirects.
    pending_tos?: PendingTos[];
  };

  onMount(async () => {
    try {
      const saved = localStorage.getItem(REMEMBERED_EMAIL_KEY);
      if (saved) {
        email = saved;
        rememberEmail = true;
      }
    } catch {}

    try {
      const me = await invoke<Me | null>("current_user");
      if (me) {
        // #320 — if the cached profile has pending ToS / privacy
        // rows, route past the recording surface and straight to the
        // gate so the layout doesn't have to bounce a half-mounted
        // /calls page.
        const pending = me.pending_tos ?? [];
        if (pending.length > 0) goto("/accept-terms");
        else goto("/");
      }
    } catch (e) {
      console.warn("current_user check failed", e);
    }
  });

  // #32 — agent SSO entry point. Commit 1 ships a thin "sign in with
  // browser" experience: clicking opens the portal's /login page in
  // the system browser (which carries the same three SSO buttons +
  // password form). Once the user has signed in there, the existing
  // /handoff flow brings the session back into the agent on the
  // user's next "Open desktop app" click. The localhost-listener
  // PKCE flow described in the architect plan lands in a follow-up.
  //
  // #335 — after openUrl returns, flip into the waiting state so the
  // agent's login card shows a "Return to aftercalls" hint. The user
  // sees this while their browser handles SSO and knows to switch back.
  //
  // #381 — the waiting state also surfaces a "Refresh sign-in state"
  // button (ssoRefreshCheck) as a manual recovery path if the
  // auto-detection doesn't trigger (e.g. user completed SSO in the
  // browser but the agent didn't pick it up automatically).
  async function ssoSignIn(provider: string) {
    ssoRefreshError = "";
    try {
      // Hardcoded portal URL — same shape as other "go to portal"
      // links elsewhere in the agent (see settings/+page.svelte).
      // The portal's /login renders the SSO buttons; the user picks
      // their provider there. Provider param is hint-only — the
      // portal renders all three regardless.
      const url = `https://app.aftercalls.io/login?sso_hint=${encodeURIComponent(provider)}`;
      await openUrl(url);
      // Browser launched successfully — show the waiting/hint state.
      ssoWaiting = true;
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }

  // #381 — manual "Refresh sign-in state" handler. Re-checks whether
  // the backend session exists (the user may have signed in via browser
  // and the agent didn't auto-detect). On success: navigate to /. On
  // failure: show an inline error so the user can try again or cancel.
  async function ssoRefreshCheck() {
    if (ssoRefreshing) return;
    ssoRefreshing = true;
    ssoRefreshError = "";
    try {
      const me = await invoke<Me | null>("current_user");
      if (me) {
        window.dispatchEvent(new Event("aftercalls-login"));
        // #320 — same ToS-aware route-out as the password flow.
        const pending = me.pending_tos ?? [];
        if (pending.length > 0) goto("/accept-terms");
        else goto("/");
      } else {
        ssoRefreshError = "Sign-in not detected yet. Complete the sign-in in your browser, then try again.";
      }
    } catch (e) {
      ssoRefreshError = "Couldn't check sign-in status. Check your connection and try again.";
      console.warn("ssoRefreshCheck current_user failed", e);
    } finally {
      ssoRefreshing = false;
    }
  }

  function cancelSsoWaiting() {
    ssoWaiting = false;
    ssoRefreshError = "";
  }

  async function submit(e: Event) {
    e.preventDefault();
    error = "";
    submitting = true;
    try {
      const trimmed = email.trim();
      const result = await invoke<Me>("login", { email: trimmed, password });
      try {
        if (rememberEmail) localStorage.setItem(REMEMBERED_EMAIL_KEY, trimmed);
        else localStorage.removeItem(REMEMBERED_EMAIL_KEY);
      } catch {}
      window.dispatchEvent(new Event("aftercalls-login"));
      // #320 — route past the recording surface when the user has
      // outstanding ToS / privacy versions so the gate page renders
      // cleanly rather than the layout having to bounce a half-
      // mounted /calls. The layout still owns the post-mount
      // re-evaluation in case the cached state lags.
      const pending = result.pending_tos ?? [];
      if (pending.length > 0) goto("/accept-terms");
      else goto("/");
    } catch (e: unknown) {
      error = portalErrorToText(e).replace(/^Error:\s*/, "");
    } finally {
      submitting = false;
    }
  }
</script>

<main class="page">
  <div class="card">
    <div class="head">
      <span class="dot"></span>
      <h1 class="wordmark">aftercalls</h1>
    </div>

    {#if ssoWaiting}
      <!-- #335 / #381 — SSO waiting state. Shown after the user clicks
           an SSO button and the browser is launched. The "Return to
           aftercalls" hint tells them to switch back once done. The
           "Refresh sign-in state" button is the manual recovery path
           (#381) when auto-detection hasn't fired. -->
      <div class="sso-waiting">
        <p class="sso-waiting-label">
          Sign in via your browser, then switch back to aftercalls.
        </p>
        {#if ssoRefreshError}
          <p class="sso-waiting-err">{ssoRefreshError}</p>
        {/if}
        <button
          type="button"
          class="primary sso-refresh-btn"
          onclick={ssoRefreshCheck}
          disabled={ssoRefreshing}
        >
          {ssoRefreshing ? "Checking…" : "Refresh sign-in state"}
        </button>
        <button
          type="button"
          class="sso-cancel"
          onclick={cancelSsoWaiting}
          disabled={ssoRefreshing}
        >
          Use email &amp; password instead
        </button>
      </div>
    {:else}
      <p class="sub">Sign in to your workspace.</p>

      <!-- #32 — SSO buttons that bounce out to the system browser.
           The agent-native PKCE + localhost-listener flow lands as a
           follow-up; this thin variant keeps SSO usable from day one
           without the IPC/listener complexity. -->
      <div class="sso">
        <button type="button" class="sso-btn" onclick={() => ssoSignIn("google")}>
          <span class="sso-glyph">G</span>
          Continue with Google
        </button>
        <button type="button" class="sso-btn" onclick={() => ssoSignIn("microsoft")}>
          <span class="sso-glyph">M</span>
          Continue with Microsoft
        </button>
        <button type="button" class="sso-btn" onclick={() => ssoSignIn("zoho")}>
          <span class="sso-glyph">Z</span>
          Continue with Zoho
        </button>
      </div>

      <div class="divider"><span>or</span></div>

      <form onsubmit={submit}>
      <label>
        <span>Email</span>
        <input
          type="email"
          autocomplete="email"
          required
          autofocus
          bind:value={email}
          disabled={submitting}
        />
      </label>
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

      <label class="remember">
        <input
          type="checkbox"
          bind:checked={rememberEmail}
          disabled={submitting}
        />
        <span>Remember my email</span>
      </label>

      {#if error}
        <p class="error">{error}</p>
      {/if}

      <button type="submit" class="primary" disabled={submitting}>
        {submitting ? "Signing in…" : "Sign in"}
      </button>

      <p class="forgot">
        <a href="https://app.aftercalls.io/forgot-password" target="_blank">
          Forgot password?
        </a>
      </p>
    </form>
    {/if}
  </div>
</main>

<style>
  .page {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: calc(100vh - var(--topbar-h));
    padding: 2rem;
    position: relative;
    z-index: 2;
  }

  .card {
    width: 100%;
    max-width: 360px;
    padding: 2.2rem 2rem 2rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    box-shadow: 0 24px 50px -32px rgba(0, 0, 0, 0.6);
  }

  .head {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    margin-bottom: 0.2rem;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 10px var(--accent-glow);
  }

  h1 {
    margin: 0;
  }

  .sub {
    margin: 0 0 1.3rem;
    color: var(--bone-3);
    font-size: 0.88rem;
  }

  .sso {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    margin-bottom: 1.1rem;
  }
  .sso-btn {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    padding: 0.55rem 0.85rem;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: var(--bone-0);
    font-size: 0.9rem;
    text-decoration: none;
    cursor: pointer;
    font-family: inherit;
    text-align: left;
    transition: border-color 0.15s, background 0.15s;
  }
  .sso-btn:hover {
    border-color: var(--accent);
    background: var(--ink-1);
  }
  .sso-glyph {
    width: 22px;
    height: 22px;
    border-radius: 4px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-family: "Geist Mono", monospace;
    font-size: 0.78rem;
    color: var(--bone-3);
    border: 1px solid var(--hairline);
    background: var(--ink-1);
  }

  .divider {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    margin: 0 0 1.1rem;
    color: var(--bone-3);
    font-size: 0.78rem;
  }
  .divider::before, .divider::after {
    content: "";
    flex: 1;
    height: 1px;
    background: var(--hairline);
  }

  form {
    display: flex;
    flex-direction: column;
    gap: 0.9rem;
  }

  label {
    display: flex;
    flex-direction: column;
    gap: 0.32rem;
    font-size: 0.78rem;
    color: var(--bone-2);
    letter-spacing: 0.01em;
  }

  input {
    padding: 0.55rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    color: var(--bone-0);
    font-size: 0.92rem;
    font-family: inherit;
    transition: border-color 0.15s;
  }

  input:focus {
    outline: none;
    border-color: var(--accent);
  }

  input:disabled {
    opacity: 0.6;
  }

  .remember {
    flex-direction: row;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.82rem;
    color: var(--bone-2);
    cursor: pointer;
    user-select: none;
  }

  .remember input[type="checkbox"] {
    width: 14px;
    height: 14px;
    accent-color: var(--accent);
    cursor: pointer;
  }

  .error {
    margin: 0;
    padding: 0.5rem 0.7rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.82rem;
  }

  .primary {
    margin-top: 0.2rem;
    padding: 0.65rem 1rem;
    border-radius: 8px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.92rem;
    font-weight: 600;
    transition: all 0.15s;
  }

  .primary:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }

  .primary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .forgot {
    margin: 0.25rem 0 0;
    font-size: 0.8rem;
    text-align: center;
  }

  .forgot a {
    color: var(--bone-3);
  }
  .forgot a:hover {
    color: var(--accent);
  }

  /* #335 / #381 — SSO waiting state. */
  .sso-waiting {
    display: flex;
    flex-direction: column;
    gap: 0.85rem;
    padding: 0.5rem 0 0.2rem;
  }
  .sso-waiting-label {
    margin: 0;
    color: var(--bone-2);
    font-size: 0.9rem;
    line-height: 1.45;
  }
  .sso-waiting-err {
    margin: 0;
    padding: 0.5rem 0.7rem;
    border-radius: 6px;
    background: var(--live-soft);
    color: var(--live);
    font-size: 0.82rem;
  }
  .sso-refresh-btn {
    margin-top: 0;
    width: 100%;
  }
  .sso-cancel {
    appearance: none;
    background: transparent;
    border: none;
    color: var(--bone-3);
    font-family: inherit;
    font-size: 0.82rem;
    cursor: pointer;
    text-align: center;
    padding: 0.25rem 0;
    text-decoration: underline;
  }
  .sso-cancel:hover {
    color: var(--bone-1);
  }
  .sso-cancel:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
