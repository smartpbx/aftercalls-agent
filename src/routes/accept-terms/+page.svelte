<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { renderMarkdown } from "$lib/tosMarkdown";
  import { portalErrorToText } from "$lib/portalError";

  // #320 — agent-side ToS gate, mirror of
  // `portal/src/routes/accept-terms/+page.svelte`. Same fetch +
  // accept-by-checkbox flow; the only structural difference is that
  // network calls go through Tauri commands (`tos_current`,
  // `tos_accept`, `current_user`) instead of `fetch()`. The accept
  // command refetches `/v1/auth/me` and rewrites `auth.json` with the
  // cleared `pending_tos` so the layout's gate clears on the same tick
  // and the user lands on `/calls` without a relaunch.
  //
  // Markdown rendering routes through the same escape-first pipeline
  // (`tosMarkdown.ts` is a byte-identical mirror of the portal copy);
  // a `<script>` in body_md cannot execute.

  type PendingTos = {
    id: string;
    kind: "terms" | "privacy" | string;
    slug: string;
    effective_at: string;
  };

  type TosVersion = {
    id: string;
    kind: "terms" | "privacy" | string;
    slug: string;
    effective_at: string;
    body_md: string;
    required_for_use: boolean;
    created_at: string;
  };

  type Me = {
    pending_tos?: PendingTos[];
  };

  let loading = $state(true);
  let err = $state("");
  let docs = $state<TosVersion[]>([]);
  let agreed = $state<Record<string, boolean>>({});
  let submitting = $state(false);

  onMount(async () => {
    try {
      const me = await invoke<Me | null>("current_user");
      if (!me) {
        await goto("/login");
        return;
      }
      const pending = me.pending_tos ?? [];
      if (pending.length === 0) {
        // Already accepted — bounce away so this page isn't a dead
        // end if someone navigates to it manually. Mirrors the portal.
        await goto("/calls");
        return;
      }
      // /v1/tos/current returns { terms, privacy } — render only the
      // kinds that are still pending for this user.
      const current = await invoke<{
        terms: TosVersion | null;
        privacy: TosVersion | null;
      }>("tos_current");
      const pendingByKind = new Set(pending.map((v) => v.kind));
      const out: TosVersion[] = [];
      if (current.terms && pendingByKind.has("terms"))
        out.push(current.terms);
      if (current.privacy && pendingByKind.has("privacy"))
        out.push(current.privacy);
      docs = out;
      // All boxes unchecked by default — the user has to opt in.
      for (const d of docs) agreed[d.id] = false;
    } catch (e: unknown) {
      err = portalErrorToText(e).replace(/^Error:\s*/, "");
    } finally {
      loading = false;
    }
  });

  let allAgreed = $derived(
    docs.length > 0 && docs.every((d) => agreed[d.id]),
  );

  async function accept() {
    if (!allAgreed || submitting) return;
    submitting = true;
    err = "";
    try {
      // The Rust-side `tos_accept` POSTs the acceptance, refetches
      // /v1/auth/me, and rewrites auth.json — so the next layout
      // gate-evaluation (or `current_user` poll) sees the cleared
      // `pending_tos` immediately.
      const refreshed = await invoke<Me>("tos_accept", {
        ids: docs.map((d) => d.id),
      });
      if (
        !refreshed.pending_tos ||
        refreshed.pending_tos.length === 0
      ) {
        window.dispatchEvent(new CustomEvent("aftercalls-auth-refresh"));
        await goto("/calls");
      } else {
        err =
          "Acceptance recorded but the server still reports pending items. Try reloading.";
      }
    } catch (e: unknown) {
      err = `Acceptance failed: ${portalErrorToText(e).replace(/^Error:\s*/, "")}`;
    } finally {
      submitting = false;
    }
  }

  async function signOut() {
    try {
      await invoke("logout");
    } catch {
      // Best-effort — even if the server-side revoke fails, auth.json
      // is wiped locally. Fall through to the login page.
    }
    await goto("/login");
  }

  function title(kind: string): string {
    if (kind === "terms") return "Terms of Service";
    if (kind === "privacy") return "Privacy Policy";
    return kind;
  }
</script>

<main class="page">
  <header class="head">
    <h1>Terms you need to accept</h1>
    <p class="sub">
      Before you continue, please review and agree to the documents below.
      You only see this on first sign-in and whenever we publish a new
      version.
    </p>
  </header>

  {#if loading}
    <p class="state">Loading…</p>
  {:else if err}
    <p class="error">{err}</p>
  {:else}
    {#each docs as doc (doc.id)}
      <section class="doc">
        <div class="doc-head">
          <h2>{title(doc.kind)}</h2>
          <span class="slug">{doc.slug}</span>
        </div>
        <div class="body">
          {@html renderMarkdown(doc.body_md)}
        </div>
        <label class="agree">
          <input
            type="checkbox"
            bind:checked={agreed[doc.id]}
            disabled={submitting}
          />
          <span>I have read and agree to the {title(doc.kind)}.</span>
        </label>
      </section>
    {/each}

    <div class="actions">
      <button
        class="primary"
        disabled={!allAgreed || submitting}
        onclick={accept}
      >
        {submitting ? "Recording acceptance…" : "Accept and continue"}
      </button>
      <button class="ghost" disabled={submitting} onclick={signOut}>
        Sign out instead
      </button>
    </div>
  {/if}
</main>

<style>
  .page {
    max-width: 820px;
    margin: 0 auto;
    padding: 2.4rem 2rem 4rem;
  }
  .head h1 {
    margin: 0 0 0.4rem;
  }
  .sub {
    margin: 0 0 2rem;
    color: var(--bone-3);
    font-size: 0.9rem;
    max-width: 60ch;
    line-height: 1.55;
  }
  .state {
    color: var(--bone-3);
  }
  .error {
    padding: 0.7rem 0.9rem;
    background: var(--live-soft);
    color: var(--live);
    border-radius: var(--radius);
    font-size: 0.88rem;
    margin-bottom: 1rem;
  }
  .doc {
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    padding: 1.4rem 1.6rem;
    margin-bottom: 1.4rem;
  }
  .doc-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin-bottom: 1rem;
    gap: 1rem;
  }
  .doc-head h2 {
    margin: 0;
  }
  .slug {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-4);
    letter-spacing: 0.04em;
  }
  .body {
    max-height: 40vh;
    overflow-y: auto;
    padding: 1rem 1.2rem;
    background: var(--ink-0);
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    color: var(--bone-1);
    font-size: 0.88rem;
    line-height: 1.65;
  }
  .body :global(h1) {
    font-size: 1.2rem;
    margin: 0 0 0.8rem;
  }
  .body :global(h2) {
    font-size: 1rem;
    margin: 1.2rem 0 0.5rem;
    color: var(--bone-0);
  }
  .body :global(p) {
    margin: 0 0 0.8rem;
  }
  .body :global(ul),
  .body :global(ol) {
    margin: 0 0 0.8rem;
    padding-left: 1.2rem;
  }
  .body :global(li) {
    margin-bottom: 0.35rem;
  }
  .body :global(strong) {
    color: var(--bone-0);
  }
  .body :global(em) {
    color: var(--bone-2);
  }
  .agree {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    margin-top: 1rem;
    font-size: 0.9rem;
    color: var(--bone-1);
    cursor: pointer;
    user-select: none;
  }
  .agree input {
    width: 16px;
    height: 16px;
    accent-color: var(--accent);
    cursor: pointer;
  }
  .actions {
    display: flex;
    gap: 0.6rem;
    align-items: center;
    margin-top: 1.5rem;
  }
  .primary {
    padding: 0.7rem 1.3rem;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 600;
    font-size: 0.92rem;
    border-radius: 8px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .primary:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .ghost {
    padding: 0.7rem 1.1rem;
    background: transparent;
    border: 1px solid var(--hairline);
    color: var(--bone-3);
    font-size: 0.88rem;
    border-radius: 8px;
    cursor: pointer;
  }
  .ghost:hover:not(:disabled) {
    color: var(--bone-0);
    border-color: var(--hairline-hi);
  }
</style>
