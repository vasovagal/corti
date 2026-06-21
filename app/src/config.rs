//! App configuration: **defaults → persisted `config.toml` → environment overrides**.
//!
//! `AppConfig` is loaded once at startup and cloned into the pipeline worker; it is the single consumer of
//! the runtime backend seam. It is persisted as TOML at `~/.local/share/corti/config.toml` (a sibling of
//! `queue.db`, outside any vault — guardrail #5), written by the in-app Settings screen. Environment vars
//! still override file values, so existing `CORTI_*` workflows and tests are unaffected.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which transcription backend to run. Both can be compiled in (`aws` + `local` features); this picks the
/// active one at **runtime**. Persisted/serialized as the lowercase token (`"aws"` / `"local"`), matching
/// the `CORTI_TRANSCRIBE_BACKEND` grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendChoice {
    Aws,
    Local,
}

/// Runtime configuration. Cloned into the pipeline worker; cheap and `Send`. Serialized to/from
/// `config.toml`: a flat document of scalars, so TOML field-ordering rules never bite. `#[serde(default)]`
/// on the container means a partial or older file deserializes cleanly, with every missing key filled from
/// [`AppConfig::default`].
// No `Eq`: `local_diarize_threshold` is an `f32` (which is only `PartialEq`). `PartialEq` is all the tests
// (and any config comparison) need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Which backend to use at runtime (`CORTI_TRANSCRIBE_BACKEND` = `aws` | `local`).
    pub transcribe_backend: BackendChoice,
    /// S3 bucket for the AWS backend (`CORTI_AWS_BUCKET`). The backend errors clearly if this is unset.
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_bucket: Option<String>,
    /// BCP-47 language passed to the AWS backend (`CORTI_LANGUAGE`, default `en-US`).
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    pub language: String,
    /// AWS named profile to resolve credentials/region from (like `aws --profile`). Applied as a
    /// **fallback** only when the environment doesn't already pin creds (`AWS_ACCESS_KEY_ID`/`AWS_PROFILE`);
    /// the Settings screen surfaces and edits it.
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    /// AWS region for Transcribe/S3. Applied as a **fallback** only when `AWS_REGION`/`AWS_DEFAULT_REGION`
    /// are unset. Must match the bucket's region (Transcribe is regional).
    #[cfg_attr(not(feature = "aws"), allow(dead_code))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,
    /// Model directory for the local backend (`CORTI_LOCAL_MODEL_DIR`); `None` ⇒ default cache
    /// (`~/Library/Caches/corti/models/`), resolved inside the backend.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_model_dir: Option<PathBuf>,
    /// ONNX Runtime provider for the local backend (`CORTI_LOCAL_PROVIDER`, default `cpu`; `coreml` opt-in).
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_provider: String,
    /// ONNX intra-op thread count for the local backend (`CORTI_LOCAL_THREADS`, default 4).
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_threads: i32,
    /// Split the far-end channel into per-speaker labels with the local backend (`CORTI_LOCAL_DIARIZE`,
    /// default off). Uses the selected English embedding model; tune `CORTI_LOCAL_DIARIZE_THRESHOLD` if it
    /// over-clusters (issue #18). Off ⇒ ch1 → single `Them`.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_diarize_far_end: bool,
    /// Which English speaker-embedding model far-end diarization uses (`CORTI_LOCAL_EMBEDDING`; one of
    /// `corti_transcribe_local::models::EMBEDDING_IDS`, default `titanet`). Surfaced in the Settings UI; an
    /// unknown/empty value falls back to the default inside the backend.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    #[serde(default = "default_embedding_model")]
    pub local_embedding_model: String,
    /// Diarization clustering threshold (`CORTI_LOCAL_DIARIZE_THRESHOLD`, 0.0–1.0, default 0.5) — higher
    /// merges more speakers, lower splits more. An env-only knob to tune far-end over-clustering (#18); not
    /// surfaced in the webview.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub local_diarize_threshold: f32,
    /// Whether to run offline echo cancellation on speaker recordings before transcription
    /// (`CORTI_AEC`, default on; set `0`/`false`/`off`/`no` to disable).
    pub aec_enabled: bool,
    /// AEC adaptive-filter length in taps (`CORTI_AEC_FILTER_LEN`). `None` ⇒ crate default (4096).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aec_filter_len: Option<usize>,
    /// AEC step size (`CORTI_AEC_MU`). `None` ⇒ crate default (0.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aec_mu: Option<f32>,
    /// AEC regularization (`CORTI_AEC_EPS`). `None` ⇒ crate default (1e-6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aec_eps: Option<f32>,
    /// AEC far-end power smoothing (`CORTI_AEC_POWER_SMOOTHING`). `None` ⇒ crate default (0.9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aec_power_smoothing: Option<f32>,
    /// AEC double-talk ratio (`CORTI_AEC_DOUBLE_TALK_RATIO`). `None` ⇒ crate default (2.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aec_double_talk_ratio: Option<f32>,
    /// AEC residual echo suppression factor (`CORTI_AEC_SUPPRESS_RESIDUAL`). `None` ⇒ crate default (2.5).
    /// Set 0 to disable. Higher values suppress more aggressively.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aec_suppress_residual: Option<f32>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            transcribe_backend: default_backend(),
            aws_bucket: None,
            language: "en-US".to_string(),
            aws_profile: None,
            aws_region: None,
            local_model_dir: None,
            local_provider: "cpu".to_string(),
            local_threads: 4,
            local_diarize_far_end: false,
            local_embedding_model: default_embedding_model(),
            local_diarize_threshold: 0.5,
            aec_enabled: true,
            aec_filter_len: None,
            aec_mu: None,
            aec_eps: None,
            aec_power_smoothing: None,
            aec_double_talk_ratio: None,
            aec_suppress_residual: None,
        }
    }
}

impl AppConfig {
    /// Build an [`corti_aec::AecConfig`] from the app config, overlaying any tuned `Some` values onto the
    /// crate defaults. `None` fields use the crate's built-in defaults.
    #[allow(deprecated)] // `passes` is a deprecated AecConfig field (ADR 0007); set from the default, ignored by the streaming path.
    pub fn aec_config(&self) -> corti_aec::AecConfig {
        let d = corti_aec::AecConfig::default();
        corti_aec::AecConfig {
            filter_len: self.aec_filter_len.unwrap_or(d.filter_len),
            mu: self.aec_mu.unwrap_or(d.mu),
            eps: self.aec_eps.unwrap_or(d.eps),
            power_smoothing: self.aec_power_smoothing.unwrap_or(d.power_smoothing),
            double_talk_ratio: self.aec_double_talk_ratio.unwrap_or(d.double_talk_ratio),
            passes: d.passes,
            suppress_residual: self.aec_suppress_residual.unwrap_or(d.suppress_residual),
        }
    }
}

impl AppConfig {
    /// Load the runtime config: start from the persisted file (or defaults if absent/corrupt), then let
    /// `CORTI_*` env vars override **only the fields they set** — so a present env var behaves exactly as
    /// before, while file values survive when the var is unset.
    pub fn load() -> Self {
        let mut cfg = Self::load_file().unwrap_or_default();

        if let Some(v) = env_non_empty("CORTI_TRANSCRIBE_BACKEND") {
            cfg.transcribe_backend = parse_backend(Some(v));
        }
        if let Some(v) = env_non_empty("CORTI_AWS_BUCKET") {
            cfg.aws_bucket = Some(v);
        }
        if let Some(v) = env_non_empty("CORTI_LANGUAGE") {
            cfg.language = v;
        }
        if let Some(v) = env_non_empty("CORTI_LOCAL_MODEL_DIR") {
            cfg.local_model_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = env_non_empty("CORTI_LOCAL_PROVIDER") {
            cfg.local_provider = v;
        }
        if let Some(n) = env_non_empty("CORTI_LOCAL_THREADS")
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|&n| n > 0)
        {
            cfg.local_threads = n;
        }
        if env_non_empty("CORTI_LOCAL_DIARIZE").is_some() {
            cfg.local_diarize_far_end = env_bool("CORTI_LOCAL_DIARIZE", cfg.local_diarize_far_end);
        }
        if let Some(v) = env_non_empty("CORTI_LOCAL_EMBEDDING") {
            cfg.local_embedding_model = v;
        }
        if let Some(t) = env_non_empty("CORTI_LOCAL_DIARIZE_THRESHOLD")
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|t| (0.0..=1.0).contains(t))
        {
            cfg.local_diarize_threshold = t;
        }
        if env_non_empty("CORTI_AEC").is_some() {
            cfg.aec_enabled = env_bool("CORTI_AEC", cfg.aec_enabled);
        }
        if let Some(n) = env_non_empty("CORTI_AEC_FILTER_LEN").and_then(|s| s.parse().ok()) {
            cfg.aec_filter_len = Some(n);
        }
        if let Some(v) = env_non_empty("CORTI_AEC_MU").and_then(|s| s.parse().ok()) {
            cfg.aec_mu = Some(v);
        }
        if let Some(v) = env_non_empty("CORTI_AEC_EPS").and_then(|s| s.parse().ok()) {
            cfg.aec_eps = Some(v);
        }
        if let Some(v) = env_non_empty("CORTI_AEC_POWER_SMOOTHING").and_then(|s| s.parse().ok()) {
            cfg.aec_power_smoothing = Some(v);
        }
        if let Some(v) = env_non_empty("CORTI_AEC_DOUBLE_TALK_RATIO").and_then(|s| s.parse().ok()) {
            cfg.aec_double_talk_ratio = Some(v);
        }
        if let Some(v) = env_non_empty("CORTI_AEC_SUPPRESS_RESIDUAL").and_then(|s| s.parse().ok()) {
            cfg.aec_suppress_residual = Some(v);
        }

        cfg
    }

    /// Read and parse `config.toml`. `None` when the file is absent (⇒ fall back to defaults) or corrupt
    /// (logged, then ignored — a bad file never panics the app or silently wipes good config). Public so the
    /// Settings layer can seed a save from the **file** baseline rather than the env-overridden runtime
    /// config (avoids baking a transient env var into the persisted document).
    pub fn load_file() -> Option<Self> {
        let path = config_path().ok()?;
        let text = std::fs::read_to_string(&path).ok()?; // missing file ⇒ None ⇒ defaults
        match toml::from_str::<Self>(&text) {
            Ok(cfg) => {
                tracing::info!(
                    target: "corti::config",
                    path = %path.display(),
                    bytes = text.len(),
                    "loaded config"
                );
                Some(cfg)
            }
            Err(e) => {
                tracing::warn!(
                    target: "corti::config",
                    path = %path.display(),
                    error = %e,
                    "ignoring corrupt config file"
                );
                None
            }
        }
    }

    /// Persist this config to `config.toml` atomically (write a sibling temp file, then rename — a crash
    /// mid-write can never leave a half-written or empty config in place).
    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = toml::to_string_pretty(self)?;
        let bytes = body.len();
        let tmp = path.with_extension("toml.tmp"); // same dir ⇒ rename is atomic on one volume
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        tracing::info!(
            target: "corti::config",
            path = %path.display(),
            bytes,
            "saved config"
        );
        Ok(())
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

    /// Stable, low-cardinality label for stats records ("aws" | "local" | "none"), distinct from the
    /// human-readable [`backend_name`]. Feature-gated like backend_name so an uncompiled backend reads
    /// "none".
    pub fn backend_label(&self) -> &'static str {
        match self.transcribe_backend {
            BackendChoice::Aws if cfg!(feature = "aws") => "aws",
            BackendChoice::Local if cfg!(feature = "local") => "local",
            _ => "none",
        }
    }
}

/// Path to the persisted config: a sibling of `queue.db` in `corti_queue::data_dir()`
/// (`~/.local/share/corti/config.toml`, honoring `$CORTI_DATA_DIR`). Durable user state, outside any vault.
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join("config.toml"))
}

/// The [`AppConfig`]/`SettingsDto` field names currently pinned by a `CORTI_*` env var. The Settings UI
/// disables these (editing them wouldn't take effect while the override is set), and `set_config` leaves
/// them untouched in the file so a transient override is never baked into `config.toml`.
pub fn env_managed_fields() -> Vec<String> {
    [
        ("CORTI_TRANSCRIBE_BACKEND", "transcribe_backend"),
        ("CORTI_AWS_BUCKET", "aws_bucket"),
        ("CORTI_LANGUAGE", "language"),
        ("CORTI_LOCAL_MODEL_DIR", "local_model_dir"),
        ("CORTI_LOCAL_PROVIDER", "local_provider"),
        ("CORTI_LOCAL_THREADS", "local_threads"),
        ("CORTI_LOCAL_DIARIZE", "local_diarize_far_end"),
        ("CORTI_LOCAL_EMBEDDING", "local_embedding_model"),
        ("CORTI_LOCAL_DIARIZE_THRESHOLD", "local_diarize_threshold"),
        ("CORTI_AEC", "aec_enabled"),
    ]
    .into_iter()
    .filter(|(var, _)| env_non_empty(var).is_some())
    .map(|(_, field)| field.to_string())
    .collect()
}

/// Parse `CORTI_TRANSCRIBE_BACKEND`; unset/unknown falls back to the build's default backend.
fn parse_backend(s: Option<String>) -> BackendChoice {
    match s.as_deref() {
        Some("aws") => BackendChoice::Aws,
        Some("local") => BackendChoice::Local,
        Some(other) => {
            tracing::warn!(
                target: "corti::config",
                value = other,
                "unknown CORTI_TRANSCRIBE_BACKEND; using build default"
            );
            default_backend()
        }
        None => default_backend(),
    }
}

/// Default speaker-embedding model id for far-end diarization. A literal (not the cross-crate
/// `corti_transcribe_local::models::DEFAULT_EMBEDDING_ID`) so this file compiles without the `local` feature;
/// keep the two in lock-step.
fn default_embedding_model() -> String {
    "titanet".to_string()
}

/// Default backend for this build: prefer the on-device `local` backend so a fresh install transcribes out
/// of the box with no cloud setup (and nothing leaves the Mac — matching the README's offline-by-default
/// framing). Fall back to `aws` only when `local` isn't compiled in. `CORTI_TRANSCRIBE_BACKEND` and the
/// Settings screen override this.
fn default_backend() -> BackendChoice {
    if cfg!(feature = "local") {
        BackendChoice::Local
    } else if cfg!(feature = "aws") {
        BackendChoice::Aws
    } else {
        BackendChoice::Local
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `CORTI_*`/`CORTI_DATA_DIR` mutations touch process-global env; serialize the tests that do so.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Remove every config env var so a test sees the file/defaults as its baseline.
    fn clear_config_env() {
        for k in [
            "CORTI_TRANSCRIBE_BACKEND",
            "CORTI_AWS_BUCKET",
            "CORTI_LANGUAGE",
            "CORTI_LOCAL_MODEL_DIR",
            "CORTI_LOCAL_PROVIDER",
            "CORTI_LOCAL_THREADS",
            "CORTI_LOCAL_DIARIZE",
            "CORTI_LOCAL_EMBEDDING",
            "CORTI_LOCAL_DIARIZE_THRESHOLD",
            "CORTI_AEC",
        ] {
            // SAFETY: callers hold ENV_LOCK, so no other thread reads/writes env concurrently.
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn toml_round_trips() {
        // Defaults.
        let cfg = AppConfig::default();
        let back: AppConfig = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, back);

        // Fully-populated, the other backend, Some(PathBuf), non-default scalars.
        let cfg2 = AppConfig {
            transcribe_backend: BackendChoice::Local,
            aws_bucket: Some("my-bucket".into()),
            language: "fr-FR".into(),
            aws_profile: Some("scientist".into()),
            aws_region: Some("us-west-2".into()),
            local_model_dir: Some(PathBuf::from("/tmp/models")),
            local_provider: "coreml".into(),
            local_threads: 8,
            local_diarize_far_end: true,
            local_embedding_model: "wespeaker-resnet34".into(),
            local_diarize_threshold: 0.7,
            aec_enabled: false,
            aec_filter_len: Some(8192),
            aec_mu: Some(0.5),
            aec_eps: None,
            aec_power_smoothing: Some(0.95),
            aec_double_talk_ratio: Some(1.5),
            aec_suppress_residual: Some(3.0),
        };
        let back2: AppConfig = toml::from_str(&toml::to_string_pretty(&cfg2).unwrap()).unwrap();
        assert_eq!(cfg2, back2);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let cfg: AppConfig = toml::from_str("language = \"fr-FR\"\n").unwrap();
        let d = AppConfig::default();
        assert_eq!(cfg.language, "fr-FR"); // the one present key
        assert_eq!(cfg.local_provider, d.local_provider); // everything else = default
        assert_eq!(cfg.local_threads, d.local_threads);
        assert_eq!(cfg.aec_enabled, d.aec_enabled);
        assert_eq!(cfg.transcribe_backend, d.transcribe_backend);
        assert_eq!(cfg.aws_bucket, None);
    }

    #[test]
    fn corrupt_file_falls_back() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("corti-cfg-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "!!! definitely [ not valid toml").unwrap();
        // SAFETY: single-threaded under ENV_LOCK.
        unsafe { std::env::set_var("CORTI_DATA_DIR", &dir) };

        assert!(AppConfig::load_file().is_none());

        // SAFETY: still under ENV_LOCK.
        unsafe { std::env::remove_var("CORTI_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_file_and_file_wins_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("corti-cfg-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single-threaded under ENV_LOCK; cleaned up below.
        unsafe { std::env::set_var("CORTI_DATA_DIR", &dir) };
        clear_config_env();

        // Persist a file whose language differs from the default.
        let on_disk = AppConfig {
            language: "de-DE".into(),
            ..AppConfig::default()
        };
        on_disk.save().unwrap();

        // Env present ⇒ env wins.
        // SAFETY: under ENV_LOCK.
        unsafe { std::env::set_var("CORTI_LANGUAGE", "es-ES") };
        assert_eq!(AppConfig::load().language, "es-ES");

        // Env unset ⇒ the file value wins (the new layering).
        // SAFETY: under ENV_LOCK.
        unsafe { std::env::remove_var("CORTI_LANGUAGE") };
        assert_eq!(AppConfig::load().language, "de-DE");

        // SAFETY: under ENV_LOCK.
        unsafe { std::env::remove_var("CORTI_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
