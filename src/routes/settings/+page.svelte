<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";

  type Theme = "dark" | "light" | "system";
  let theme = $state<Theme>("dark");

  type Me = {
    email: string;
    display_name: string;
    role: string;
    org_display_name: string;
  };
  let me = $state<Me | null>(null);
  let signingOut = $state(false);

  async function signOut() {
    signingOut = true;
    try {
      await invoke("logout");
      goto("/login");
    } catch (e) {
      error = String(e);
    } finally {
      signingOut = false;
    }
  }

  function setTheme(t: Theme) {
    theme = t;
    document.documentElement.setAttribute("data-theme", t);
    try {
      localStorage.setItem("aftercalls:theme", t);
    } catch (_) {}
  }

  let error = $state("");

  // ── App preferences (per-machine) ───────────────────────────────────
  let closeToTray = $state(true);
  let autoDetect = $state(true);
  let telemetryEnabled = $state(true);
  let soundsEnabled = $state(true);
  // Hard recording-length ceiling (#75). Default 120 min; Rust clamps
  // to [5, 1440] on save. Kept as a plain number so the <input> binds
  // cleanly; we coerce to integer inside saveAppPrefs before sending.
  let maxRecordingMinutes = $state(120);
  // Manual notes panel (#73). When on, the record screen shows a
  // CodeMirror editor during active recording; notes ride into
  // create_call and optionally feed into the summary. Off by default
  // so the record screen stays minimal for non-note-takers.
  let manualNotesEnabled = $state(false);
  let prefsSavedAt = $state(0);

  // ── Launch-at-sign-in (#4) ─────────────────────────────────────────
  // OS-sourced (Linux: ~/.config/autostart/*.desktop, Windows:
  // HKCU\...\Run). Deliberately NOT persisted in config.toml — we read
  // fresh on every Settings mount so a hand-removed .desktop file shows
  // up as OFF immediately. `autostartLoading` gates the switch into a
  // disabled state until the first get_autostart resolves (typically
  // sub-100ms). `autostartSavedAt` drives a row-local saved pip,
  // separate from the card-head `prefsSavedAt` because autostart is
  // out-of-band from `AppPrefs` and shouldn't imply the whole card
  // saved. `autostartError` populates the row's .error-inline on
  // failure and clears on the next successful toggle or mount.
  let autostart = $state(false);
  let autostartLoading = $state(true);
  let autostartSavedAt = $state(0);
  let autostartError = $state("");
  let autostartSavedRecently = $derived(
    autostartSavedAt > 0 && Date.now() - autostartSavedAt < 2000,
  );

  async function loadAutostart() {
    autostartLoading = true;
    autostartError = "";
    try {
      autostart = await invoke<boolean>("get_autostart");
    } catch (e) {
      // A read failure is rare and usually means the plugin couldn't
      // talk to the OS (permission, missing ~/.config on a fresh
      // account). Show the error inline; the switch stays at its
      // default (off) so the user can still try to flip it on.
      autostartError = "Couldn't update launch setting. Try again.";
      console.warn("get_autostart failed", e);
    } finally {
      autostartLoading = false;
    }
  }

  async function toggleAutostart(next: boolean) {
    // Optimistic flip. Roll back on error so the UI never shows a
    // state that diverges from disk.
    const prev = autostart;
    autostart = next;
    autostartError = "";
    try {
      await invoke("set_autostart", { enabled: next });
      autostartSavedAt = Date.now();
    } catch (e) {
      autostart = prev;
      autostartError = "Couldn't update launch setting. Try again.";
      console.warn("set_autostart failed", e);
    }
  }

  async function loadAppPrefs() {
    try {
      const p = await invoke<{
        close_to_tray: boolean;
        auto_detect: boolean;
        telemetry_enabled: boolean;
        sounds_enabled: boolean;
        max_recording_minutes: number;
        manual_notes_enabled: boolean;
      }>("get_app_prefs");
      closeToTray = p.close_to_tray;
      autoDetect = p.auto_detect;
      telemetryEnabled = p.telemetry_enabled;
      soundsEnabled = p.sounds_enabled;
      maxRecordingMinutes = p.max_recording_minutes ?? 120;
      manualNotesEnabled = p.manual_notes_enabled ?? false;
    } catch (e) {
      console.warn("get_app_prefs failed", e);
    }
  }

  async function saveAppPrefs() {
    try {
      // Round + clamp on the JS side so a partially-typed value (e.g.
      // empty input while the user is mid-edit) can't ship 0 to Rust
      // and trigger an immediate auto-stop on the next recording.
      const mins = Math.min(
        1440,
        Math.max(5, Math.round(Number(maxRecordingMinutes) || 120)),
      );
      await invoke("set_app_prefs", {
        closeToTray,
        autoDetect,
        telemetryEnabled,
        soundsEnabled,
        maxRecordingMinutes: mins,
        manualNotesEnabled,
      });
      prefsSavedAt = Date.now();
    } catch (e) {
      error = String(e);
    }
  }

  let prefsSavedRecently = $derived(
    prefsSavedAt > 0 && Date.now() - prefsSavedAt < 2000,
  );

  // ── Obsidian vault (per-machine) ─────────────────────────────────────
  type VaultSettings = {
    enabled: boolean;
    path: string;
    clients_subpath: string;
  };
  let vault = $state<VaultSettings>({
    enabled: false,
    path: "",
    clients_subpath: "",
  });
  let vaultSaving = $state(false);
  let vaultSavedAt = $state(0);
  let vaultError = $state("");

  onMount(async () => {
    // Read the theme that the bootstrap script already applied so the UI
    // starts matching what the page is rendering.
    const applied = document.documentElement.getAttribute("data-theme");
    if (applied === "light" || applied === "system") theme = applied;
    else theme = "dark";

    try {
      me = await invoke<Me | null>("current_user");
    } catch (e) {
      console.warn("current_user failed", e);
    }

    try {
      vault = await invoke<VaultSettings>("get_vault_settings");
    } catch (e) {
      console.warn("get_vault_settings failed", e);
    }

    await loadAppPrefs();
    await loadAutostart();
  });

  async function pickVaultDir() {
    try {
      const chosen = await openDialog({
        directory: true,
        multiple: false,
        title: "Select your Obsidian vault folder",
      });
      if (typeof chosen === "string" && chosen) {
        vault.path = chosen;
      }
    } catch (e) {
      vaultError = String(e);
    }
  }

  async function saveVault() {
    vaultSaving = true;
    vaultError = "";
    try {
      await invoke("set_vault_settings", {
        enabled: vault.enabled,
        path: vault.path,
        clientsSubpath: vault.clients_subpath,
      });
      vaultSavedAt = Date.now();
    } catch (e) {
      vaultError = String(e);
    } finally {
      vaultSaving = false;
    }
  }

  // Flipping the switch should "just work" — no Save button nag when
  // turning the feature off entirely. When flipping *on* we still
  // require Save because the path/subfolder fields haven't been
  // filled in yet; writing enabled=true with an empty path would
  // make the pipeline try to write to the filesystem root.
  async function toggleVault(next: boolean) {
    vault.enabled = next;
    if (!next) await saveVault();
  }

  let vaultSavedRecently = $derived(
    vaultSavedAt > 0 && Date.now() - vaultSavedAt < 3000,
  );

  // Org vocab management now lives in the portal — changes affect every
  // teammate's calls. The agent still reads vocab via get_org_vocab at
  // pipeline time; it's just no longer editable from here.
  async function openPortalVocab() {
    try {
      await openUrl("https://app.aftercalls.io/admin/vocab");
    } catch (e) {
      console.warn("openUrl failed", e);
    }
  }
</script>

<main class="page reveal">
  <header class="head" style="--i: 0">
    <h1>Settings</h1>
    <p class="sub">
      Transcription hints for your organization. Applied to every call the
      agent processes.
    </p>
  </header>

  {#if me}
    <section class="card" style="--i: 0.5">
      <div class="card-head">
        <div>
          <h2>Account</h2>
          <p class="hint">
            Signed in as <strong>{me.email}</strong> ({me.role}) on
            <strong>{me.org_display_name}</strong>.
          </p>
        </div>
        <button
          type="button"
          class="add"
          disabled={signingOut}
          onclick={signOut}
        >
          {signingOut ? "Signing out…" : "Sign out"}
        </button>
      </div>
    </section>
  {/if}

  <section class="card" style="--i: 1">
    <div class="card-head">
      <div>
        <h2>Appearance</h2>
        <p class="hint">
          System mode follows your OS light/dark preference.
        </p>
      </div>
    </div>
    <div class="theme-row" role="group" aria-label="Theme">
      {#each [
        { v: "dark", label: "Dark" },
        { v: "light", label: "Light" },
        { v: "system", label: "System" },
      ] as opt (opt.v)}
        <button
          type="button"
          class="theme-opt"
          class:active={theme === opt.v}
          aria-pressed={theme === opt.v}
          onclick={() => setTheme(opt.v as Theme)}
        >
          <span class="theme-swatch {opt.v}"></span>
          <span>{opt.label}</span>
        </button>
      {/each}
    </div>
  </section>

  <section class="card" style="--i: 1.5">
    <div class="card-head">
      <div>
        <h2>Behavior</h2>
        <p class="hint">
          How the app handles window close and automatic call detection.
          Stored per computer.
        </p>
      </div>
      {#if prefsSavedRecently}<span class="saved">Saved</span>{/if}
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Launch at sign-in</span>
        <span class="pref-hint" id="autostart-hint">
          {autostart
            ? "Starts aftercalls automatically when you sign in to your computer, so it's ready in the tray without a manual launch."
            : "aftercalls won't start automatically — you'll launch it yourself after signing in."}
        </span>
      </div>
      <div class="autostart-control">
        <label
          class="switch"
          aria-busy={autostartLoading ? "true" : undefined}
        >
          <input
            type="checkbox"
            checked={autostart}
            disabled={autostartLoading}
            aria-describedby="autostart-hint"
            onchange={(e) => {
              toggleAutostart((e.currentTarget as HTMLInputElement).checked);
            }}
          />
          <span class="track" aria-hidden="true">
            <span class="knob"></span>
          </span>
          <span class="switch-label">
            {autostartLoading ? "…" : autostart ? "On" : "Off"}
          </span>
        </label>
        {#if autostartSavedRecently}<span class="saved autostart-saved">Saved</span>{/if}
      </div>
    </div>
    {#if autostartError}
      <p class="error-inline autostart-error" role="alert">{autostartError}</p>
    {/if}

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Close button</span>
        <span class="pref-hint">
          {closeToTray
            ? "Clicking ✕ hides the window to the tray and keeps recording available."
            : "Clicking ✕ exits the app. Tray is still available from re-launch."}
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={closeToTray}
          onchange={(e) => {
            closeToTray = (e.currentTarget as HTMLInputElement).checked;
            saveAppPrefs();
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {closeToTray ? "To tray" : "Exit"}
        </span>
      </label>
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Auto-detect calls</span>
        <span class="pref-hint">
          Watches the mic for apps like Zoom, Teams, SmartPBX, and offers to
          record. When off, the detector doesn't run at all — you'll only
          record via the button or the hotkey.
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={autoDetect}
          onchange={(e) => {
            autoDetect = (e.currentTarget as HTMLInputElement).checked;
            saveAppPrefs();
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {autoDetect ? "On" : "Off"}
        </span>
      </label>
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Max recording length (minutes)</span>
        <span class="pref-hint">
          Hard ceiling on a single recording. If a softphone holds the
          mic open long after a call ends, the agent auto-stops at this
          limit so you don't end up with a runaway multi-hour file.
          Minimum 5, maximum 1440 (24 hours). Default 120.
        </span>
      </div>
      <input
        class="input num-input"
        type="number"
        min="5"
        max="1440"
        step="5"
        bind:value={maxRecordingMinutes}
        onchange={saveAppPrefs}
      />
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Manual notes panel</span>
        <span class="pref-hint">
          Shows a notes editor on the record screen during an active
          recording. Notes are saved with the call and can optionally
          be fed into the AI summary as primary context. Edit after
          the fact on the call detail page.
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={manualNotesEnabled}
          onchange={(e) => {
            manualNotesEnabled = (e.currentTarget as HTMLInputElement).checked;
            saveAppPrefs();
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">{manualNotesEnabled ? "On" : "Off"}</span>
      </label>
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Notification sounds</span>
        <span class="pref-hint">
          Short tones when recording starts and stops, when the
          pipeline finishes, and when the app detects a call and
          asks to record. Off silences all of them.
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={soundsEnabled}
          onchange={(e) => {
            soundsEnabled = (e.currentTarget as HTMLInputElement).checked;
            saveAppPrefs();
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">{soundsEnabled ? "On" : "Off"}</span>
      </label>
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Diagnostic telemetry</span>
        <span class="pref-hint">
          Sends a buffered log of errors, panics, and pipeline events to
          the aftercalls team so we can diagnose issues without remote
          access to your machine. No call audio, transcripts, or
          summaries are included — only app-level events and error
          messages. Off disables all outbound telemetry.
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={telemetryEnabled}
          onchange={(e) => {
            telemetryEnabled = (e.currentTarget as HTMLInputElement).checked;
            saveAppPrefs();
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {telemetryEnabled ? "On" : "Off"}
        </span>
      </label>
    </div>
  </section>

  <section class="card" style="--i: 2">
    <div class="card-head">
      <div>
        <h2>Org vocab</h2>
        <p class="hint">
          Spelling corrections and word-boost hints are managed by your
          org's admin in the web portal — changes take effect for every
          teammate's calls.
        </p>
      </div>
      <a
        class="add"
        href="https://app.aftercalls.io/admin/vocab"
        target="_blank"
        rel="noopener"
        onclick={(e) => {
          e.preventDefault();
          openPortalVocab();
        }}
      >
        Open portal ↗
      </a>
    </div>
  </section>

  <section class="card" style="--i: 3">
    <div class="card-head">
      <div>
        <h2>Obsidian vault (this machine)</h2>
        <p class="hint">
          Optional: save a Markdown note to your local Obsidian vault
          after each call, organized under a client subfolder. Stored
          per computer — leave off if this machine doesn't have your
          vault.
        </p>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={vault.enabled}
          onchange={(e) => toggleVault((e.currentTarget as HTMLInputElement).checked)}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {vault.enabled ? "On" : "Off"}
        </span>
      </label>
    </div>

    {#if vault.enabled}
      <div class="vault-field">
        <label class="field-label">Vault folder</label>
        <div class="vault-row">
          <input
            class="input vault-path"
            placeholder="/home/you/Documents/ObsidianVault"
            bind:value={vault.path}
          />
          <button type="button" class="add" onclick={pickVaultDir}>
            Browse…
          </button>
        </div>
      </div>
      <div class="vault-field">
        <label class="field-label">Clients subfolder (optional)</label>
        <input
          class="input"
          placeholder="20 Clients"
          bind:value={vault.clients_subpath}
        />
        <p class="hint small">
          Relative to the vault folder. Notes land under
          <code>{"{"}vault{"}"}/{"{"}subpath{"}"}/{"{"}matched-client{"}"}/</code>.
          Leave blank to drop all notes in the vault root.
        </p>
      </div>

      <div class="vault-actions">
        <button
          type="button"
          class="save"
          disabled={vaultSaving}
          onclick={saveVault}
        >
          {vaultSaving ? "Saving…" : "Save vault settings"}
        </button>
        {#if vaultSavedRecently}<span class="saved">Saved</span>{/if}
        {#if vaultError}<span class="error-inline">{vaultError}</span>{/if}
      </div>
    {:else if vaultSavedRecently || vaultError}
      <div class="vault-actions">
        {#if vaultSavedRecently}<span class="saved">Saved</span>{/if}
        {#if vaultError}<span class="error-inline">{vaultError}</span>{/if}
      </div>
    {/if}
  </section>

  {#if error}<p class="error" style="--i: 4">{error}</p>{/if}

  <footer class="legal-footer" style="--i: 5">
    <a
      href="https://aftercalls.io/licenses"
      onclick={(e) => { e.preventDefault(); openUrl("https://aftercalls.io/licenses"); }}
    >Licenses</a>
    <span class="sep" aria-hidden="true">·</span>
    <a
      href="https://aftercalls.io/terms"
      onclick={(e) => { e.preventDefault(); openUrl("https://aftercalls.io/terms"); }}
    >Terms</a>
    <span class="sep" aria-hidden="true">·</span>
    <a
      href="https://aftercalls.io/privacy"
      onclick={(e) => { e.preventDefault(); openUrl("https://aftercalls.io/privacy"); }}
    >Privacy</a>
  </footer>
</main>

<style>
  .page {
    max-width: 820px;
    margin: 0 auto;
    padding: 2.2rem 2rem 4rem;
    position: relative;
    z-index: 2;
  }

  .head {
    margin-bottom: 1.6rem;
  }

  /* ── Behavior preferences ───────────────────────────────────────── */
  .pref-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1.2rem;
    padding: 0.7rem 0;
    border-top: 1px solid var(--hairline);
  }
  .pref-row:first-of-type {
    border-top: none;
    padding-top: 0.2rem;
  }
  .pref-label {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    flex: 1 1 auto;
    min-width: 0;
  }
  .pref-title {
    font-size: 0.9rem;
    color: var(--bone-0);
    font-weight: 500;
  }
  .pref-hint {
    font-size: 0.78rem;
    color: var(--bone-3);
    line-height: 1.45;
    max-width: 52ch;
  }

  /* ── Vault section ──────────────────────────────────────────────── */
  .switch {
    display: inline-flex;
    align-items: center;
    gap: 0.55rem;
    cursor: pointer;
    font-size: 0.85rem;
    color: var(--bone-1);
    user-select: none;
    flex-shrink: 0;
  }
  .switch input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  .switch .track {
    position: relative;
    width: 36px;
    height: 20px;
    border-radius: 999px;
    background: var(--ink-3);
    border: 1px solid var(--hairline);
    transition: background 0.18s, border-color 0.18s;
  }
  .switch .knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--bone-2);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.25);
    transition: transform 0.18s, background 0.18s;
  }
  .switch input:checked + .track {
    background: var(--accent);
    border-color: var(--accent);
  }
  .switch input:checked + .track .knob {
    transform: translateX(16px);
    background: #ffffff;
  }
  .switch input:focus-visible + .track {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .switch-label {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--bone-3);
    /* Fixed-width slot right of the switch so different label widths
       (e.g. "TO TRAY" vs "EXIT") don't shove the knob around. Text
       is left-aligned inside the slot so the state is still readable
       without making the switch itself appear to move. */
    min-width: 4.5rem;
    text-align: left;
  }
  .vault-field {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    margin-top: 0.9rem;
  }
  .vault-row {
    display: flex;
    gap: 0.5rem;
  }
  .vault-path {
    flex: 1;
  }
  .field-label {
    font-size: 0.78rem;
    color: var(--bone-2);
    font-weight: 500;
    letter-spacing: 0.02em;
  }
  .input {
    padding: 0.5rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 8px;
    background: var(--ink-0);
    color: var(--bone-0);
    font: inherit;
    font-size: 0.88rem;
    font-family: var(--font-mono);
  }
  .input:focus {
    outline: none;
    border-color: var(--accent);
  }
  /* Narrow slot for the minutes input — matches the .switch control
     width so the Behavior section's right column stays visually
     aligned across rows. */
  .num-input {
    flex: 0 0 7rem;
    text-align: right;
  }
  .hint.small code {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--bone-2);
    background: var(--ink-2);
    padding: 0.05rem 0.3rem;
    border-radius: 4px;
  }
  .vault-actions {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    margin-top: 1rem;
  }
  .save {
    padding: 0.5rem 1rem;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-weight: 600;
    border-radius: 8px;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .save:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .save:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .saved {
    color: var(--olive);
    font-size: 0.82rem;
  }
  .error-inline {
    color: var(--live);
    font-size: 0.82rem;
  }

  /* Autostart row: inline saved-pip next to the switch (row-local, not
     card-head — see .claude/plans/issue-4/decisions.md Q1). Error
     slides under the row on a new line so the row layout itself stays
     stable when it appears. */
  .autostart-control {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    flex-shrink: 0;
  }
  .autostart-saved {
    /* Keeps the pip out of the switch's fixed-width label slot so the
       knob doesn't shift when "Saved" appears. */
    white-space: nowrap;
  }
  .autostart-error {
    margin: 0.25rem 0 0.5rem;
  }

  .sub {
    margin: 0.3rem 0 0;
    color: var(--bone-2);
    font-size: 0.9rem;
    max-width: 54ch;
  }

  .state {
    color: var(--bone-3);
  }

  .theme-row {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 0.5rem;
  }

  .theme-opt {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.7rem 0.9rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius);
    background: var(--ink-0);
    color: var(--bone-1);
    font-size: 0.9rem;
    font-weight: 500;
    transition:
      border-color 0.15s,
      color 0.15s,
      background 0.15s;
  }

  .theme-opt:hover {
    border-color: var(--hairline-hi);
    color: var(--bone-0);
  }

  .theme-opt.active {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--bone-0);
  }

  .theme-swatch {
    width: 22px;
    height: 22px;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    flex-shrink: 0;
    position: relative;
    overflow: hidden;
  }
  /* Little previews that stay true to each theme regardless of current mode
   * so the user can see what they're picking. */
  .theme-swatch.dark {
    background: linear-gradient(135deg, #0e0d0c 0%, #24211d 100%);
    border-color: #3a9b92;
  }
  .theme-swatch.light {
    background: linear-gradient(135deg, #faf6ec 0%, #e1d8c3 100%);
    border-color: #237e76;
  }
  .theme-swatch.system {
    background: linear-gradient(135deg, #0e0d0c 0%, #0e0d0c 50%, #faf6ec 50%, #faf6ec 100%);
    border-color: var(--accent);
  }

  .card {
    padding: 1.2rem 1.3rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--ink-1);
    margin-bottom: 1.2rem;
  }

  .card-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
    margin-bottom: 0.9rem;
  }

  .hint {
    margin: 0.3rem 0 0;
    color: var(--bone-3);
    font-size: 0.8rem;
    line-height: 1.55;
    max-width: 58ch;
  }

  .add {
    flex-shrink: 0;
    padding: 0.38rem 0.85rem;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-2);
    color: var(--bone-1);
    transition: all 0.15s;
  }
  .add:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .empty {
    margin: 0;
    padding: 0.8rem 0.2rem;
    color: var(--bone-3);
    font-size: 0.85rem;
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }

  .row {
    display: grid;
    grid-template-columns: 12rem 1fr auto;
    gap: 0.5rem;
    align-items: center;
  }

  .row-head {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--bone-3);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    padding: 0 0.2rem;
    margin-bottom: 0.15rem;
  }

  .input,
  .boost {
    padding: 0.5rem 0.7rem;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--ink-0);
    color: inherit;
    font: inherit;
    font-size: 0.88rem;
    width: 100%;
    box-sizing: border-box;
    transition: border-color 0.15s;
  }

  .input::placeholder,
  .boost::placeholder {
    color: var(--bone-4);
  }

  .input:focus,
  .boost:focus {
    outline: none;
    border-color: var(--accent);
  }

  .boost {
    resize: vertical;
    line-height: 1.55;
  }

  .remove {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border-radius: 6px;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--bone-3);
    transition: all 0.15s;
  }
  .remove:hover {
    color: var(--live);
    border-color: var(--live);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 0.8rem;
    padding-top: 0.5rem;
  }

  .save {
    padding: 0.55rem 1.2rem;
    border-radius: 8px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--ink-0);
    font-size: 0.88rem;
    font-weight: 600;
    transition: all 0.15s;
  }
  .save:hover:not(:disabled) {
    background: var(--accent-hi);
    border-color: var(--accent-hi);
  }
  .save:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .saved {
    color: var(--olive);
    font-size: 0.83rem;
    font-weight: 500;
  }

  .error {
    color: var(--live);
    font-size: 0.83rem;
  }

  .legal-footer {
    margin-top: 2.4rem;
    padding-top: 1rem;
    border-top: 1px solid var(--hairline);
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.78rem;
    color: var(--bone-3);
  }
  .legal-footer a {
    color: var(--bone-3);
    text-decoration: none;
    transition: color 0.15s;
  }
  .legal-footer a:hover {
    color: var(--bone-0);
    text-decoration: underline;
  }
  .legal-footer .sep {
    color: var(--bone-4);
  }
</style>
