//! Embed `Info.plist` into the binary's `__TEXT,__info_plist` section on macOS.
//!
//! The Core Audio process-tap API requires `NSAudioCaptureUsageDescription`, and TCC only shows a
//! permission prompt (and delivers audio) if the binary carries an Info.plist identity. A bare `cargo run`
//! binary without this is silently denied — the IO proc never fires. The arm64 linker applies an ad-hoc
//! signature that covers the embedded section; see SPIKE.md.

// rustc's unexpected_cfgs lint heuristically flags the `CARGO_CFG_TARGET_OS` env-var read below as if it
// were a `cfg(target_os)` invocation; it isn't, so silence the false positive.
#![allow(unexpected_cfgs)]

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let plist = format!("{manifest}/Info.plist");
        println!("cargo:rerun-if-changed=Info.plist");
        // `-sectcreate __TEXT __info_plist <path>` is a 4-token ld directive. Pass each token through its
        // own `-Xlinker` so clang forwards them verbatim — the comma-packed `-Wl,...` form gets mis-split
        // by ld (`file not found: __TEXT`).
        for tok in ["-sectcreate", "__TEXT", "__info_plist", &plist] {
            println!("cargo:rustc-link-arg-bins=-Xlinker");
            println!("cargo:rustc-link-arg-bins={tok}");
        }
    }
}
