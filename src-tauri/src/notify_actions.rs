//! Cross-platform actionable OS notification (#89).
//!
//! Mic-detect needs to ask the user "record this call?" without stealing
//! window focus. The existing `tauri-plugin-notification` desktop builder
//! is body-only (action types are mobile-only on plugin v2.3.x), so this
//! module bypasses the plugin and talks to each OS's native notification
//! API directly:
//!
//! - Linux  → `notify-rust` (DBus, `org.freedesktop.Notifications`)
//! - Windows → `tauri-winrt-notification` (WinRT toast + Action Center)
//! - macOS  → `mac-notification-sys` (NSUserNotificationCenter)
//!
//! All three crates call the OS directly from Rust, so there is no Tauri
//! IPC capability surface to register; `capabilities/default.json` is
//! unchanged. The pre-existing `tauri-plugin-notification` integration in
//! `lib.rs::notify_call_ready` (#286) coexists — that path delivers
//! body-only "your call is ready" toasts and has no action buttons, so
//! it can keep using the plugin.
//!
//! ## Contract
//!
//! [`show_actionable_notification`] posts the notification synchronously
//! (returns `Ok(())` once it's been handed to the OS daemon) and spawns
//! a tokio task that waits for either the user's choice OR a `timeout`.
//! When that resolves, the task emits a Tauri event named
//! `callback_event_name` on the supplied [`tauri::AppHandle`] with the
//! payload shape:
//!
//! ```json
//! { "action_id": "record" | "dismiss" | <custom>, "auto_dismissed": false }
//! ```
//!
//! `auto_dismissed=true` carries the timeout case so the frontend can
//! distinguish a passive expiry from an explicit "Not now" click. v1
//! routes both to the same dismiss path; the bit is there for future
//! telemetry.
//!
//! Callers MUST pattern-match [`NotifyError::PermissionDenied`] (and
//! treat any other [`Err`] as a permission proxy) to drive their
//! fallback — typically [`crate::show_main_window`]. Never silently
//! swallow.
//!
//! ## Snooze
//!
//! Pass `snooze_key = Some(consumer_name)` to suppress repeat
//! notifications for the same mic consumer for [`SNOOZE_WINDOW`]. A
//! successful "record" choice clears the snooze immediately so the next
//! distinct mic event still toasts. Belt-and-braces — the detector's
//! own `Phase::Suppressed` arm is the primary gate, this is a backstop
//! against rapid-cycle Linux poll edges.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};

/// Window during which a repeated `show_actionable_notification` call
/// for the same `snooze_key` is silently turned into `Ok(())` without
/// posting another OS toast. 5 min picked to roughly straddle a
/// re-join after a brief disconnect; long enough that the user isn't
/// re-asked every poll cycle.
pub const SNOOZE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// One action button on the notification. `id` round-trips to the
/// frontend `notify-action` event payload; `label` is the user-visible
/// text. Linux + Windows preserve both. macOS exposes only the label,
/// so the macOS impl maps the returned label back to the matching
/// `id` before emitting.
#[derive(Clone, Debug)]
pub struct ActionSpec {
    pub id: String,
    pub label: String,
}

#[derive(thiserror::Error, Debug)]
pub enum NotifyError {
    /// macOS / Windows: user toggled notifications off at the OS
    /// level. Linux has no equivalent — DaemonUnavailable covers the
    /// closest case there.
    #[error("notifications not permitted by OS")]
    PermissionDenied,
    /// Linux: no DBus notification daemon present (headless CI,
    /// broken session). Windows / macOS use this for bus-level
    /// failures that aren't permission denials.
    #[error("notification daemon unavailable")]
    DaemonUnavailable,
    /// macOS: caller is not running inside a code-signed .app with a
    /// valid CFBundleIdentifier (e.g. plain `cargo run` from a
    /// terminal).
    #[error("app bundle id missing")]
    BundleMissing,
    /// Catch-all: backend-specific error message. Caller treats this
    /// the same as the per-OS variants above (fall through to
    /// show_main_window).
    #[error("backend error: {0}")]
    Backend(String),
}

// PermissionDenied / BundleMissing / Backend variants are only
// constructed under Windows / macOS cfgs; allow dead_code on the
// enum so a Linux-only build (the dev loop) doesn't spam the
// compiler with "never constructed" warnings for variants that are
// load-bearing on the other targets.
#[cfg(target_os = "linux")]
const _: fn() = || {
    let _ = NotifyError::PermissionDenied;
    let _ = NotifyError::BundleMissing;
    let _ = NotifyError::Backend(String::new());
};

/// Snooze table — `(snooze_key, last_shown_at)`.
///
/// Tiny global state; locked only at the start of each call and at
/// emit-time. Std `Mutex` is fine — no async holding, no contention
/// (mic-detect ticks at 5s + the lock is held for microseconds).
static SNOOZE: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

fn snooze_active(key: &str) -> bool {
    let mut guard = match SNOOZE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    // Drop expired entries opportunistically so the Vec doesn't grow
    // unbounded across long sessions with many distinct consumers.
    guard.retain(|(_, t)| t.elapsed() < SNOOZE_WINDOW);
    guard.iter().any(|(k, _)| k == key)
}

fn snooze_set(key: &str) {
    let mut guard = match SNOOZE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.retain(|(k, _)| k != key);
    guard.push((key.to_string(), Instant::now()));
}

/// Clear the snooze entry for `key` (if any). Called after a confirmed
/// "record" so the next distinct mic event for the same consumer can
/// still toast.
pub fn clear_snooze(key: &str) {
    let mut guard = match SNOOZE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.retain(|(k, _)| k != key);
}

#[derive(serde::Serialize, Clone)]
struct NotifyActionPayload {
    action_id: String,
    auto_dismissed: bool,
}

/// Emit the resolution event back to the frontend.
///
/// Wraps `app.emit` with an eprintln on failure — emit can only fail
/// if the Tauri runtime is shutting down, in which case the user is
/// past caring about the toast anyway.
fn emit_resolution(app: &AppHandle, event: &str, action_id: &str, auto_dismissed: bool) {
    if let Err(e) = app.emit(
        event,
        NotifyActionPayload {
            action_id: action_id.to_string(),
            auto_dismissed,
        },
    ) {
        eprintln!("aftercalls: notify_actions emit failed: {e}");
    }
}

/// Show an OS-native notification with action buttons.
///
/// Posts the notification, then spawns a background task that waits
/// for the user to click an action OR for `timeout` to fire. Either
/// way, the task emits `callback_event_name` on `app` with payload
/// `{ action_id, auto_dismissed }`.
///
/// Returns `Ok(())` once the toast has been handed to the OS daemon.
/// The caller does NOT await the user's choice.
///
/// `snooze_key` (if provided) suppresses repeats for the same key
/// within [`SNOOZE_WINDOW`] — the second call returns `Ok(())` without
/// posting a new toast and without spawning a wait-task. Pass `None`
/// to bypass.
pub fn show_actionable_notification(
    app: AppHandle,
    title: &str,
    body: &str,
    actions: Vec<ActionSpec>,
    callback_event_name: &str,
    timeout: Duration,
    snooze_key: Option<&str>,
) -> Result<(), NotifyError> {
    if let Some(key) = snooze_key {
        if snooze_active(key) {
            return Ok(());
        }
    }

    let result = backend::show(
        app,
        title,
        body,
        actions,
        callback_event_name.to_string(),
        timeout,
    );

    if result.is_ok() {
        if let Some(key) = snooze_key {
            snooze_set(key);
        }
    }

    result
}

// ─────────────────────────────────────────────────────────────────────
// Linux backend (notify-rust + DBus)
// ─────────────────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod backend {
    use super::*;

    pub(super) fn show(
        app: AppHandle,
        title: &str,
        body: &str,
        actions: Vec<ActionSpec>,
        callback_event_name: String,
        timeout: Duration,
    ) -> Result<(), NotifyError> {
        use notify_rust::{Notification, Timeout};

        let mut n = Notification::new();
        n.summary(title).body(body).appname("aftercalls");
        for a in &actions {
            n.action(&a.id, &a.label);
        }
        // notify-rust's Timeout::Milliseconds is a hint to the daemon;
        // the wait_for_action future also resolves on the daemon's
        // own expiry (which most daemons honor). We additionally race
        // a tokio sleep below for daemons that ignore the hint
        // (gsd-notification-daemon falls into this bucket).
        let timeout_ms: i32 = timeout
            .as_millis()
            .try_into()
            .unwrap_or(i32::MAX);
        n.timeout(Timeout::Milliseconds(timeout_ms as u32));

        let handle = match n.show() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("aftercalls: notify failed on linux: {e}");
                // notify-rust returns DBus errors when the session bus
                // has no notification daemon (headless CI, broken
                // session) — surface as DaemonUnavailable so the
                // caller falls through to show_main_window.
                return Err(NotifyError::DaemonUnavailable);
            }
        };

        // Move the handle into a blocking thread because
        // wait_for_action blocks on a synchronous DBus signal listen.
        // We race it against a tokio sleep so a non-cooperating daemon
        // (e.g. gnome-shell when actions are restricted) can't pin
        // the wait-task forever.
        let event = callback_event_name.clone();
        let app_for_emit = app.clone();
        let actions_for_emit = actions.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<(String, bool)>();

        std::thread::spawn(move || {
            // wait_for_action blocks until the user clicks an action,
            // closes the toast, or the daemon expires it. The
            // high-level `&str` overload collapses ActionResponse —
            // Custom(id) → id, Closed(_) → "__closed" (a sentinel
            // baked into notify-rust 4.x for back-compat with 5.0
            // pre-release; treated here as auto-dismiss).
            handle.wait_for_action(|action| {
                let (id, auto) = if action == "__closed" {
                    // Closed by user gesture OR by daemon expiry — v1
                    // routes both to dismiss. The tokio sleep below
                    // also covers daemons that swallow this signal.
                    ("dismiss".to_string(), true)
                } else if action == "default" {
                    // Some daemons emit "default" when the user clicks
                    // the body of the toast — treat as the primary
                    // (first) action's id.
                    let id = actions_for_emit
                        .first()
                        .map(|a| a.id.clone())
                        .unwrap_or_else(|| "default".to_string());
                    (id, false)
                } else {
                    (action.to_string(), false)
                };
                let _ = tx.send((id, auto));
            });
        });

        tauri::async_runtime::spawn(async move {
            let outcome = tokio::select! {
                got = rx => got.ok(),
                _ = tokio::time::sleep(timeout) => None,
            };
            let (action_id, auto) = outcome.unwrap_or_else(|| ("dismiss".to_string(), true));
            emit_resolution(&app_for_emit, &event, &action_id, auto);
        });

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Windows backend (tauri-winrt-notification + WinRT Action Center)
// ─────────────────────────────────────────────────────────────────────
//
// Notes for the operator's Windows VM smoke test (builder cannot run
// this locally; verification is gated on a real installed agent):
//
// 1. AUMID. WinRT toasts route activations through an Application User
//    Model ID that maps to a Start-menu shortcut. Tauri's NSIS / WiX
//    installer registers a shortcut and uses the bundle identifier
//    (`io.aftercalls.app` from tauri.conf.json) as the AUMID; the
//    `Toast::new(AUMID)` call below passes that string directly.
//    Verify post-install with PowerShell:
//
//        Get-StartApps | Where-Object { $_.Name -like '*aftercalls*' }
//
//    If the listed AppID does not match `io.aftercalls.app`, update
//    AUMID below + the installer config in tandem.
//
// 2. Dev-mode caveat. Running `cargo run` (no installed Start-menu
//    shortcut) falls back to the PowerShell AUMID and shows a toast
//    branded as PowerShell. Known crate limitation; only matters
//    during local dev. Smoke testing happens on installer-built
//    binaries.
#[cfg(target_os = "windows")]
mod backend {
    use super::*;

    /// Bundle identifier from `agent/src-tauri/tauri.conf.json`'s
    /// `bundle.identifier`. Kept as a const so any future bundle-id
    /// change has to consciously update both surfaces.
    const AUMID: &str = "io.aftercalls.app";

    pub(super) fn show(
        app: AppHandle,
        title: &str,
        body: &str,
        actions: Vec<ActionSpec>,
        callback_event_name: String,
        timeout: Duration,
    ) -> Result<(), NotifyError> {
        use tauri_winrt_notification::{Duration as ToastDuration, Toast};

        // Shared sender: drained by whichever fires first — a button
        // click (on_activated) or the toast's dismissal
        // (on_dismissed). Wrapped in Arc<Mutex<Option<_>>> so both
        // closures can take() it safely; the second .take() returns
        // None and is a silent no-op. The tokio task below races a
        // sleep against the receiver so a toast that lingers in
        // Action Center past `timeout` still resolves the wait.
        let (tx, rx) = tokio::sync::oneshot::channel::<(String, bool)>();
        let shared: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<(String, bool)>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let shared_for_activated = shared.clone();
        let shared_for_dismissed = shared.clone();

        let actions_for_activated = actions.clone();

        let mut toast = Toast::new(AUMID).title(title).text1(body);
        for a in &actions {
            toast = toast.add_button(&a.label, &a.id);
        }
        toast = toast
            .duration(ToastDuration::Short)
            .on_activated(move |arg| {
                // arg is the action string passed to add_button —
                // i.e. the ActionSpec.id we registered. None means
                // body-click; resolve to the first action's id.
                let resolved = match arg {
                    Some(s) if !s.is_empty() => s,
                    _ => actions_for_activated
                        .first()
                        .map(|a| a.id.clone())
                        .unwrap_or_else(|| "default".to_string()),
                };
                if let Ok(mut g) = shared_for_activated.lock() {
                    if let Some(s) = g.take() {
                        let _ = s.send((resolved, false));
                    }
                }
                Ok(())
            })
            .on_dismissed(move |_reason| {
                if let Ok(mut g) = shared_for_dismissed.lock() {
                    if let Some(s) = g.take() {
                        let _ = s.send(("dismiss".to_string(), true));
                    }
                }
                Ok(())
            });

        if let Err(e) = toast.show() {
            eprintln!("aftercalls: notify failed on windows: {e}");
            // E_ACCESSDENIED (0x80070005) — user has notifications
            // disabled at the OS level. Surface as PermissionDenied
            // so the caller falls through to show_main_window.
            // The crate wraps windows::core::Error in its Os variant;
            // check the HRESULT via the Display impl since the inner
            // Error type's accessor surface drifts across windows
            // crate minor versions.
            let msg = format!("{e}");
            // 0x80070005 in the message is the canonical access-denied
            // signature; HRESULT(0x80070005) renders as the same hex
            // string on every windows-rs >= 0.50.
            if msg.contains("0x80070005") {
                return Err(NotifyError::PermissionDenied);
            }
            return Err(NotifyError::Backend(format!("WinRT toast: {msg}")));
        }

        // Spawn the wait task: race the user's choice against `timeout`.
        let event = callback_event_name.clone();
        let app_for_emit = app.clone();
        tauri::async_runtime::spawn(async move {
            let outcome = tokio::select! {
                got = rx => got.ok(),
                _ = tokio::time::sleep(timeout) => None,
            };
            let (action_id, auto) = outcome.unwrap_or_else(|| ("dismiss".to_string(), true));
            emit_resolution(&app_for_emit, &event, &action_id, auto);
        });

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// macOS backend (mac-notification-sys + NSUserNotificationCenter)
// ─────────────────────────────────────────────────────────────────────
//
// Notes for the operator's macOS smoke test (builder cannot run this
// locally; verification is gated on a real installed agent — and
// fully on signed builds once #68 lands):
//
// 1. Bundle ID. mac-notification-sys requires the calling binary to
//    run inside an .app with an Info.plist carrying CFBundleIdentifier.
//    Tauri's bundle.identifier (`io.aftercalls.app`) gives us this in
//    packaged builds. `cargo run` from a terminal will fail
//    set_application with 'not running from a bundle' — the operator
//    must `pnpm tauri build --bundles app` (or run from the .app) to
//    smoke test.
//
// 2. Permissions. The first send() triggers the OS-level
//    'aftercalls would like to send notifications' prompt. Approving
//    it persists across launches; declining silently drops every
//    subsequent toast. The crate has no permission-denied error path,
//    so a denied prompt looks identical to a successful send to us —
//    the wait-task's tokio timeout fires and the user gets the
//    in-app slide-out fallback on the next mic-event. Acceptable v1
//    UX; first-run permission prompt is visible to the user.
//
// 3. Unsigned-bundle interaction with #68. Notifications work on
//    unsigned bundles today (Gatekeeper-launched first-runs prompt
//    once for permission). When #68 ships notarization, retest:
//    signed bundles MAY (no documented change) tighten permission
//    flow. Update this note + retest acceptance under the signed
//    build at that time.
//
// 4. send() is BLOCKING. mac-notification-sys returns the user's
//    response synchronously — the function does not return until the
//    user clicks or the system dismisses. We wrap it in
//    spawn_blocking so the calling tokio task isn't pinned, and
//    race the result against an explicit timeout via tokio::select!.
#[cfg(target_os = "macos")]
mod backend {
    use super::*;

    /// Bundle identifier from `agent/src-tauri/tauri.conf.json`'s
    /// `bundle.identifier`. Must match the Info.plist
    /// CFBundleIdentifier of the running .app or set_application
    /// fails with NotificationError::ApplicationError.
    const BUNDLE_ID: &str = "io.aftercalls.app";

    pub(super) fn show(
        app: AppHandle,
        title: &str,
        body: &str,
        actions: Vec<ActionSpec>,
        callback_event_name: String,
        timeout: Duration,
    ) -> Result<(), NotifyError> {
        use mac_notification_sys::{
            error::{ApplicationError, Error as MacNotifyError},
            set_application, MainButton, Notification, NotificationResponse,
        };

        // set_application uses a Once internally — only the FIRST call
        // actually sets the bundle identifier. Subsequent calls return
        // Err(ApplicationError::AlreadySet(_)), which is not actually
        // a failure for us; treat it as Ok. The CouldNotSet variant
        // (failed sys-level set) is the real bundle-missing case →
        // BundleMissing → caller falls through to show_main_window.
        match set_application(BUNDLE_ID) {
            Ok(()) => {}
            Err(MacNotifyError::Application(ApplicationError::AlreadySet(_))) => {}
            Err(e) => {
                eprintln!("aftercalls: notify failed on macos: set_application: {e}");
                return Err(NotifyError::BundleMissing);
            }
        }

        // Map our ActionSpec list → main_button + close_button. The
        // crate's MainButton::SingleAction renders a single-action
        // primary button; close_button renders the secondary
        // dismiss-style button. We need both, so the contract is:
        // first ActionSpec → main button (Record), last → close
        // button (Not now). Anything in between is ignored on macOS
        // (the crate doesn't expose multi-button without a dropdown,
        // and a dropdown for 2 actions would be UX-hostile).
        let main_label = actions
            .first()
            .map(|a| a.label.clone())
            .unwrap_or_else(|| "OK".to_string());
        let close_label = actions
            .iter()
            .nth(1)
            .map(|a| a.label.clone())
            .unwrap_or_else(|| "Dismiss".to_string());
        let actions_for_map = actions.clone();

        // mac-notification-sys's Notification builder uses &'a refs
        // bound to the buffer's lifetime, so we have to materialize
        // owned strings before the builder borrows them, then move
        // those into the spawn_blocking closure.
        let title_owned = title.to_string();
        let body_owned = body.to_string();
        let event = callback_event_name.clone();
        let app_for_emit = app.clone();

        // send() blocks until the user resolves the notification or
        // the system drops it. Wrap in spawn_blocking so we don't pin
        // a tokio worker; race with timeout via select!.
        let join = tauri::async_runtime::spawn_blocking(move || {
            let mut n = Notification::new();
            n.title(&title_owned)
                .message(&body_owned)
                .main_button(MainButton::SingleAction(&main_label))
                .close_button(&close_label);
            n.send()
        });

        tauri::async_runtime::spawn(async move {
            let outcome = tokio::select! {
                got = join => match got {
                    Ok(Ok(resp)) => Some(resp),
                    Ok(Err(e)) => {
                        eprintln!("aftercalls: notify failed on macos: send: {e}");
                        None
                    }
                    Err(e) => {
                        eprintln!("aftercalls: notify spawn_blocking joined with err: {e}");
                        None
                    }
                },
                _ = tokio::time::sleep(timeout) => None,
            };

            // Map the macOS response back to our ActionSpec.id by
            // matching on the user-visible label string the system
            // returns. Anything unmapped → dismiss.
            let (action_id, auto) = match outcome {
                Some(NotificationResponse::ActionButton(label))
                | Some(NotificationResponse::CloseButton(label)) => {
                    let id = actions_for_map
                        .iter()
                        .find(|a| a.label == label)
                        .map(|a| a.id.clone())
                        .unwrap_or_else(|| {
                            // Unknown label → treat as dismiss for
                            // safety. Logged so operator can spot
                            // label drift between the toast spec and
                            // what the system actually echoed back.
                            eprintln!(
                                "aftercalls: macos notify response \"{label}\" did not match any registered action"
                            );
                            "dismiss".to_string()
                        });
                    (id, false)
                }
                Some(NotificationResponse::Click) => {
                    // Body click → first action's id (record).
                    let id = actions_for_map
                        .first()
                        .map(|a| a.id.clone())
                        .unwrap_or_else(|| "default".to_string());
                    (id, false)
                }
                Some(NotificationResponse::Reply(_)) | Some(NotificationResponse::None) | None => {
                    // No interaction OR reply (we don't use Response
                    // input) OR timeout → auto-dismiss.
                    ("dismiss".to_string(), true)
                }
            };
            emit_resolution(&app_for_emit, &event, &action_id, auto);
        });

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Other platforms (BSDs, etc.) — auto-detect itself is a no-op there
// (detector::raw_mic_consumers returns Vec::new() for non-linux,
// non-windows targets), so this stub will never actually be called.
// Kept for cargo check completeness.
// ─────────────────────────────────────────────────────────────────────
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod backend {
    use super::*;

    pub(super) fn show(
        _app: AppHandle,
        _title: &str,
        _body: &str,
        _actions: Vec<ActionSpec>,
        _callback_event_name: String,
        _timeout: Duration,
    ) -> Result<(), NotifyError> {
        Err(NotifyError::Backend(
            "notify_actions backend not implemented for this OS".into(),
        ))
    }
}
