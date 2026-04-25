<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { onMount } from "svelte";
  import { portalErrorToText } from "$lib/portalError";

  const REMEMBERED_EMAIL_KEY = "aftercalls.login.rememberedEmail";

  let email = $state("");
  let password = $state("");
  let rememberEmail = $state(false);
  let submitting = $state(false);
  let error = $state("");

  type Me = {
    email: string;
    display_name: string;
    role: string;
    org_display_name: string;
  };

  onMount(async () => {
    // Pre-fill last-used email if the user opted into remember-me on a
    // previous sign-in. Only the address is persisted — never the password.
    try {
      const saved = localStorage.getItem(REMEMBERED_EMAIL_KEY);
      if (saved) {
        email = saved;
        rememberEmail = true;
      }
    } catch {}

    // If someone hits /login while already logged in, bounce them home.
    try {
      const me = await invoke<Me | null>("current_user");
      if (me) goto("/");
    } catch (e) {
      console.warn("current_user check failed", e);
    }
  });

  async function submit(e: Event) {
    e.preventDefault();
    error = "";
    submitting = true;
    try {
      const trimmed = email.trim();
      await invoke<Me>("login", { email: trimmed, password });
      try {
        if (rememberEmail) localStorage.setItem(REMEMBERED_EMAIL_KEY, trimmed);
        else localStorage.removeItem(REMEMBERED_EMAIL_KEY);
      } catch {}
      // Tell the layout that auth just changed — it'll refresh `me` and
      // fire the release-notes modal personalized to the signed-in name.
      window.dispatchEvent(new Event("aftercalls-login"));
      goto("/");
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
    <p class="sub">Sign in to your workspace.</p>

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
</style>
