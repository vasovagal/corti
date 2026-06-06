//! App configuration, resolved from the environment at startup (v1 keeps it env-only — no config file yet).

use std::path::PathBuf;

/// Runtime configuration. Cloned into the pipeline worker; cheap and `Send`.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// S3 bucket for the AWS backend (`CORTI_AWS_BUCKET`). The backend errors clearly if this is unset.
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    pub aws_bucket: Option<String>,
    /// BCP-47 language passed to the transcription backend (`CORTI_LANGUAGE`, default `en-US`).
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    pub language: String,
    /// Local whisper model path for the `whisper` backend (`CORTI_WHISPER_MODEL`).
    #[cfg_attr(not(feature = "whisper"), allow(dead_code))]
    pub whisper_model: Option<PathBuf>,
    /// Whether to run offline echo cancellation on speaker recordings before transcription
    /// (`CORTI_AEC`, default on; set `0`/`false`/`off`/`no` to disable).
    pub aec_enabled: bool,
}

impl AppConfig {
    pub fn load() -> Self {
        Self {
            aws_bucket: env_non_empty("CORTI_AWS_BUCKET"),
            language: env_non_empty("CORTI_LANGUAGE").unwrap_or_else(|| "en-US".to_string()),
            whisper_model: env_non_empty("CORTI_WHISPER_MODEL").map(PathBuf::from),
            aec_enabled: env_bool("CORTI_AEC", true),
        }
    }

    /// Human-readable backend name for the tray's Settings section. AWS wins when both features are on
    /// (it is the default + the only complete backend), matching the transcribe dispatch.
    pub fn backend_name(&self) -> &'static str {
        if cfg!(feature = "aws") {
            "AWS Transcribe"
        } else if cfg!(feature = "whisper") {
            "Whisper (local)"
        } else {
            "none"
        }
    }
}

/// Read an env var, trimming whitespace and treating empty as unset.
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read a boolean env var. Unset/empty ⇒ `default`; otherwise anything but `0`/`false`/`off`/`no`
/// (case-insensitive) is treated as `true`.
fn env_bool(key: &str, default: bool) -> bool {
    match env_non_empty(key) {
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => default,
    }
}
