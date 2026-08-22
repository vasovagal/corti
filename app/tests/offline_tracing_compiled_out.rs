#![cfg(all(target_os = "macos", not(feature = "offline-tracing")))]

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn compiled_out_binary_ignores_activation_and_creates_no_trace_path() {
    let base = std::env::temp_dir().join(format!(
        "corti-tracing-compiled-out-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config = base.join("config/vasovagal");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("corti.yaml"),
        "version: 1\ntracing:\n  enabled: true\n",
    )
    .unwrap();
    let state: PathBuf = base.join("state-must-not-exist");

    let output = Command::new(env!("CARGO_BIN_EXE_corti"))
        .arg("--version")
        .env("VASOVAGAL_TRACE", "true")
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("HOME", base.join("home"))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("corti {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(
        !state.exists(),
        "a compiled-out build must not resolve or create tracing storage"
    );
    std::fs::remove_dir_all(base).unwrap();
}
