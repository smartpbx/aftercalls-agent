<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { getVersion } from "@tauri-apps/api/app";
  import { loadRecordingPrefs, type RecordingNotificationMode } from "$lib/compliance";
  import { invalidateAppPrefs } from "$lib/stores/appPrefs.svelte";
  import { setSentryTelemetryAllowed } from "$lib/sentry";
  import { autoRecordStore } from "$lib/stores/autoRecord.svelte";
  import { portalErrorToText } from "$lib/portalError";
  import type { Me } from "@aftercalls/shared/types";

  // #69 — About card surfaces the running version + the LGPL ffmpeg
  // notice + a link to the canonical public manifest at
  // aftercalls.io/licenses. Lives in Settings (rather than a new
  // Help page) so the existing legal-footer can stay where it is and
  // we don't restructure the Settings tree. Stays empty / falls back
  // gracefully if getVersion() rejects (offline-only Tauri command,
  // shouldn't fail in practice but the rail in +layout.svelte
  // already uses the same defensive pattern).
  let appVersion = $state<string | null>(null);

  type Theme = "dark" | "light" | "system";
  let theme = $state<Theme>("dark");

  let me = $state<Me | null>(null);
  let signingOut = $state(false);

  // ── Profile (#96) ──────────────────────────────────────────────────
  let firstDraft = $state("");
  let lastDraft = $state("");
  let savingName = $state(false);
  let nameMsg = $state("");
  let nameErr = $state("");
  let firstBlurErr = $state(false);
  // #484 — character counter for the first-name input. maxlength is 120;
  // counter appears once the user gets within 20 chars of the limit so the
  // field doesn't feel cluttered at typical name lengths.
  const FIRST_NAME_MAX = 120;
  const FIRST_NAME_COUNTER_THRESHOLD = 20;
  let firstCharCount = $derived(firstDraft.length);
  let showFirstCounter = $derived(firstCharCount >= FIRST_NAME_MAX - FIRST_NAME_COUNTER_THRESHOLD);

  let canSaveName = $derived(
    !!me &&
      !savingName &&
      firstDraft.trim().length > 0 &&
      (firstDraft.trim() !== (me.first_name ?? "") ||
        lastDraft.trim() !== (me.last_name ?? "")),
  );

  async function saveName() {
    if (!me || !canSaveName) return;
    const first = firstDraft.trim();
    const last = lastDraft.trim();
    savingName = true;
    nameMsg = "";
    nameErr = "";
    firstBlurErr = false;
    try {
      const next = await invoke<Me>("update_me", {
        firstName: first,
        lastName: last,
      });
      me = next;
      firstDraft = next.first_name ?? "";
      lastDraft = next.last_name ?? "";
      nameMsg = "Saved.";
      setTimeout(() => (nameMsg = ""), 2200);
    } catch (e: any) {
      nameErr = String(e?.message ?? e);
    } finally {
      savingName = false;
    }
  }

  function onNameKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault();
      saveName();
    }
  }

  function onFirstBlur() {
    firstBlurErr = firstDraft.trim().length === 0;
  }

  function onFirstInput() {
    if (firstBlurErr && firstDraft.trim().length > 0) firstBlurErr = false;
  }

  async function signOut() {
    signingOut = true;
    try {
      await invoke("logout");
      goto("/login");
    } catch (e) {
      error = portalErrorToText(e);
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
  // #180 (v0.6.x) — sub-toggle of autoDetect. When autoDetect is on
  // but this is off, the in-app slide-out still renders on detection
  // (the Rust side keeps emitting the `auto-detect` event) but the
  // window-show / focus-steal path is skipped. Tiling-WM users
  // (Hyprland, Sway) want detection to keep running without losing
  // focus mid-task. Default true preserves historical behavior for
  // every existing install.
  let autoDetectPopup = $state(true);
  let telemetryEnabled = $state(true);
  let soundsEnabled = $state(true);
  // Hard recording-length ceiling (#75). Default 120 min; Rust clamps
  // to [5, 1440] on save. Kept as a plain number so the <input> binds
  // cleanly; we coerce to integer inside saveAppPrefs before sending.
  let maxRecordingMinutes = $state(120);
  // #142 · v0.4.5 — note-to-self cap. Default 5 min, Rust clamps to
  // [1, 60]. Shorter range than the regular cap because dictations
  // should be short by design; the tighter ceiling protects against
  // a user walking away from the mic mid-note.
  let maxSelfNoteMinutes = $state(5);
  // #149 (v0.4.7) — user-configurable self-note hotkey. `null` means
  // the global hotkey is disabled; tray + button + CLI keep working.
  // The capture widget below writes either a canonical "Super+Shift+N"
  // style string or null. Shipped default (fresh install) is
  // "Super+Shift+N", matching v0.4.5 behaviour.
  let selfNoteShortcut = $state<string | null>("Super+Shift+N");
  // Live capture state: when `capturingShortcut === true`, the hidden
  // input on the row has focus and the next keydown event is recorded
  // as the new combo. Separate from `selfNoteShortcut` so the
  // persisted value only changes once the user confirms the capture.
  let capturingShortcut = $state(false);
  let shortcutError = $state<string | null>(null);
  // #161 (v0.5.2) — parallel state for the record-toggle shortcut.
  // Same shape as the self-note wiring above. Shipped default
  // "Super+Shift+R" matches v0.5.1 behaviour; user can rebind from
  // Settings, clear to disable (tray + UI + CLI keep working).
  let recordToggleShortcut = $state<string | null>("Super+Shift+R");
  let capturingRecordToggleShortcut = $state(false);
  let recordToggleError = $state<string | null>(null);
  // Manual notes panel (#73). When on, the record screen shows a
  // CodeMirror editor during active recording; notes ride into
  // create_call and optionally feed into the summary. Off by default
  // so the record screen stays minimal for non-note-takers.
  let manualNotesEnabled = $state(false);
  // #56 — per-machine pref for the spoken recording-start
  // announcement. Defaults OFF so an existing user's first recording
  // after upgrade doesn't unexpectedly speak. Hidden from the UI
  // entirely when the org's recording_notification_mode is
  // 'enforced' (compliance posture is mandatory; no toggle to flip)
  // or 'off' (chime suppressed → speech makes no sense).
  let consentAnnouncementEnabled = $state(false);
  // #596 — auto-record master toggles. Both default OFF so an existing
  // upgrade-installed user is never surprised by an automatic
  // recording. The new "Auto-record" section in the markup binds to
  // these and persists them through the existing saveAppPrefs path.
  // The per-app whitelist (which apps trigger an auto-start when this
  // is on) lives in the autoRecordStore (sqlite-backed); these two are
  // the gates that cover that list.
  let autoRecordStartEnabled = $state(false);
  let autoRecordStopEnabled = $state(false);
  // Org-level recording notification mode, fetched once on mount so
  // the row can decide whether to render itself. Null while loading;
  // treated as 'user' on error so the toggle stays visible (a
  // backend hiccup shouldn't lock the user out of opting in).
  let recordingNotificationMode = $state<RecordingNotificationMode | null>(null);
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

  // Mirror of the config.toml `wayland_hotkey_notice_dismissed` pref.
  // The Settings UI doesn't expose this directly — the Record page's
  // in-app hotkey note owns the only user-facing dismiss control —
  // but `saveAppPrefs` must round-trip the value so toggling any
  // other pref here doesn't reset it to false.
  let waylandHotkeyNoticeDismissed = $state(false);

  // ── Input microphone (#3) ──────────────────────────────────────────
  // Saved cpal device name — null means "use system default" (the
  // fresh-install state). Persisted in config.toml via set_app_prefs.
  let inputDevice = $state<string | null>(null);
  type DeviceEntry = { name: string; is_default: boolean };
  let inputDevices = $state<DeviceEntry[]>([]);
  // Loading / error / empty states for the device enumeration. The
  // <select> is disabled whenever loading or error is set; empty
  // shows a hint under the row instead of an error.
  let micListLoading = $state(true);
  let micListError = $state("");
  // Pre-compute the set of names we know about after load so the
  // "saved name no longer enumerates" detection is a cheap lookup.
  // Populated after loadInputDevices().
  let inputDeviceNames = $derived(new Set(inputDevices.map((d) => d.name)));
  let savedDeviceIsStale = $derived(
    !!inputDevice && !micListLoading && !micListError &&
      !inputDeviceNames.has(inputDevice),
  );
  // Resolves the cpal-reported name of the currently-defaulted device
  // so the "System default (currently: X)" label can surface it. Falls
  // back to a generic phrasing when the default isn't in the list
  // (e.g. enumeration skipped it for some reason).
  let currentDefaultName = $derived(
    inputDevices.find((d) => d.is_default)?.name ?? "",
  );
  // Display labels with per-collision (1)(2)... suffixes (decisions.md
  // Q1). Suffix is DISPLAY ONLY — the underlying cpal name is what's
  // stored, so the resolver walks devices in enumeration order and
  // binds to the first match of the same name. Duplicates with unique
  // names get no suffix.
  let inputDeviceLabels = $derived.by(() => {
    const counts = new Map<string, number>();
    return inputDevices.map((d) => {
      const seen = counts.get(d.name) ?? 0;
      counts.set(d.name, seen + 1);
      // First occurrence: no suffix. Second+: (1), (2)...
      const suffix = seen === 0 ? "" : ` (${seen})`;
      const defaultTag = d.is_default ? "  ·  system default" : "";
      return {
        value: d.name,
        label: `${d.name}${suffix}${defaultTag}`,
      };
    });
  });

  async function loadInputDevices() {
    micListLoading = true;
    micListError = "";
    try {
      inputDevices = await invoke<DeviceEntry[]>("list_input_devices");
    } catch (e) {
      // Command-level failure — either cpal couldn't walk the host's
      // device list (rare) or the invoke itself rejected. Surface the
      // "Couldn't load devices" state; user can retry without
      // leaving Settings.
      micListError = "Couldn't load input devices.";
      console.warn("list_input_devices failed", e);
    } finally {
      micListLoading = false;
    }
  }

  function onInputDeviceChange(value: string) {
    // Empty string = "System default" option (we emit "" as the
    // option value for System default since native <select> coerces
    // null to "null"). Round-trip to null on the way to Rust; Rust
    // also coerces empty-to-None defensively.
    inputDevice = value === "" ? null : value;
    saveAppPrefs();
  }

  async function loadAppPrefs() {
    try {
      const p = await invoke<{
        close_to_tray: boolean;
        auto_detect: boolean;
        auto_detect_popup: boolean;
        telemetry_enabled: boolean;
        sounds_enabled: boolean;
        max_recording_minutes: number;
        max_self_note_minutes: number;
        manual_notes_enabled: boolean;
        wayland_hotkey_notice_dismissed: boolean;
        input_device: string | null;
        self_note_shortcut: string | null;
        record_toggle_shortcut: string | null;
        consent_announcement_enabled: boolean;
        auto_record_start_enabled: boolean;
        auto_record_stop_enabled: boolean;
      }>("get_app_prefs");
      closeToTray = p.close_to_tray;
      autoDetect = p.auto_detect;
      autoDetectPopup = p.auto_detect_popup ?? true;
      telemetryEnabled = p.telemetry_enabled;
      soundsEnabled = p.sounds_enabled;
      maxRecordingMinutes = p.max_recording_minutes ?? 120;
      maxSelfNoteMinutes = p.max_self_note_minutes ?? 5;
      manualNotesEnabled = p.manual_notes_enabled ?? false;
      waylandHotkeyNoticeDismissed = p.wayland_hotkey_notice_dismissed ?? false;
      inputDevice = p.input_device ?? null;
      selfNoteShortcut = p.self_note_shortcut ?? null;
      recordToggleShortcut = p.record_toggle_shortcut ?? null;
      consentAnnouncementEnabled = p.consent_announcement_enabled ?? false;
      // #596 master toggles ride along with the rest of the prefs blob;
      // they're displayed in their own section below but persist through
      // the same set_app_prefs round-trip so a single Save covers any
      // combination of pref edits.
      autoRecordStartEnabled = p.auto_record_start_enabled ?? false;
      autoRecordStopEnabled = p.auto_record_stop_enabled ?? false;
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
      const noteMins = Math.min(
        60,
        Math.max(1, Math.round(Number(maxSelfNoteMinutes) || 5)),
      );
      await invoke("set_app_prefs", {
        closeToTray,
        autoDetect,
        autoDetectPopup,
        telemetryEnabled,
        soundsEnabled,
        maxRecordingMinutes: mins,
        maxSelfNoteMinutes: noteMins,
        manualNotesEnabled,
        waylandHotkeyNoticeDismissed,
        inputDevice,
        selfNoteShortcut,
        recordToggleShortcut,
        consentAnnouncementEnabled,
        autoRecordStartEnabled,
        autoRecordStopEnabled,
      });
      // #501 — drop the shared cache so the next consumer
      // (compliance.ts / notify.ts) sees the freshly-saved values
      // instead of stale module-level state.
      invalidateAppPrefs();
      setSentryTelemetryAllowed(telemetryEnabled);
      prefsSavedAt = Date.now();
    } catch (e) {
      error = portalErrorToText(e);
    }
  }

  // #596 — auto-record helpers. The Settings → Auto-record section
  // surfaces relative timestamps ("Last used: 2 hours ago"), so this
  // tiny formatter keeps the markup clean. Coarse buckets are good
  // enough — the user is making a "do I trust this app?" decision,
  // not auditing call history.
  function fmtRelative(iso: string): string {
    const t = new Date(iso).getTime();
    if (!Number.isFinite(t)) return "—";
    const diffMs = Date.now() - t;
    if (diffMs < 60_000) return "just now";
    const mins = Math.floor(diffMs / 60_000);
    if (mins < 60) return `${mins} minute${mins === 1 ? "" : "s"} ago`;
    const hours = Math.floor(mins / 60);
    if (hours < 24) return `${hours} hour${hours === 1 ? "" : "s"} ago`;
    const days = Math.floor(hours / 24);
    if (days < 30) return `${days} day${days === 1 ? "" : "s"} ago`;
    const months = Math.floor(days / 30);
    if (months < 12) return `${months} month${months === 1 ? "" : "s"} ago`;
    const years = Math.floor(days / 365);
    return `${years} year${years === 1 ? "" : "s"} ago`;
  }

  // Per-row "Forget" handler. Originally guarded by an OS-native
  // confirm via @tauri-apps/plugin-dialog; that dialog never appeared
  // for users on Linux Wayland under wlroots-derived compositors
  // (Hyprland), making the button look broken (#605). Removed because
  // Forget is non-destructive — the row reappears the next time the
  // app uses the mic, so a misclick costs nothing. The store's
  // optimistic mutation already gives the user immediate feedback
  // (the row disappears on click).
  async function onForgetApp(app: { bundle_id: string }) {
    void autoRecordStore.forget(app.bundle_id);
  }

  // #149 (v0.4.7) + #161 (v0.5.2) — capture-widget handlers. The row
  // renders a read-only text box showing the current combo; clicking
  // "Change" flips the matching `capturing*` flag, which unhides a
  // transparent input that grabs focus + swallows the next keydown.
  // Esc cancels. Collision logic checks the candidate against the
  // OTHER configurable shortcut (either direction) so the two can
  // never bind to the same combo.

  function normalizeShortcut(mods: string[], key: string): string {
    // Canonical ordering: Ctrl → Alt → Shift → Super → Key. Keeps
    // the display stable across keyboard layouts (Linux emits Super
    // first, macOS emits Cmd differently) so two users with the
    // "same" combo get the same persisted string.
    const order = ["Ctrl", "Alt", "Shift", "Super"];
    const seen = new Set(mods);
    const ordered = order.filter((m) => seen.has(m));
    return [...ordered, key].join("+");
  }

  function onShortcutKeyDown(e: KeyboardEvent) {
    if (!capturingShortcut) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      capturingShortcut = false;
      shortcutError = null;
      return;
    }
    // Ignore bare modifier presses; keep listening until the user
    // presses a real key alongside them.
    if (["Shift", "Control", "Alt", "Meta", "Super"].includes(e.key)) {
      return;
    }
    const mods: string[] = [];
    if (e.ctrlKey) mods.push("Ctrl");
    if (e.altKey) mods.push("Alt");
    if (e.shiftKey) mods.push("Shift");
    if (e.metaKey) mods.push("Super");
    // Promote the keycode to a canonical token the Rust parser
    // understands: letters A..Z, digits 0..9, function keys F1..F24.
    let key = "";
    if (e.code.startsWith("Key") && e.code.length === 4) {
      key = e.code.slice(3).toUpperCase();
    } else if (e.code.startsWith("Digit") && e.code.length === 6) {
      key = e.code.slice(5);
    } else if (/^F\d{1,2}$/.test(e.code)) {
      key = e.code;
    } else {
      // Unsupported key; surface an inline error + stay in capture
      // mode so the user can try again without clicking Change again.
      shortcutError = "Use a letter, digit, or F-key with at least one modifier.";
      return;
    }
    if (mods.length === 0) {
      shortcutError = "Add at least one modifier (Ctrl / Alt / Shift / Super).";
      return;
    }
    const candidate = normalizeShortcut(mods, key);
    if (candidate === recordToggleShortcut) {
      shortcutError = `${candidate} is already bound to start/stop recording.`;
      return;
    }
    selfNoteShortcut = candidate;
    capturingShortcut = false;
    shortcutError = null;
    void saveAppPrefs();
  }

  function beginCaptureShortcut() {
    capturingShortcut = true;
    shortcutError = null;
  }
  function clearShortcut() {
    selfNoteShortcut = null;
    capturingShortcut = false;
    shortcutError = null;
    void saveAppPrefs();
  }

  // #161 (v0.5.2) — record-toggle twins of the handlers above.
  // Identical shape modulo the target state + the collision target
  // (blocks assigning the self-note combo to the record toggle).
  function onRecordToggleShortcutKeyDown(e: KeyboardEvent) {
    if (!capturingRecordToggleShortcut) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      capturingRecordToggleShortcut = false;
      recordToggleError = null;
      return;
    }
    if (["Shift", "Control", "Alt", "Meta", "Super"].includes(e.key)) {
      return;
    }
    const mods: string[] = [];
    if (e.ctrlKey) mods.push("Ctrl");
    if (e.altKey) mods.push("Alt");
    if (e.shiftKey) mods.push("Shift");
    if (e.metaKey) mods.push("Super");
    let key = "";
    if (e.code.startsWith("Key") && e.code.length === 4) {
      key = e.code.slice(3).toUpperCase();
    } else if (e.code.startsWith("Digit") && e.code.length === 6) {
      key = e.code.slice(5);
    } else if (/^F\d{1,2}$/.test(e.code)) {
      key = e.code;
    } else {
      recordToggleError = "Use a letter, digit, or F-key with at least one modifier.";
      return;
    }
    if (mods.length === 0) {
      recordToggleError = "Add at least one modifier (Ctrl / Alt / Shift / Super).";
      return;
    }
    const candidate = normalizeShortcut(mods, key);
    if (candidate === selfNoteShortcut) {
      recordToggleError = `${candidate} is already bound to note-to-self.`;
      return;
    }
    recordToggleShortcut = candidate;
    capturingRecordToggleShortcut = false;
    recordToggleError = null;
    void saveAppPrefs();
  }

  function beginCaptureRecordToggleShortcut() {
    capturingRecordToggleShortcut = true;
    recordToggleError = null;
  }
  function clearRecordToggleShortcut() {
    recordToggleShortcut = null;
    capturingRecordToggleShortcut = false;
    recordToggleError = null;
    void saveAppPrefs();
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
      if (me) {
        firstDraft = me.first_name ?? "";
        lastDraft = me.last_name ?? "";
      }
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
    await loadInputDevices();
    // #596 — auto-record bundle (master toggles + observed_apps list).
    // Errors here are non-fatal: the section renders an inline retry
    // hint and the rest of the page stays usable.
    void autoRecordStore.refresh();
    // #56 — fetch the org's recording-notification mode so the
    // consent-announcement toggle can decide whether to render.
    // Cache lives in $lib/compliance, so the Record page and the
    // auto-detect slide-out share the same in-memory copy. Errors
    // fall through to mode = null → row treated as 'user'-equivalent
    // so the user isn't locked out of opting in.
    try {
      const rp = await loadRecordingPrefs();
      recordingNotificationMode = rp?.recording_notification_mode ?? "user";
    } catch (e) {
      console.warn("loadRecordingPrefs (settings) failed", e);
      recordingNotificationMode = "user";
    }

    // #69 — load running app version for the About card. getVersion is
    // a synchronous-feeling Tauri call but still async; it can't
    // realistically fail here (no IPC over the network), so a missing
    // value just means we render a single em-dash instead.
    try {
      appVersion = await getVersion();
    } catch (e) {
      console.warn("getVersion failed", e);
    }
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
      vaultError = portalErrorToText(e);
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
      vaultError = portalErrorToText(e);
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
    <!-- Profile card (#96). First + Last side-by-side on wide,
         stacked below 540px. Sits above Account because the user's
         own data-to-edit logically precedes identity/session
         context. -->
    <section class="card" style="--i: 0.5">
      <div class="card-head">
        <div>
          <h2>Profile</h2>
          <p class="hint">
            Your name as it appears on summaries, the team picker, and
            transcripts.
          </p>
        </div>
      </div>
      <div class="name-row">
        <label class="name-field">
          <span class="field-label">First name</span>
          <input
            class="input"
            id="first-name"
            type="text"
            bind:value={firstDraft}
            disabled={savingName}
            maxlength="120"
            autocomplete="given-name"
            aria-invalid={firstBlurErr ? "true" : undefined}
            onblur={onFirstBlur}
            oninput={onFirstInput}
            onkeydown={onNameKeydown}
          />
          {#if firstBlurErr}
            <p class="error-inline name-err" role="alert">
              First name is required.
            </p>
          {:else if showFirstCounter}
            <!-- #484 — char counter appears when nearing maxlength -->
            <p class="char-counter" class:char-counter-near={firstCharCount >= FIRST_NAME_MAX - 5} aria-live="polite">
              {firstCharCount}/{FIRST_NAME_MAX}
            </p>
          {/if}
        </label>
        <label class="name-field">
          <span class="field-label">Last name</span>
          <input
            class="input"
            id="last-name"
            type="text"
            bind:value={lastDraft}
            disabled={savingName}
            maxlength="120"
            autocomplete="family-name"
            onkeydown={onNameKeydown}
          />
        </label>
      </div>
      <div class="name-actions">
        <button
          type="button"
          class="save"
          disabled={!canSaveName}
          onclick={saveName}
        >
          {savingName ? "Saving…" : "Save"}
        </button>
        {#if nameMsg}<span class="saved">{nameMsg}</span>{/if}
        {#if nameErr}<span class="error-inline">{nameErr}</span>{/if}
      </div>
    </section>

    <section class="card" style="--i: 0.75">
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

  <!-- #572 v2 follow-up — Integrations live on the web portal because
       the OAuth callback flow needs a browser anyway. Surface the link
       here so a user looking for "where do I connect my Zoho account"
       finds it in the same Settings page they're already on. -->
  <section class="card" style="--i: 0.85">
    <div class="card-head">
      <div>
        <h2>Integrations</h2>
        <p class="hint">
          Connect your own accounts (Zoho Meeting, etc.) on the web
          portal — your administrator must first enable each integration
          for you.
        </p>
      </div>
    </div>
    <button
      type="button"
      class="primary"
      onclick={() => {
        openUrl("https://app.aftercalls.io/settings/integrations").catch((e) =>
          console.warn("openUrl failed", e),
        );
      }}
    >
      Open integrations on the web portal
    </button>
  </section>

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
          App preferences stored per computer.
        </p>
      </div>
      {#if prefsSavedRecently}<span class="saved">Saved</span>{/if}
    </div>

    <!-- #353 — subdivide behavior card into labelled sections -->
    <h3 class="pref-section">Startup &amp; window</h3>

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

    <h3 class="pref-section">Call detection</h3>

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

    <!-- #180 — sub-toggle of auto-detect. Detection runs in both
         positions; this only controls whether the agent also raises
         + focuses the window. Disabled when the master toggle is off
         (nothing to gate). aria-describedby ties hint to the input
         per the accessibility pattern used elsewhere on this page. -->
    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Show window when a call is detected</span>
        <span class="pref-hint" id="auto-detect-popup-hint">
          {autoDetect
            ? autoDetectPopup
              ? "When a call starts, aftercalls raises and focuses the window so the prompt is unmissable."
              : "Detection still updates the in-app prompt, but the window won't take focus. Helpful on tiling window managers (Hyprland, Sway) where focus-steal disrupts what you're doing."
            : "Available when auto-detect is on."}
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={autoDetectPopup}
          disabled={!autoDetect}
          aria-describedby="auto-detect-popup-hint"
          onchange={(e) => {
            autoDetectPopup = (e.currentTarget as HTMLInputElement).checked;
            saveAppPrefs();
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {autoDetectPopup ? "On" : "Off"}
        </span>
      </label>
    </div>

    <!-- #596 — Auto-record. Sits between Call detection (its
         spiritual sibling — both watch for mic activity) and the
         capture concerns (Input microphone, max length, etc.) below.
         Master toggles + observed-apps list. The per-app checkboxes
         are the gate: master "Auto-record when an allowed app starts
         a call" is meaningless without at least one ticked row, so
         the empty-state copy points users at the list. -->
    <h3 class="pref-section">Auto-record</h3>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">
          Auto-record when an allowed app starts a call
        </span>
        <span class="pref-hint">
          Apps you've allowed below will start a recording automatically
          when they begin using the microphone. You'll have 5 seconds to
          cancel from the in-app toast before the recording fires.
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={autoRecordStartEnabled}
          disabled={!autoRecordStore.platformSupported}
          onchange={(e) => {
            autoRecordStartEnabled = (e.currentTarget as HTMLInputElement)
              .checked;
            // Persist via the existing AppPrefs round-trip AND mirror
            // into the runed store so the next paint of this page (or
            // any other consumer) sees the new value without a fresh
            // get_app_prefs.
            void saveAppPrefs();
            void autoRecordStore.setMaster(
              autoRecordStartEnabled,
              autoRecordStopEnabled,
            );
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {autoRecordStartEnabled ? "On" : "Off"}
        </span>
      </label>
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Auto-stop when the call ends</span>
        <span class="pref-hint">
          When an allowed app releases the microphone, the recording
          stops automatically. Manual recordings are never auto-stopped
          — only ones the agent started for you.
        </span>
      </div>
      <label class="switch">
        <input
          type="checkbox"
          checked={autoRecordStopEnabled}
          disabled={!autoRecordStore.platformSupported}
          onchange={(e) => {
            autoRecordStopEnabled = (e.currentTarget as HTMLInputElement)
              .checked;
            void saveAppPrefs();
            void autoRecordStore.setMaster(
              autoRecordStartEnabled,
              autoRecordStopEnabled,
            );
          }}
        />
        <span class="track" aria-hidden="true">
          <span class="knob"></span>
        </span>
        <span class="switch-label">
          {autoRecordStopEnabled ? "On" : "Off"}
        </span>
      </label>
    </div>

    {#if !autoRecordStore.platformSupported}
      <!-- macOS / other unsupported targets (Linux + Windows ship a
           working observer in v1). Banner copy stays vendor-opaque —
           never names CoreAudio / WASAPI / PipeWire. -->
      <p class="pref-hint auto-record-banner" role="status">
        App detection isn't supported on this OS yet — manual record
        still works.
      </p>
    {:else}
      <div class="auto-record-apps">
        <div class="auto-record-apps-header">
          <span class="pref-title">Apps that have used the microphone</span>
          <span class="pref-hint">
            Tick the apps you want to auto-record. Apps you don't tick
            stay observed only — they appear here so you can decide,
            but they never trigger a recording on their own.
            Browser-based call apps appear as the browser itself;
            ticking your browser will auto-record any tab that uses
            the microphone.
          </span>
        </div>

        {#if autoRecordStore.loading && !autoRecordStore.loaded}
          <p class="pref-hint auto-record-empty">Loading…</p>
        {:else if autoRecordStore.error}
          <p class="error-inline" role="alert">
            Couldn't load the apps list.
            <button
              type="button"
              class="add"
              onclick={() => autoRecordStore.refresh()}
            >
              Try again
            </button>
          </p>
        {:else if autoRecordStore.apps.length === 0}
          <p class="pref-hint auto-record-empty">
            Open the apps you want to record from. They'll appear here
            once they use the microphone, and you can pick which ones
            should auto-record.
          </p>
        {:else}
          <ul class="auto-record-list">
            {#each autoRecordStore.apps as app (app.bundle_id)}
              <li class="auto-record-row">
                <label class="switch switch--compact">
                  <input
                    type="checkbox"
                    checked={app.enabled}
                    aria-label={`Auto-record ${app.friendly_name}`}
                    onchange={(e) =>
                      autoRecordStore.toggleApp(
                        app.bundle_id,
                        (e.currentTarget as HTMLInputElement).checked,
                      )}
                  />
                  <span class="track" aria-hidden="true">
                    <span class="knob"></span>
                  </span>
                </label>
                <div class="auto-record-row-meta">
                  <span class="auto-record-row-name">{app.friendly_name}</span>
                  <span class="auto-record-row-times">
                    Last used {fmtRelative(app.last_seen_at)} • First seen
                    {fmtRelative(app.first_seen_at)}
                  </span>
                </div>
                <button
                  type="button"
                  class="add auto-record-forget"
                  onclick={() => onForgetApp(app)}
                >
                  Forget
                </button>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/if}

    <!-- Input microphone (#3). Capture concern, sits next to auto-
         detect so multi-mic users can pin recordings to the device
         they want without hunting in their OS settings. Native
         <select> reuses the .input class — no new primitive, no CSS
         change. Stale-saved devices are appended to the list with a
         "(not connected)" suffix so the user sees why fallback
         fires. -->
    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title" id="mic-select-label">Input microphone</span>
        <span class="pref-hint" id="mic-select-hint">
          Which microphone the agent records from. "System default"
          follows your OS default; picking a specific device pins
          recordings to that microphone.
        </span>
      </div>
      <select
        class="input mic-select"
        aria-labelledby="mic-select-label"
        aria-describedby="mic-select-hint"
        disabled={micListLoading || !!micListError}
        value={inputDevice ?? ""}
        onchange={(e) =>
          onInputDeviceChange((e.currentTarget as HTMLSelectElement).value)}
      >
        {#if micListLoading}
          <option value="">Loading devices…</option>
        {:else if micListError}
          <option value="">Couldn't load devices</option>
        {:else if inputDevices.length === 0}
          <option value="">No input devices found</option>
        {:else}
          <!-- Standalone "System default" option (decisions.md Q2 —
               native <optgroup>, no separator hack). The "(currently:
               X)" suffix shows which physical device the system
               default would resolve to right now so the user knows
               what they're picking. -->
          <option value="">
            {currentDefaultName
              ? `System default (currently: ${currentDefaultName})`
              : "System default"}
          </option>
          <optgroup label="Devices">
            {#each inputDeviceLabels as opt (opt.value + opt.label)}
              <option value={opt.value} title={opt.value}>
                {opt.label}
              </option>
            {/each}
          </optgroup>
          {#if savedDeviceIsStale && inputDevice}
            <!-- Stale saved device — keep visible so the user sees
                 what's currently breaking, and can re-pick when the
                 device returns. Picking System default or a live
                 device clears it on next save. -->
            <optgroup label="Not connected">
              <option value={inputDevice} title={inputDevice}>
                {inputDevice} — (not connected)
              </option>
            </optgroup>
          {/if}
        {/if}
      </select>
    </div>
    {#if micListError}
      <p class="error-inline mic-error" role="alert">
        Couldn't load input devices.
        <button type="button" class="add mic-retry" onclick={loadInputDevices}>
          Try again
        </button>
      </p>
    {:else if !micListLoading && inputDevices.length === 0}
      <p class="pref-hint mic-empty-hint">
        No microphone is available. Connect one, then reopen Settings.
      </p>
    {/if}

    <h3 class="pref-section">Recording</h3>

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
        <span class="pref-title">Maximum note-to-self length (minutes)</span>
        <span class="pref-hint">
          Recording stops automatically at this length. Tighter cap
          than regular recordings because a dictation is usually
          short. Minimum 1, maximum 60. Default 5.
        </span>
      </div>
      <input
        class="input num-input"
        type="number"
        min="1"
        max="60"
        step="1"
        bind:value={maxSelfNoteMinutes}
        onchange={saveAppPrefs}
      />
    </div>

    <h3 class="pref-section">Shortcuts</h3>

    <div class="pref-row" id="shortcuts">
      <div class="pref-label">
        <span class="pref-title">Note-to-self shortcut</span>
        <span class="pref-hint">
          Global keyboard shortcut that starts a note-to-self
          recording, even when aftercalls isn't focused. Defaults to
          Super+Shift+N (Super = Win/Meta). Click Change and press
          your combo. Clear to disable — the tray, button, and
          command-line still work.
        </span>
        {#if shortcutError}
          <span class="error-inline" role="alert">{shortcutError}</span>
        {/if}
      </div>
      <div class="shortcut-controls">
        {#if capturingShortcut}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="input shortcut-input shortcut-capturing"
            type="text"
            readonly
            autofocus
            value="Press a key combination…"
            onkeydown={onShortcutKeyDown}
            onblur={() => (capturingShortcut = false)}
          />
        {:else}
          <input
            class="input shortcut-input"
            type="text"
            readonly
            value={selfNoteShortcut ?? "Disabled"}
          />
        {/if}
        <button
          type="button"
          class="add"
          onclick={beginCaptureShortcut}
          disabled={capturingShortcut}
        >
          Change
        </button>
        <button
          type="button"
          class="add"
          onclick={clearShortcut}
          disabled={selfNoteShortcut === null}
        >
          Clear
        </button>
      </div>
    </div>

    <div class="pref-row">
      <div class="pref-label">
        <span class="pref-title">Start/stop recording shortcut</span>
        <span class="pref-hint">
          Global keyboard shortcut that starts or stops recording,
          even when aftercalls isn't focused. Defaults to
          Super+Shift+R. Clear to disable — the tray and record
          button still work.
        </span>
        {#if recordToggleError}
          <span class="error-inline" role="alert">{recordToggleError}</span>
        {/if}
      </div>
      <div class="shortcut-controls">
        {#if capturingRecordToggleShortcut}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="input shortcut-input shortcut-capturing"
            type="text"
            readonly
            autofocus
            value="Press a key combination…"
            onkeydown={onRecordToggleShortcutKeyDown}
            onblur={() => (capturingRecordToggleShortcut = false)}
          />
        {:else}
          <input
            class="input shortcut-input"
            type="text"
            readonly
            value={recordToggleShortcut ?? "Disabled"}
          />
        {/if}
        <button
          type="button"
          class="add"
          onclick={beginCaptureRecordToggleShortcut}
          disabled={capturingRecordToggleShortcut}
        >
          Change
        </button>
        <button
          type="button"
          class="add"
          onclick={clearRecordToggleShortcut}
          disabled={recordToggleShortcut === null}
        >
          Clear
        </button>
      </div>
    </div>

    <!-- #282 — pointer to the full shortcut catalogue on the public
         help page. Lives directly under the two recording-shortcut
         rows so users discovering the recording bindings see there's
         more they can do. Opens the system browser via the existing
         Tauri opener plugin (same helper used by the legal links). -->
    <div class="pref-row shortcut-help-row">
      <div class="pref-label">
        <span class="pref-title">All keyboard shortcuts</span>
        <span class="pref-hint">
          See every shortcut available across the desktop app and the
          web portal — playback controls on call pages, list
          navigation, action-item shortcuts, and more.
        </span>
      </div>
      <a
        class="settings-link"
        href="https://aftercalls.io/help#keyboard-shortcuts"
        onclick={(e) => {
          e.preventDefault();
          openUrl("https://aftercalls.io/help#keyboard-shortcuts");
        }}
      >
        See all keyboard shortcuts →
      </a>
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

    <h3 class="pref-section">Notifications</h3>

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

    <!-- #56 — spoken recording-start announcement. Visible only when
         the org's recording_notification_mode is 'user'. In 'enforced'
         mode the announcement plays unconditionally and there's no
         local toggle (an admin's compliance posture isn't silenceable
         here). In 'off' mode the chime is suppressed too, so a
         spoken announcement on its own would be incoherent — row
         hidden. While the mode is still loading (null) we keep the
         row hidden to avoid a flash of UI that disappears. -->
    {#if recordingNotificationMode === "user"}
      <div class="pref-row">
        <div class="pref-label">
          <span class="pref-title">Spoken recording reminder</span>
          <span class="pref-hint">
            Plays a short voice line — "Please note, this call is being
            recorded" — right after the start chime. Off by default;
            turn on if a single tone isn't enough disclosure for your
            calls. Disabled when notification sounds are off.
          </span>
        </div>
        <label class="switch">
          <input
            type="checkbox"
            checked={consentAnnouncementEnabled}
            disabled={!soundsEnabled}
            onchange={(e) => {
              consentAnnouncementEnabled = (e.currentTarget as HTMLInputElement).checked;
              saveAppPrefs();
            }}
          />
          <span class="track" aria-hidden="true">
            <span class="knob"></span>
          </span>
          <span class="switch-label">
            {consentAnnouncementEnabled ? "On" : "Off"}
          </span>
        </label>
      </div>
    {:else if recordingNotificationMode === "enforced"}
      <!-- Read-only disclosure so admins-applied compliance posture
           is at least visible to the user — they shouldn't be
           surprised by a voice line they can't turn off. -->
      <div class="pref-row">
        <div class="pref-label">
          <span class="pref-title">Spoken recording reminder</span>
          <span class="pref-hint">
            Your organization requires a spoken reminder when
            recording starts. It plays after the chime on every
            recording and can't be turned off here.
          </span>
        </div>
        <span class="enforced-badge" aria-label="Required by your organization">
          Required
        </span>
      </div>
    {/if}

    <h3 class="pref-section">Privacy</h3>

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

  <!-- #592 — Privacy summary entry point. Mirrors the portal's
       `/settings → /settings/privacy` card so a user on the agent can
       reach the same read-only posture surface (TOS history, calls
       count, access log) and the "Export my data" action without
       round-tripping through the web portal. The deeper page renders
       at `/settings/privacy`. -->
  <section class="card" style="--i: 1.7">
    <div class="card-head">
      <div>
        <h2>Privacy</h2>
        <p class="hint">
          See what your account holds, what terms you've accepted, who
          has opened the calls you've recorded, and download a copy of
          your data.
        </p>
      </div>
      <a class="add" href="/settings/privacy">View privacy summary →</a>
    </div>
  </section>

  <!-- #317 — recording-policy explainer. Relocated from the auto-detect
       ack modal so the slim variant of that modal can stay scoped to a
       single sentence + checkbox (auto-detect's 30s OS-toast deadline
       wouldn't accommodate a wall of legal text). The copy below is
       the verbatim text that shipped in the modal — legal review
       attaches to the wording, not the surface. The slim modal links
       to this section via the `#recording-policy` anchor. -->
  <section class="card" style="--i: 1.75" id="recording-policy">
    <div class="card-head">
      <div>
        <h2>Recording policy</h2>
        <p class="hint">
          What you confirm every time you start recording a call.
        </p>
      </div>
    </div>
    <div class="recording-policy">
      <p>
        Under Canadian PIPEDA — and equivalent privacy laws in most
        jurisdictions — you are responsible for notifying every
        participant that the call is being recorded, obtaining their
        consent, and using the recording only for the purpose you
        disclosed.
      </p>
      <p>aftercalls doesn't automate consent. You do.</p>
      <p>
        When you do record, your audio is transcribed and summarized
        by automated services to generate transcripts, summaries, and
        suggested action items. These outputs are advisory — you can
        edit or delete any of them, or delete the entire call, at any
        time. See the Privacy Policy for the full disclosure under
        GDPR Article 22.
      </p>
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
        <label class="field-label" for="vault-folder-input">Vault folder</label>
        <div class="vault-row">
          <input
            id="vault-folder-input"
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
        <label class="field-label" for="vault-clients-subpath-input">Clients subfolder (optional)</label>
        <input
          id="vault-clients-subpath-input"
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

  <!-- #69 — About card. Surfaces running version + the LGPL ffmpeg
       notice next to a link out to the canonical public manifest.
       The legal-footer below already links to /licenses, but that
       footer is intentionally muted; this card is where the LGPL
       notice itself is *visible* in the app, satisfying the
       "notice in documentation" bar more loudly than a footer
       link alone. -->
  <section class="card about-card" style="--i: 4.5">
    <div class="card-head">
      <div>
        <h2>About</h2>
        <p class="hint">
          Build info and the open-source components bundled with the
          desktop app.
        </p>
      </div>
    </div>
    <dl class="about-grid">
      <dt>aftercalls</dt>
      <dd><code>{appVersion ?? "—"}</code></dd>
    </dl>
    <p class="about-lgpl">
      This build bundles a sidecar copy of <strong>ffmpeg</strong>,
      distributed under the <strong>LGPL v2.1+</strong>. Pinned upstream
      versions, the LGPL license text, and the corresponding source
      tarballs (mirrored on our infrastructure so you can fetch them
      directly) are listed on the
      <a
        href="https://aftercalls.io/licenses"
        onclick={(e) => { e.preventDefault(); openUrl("https://aftercalls.io/licenses"); }}
      >open-source licenses page</a>.
    </p>
    <div class="about-actions">
      <a
        class="add"
        href="https://aftercalls.io/licenses"
        onclick={(e) => { e.preventDefault(); openUrl("https://aftercalls.io/licenses"); }}
      >Open source licenses ↗</a>
    </div>
  </section>

  <footer class="legal-footer" style="--i: 5">
    <a
      href="https://aftercalls.io/help/system-requirements"
      onclick={(e) => { e.preventDefault(); openUrl("https://aftercalls.io/help/system-requirements"); }}
    >System requirements</a>
    <span class="sep" aria-hidden="true">·</span>
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

  /* #96: Profile card — First + Last side-by-side on wide, stack
     below 540px. Scoped to this file (mirror of portal's settings
     .name-row); app.css untouched so the byte-identity invariant
     stays satisfied. */
  .name-row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.75rem;
  }
  @media (max-width: 540px) {
    .name-row {
      grid-template-columns: 1fr;
    }
  }
  .name-field {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    min-width: 0;
  }
  .name-field .input {
    min-width: 0;
    width: 100%;
    box-sizing: border-box;
    /* Geist sans for human names — `.input` in this file defaults
       to the mono stack for technical values. Names are not
       technical. */
    font-family: inherit;
  }
  .name-err {
    margin: 0.2rem 0 0;
  }
  /* #484 — character counter for the first-name input */
  .char-counter {
    margin: 0.2rem 0 0;
    font-size: 0.72rem;
    color: var(--bone-4);
    text-align: right;
  }
  .char-counter.char-counter-near {
    color: var(--live);
  }
  .name-actions {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    margin-top: 0.9rem;
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

  /* #353 — behavior card section dividers */
  .pref-section {
    margin: 1.1rem 0 0;
    padding-bottom: 0.35rem;
    border-bottom: 1px solid var(--hairline);
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--bone-3);
  }
  .pref-section:first-of-type {
    margin-top: 0.2rem;
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
  /* #596 — compact switch for the per-app auto-record rows.
     Smaller knob/track so the row stays light next to the app name +
     timestamps. Mirrored in the design-system app.css so the portal
     stays in lockstep. */
  .switch--compact .track {
    width: 28px;
    height: 16px;
  }
  .switch--compact .knob {
    width: 10px;
    height: 10px;
  }
  .switch--compact input:checked + .track .knob {
    transform: translateX(12px);
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
  /* #149 — self-note shortcut capture widget. Read-only input + two
     Change/Clear pills so the shape matches the adjacent number +
     toggle rows without introducing new primitives. */
  .shortcut-controls {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    flex: 0 0 auto;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .shortcut-input {
    flex: 0 0 12rem;
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 0.82rem;
    text-align: left;
  }
  .shortcut-capturing {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  /* Mic-device dropdown (#3). Wider than .num-input because device
     names are long; truncates rather than pushes the row layout
     around. Matches `.input` styling otherwise — no new primitive. */
  .mic-select {
    flex: 0 0 16rem;
    min-width: 0;
    max-width: 16rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* Native <select> needs explicit cursor + appearance hints to
       look like a control on Linux/GTK. Keep the right-edge caret
       affordance — `appearance: auto` (the default) is fine. */
    cursor: pointer;
  }
  .mic-select:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  /* Inline retry / empty hint slide under the mic row on a new
     line so the row layout itself stays stable when they appear,
     mirroring the .autostart-error pattern. */
  .mic-error,
  .mic-empty-hint {
    margin: 0.25rem 0 0.5rem;
  }
  .mic-retry {
    margin-left: 0.6rem;
    padding: 0.2rem 0.6rem;
    font-size: 0.75rem;
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

  /* #56 — read-only "Required" pill rendered in place of the toggle
     when the org's recording_notification_mode is 'enforced'.
     Mirrors the muted-mono treatment used elsewhere on the page so
     it reads as informational, not a button. Sits in the same slot
     a .switch would occupy so the row layout doesn't reflow. */
  .enforced-badge {
    flex-shrink: 0;
    padding: 0.32rem 0.7rem;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--ink-2);
    color: var(--bone-2);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
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

  /* #282 — "See all keyboard shortcuts" link sitting under the
     two recording-shortcut config rows. Borderless link styling so
     it reads as a pointer rather than a primary action. */
  .settings-link {
    flex-shrink: 0;
    color: var(--accent);
    font-size: 0.85rem;
    text-decoration: none;
    padding: 0.3rem 0.2rem;
    transition: color 0.15s;
  }
  .settings-link:hover {
    color: var(--accent-hi);
    text-decoration: underline;
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

  /* #69 — About card. Mirror of `.card` styling (so it inherits the
     surrounding rhythm), with a small grid for the version row + an
     LGPL paragraph that reads as informational, not a CTA. Scoped
     here so the byte-identity invariant on app.css stays satisfied. */
  .about-grid {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.4rem 0.9rem;
    margin: 0 0 0.8rem;
    align-items: baseline;
    font-size: 0.86rem;
  }
  .about-grid dt {
    color: var(--bone-3);
    font-size: 0.78rem;
    font-family: var(--font-mono);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .about-grid dd {
    margin: 0;
    color: var(--bone-0);
  }
  .about-grid code {
    font-family: var(--font-mono);
    font-size: 0.82rem;
    color: var(--bone-0);
  }
  .about-lgpl {
    margin: 0 0 0.9rem;
    color: var(--bone-2);
    font-size: 0.82rem;
    line-height: 1.55;
    max-width: 58ch;
  }
  .about-lgpl a {
    color: var(--accent);
  }
  .about-lgpl a:hover {
    color: var(--accent-hi);
  }
  .about-actions {
    display: flex;
    align-items: center;
    gap: 0.6rem;
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

  /* #317 — recording-policy explainer block. Plain prose; the card
     shell gives it the same chrome as every other settings section
     so it doesn't read as an alert. */
  .recording-policy p {
    margin: 0 0 0.7rem;
    color: var(--bone-2);
    font-size: 0.88rem;
    line-height: 1.55;
    max-width: 64ch;
  }
  .recording-policy p:last-child {
    margin-bottom: 0;
  }

  /* #596 — Auto-record section. Reuses .pref-row / .pref-section /
     .switch primitives; the only new chrome is the per-app list +
     the smaller `switch--compact` modifier (which lives in the
     design-system app.css so the portal stays in lockstep). */
  .auto-record-banner {
    margin: 0.5rem 0 0.2rem;
  }
  .auto-record-apps {
    margin-top: 0.6rem;
    padding: 0.7rem 0;
    border-top: 1px solid var(--hairline);
  }
  .auto-record-apps-header {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    margin-bottom: 0.7rem;
  }
  .auto-record-empty {
    margin: 0;
  }
  .auto-record-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0;
  }
  .auto-record-row {
    display: flex;
    align-items: center;
    gap: 0.85rem;
    padding: 0.55rem 0;
    border-top: 1px solid var(--hairline);
  }
  .auto-record-row:first-child {
    border-top: none;
  }
  .auto-record-row-meta {
    display: flex;
    flex-direction: column;
    flex: 1 1 auto;
    min-width: 0;
    gap: 0.1rem;
  }
  .auto-record-row-name {
    font-size: 0.88rem;
    color: var(--bone-0);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .auto-record-row-times {
    font-size: 0.75rem;
    color: var(--bone-3);
    font-family: var(--font-mono);
  }
  .auto-record-forget {
    flex-shrink: 0;
  }
</style>
