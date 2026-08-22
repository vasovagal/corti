#![cfg(all(target_os = "macos", feature = "offline-tracing"))]

use std::fs;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use vasovagal_tracing::{TailPolicy, validate_jsonl};

fn unique_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/offline-tracing-tests")
        .join(format!(
            "corti-offline-tracing-{}-{nonce}",
            std::process::id()
        ))
}

fn run(base: &Path, state_name: &str, enabled: bool, args: &[&str]) -> Output {
    let state = base.join(state_name);
    Command::new(env!("CARGO_BIN_EXE_corti"))
        .args(args)
        .env("VASOVAGAL_TRACE", if enabled { "true" } else { "false" })
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("HOME", base.join("home"))
        .env("RUST_LOG", "off")
        .output()
        .unwrap()
}

fn validated_bytes(path: &Path) -> Vec<u8> {
    let report = validate_jsonl(
        BufReader::new(fs::File::open(path).unwrap()),
        TailPolicy::Strict,
    );
    assert!(report.is_valid(), "schema failures: {:?}", report.errors);
    fs::read(path).unwrap()
}

#[test]
fn headless_trace_is_schema_valid_and_observationally_identical() {
    let base = unique_dir();
    fs::create_dir_all(&base).unwrap();

    let disabled = run(&base, "disabled-state", false, &["--version"]);
    let enabled = run(&base, "enabled-state", true, &["--version"]);
    assert_eq!(enabled.status.code(), disabled.status.code());
    assert_eq!(enabled.stdout, disabled.stdout);
    assert_eq!(enabled.stderr, disabled.stderr);
    assert!(enabled.status.success());
    assert!(!base.join("disabled-state").exists());

    let trace_dir = base.join("enabled-state").join("vasovagal/traces/corti");
    let files = fs::read_dir(&trace_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1, "expected one headless trace file");
    assert_eq!(
        fs::metadata(&trace_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let file = fs::File::open(&files[0]).unwrap();
    let report = validate_jsonl(BufReader::new(file), TailPolicy::Strict);
    assert!(report.is_valid(), "schema failures: {:?}", report.errors);

    let bytes = fs::read(&files[0]).unwrap();
    let records = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(records.iter().any(|record| {
        record["record_type"] == "span_end"
            && record["operation"] == "corti.cli"
            && record["attributes"]["command"] == "other"
            && record["outcome"] == "ok"
    }));
    assert!(
        records
            .iter()
            .any(|record| record["record_type"] == "session_summary")
    );
    assert!(
        !String::from_utf8_lossy(&bytes).contains(&base.to_string_lossy().to_string()),
        "trace must not contain the temporary state path"
    );

    fs::remove_dir_all(base).unwrap();
}

#[test]
fn headless_error_closes_the_root_and_summary_before_exit() {
    let base = unique_dir();
    fs::create_dir_all(&base).unwrap();
    let missing = base.join("private-missing-input.wav");
    let output = run(
        &base,
        "error-state",
        true,
        &["--input", missing.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(1));

    let trace_dir = base.join("error-state/vasovagal/traces/corti");
    let file = fs::read_dir(trace_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .unwrap();
    let bytes = validated_bytes(&file);
    let records = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(records.iter().any(|record| {
        record["record_type"] == "span_end"
            && record["operation"] == "corti.cli"
            && record["outcome"] == "error"
            && record["error_code"] == "other"
    }));
    assert!(
        records
            .iter()
            .any(|record| record["record_type"] == "session_summary")
    );
    assert!(
        !String::from_utf8_lossy(&bytes).contains("private-missing-input"),
        "trace must not contain a CLI path"
    );

    fs::remove_dir_all(base).unwrap();
}

#[cfg(feature = "local")]
#[test]
fn catalogued_pipeline_and_backend_spans_project_without_rejections() {
    let base = unique_dir();
    fs::create_dir_all(&base).unwrap();
    let input = base.join("private-audio-name.wav");
    fs::write(&input, []).unwrap();
    let state = base.join("pipeline-state");
    let output = Command::new(env!("CARGO_BIN_EXE_corti"))
        .args(["--input", input.to_str().unwrap(), "--local", "--no-aec"])
        .env("VASOVAGAL_TRACE", "true")
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("HOME", base.join("home"))
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));

    let trace_dir = state.join("vasovagal/traces/corti");
    let file = fs::read_dir(trace_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .unwrap();
    let bytes = validated_bytes(&file);
    let records = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    for operation in [
        "corti.pipeline.recording",
        "corti.transcription",
        "corti.transcription.backend",
    ] {
        assert!(
            records
                .iter()
                .any(|record| record["operation"] == operation),
            "missing {operation}"
        );
    }
    let summary = records
        .iter()
        .find(|record| record["record_type"] == "session_summary")
        .unwrap();
    for counter in [
        "rejected_operations",
        "rejected_fields",
        "rejected_types",
        "privacy_violations",
    ] {
        assert_eq!(summary["counters"][counter], 0, "counter {counter}");
    }
    assert!(
        !String::from_utf8_lossy(&bytes).contains("private-audio-name"),
        "trace must not contain the input path"
    );

    fs::remove_dir_all(base).unwrap();
}
