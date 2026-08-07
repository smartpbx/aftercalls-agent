fn main() {
    // An explicit application manifest turns custom commands into ACL-managed
    // resources. `permissions/app.toml` then assigns separate command sets to
    // the main, co-pilot and region-selection windows.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new()),
    )
    .expect("build Tauri application manifest");

    #[cfg(target_os = "macos")]
    macos::build_loopback();
}

// macOS-only build glue: scan the Rust ↔ Swift bridge module via
// `swift-bridge-build`, compile the Swift implementation against
// ScreenCaptureKit + AVFoundation, and link the resulting static
// archive into the agent binary. All of this is compile-time-isolated
// behind `cfg(target_os = "macos")` so non-Mac targets see a stock
// `tauri_build::build()` and nothing else.
//
// VALIDATED 2026-08-07 on `macos-latest` in CI (#681). This was
// previously flagged as unverified guesswork carried over from the
// `swift-bridge` examples, with a TODO(#621) to check it on real
// hardware. It needed no changes: the bridge scan, the swiftc
// invocation, the `-target arm64-apple-macosx13.0` flag, the
// generated-header include path, and the framework links all worked
// untouched on the first Mac build.
//
// The `macos-compile-check` job in `.github/workflows/build-agent.yml`
// now runs this on every PR, so a regression here fails CI rather than
// reaching a Mac user. Note that job stubs the ffmpeg sidecar and does
// not bundle — it proves this script compiles and links, not that
// packaging or a signed installer works.
#[cfg(target_os = "macos")]
mod macos {
    use std::env;
    use std::path::PathBuf;
    use std::process::Command;

    pub fn build_loopback() {
        let bridges = vec!["src/macos_loopback.rs"];
        for path in &bridges {
            println!("cargo:rerun-if-changed={path}");
        }
        println!("cargo:rerun-if-changed=macos/AftercallsLoopback.swift");

        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
        let bridge_dir = out_dir.join("swift-bridge");
        std::fs::create_dir_all(&bridge_dir).expect("create swift-bridge out dir");

        // Generate the Rust glue + Swift `.swift` header file from
        // the bridge module. `write_all_concatenated` produces:
        //   <out>/SwiftBridgeCore.swift
        //   <out>/SwiftBridgeCore.h
        //   <out>/agent/agent.swift
        //   <out>/agent/agent.h
        swift_bridge_build::parse_bridges(bridges)
            .write_all_concatenated(&bridge_dir, "agent");

        // Compile the Swift sources into a static archive that we
        // link below. `swiftc` builds a `.a` we hand to rustc via
        // `-l static=...` + `-L`.
        //
        // Architecture: Apple Silicon primary (plan §2.4). The build
        // host is the developer's Mac; we let `swiftc` default to
        // the host arch unless a non-default `CARGO_CFG_TARGET_ARCH`
        // is set (which would only happen in cross-compile, and we
        // don't support that yet).
        let lib_name = "aftercalls_macos";
        let lib_path = out_dir.join(format!("lib{lib_name}.a"));

        let bridge_glue_swift = bridge_dir.join("SwiftBridgeCore.swift");
        let agent_glue_swift = bridge_dir.join("agent").join("agent.swift");

        // Cargo's TARGET (e.g. `aarch64-apple-darwin`) is not the same
        // shape Swift accepts (`arm64-apple-macosX.Y`). Convert.
        let cargo_target = env::var("TARGET").unwrap_or_default();
        let swift_arch = if cargo_target.starts_with("aarch64-") {
            "arm64"
        } else {
            "x86_64"
        };
        let macos_version =
            env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "13.0".into());
        let target_triple = format!("{swift_arch}-apple-macosx{macos_version}");

        let swift_status = Command::new("swiftc")
            .args([
                "-emit-library",
                "-static",
                "-parse-as-library",
                "-O",
                "-target",
            ])
            .arg(&target_triple)
            .args([
                "-module-name",
                lib_name,
                "-import-objc-header",
            ])
            .arg(bridge_dir.join("SwiftBridgeCore.h"))
            .arg("macos/AftercallsLoopback.swift")
            .arg(&bridge_glue_swift)
            .arg(&agent_glue_swift)
            .arg("-o")
            .arg(&lib_path)
            .args([
                "-framework",
                "ScreenCaptureKit",
                "-framework",
                "AVFoundation",
                "-framework",
                "CoreMedia",
                "-framework",
                "Foundation",
            ])
            .status()
            .expect("invoke swiftc");

        if !swift_status.success() {
            panic!("swiftc failed (status {swift_status:?})");
        }

        println!("cargo:rustc-link-search=native={}", out_dir.display());
        println!("cargo:rustc-link-lib=static={lib_name}");
        println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
        println!("cargo:rustc-link-lib=framework=AVFoundation");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=Foundation");

        // Swift runtime libs ship with macOS 10.14.4+; pulling them
        // in explicitly is required when linking a Swift static lib
        // from clang/rustc. The exact rpath depends on Xcode
        // version — `xcrun --show-sdk-path` is the more durable
        // lookup once we add a real swiftc-detection helper.
        if let Ok(out) = Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
        {
            if out.status.success() {
                let sdk = String::from_utf8_lossy(&out.stdout).trim().to_string();
                println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
            }
        }
    }
}
