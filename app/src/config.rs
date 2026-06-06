//! App configuration, resolved from the environment at startup (v1 keeps it env-only — no config file yet;
//! a settings screen + persisted config is the planned consumer of the runtime backend seam below).

use std::path::PathBuf;

/// Which transcription backend to run. Both can be compiled in (`aws` + `local` features); this picks the
/// active one at **runtime**. The future settings screen writes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    Aws,
    Local,
}

/// Runtime configuration. Cloned into the pipeline worker; cheap and `Send`.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Which backend to use at runtime (`CORTI_TRANSCRIBE_BACKEND` = `aws` | `local`).
    pub transcribe_backend: BackendChoice,
    /// S3 bucket for the AWS backend (`CORTI_AWS_BUCKET`). The backend errors clearly if this is unset.
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    pub aws_bucket: Option<String>,
    /// BCP-47 language passed to the AWS backend (`CORTI_LANGUAGE`, default `en-US`).
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    pub language: String,
    /// Model directory for the local backend (`CORTI_LOCAL_MODEL_DIR`); `None` ⇒ default cache
    /// (`~/Library/Caches/corti/models/`), resolved inside the backend.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_model_dir: Option<PathBuf>,
    /// ONNX Runtime provider for the local backend (`CORTI_LOCAL_PROVIDER`, default `cpu`; `coreml` opt-in).
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_provider: String,
    /// ONNX intra-op thread count for the local backend (`CORTI_LOCAL_THREADS`, default 4).
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_threads: i32,
    /// Whether to run offline echo cancellation on speaker recordings before transcription
    /// (`CORTI_AEC`, default on; set `0`/`false`/`off`/`no` to disable).
    pub aec_enabled: bool,
}

impl AppConfig {
    pub fn load() -> Self {
        Self {
            transcribe_backend: parse_backend(env_non_empty("CORTI_TRANSCRIBE_BACKEND")),
            aws_bucket: env_non_empty("CORTI_AWS_BUCKET"),
            language: env_non_empty("CORTI_LANGUAGE").unwrap_or_else(|| "en-US".to_string()),
            local_model_dir: env_non_empty("CORTI_LOCAL_MODEL_DIR").map(PathBuf::from),
            local_provider: env_non_empty("CORTI_LOCAL_PROVIDER")
                .unwrap_or_else(|| "cpu".to_string()),
            local_threads: env_non_empty("CORTI_LOCAL_THREADS")
                .and_then(|s| s.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(4),
            aec_enabled: env_bool("CORTI_AEC", true),
        }
    }

    /// Human-readable name of the **runtime-selected** backend for the tray's Settings section. Returns
    /// `"none"` if the chosen backend isn't compiled into this build.
    pub fn backend_name(&self) -> &'static str {
        match self.transcribe_backend {
            BackendChoice::Aws if cfg!(feature = "aws") => "AWS Transcribe",
            BackendChoice::Local if cfg!(feature = "local") => "Parakeet (local)",
            _ => "none",
        }
    }
}

/// Parse `CORTI_TRANSCRIBE_BACKEND`; unset/unknown falls back to the build's default backend.
fn parse_backend(s: Option<String>) -> BackendChoice {
    match s.as_deref() {
        Some("aws") => BackendChoice::Aws,
        Some("local") => BackendChoice::Local,
        Some(other) => {
            eprintln!("[corti] unknown CORTI_TRANSCRIBE_BACKEND={other:?}; using build default");
            default_backend()
        }
        None => default_backend(),
    }
}

/// Default backend for this build: prefer `aws` (the historical default), else `local`.
fn default_backend() -> BackendChoice {
    if cfg!(feature = "aws") {
        BackendChoice::Aws
    } else if cfg!(feature = "local") {
        BackendChoice::Local
    } else {
        BackendChoice::Aws
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
