//! Mic-consumer enumeration shared by the auto-detect prompt path
//! (`detector.rs`) and the auto-record observer (`audio_observer.rs`).
//!
//! Single source of truth for "which other apps are currently capturing
//! the system microphone". Extracted out of `detector.rs` (#596) so the
//! observer doesn't have to spawn its own `pactl` / WASAPI poll, and the
//! two consumers stay byte-identical in their view of who's holding the
//! mic.
//!
//! Each `RawMicConsumer` exposes both:
//!   * `bundle_id`: a stable key (process binary on Linux, exe basename
//!     on Windows, bundle id on macOS once that lands). `auto_recorder`
//!     and the `observed_apps` sqlite table key off this.
//!   * `friendly_name`: a human-readable display string. Falls back to
//!     `bundle_id` when the OS-supplied label is empty OR matches a
//!     generic audio-framework boilerplate pattern (e.g. PipeWire's
//!     "PipeWire ALSA [<binary>]" placeholder for raw ALSA streams).
//!
//! Public-copy posture: the strings the observer surfaces to the
//! webview come straight from `friendly_name` (or `bundle_id`), so
//! they're whatever the user's OS named the app — never our own
//! "PipeWire" / "PulseAudio" label.
//!
//! ## Cross-platform contract (read this before adding macOS, or
//!  before touching the Windows path)
//!
//! Every `raw_mic_consumers()` impl MUST:
//!
//! 1. **Filter our own process.** Whatever the agent's bundled binary
//!    is named on the host (`aftercalls` on Linux/macOS, `aftercalls.exe`
//!    on Windows), the impl skips any source-output / audio-session
//!    whose owning process is us. Both PID-based filtering (cheap,
//!    works for the main agent process) AND bundle-id matching against
//!    the agent's binary name (catches the cpal capture stream that PA
//!    sometimes attributes to a child PID) are required. Failing to
//!    do this will let `aftercalls` appear in the observer's catalog,
//!    and an enabled row would loop a recording onto itself (#604 was
//!    exactly this on Linux because `own_bundle_id()` returned the
//!    crate name `agent` instead of the bundled binary `aftercalls`).
//!
//! 2. **Apply `MIC_CONSUMER_BLACKLIST`.** Vendor helpers we spawn
//!    (`parec`, `pacat`, `pw-cat`, `pw-record`, `ffmpeg`) and generic
//!    framework labels (`PipeWire ALSA [client]`, `Chromium input`)
//!    must be filtered at the source — NOT only inside
//!    `detector::interesting_mic_consumers`. The observer used to skip
//!    this filter and leaked `parec` + `Chromium input` into the
//!    user-visible catalog (#604).
//!
//! 3. **Apply `is_helper_proc()`.** Crashpad handlers, Chromium GPU
//!    helpers, Electron renderer subprocesses (binaries named
//!    `*_helper`, `crashpad_*`, `*-renderer`, `*_renderer`,
//!    `gpu-process`) are filtered. The user's mental model is the
//!    parent app, never the helper.
//!
//! 4. **Surface `friendly_name` only when it's actually friendly.**
//!    On Linux, PipeWire reports raw ALSA streams as
//!    "PipeWire ALSA [<binary>]" with no `application.name` set by the
//!    app itself. Surfacing that string in the observer catalog reads
//!    as a vendor-name leak ("PipeWire" is our infra) and doesn't
//!    match the detector's `bundle_id` rendering. Fall back to
//!    `bundle_id` (binary basename) when the OS string matches the
//!    framework's generic pattern. Same posture applies to any future
//!    macOS impl that hits CoreAudio's default fallback strings.
//!
//! 5. **Stable `bundle_id`.** Must remain stable across app restarts
//!    and version updates so `observed_apps` rows survive. A renamed
//!    `application.name` (e.g. Slack swapping to "Slack | DM") should
//!    NOT detach the row.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::watch;

/// Vendor helpers + generic audio-framework labels we never want to
/// surface as a "mic-using app". Substring-matched case-insensitively
/// against the stable app key (`bundle_id`). Generic labels that only
/// appear as `friendly_name` fall back to the bundle key for display
/// instead of dropping the app entirely (#610).
///
/// New entries land here, NOT in any per-platform list. Per-platform
/// helper-process patterns (Chromium renderers etc.) live in
/// `is_helper_proc` because their structure is regex-like rather than
/// a literal substring.
pub const MIC_CONSUMER_BLACKLIST: &[&str] = &[
    "pipewire alsa [client]", // generic ALSA client (our cpal mic path lands here)
    "pipewire alsa [aftercalls]", // belt-and-braces if PID filter ever drops a frame
    "chromium input",         // WebKit/GStreamer media source label
    "pacat",                  // parec's binary
    "parec",
    "pw-cat",
    "pw-record",
    "speech-dispatcher",
    "aftercalls", // our own cpal mic consumer's binary basename
];

/// True when `s` matches any blacklist entry (case-insensitive
/// substring). Public so the detector + observer can apply it to
/// stable app keys consistently.
pub fn is_blacklisted(s: &str) -> bool {
    let lower = s.to_lowercase();
    MIC_CONSUMER_BLACKLIST
        .iter()
        .any(|b| lower.contains(&b.to_lowercase()))
}

/// Strip the PipeWire-ALSA boilerplate ("PipeWire ALSA [<binary>]")
/// and the framework-default labels that aren't actually informative.
/// Returns `None` if `friendly` is fine to surface as-is, or
/// `Some(replacement)` if the caller should swap to `bundle_id`.
fn looks_like_generic_framework_label(friendly: &str) -> bool {
    let trimmed = friendly.trim();
    if trimmed.starts_with("PipeWire ALSA [") && trimmed.ends_with(']') {
        return true;
    }
    // CoreAudio / WASAPI fallback strings will land here when those
    // platforms don't get a real session display name — punt with the
    // bundle_id basename instead.
    matches!(
        trimmed.to_lowercase().as_str(),
        "chromium input" | "system audio"
    )
}

/// One mic-consuming app the OS reports.
///
/// `bundle_id` is the stable key the auto-record store +
/// observer-diff logic both use; `friendly_name` is the display
/// string the UI renders. Distinct fields so a renamed Electron app
/// (e.g. `application.name` swapping from "Slack" → "Slack | DM") still
/// matches its previous `enabled` row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawMicConsumer {
    pub bundle_id: String,
    pub friendly_name: String,
}

impl RawMicConsumer {
    pub fn new(bundle_id: impl Into<String>, friendly_name: impl Into<String>) -> Self {
        Self {
            bundle_id: bundle_id.into(),
            friendly_name: friendly_name.into(),
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_secs(5);
static MIC_CONSUMER_TX: OnceLock<watch::Sender<Vec<RawMicConsumer>>> = OnceLock::new();

/// Shared 5s mic-consumer ticker. The detector prompt path and the
/// auto-record observer both subscribe here, so Linux runs one
/// `pactl list source-outputs` process per cycle while each consumer
/// keeps its own state machine.
pub fn subscribe_mic_consumers() -> watch::Receiver<Vec<RawMicConsumer>> {
    let tx = MIC_CONSUMER_TX.get_or_init(|| {
        let (tx, _rx) = watch::channel(raw_mic_consumers());
        let ticker_tx = tx.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                ticker_tx.send_replace(raw_mic_consumers());
            }
        });
        tx
    });
    tx.subscribe()
}

/// Helper-process basenames we never want to surface as a "mic-using
/// app". Crashpad handlers, Chromium renderers, GPU helper procs, etc.
/// appear in `pactl list source-outputs` when their parent app holds
/// the mic; the user's mental model is the parent app, not the helper.
///
/// Substring-matched case-insensitively against `bundle_id` (and the
/// raw application.name as a safety net), same posture as
/// detector::MIC_CONSUMER_BLACKLIST.
const HELPER_PROC_FILTERS: &[&str] = &[
    "_helper",
    "crashpad_",
    "-renderer",
    "_renderer",
    "gpu-process",
];

fn is_helper_proc(s: &str) -> bool {
    let lower = s.to_lowercase();
    HELPER_PROC_FILTERS.iter().any(|f| lower.contains(f))
}

/// Linux PipeWire / Pulse path. Reuses the same `pactl list
/// source-outputs` invocation `detector.rs` has used since #74; the
/// only difference is we surface both `bundle_id` (binary) and
/// `friendly_name` (application.name) on each entry.
///
/// Filters our own PID upstream so the agent's cpal stream is never
/// reported as a separate consumer.
#[cfg(target_os = "linux")]
pub fn raw_mic_consumers() -> Vec<RawMicConsumer> {
    let output = std::process::Command::new("pactl")
        .arg("list")
        .arg("source-outputs")
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let own_pid = std::process::id().to_string();
    parse_pactl_source_outputs(&text, &own_pid)
}

#[cfg(target_os = "linux")]
fn parse_pactl_source_outputs(text: &str, own_pid: &str) -> Vec<RawMicConsumer> {
    let mut result: Vec<RawMicConsumer> = Vec::new();
    let mut seen_bundles: HashSet<String> = HashSet::new();
    let mut cur_name: Option<String> = None;
    let mut cur_binary: Option<String> = None;
    let mut cur_pid: Option<String> = None;

    let flush = |result: &mut Vec<RawMicConsumer>,
                 seen: &mut HashSet<String>,
                 name: &mut Option<String>,
                 binary: &mut Option<String>,
                 pid: &mut Option<String>,
                 own_pid: &str| {
        let is_ours = pid.as_deref() == Some(own_pid);
        if !is_ours {
            // Prefer process.binary as the stable key. If absent (rare),
            // fall back to application.name. If both are missing, skip.
            let bundle = binary.clone().or_else(|| name.clone());
            if let Some(bundle) = bundle {
                let bundle_blacklisted = is_blacklisted(&bundle);
                if !is_helper_proc(&bundle)
                    && !name.as_deref().map(is_helper_proc).unwrap_or(false)
                    && !bundle_blacklisted
                    && seen.insert(bundle.clone())
                {
                    // Surface bundle_id as friendly_name when the OS
                    // gave us generic framework boilerplate
                    // ("PipeWire ALSA [...]", "Chromium input").
                    // Those strings aren't user-recognizable labels,
                    // but they can still accompany a real app binary
                    // such as "cliq", so they must not drop the row
                    // entirely (#610).
                    let raw_friendly = name.clone().filter(|s| !s.trim().is_empty());
                    let friendly = match raw_friendly {
                        Some(s) if !looks_like_generic_framework_label(&s) => s,
                        _ => bundle.clone(),
                    };
                    result.push(RawMicConsumer::new(bundle, friendly));
                }
            }
        }
        *name = None;
        *binary = None;
        *pid = None;
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Source Output #") {
            flush(
                &mut result,
                &mut seen_bundles,
                &mut cur_name,
                &mut cur_binary,
                &mut cur_pid,
                &own_pid,
            );
        } else if let Some(rest) = trimmed.strip_prefix("application.name = \"") {
            if let Some(end) = rest.find('"') {
                cur_name = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("application.process.binary = \"") {
            if let Some(end) = rest.find('"') {
                cur_binary = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("application.process.id = \"") {
            if let Some(end) = rest.find('"') {
                cur_pid = Some(rest[..end].to_string());
            }
        }
    }
    flush(
        &mut result,
        &mut seen_bundles,
        &mut cur_name,
        &mut cur_binary,
        &mut cur_pid,
        &own_pid,
    );
    result
}

/// Windows WASAPI path. Returns the exe basename as both bundle_id and
/// friendly_name; the `friendly_name` path can be improved later by
/// reading `IAudioSessionControl::GetDisplayName` but the basename is
/// stable and recognizable enough for v1.
///
/// Applies the same `is_helper_proc` + `is_blacklisted` filters as the
/// Linux path so the observer's catalog stays consistent across
/// platforms — see the cross-platform contract in this module's
/// docstring (item 1 + 2).
#[cfg(target_os = "windows")]
pub fn raw_mic_consumers() -> Vec<RawMicConsumer> {
    match windows_mic_consumers() {
        Ok(v) => v
            .into_iter()
            .filter(|name| !is_helper_proc(name) && !is_blacklisted(name))
            .map(|name| RawMicConsumer::new(name.clone(), name))
            .collect(),
        Err(e) => {
            eprintln!("aftercalls: WASAPI session enumeration failed: {e}");
            Vec::new()
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_mic_consumers() -> Result<Vec<String>, String> {
    use windows::core::{Interface, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE};
    use windows::Win32::Media::Audio::{
        eCapture, AudioSessionStateInactive, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[inline]
    unsafe fn process_exe_basename(pid: u32) -> Result<String, String> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess({pid}): {e}"))?;

        let mut buf: [u16; 1024] = [0; 1024];
        let mut size: u32 = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );

        let _ = CloseHandle(handle);

        result.map_err(|e| format!("QueryFullProcessImageNameW: {e}"))?;

        let slice = &buf[..size as usize];
        let full = OsString::from_wide(slice).to_string_lossy().into_owned();
        let base = full
            .rsplit(|c| c == '\\' || c == '/')
            .next()
            .unwrap_or(&full)
            .to_string();
        Ok(base)
    }

    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = hr.ok() {
            if e.code() != RPC_E_CHANGED_MODE {
                return Err(format!("CoInitializeEx: {e}"));
            }
        }

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;

        let devices = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;

        let device_count = devices
            .GetCount()
            .map_err(|e| format!("IMMDeviceCollection::GetCount: {e}"))?;

        let own_pid = std::process::id();
        let mut result: Vec<String> = Vec::new();
        let mut seen_pids: HashSet<u32> = HashSet::new();

        for i in 0..device_count {
            let device = match devices.Item(i) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("aftercalls: IMMDeviceCollection::Item({i}): {e}");
                    continue;
                }
            };

            let session_manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("aftercalls: IMMDevice::Activate IAudioSessionManager2: {e}");
                    continue;
                }
            };

            let session_enum = match session_manager.GetSessionEnumerator() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("aftercalls: GetSessionEnumerator: {e}");
                    continue;
                }
            };

            let session_count = match session_enum.GetCount() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("aftercalls: IAudioSessionEnumerator::GetCount: {e}");
                    continue;
                }
            };

            for s in 0..session_count {
                let control = match session_enum.GetSession(s) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let control2: IAudioSessionControl2 = match control.cast() {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let pid = match control2.GetProcessId() {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                if pid == 0 || pid == own_pid {
                    continue;
                }

                if let Ok(state) = control.GetState() {
                    if state == AudioSessionStateInactive {
                        continue;
                    }
                }

                if !seen_pids.insert(pid) {
                    continue;
                }

                match process_exe_basename(pid) {
                    Ok(name) if !name.is_empty() => result.push(name),
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("aftercalls: process_exe_basename({pid}): {e}");
                    }
                }
            }
        }

        Ok(result)
    }
}

/// macOS path. Uses CoreAudio HAL's macOS-14.2 process-list API
/// (`kAudioHardwarePropertyProcessObjectList` + the per-process
/// `kAudioProcessPropertyIsRunningInput` boolean) to enumerate which
/// PIDs are currently capturing the system microphone, then resolves
/// each PID to an executable basename via `libproc::proc_pidpath`.
///
/// On older macOS (13.0–14.1) the property doesn't exist; the kernel-
/// version probe reported by `macos_kernel_supports_process_list` is
/// false on those hosts, so this function returns an empty Vec and the
/// observer's `is_supported()` reports false. The Settings UI then
/// shows the existing "App detection isn't supported on this OS yet"
/// banner unchanged.
///
/// Polled at 5s by the shared `subscribe_mic_consumers` ticker — same
/// cadence as Linux/Windows. We deliberately do NOT register a
/// CoreAudio property listener for change notifications; that would
/// require driving an NSRunLoop on a dedicated thread to receive the
/// callbacks, and the cross-platform contract here is "snapshot
/// every 5s".
///
/// Applies the same `is_helper_proc` + `is_blacklisted` filters as the
/// Linux/Windows paths (cross-platform contract items 1–3).
#[cfg(target_os = "macos")]
pub fn raw_mic_consumers() -> Vec<RawMicConsumer> {
    match macos_mic_consumers() {
        Ok(v) => v
            .into_iter()
            .filter(|name| !is_helper_proc(name) && !is_blacklisted(name))
            .map(|name| RawMicConsumer::new(name.clone(), name))
            .collect(),
        Err(e) => {
            eprintln!("aftercalls: CoreAudio mic enumeration failed: {e}");
            Vec::new()
        }
    }
}

/// Run-once probe: does the host kernel expose the macOS-14.2
/// process-list API? Reads `kern.osproductversion` via `sysctlbyname`
/// and compares against `14.2`. The result is cached in a `OnceLock`
/// because the host OS doesn't change at runtime; the syscall cost
/// only matters once.
///
/// Public so `audio_observer::is_supported()` can gate on it.
#[cfg(target_os = "macos")]
pub fn macos_kernel_supports_process_list() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        match macos_product_version() {
            Some((major, minor)) => major > 14 || (major == 14 && minor >= 2),
            None => {
                // Couldn't read the version — be conservative and
                // report unsupported rather than risk calling a
                // property that might trap on older kernels.
                eprintln!(
                    "aftercalls: could not read kern.osproductversion; assuming pre-14.2"
                );
                false
            }
        }
    })
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
pub fn macos_kernel_supports_process_list() -> bool {
    false
}

/// Read `kern.osproductversion` via `sysctlbyname` and parse the leading
/// `<major>.<minor>` pair. Returns `None` if the syscall fails or the
/// string is unexpectedly shaped (treat as unsupported upstream).
#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<(u32, u32)> {
    use std::ffi::CString;

    let name = CString::new("kern.osproductversion").ok()?;
    let mut size: libc::size_t = 0;
    // First call: discover required buffer size.
    // Safety: name is a valid C string; passing nullptr for `oldp` with
    // a size pointer is the documented sysctlbyname size-probe form.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buf: Vec<u8> = vec![0; size];
    // Safety: buffer is sized per the prior probe; sysctlbyname will
    // write up to `size` bytes including a trailing NUL.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    // Trim trailing NUL(s).
    while buf.last() == Some(&0) {
        buf.pop();
    }
    let s = std::str::from_utf8(&buf).ok()?;
    parse_product_version(s)
}

/// Parse the leading `<major>.<minor>` pair out of a macOS product
/// version string. Examples: `"14.2"` → `(14, 2)`, `"14.2.1"` →
/// `(14, 2)`, `"15.0"` → `(15, 0)`. Garbage in returns `None`.
///
/// Lives outside the `cfg(target_os = "macos")` block so the unit
/// test below exercises it on every host (the parser logic itself is
/// portable; only the `sysctlbyname` caller is Mac-specific).
#[allow(dead_code)]
fn parse_product_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    Some((major, minor))
}

#[cfg(target_os = "macos")]
fn macos_mic_consumers() -> Result<Vec<String>, String> {
    use coreaudio::sys::{
        kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyIsRunningInput,
        kAudioProcessPropertyPID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
        AudioObjectID, AudioObjectPropertyAddress,
    };

    // Belt-and-braces: even though `is_supported()` guards the spawn
    // path, anyone calling `raw_mic_consumers()` directly on a
    // pre-14.2 host should get an empty list rather than a trap on a
    // missing property.
    if !macos_kernel_supports_process_list() {
        return Ok(Vec::new());
    }

    let own_pid = std::process::id() as i32;

    // Step 1: enumerate all AudioObjectIDs that have ever opened an
    // audio object. The kernel returns a flat array of `AudioObjectID`s
    // (= `UInt32`).
    let process_list_addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyProcessObjectList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut data_size: u32 = 0;
    // Safety: kAudioObjectSystemObject is the documented well-known ID;
    // passing &address is the standard form. Out-param `data_size` is
    // u32-aligned.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &process_list_addr,
            0,
            std::ptr::null(),
            &mut data_size,
        )
    };
    if status != 0 {
        return Err(format!(
            "AudioObjectGetPropertyDataSize(ProcessObjectList) status={status}"
        ));
    }
    if data_size == 0 {
        return Ok(Vec::new());
    }

    let count = data_size as usize / std::mem::size_of::<AudioObjectID>();
    let mut process_ids: Vec<AudioObjectID> = vec![0; count];
    let mut io_size = data_size;
    // Safety: process_ids is sized per the probe; AudioObjectID = UInt32
    // matches the kernel's return shape.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &process_list_addr,
            0,
            std::ptr::null(),
            &mut io_size,
            process_ids.as_mut_ptr() as *mut std::ffi::c_void,
        )
    };
    if status != 0 {
        return Err(format!(
            "AudioObjectGetPropertyData(ProcessObjectList) status={status}"
        ));
    }

    let mut seen_pids: HashSet<i32> = HashSet::new();
    let mut result: Vec<String> = Vec::new();

    for &obj_id in &process_ids {
        // Step 2a: is this process currently capturing input? Skip
        // first so we don't pay the PID-resolve cost for idle entries.
        let is_input_addr = AudioObjectPropertyAddress {
            mSelector: kAudioProcessPropertyIsRunningInput,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut is_running: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                obj_id,
                &is_input_addr,
                0,
                std::ptr::null(),
                &mut size,
                &mut is_running as *mut u32 as *mut std::ffi::c_void,
            )
        };
        if status != 0 {
            // Per-process query failures are non-fatal — process may
            // have just exited. Skip it.
            continue;
        }
        if is_running == 0 {
            continue;
        }

        // Step 2b: get the PID for this AudioObject.
        let pid_addr = AudioObjectPropertyAddress {
            mSelector: kAudioProcessPropertyPID,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut pid: i32 = 0;
        let mut size: u32 = std::mem::size_of::<i32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                obj_id,
                &pid_addr,
                0,
                std::ptr::null(),
                &mut size,
                &mut pid as *mut i32 as *mut std::ffi::c_void,
            )
        };
        if status != 0 || pid <= 0 {
            continue;
        }
        if pid == own_pid {
            continue;
        }
        if !seen_pids.insert(pid) {
            continue;
        }

        // Step 3: PID → executable basename via libproc.
        match proc_pidpath_basename(pid) {
            Ok(name) if !name.is_empty() => result.push(name),
            Ok(_) => {}
            Err(e) => {
                eprintln!("aftercalls: proc_pidpath({pid}): {e}");
            }
        }
    }

    Ok(result)
}

#[cfg(target_os = "macos")]
fn proc_pidpath_basename(pid: i32) -> Result<String, String> {
    let mut buf: [u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize] =
        [0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // Safety: buf is sized to PROC_PIDPATHINFO_MAXSIZE per Apple's
    // libproc documentation; proc_pidpath writes at most that many
    // bytes including trailing NUL.
    let rc = unsafe {
        libc::proc_pidpath(
            pid,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if rc <= 0 {
        return Err(format!("proc_pidpath returned {rc} for pid {pid}"));
    }
    let len = rc as usize;
    let full = std::str::from_utf8(&buf[..len])
        .map_err(|e| format!("proc_pidpath utf8 for pid {pid}: {e}"))?
        .to_string();
    let base = full
        .rsplit('/')
        .next()
        .unwrap_or(&full)
        .to_string();
    Ok(base)
}

/// Other targets ship with no working observer; the auto-record
/// settings UI shows the "App detection isn't supported on this OS
/// yet" banner and the prompt-detector simply never fires.
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn raw_mic_consumers() -> Vec<RawMicConsumer> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_proc_filter_catches_chromium_renderers() {
        // Linux pactl reports `application.process.binary`, which is
        // the executable basename (underscored/hyphenated). Filter the
        // shapes we actually see — not the human-readable
        // `application.name` strings (those rarely match the helper
        // pattern and aren't what we key on anyway).
        assert!(is_helper_proc("chrome_crashpad_handler"));
        assert!(is_helper_proc("electron-renderer"));
        assert!(is_helper_proc("teams_helper"));
        assert!(is_helper_proc("gpu-process"));
        assert!(is_helper_proc("Slack_Helper"));
        assert!(!is_helper_proc("zoom"));
        assert!(!is_helper_proc("firefox"));
        assert!(!is_helper_proc("teams-for-linux"));
    }

    #[test]
    fn blacklist_catches_our_own_helpers_and_generic_framework_labels() {
        // The agent's own binary + the helper procs we spawn for
        // capture must be filtered at source so they never reach the
        // observer's catalog (#604).
        assert!(is_blacklisted("aftercalls"));
        assert!(is_blacklisted("parec"));
        assert!(is_blacklisted("pacat"));
        assert!(is_blacklisted("pw-cat"));
        assert!(is_blacklisted("pw-record"));
        assert!(is_blacklisted("speech-dispatcher"));
        // Generic framework labels — surfacing these would leak our
        // infra ("PipeWire") into user copy.
        assert!(is_blacklisted("Chromium input"));
        assert!(is_blacklisted("PipeWire ALSA [aftercalls]"));
        assert!(is_blacklisted("PipeWire ALSA [client]"));
        // Real apps must NOT match the blacklist.
        assert!(!is_blacklisted("zoom"));
        assert!(!is_blacklisted("cliq"));
        assert!(!is_blacklisted("slack"));
        assert!(!is_blacklisted("Microsoft Teams"));
    }

    #[test]
    fn generic_framework_label_detection_strips_pipewire_alsa_pattern() {
        // PipeWire labels raw ALSA streams with this boilerplate when
        // the app didn't set `application.name` — surfacing it as the
        // friendly_name reads as a vendor-name leak and doesn't match
        // the detector's bundle_id rendering.
        assert!(looks_like_generic_framework_label(
            "PipeWire ALSA [voxtype-avx2]"
        ));
        assert!(looks_like_generic_framework_label(
            "PipeWire ALSA [aftercalls]"
        ));
        assert!(looks_like_generic_framework_label("Chromium input"));
        // App-supplied names must NOT be considered generic.
        assert!(!looks_like_generic_framework_label("Slack Call"));
        assert!(!looks_like_generic_framework_label("Zoho Cliq"));
        assert!(!looks_like_generic_framework_label("Zoom Meeting"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parser_keeps_real_app_with_generic_chromium_label() {
        let text = r#"
Source Output #42
    Properties:
        application.name = "Chromium input"
        application.process.binary = "cliq"
        application.process.id = "99999"
"#;

        let consumers = parse_pactl_source_outputs(text, "12345");
        assert_eq!(consumers.len(), 1);
        assert_eq!(consumers[0].bundle_id, "cliq");
        assert_eq!(consumers[0].friendly_name, "cliq");
    }

    #[test]
    fn product_version_parser_extracts_major_minor() {
        // Apple ships `kern.osproductversion` as "14.2", "14.2.1",
        // "15.0", etc. The observer only cares about the major+minor
        // pair (the process-list API's floor is 14.2).
        assert_eq!(parse_product_version("14.2"), Some((14, 2)));
        assert_eq!(parse_product_version("14.2.1"), Some((14, 2)));
        assert_eq!(parse_product_version("15.0"), Some((15, 0)));
        assert_eq!(parse_product_version("13.7.2"), Some((13, 7)));
        // Trailing whitespace / newlines from the syscall buffer must
        // not break parsing.
        assert_eq!(parse_product_version("14.2\n"), Some((14, 2)));
        // A bare major-only string is treated as <major>.0 — defensive
        // shape we shouldn't see in practice.
        assert_eq!(parse_product_version("14"), Some((14, 0)));
        // Garbage in returns None — caller treats as unsupported.
        assert!(parse_product_version("").is_none());
        assert!(parse_product_version("not-a-version").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parser_still_drops_generic_label_without_real_binary() {
        let text = r#"
Source Output #43
    Properties:
        application.name = "Chromium input"
        application.process.id = "99999"
"#;

        let consumers = parse_pactl_source_outputs(text, "12345");
        assert!(consumers.is_empty());
    }
}
