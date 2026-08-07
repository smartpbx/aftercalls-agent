// macOS system-audio loopback bridge — talks to the Swift class in
// `macos/AftercallsLoopback.swift` via the `swift-bridge` crate. The
// public Rust surface is `MacLoopback`, owned by the recorder's
// `SystemCapture::Mac(_)` variant.
//
// Lifecycle:
//   - `MacLoopback::new(path)` constructs the Swift object and starts
//     the SCStream synchronously. A failure to start surfaces as
//     `anyhow::Error` so the recorder falls back to mic-only (matching
//     Linux/Windows error paths).
//   - `stop()` shuts the SCStream down and patches the WAV header in
//     place. Idempotent on the Swift side, so the explicit `Drop`
//     impl below is purely defensive.
//
// The whole module is compiled only on macOS — every dependent file
// (build.rs, recorder.rs SystemCapture::Mac variant) gates the same
// way.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

// `pub(crate)` so the capture-permission pre-flight (`permissions.rs`)
// can call the free TCC-status functions below without a second
// bridge. The `AftercallsLoopback` type stays internal-by-convention
// — only this module constructs it.
#[swift_bridge::bridge]
pub(crate) mod ffi {
    extern "Swift" {
        // Mirror of `AftercallsLoopback` in
        // `macos/AftercallsLoopback.swift`. The Swift class is
        // `@available(macOS 13.0, *)` — earlier macOS hits the
        // start() bool=false path and we surface a Rust error.
        type AftercallsLoopback;

        #[swift_bridge(init)]
        fn new(output_path: String) -> AftercallsLoopback;

        // Both methods wrap the Swift async API behind a dispatch
        // semaphore so the FFI surface stays sync. Returns `true` on
        // success; on failure, `last_error()` carries the message.
        fn start(&mut self) -> bool;
        fn stop(&mut self) -> bool;
        fn lastError(&self) -> String;

        // ── TCC permission pre-flight (#623) ─────────────────────
        // Free functions (no `self`) mirroring the same-named free
        // funcs in `AftercallsLoopback.swift`. `permissions.rs` maps
        // the raw values to the serialisable `PermStatus` enum.
        //
        // `micAuthStatus` returns the raw AVAuthorizationStatus
        // (.notDetermined=0, .restricted=1, .denied=2, .authorized=3).
        // `screenCaptureAuthStatus` returns the CGPreflight bool.
        // `requestScreenCaptureAccess` calls CGRequestScreenCaptureAccess
        // (prompts then returns the grant). `requestMicAccess` fires
        // the AVFoundation request prompt; the caller re-reads
        // `micAuthStatus` afterwards for the authoritative value.
        fn micAuthStatus() -> i32;
        fn screenCaptureAuthStatus() -> bool;
        fn requestScreenCaptureAccess() -> bool;
        fn requestMicAccess();
    }
}

pub(crate) struct MacLoopback {
    inner: ffi::AftercallsLoopback,
    stopped: bool,
}

impl MacLoopback {
    pub(crate) fn new(output_path: &Path) -> Result<Self> {
        let path_str = output_path
            .to_str()
            .ok_or_else(|| anyhow!("non-utf8 output path: {:?}", output_path))?
            .to_string();
        let mut inner = ffi::AftercallsLoopback::new(path_str);
        if !inner.start() {
            let msg = inner.lastError();
            return Err(anyhow!("ScreenCaptureKit start failed: {msg}"));
        }
        Ok(Self {
            inner,
            stopped: false,
        })
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        let ok = self.inner.stop();
        self.stopped = true;
        if !ok {
            let msg = self.inner.lastError();
            return Err(anyhow!("ScreenCaptureKit stop failed: {msg}"))
                .context("stop ScreenCaptureKit loopback");
        }
        Ok(())
    }
}

impl Drop for MacLoopback {
    fn drop(&mut self) {
        // Defensive: if the recorder drops us without calling
        // `stop()` explicitly (panic during finish, early return,
        // etc.), still tear down the SCStream and flush the WAV
        // header. The Swift side is idempotent on stop.
        if !self.stopped {
            let _ = self.inner.stop();
            self.stopped = true;
        }
    }
}
