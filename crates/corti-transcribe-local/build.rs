//! Extra Apple link flags for a CoreML-enabled sherpa-onnx static lib.
//!
//! `sherpa-onnx-sys` links the static archives plus `framework=Foundation`, but **not** `framework=CoreML`.
//! A CoreML-enabled `libonnxruntime.a` / `libsherpa-onnx-core.a` references the CoreML Objective-C
//! framework, so the link fails with undefined symbols unless we add it. We do that here — but only under
//! the `coreml-lib` feature, which is the single signal that this build links such a lib (supplied via
//! `SHERPA_ONNX_LIB_DIR` / `SHERPA_ONNX_ARCHIVE_DIR`). With the feature off (the default), this emits
//! nothing and the CPU-only build is unaffected. See design/adr/0003-local-asr-sherpa-onnx.md.
fn main() {
    // Nothing to do for the default (CPU-only) build — keep it byte-for-byte identical.
    if std::env::var_os("CARGO_FEATURE_COREML_LIB").is_none() {
        return;
    }

    // Re-link if the CoreML lib is swapped in/out via the sherpa-onnx-sys override hooks.
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_ARCHIVE_DIR");

    // The CoreML execution provider's Objective-C code needs CoreML.framework at link time. The framework
    // ships on every macOS and an unused `-framework` is a harmless no-op, so emitting it is safe even if a
    // particular lib doesn't reference it. (If ld64 ordering ever drops it, switch to a raw
    // `cargo:rustc-link-arg=-framework` / `=CoreML`, which is appended later on the link line.)
    println!("cargo:rustc-link-lib=framework=CoreML");

    // The feature only does something when paired with a CoreML-enabled lib. The crates.io prebuilt the sys
    // crate downloads by default has no CoreML EP, so warn loudly if no override hook is set.
    if std::env::var_os("SHERPA_ONNX_LIB_DIR").is_none()
        && std::env::var_os("SHERPA_ONNX_ARCHIVE_DIR").is_none()
    {
        println!(
            "cargo:warning=corti-transcribe-local: the `coreml-lib` feature is on but neither \
             SHERPA_ONNX_LIB_DIR nor SHERPA_ONNX_ARCHIVE_DIR is set. The default sherpa-onnx prebuilt has \
             no CoreML execution provider, so `coreml` will still run on CPU. Point one of these at a \
             CoreML-enabled sherpa-onnx static lib (see scripts/build-sherpa-coreml.sh)."
        );
    }
}
