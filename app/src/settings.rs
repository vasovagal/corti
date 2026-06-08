//! The Settings screen's command surface: serde DTOs + the `#[tauri::command]` handlers the webview calls,
//! plus the shared config state they (and the pipeline worker) read/write.
//!
//! `config.rs` stays Tauri-free (pure config + IO, unit-testable); everything that touches Tauri lives here.
//! On save we persist `config.toml` from the **file** baseline — never the env-overridden runtime config, so
//! a transient `CORTI_*` override is never baked into the document — then republish the resolved runtime
//! config and signal the pipeline worker to rebuild its backend on its next turn.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::config::{self, AppConfig, BackendChoice};
use crate::pipeline::PipelineMsg;

/// Runtime config shared between the Settings commands and the pipeline worker. The commands publish a new
/// value here on save; the worker re-reads it when it handles [`PipelineMsg::ReloadConfig`].
pub type SharedConfig = Arc<Mutex<AppConfig>>;

/// Managed state for the Settings commands: the shared runtime config + a sender to nudge the worker.
pub struct ConfigState {
    pub config: SharedConfig,
    /// A clone of the pipeline channel, used to send [`PipelineMsg::ReloadConfig`] after a save.
    pub reload_tx: Mutex<Sender<PipelineMsg>>,
}

/// Wire mirror of [`AppConfig`] for the webview, plus `env_managed`: the field names a `CORTI_*` env var
/// currently pins. The UI disables those — editing them wouldn't take effect while the override is set, and
/// we won't bake the override into the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsDto {
    pub transcribe_backend: String,
    pub aws_bucket: Option<String>,
    pub language: String,
    pub local_model_dir: Option<String>,
    pub local_provider: String,
    pub local_threads: i32,
    pub local_diarize_far_end: bool,
    pub aec_enabled: bool,
    pub env_managed: Vec<String>,
}

/// A selectable backend and whether this build can actually run it (the UI disables the rest).
#[derive(Debug, Clone, Serialize)]
pub struct BackendInfo {
    pub id: String,
    pub label: String,
    pub compiled_in: bool,
}

fn backend_id(b: BackendChoice) -> &'static str {
    match b {
        BackendChoice::Aws => "aws",
        BackendChoice::Local => "local",
    }
}

impl From<&AppConfig> for SettingsDto {
    fn from(cfg: &AppConfig) -> Self {
        Self {
            transcribe_backend: backend_id(cfg.transcribe_backend).to_string(),
            aws_bucket: cfg.aws_bucket.clone(),
            language: cfg.language.clone(),
            local_model_dir: cfg.local_model_dir.as_ref().map(|p| p.display().to_string()),
            local_provider: cfg.local_provider.clone(),
            local_threads: cfg.local_threads,
            local_diarize_far_end: cfg.local_diarize_far_end,
            aec_enabled: cfg.aec_enabled,
            env_managed: config::env_managed_fields(),
        }
    }
}

/// Current settings (runtime config = file + env), with the env-pinned field list for the UI.
#[tauri::command]
pub fn get_config(state: State<'_, ConfigState>) -> SettingsDto {
    let cfg = state.config.lock().unwrap();
    SettingsDto::from(&*cfg)
}

/// The transcription backends and whether each is compiled into this build.
#[tauri::command]
pub fn get_backends() -> Vec<BackendInfo> {
    vec![
        BackendInfo {
            id: "aws".to_string(),
            label: "AWS Transcribe".to_string(),
            compiled_in: cfg!(feature = "aws"),
        },
        BackendInfo {
            id: "local".to_string(),
            label: "Parakeet (local)".to_string(),
            compiled_in: cfg!(feature = "local"),
        },
    ]
}

/// Validate + persist the edited settings, then republish the runtime config and ask the worker to rebuild
/// its backend. Env-pinned fields are left untouched in the file (the override stays the source of truth and
/// is never written into `config.toml`).
#[tauri::command]
pub fn set_config(
    dto: SettingsDto,
    state: State<'_, ConfigState>,
    app: AppHandle,
) -> Result<(), String> {
    let backend = match dto.transcribe_backend.as_str() {
        "aws" => BackendChoice::Aws,
        "local" => BackendChoice::Local,
        other => return Err(format!("unknown transcription backend {other:?}")),
    };
    if dto.local_threads <= 0 {
        return Err("local thread count must be greater than zero".to_string());
    }

    // Seed from the FILE baseline (not the env-overridden runtime config) and apply only the edits to fields
    // that aren't pinned by env, so a transient CORTI_* override is preserved and never written to the file.
    let env_managed = config::env_managed_fields();
    let pinned = |field: &str| env_managed.iter().any(|f| f == field);

    let mut to_save = AppConfig::load_file().unwrap_or_default();
    if !pinned("transcribe_backend") {
        to_save.transcribe_backend = backend;
    }
    if !pinned("aws_bucket") {
        to_save.aws_bucket = non_empty(dto.aws_bucket);
    }
    if !pinned("language")
        && let Some(lang) = non_empty(Some(dto.language))
    {
        to_save.language = lang;
    }
    if !pinned("local_model_dir") {
        to_save.local_model_dir = non_empty(dto.local_model_dir).map(PathBuf::from);
    }
    if !pinned("local_provider")
        && let Some(p) = non_empty(Some(dto.local_provider))
    {
        to_save.local_provider = p;
    }
    if !pinned("local_threads") {
        to_save.local_threads = dto.local_threads;
    }
    if !pinned("local_diarize_far_end") {
        to_save.local_diarize_far_end = dto.local_diarize_far_end;
    }
    if !pinned("aec_enabled") {
        to_save.aec_enabled = dto.aec_enabled;
    }

    to_save.save().map_err(|e| format!("saving config: {e:#}"))?;

    // Recompute the resolved runtime config (file + env layering) and publish it for the worker + tray.
    *state.config.lock().unwrap() = AppConfig::load();

    // Nudge the worker to rebuild its backend on its next turn (immediate when idle).
    if let Err(e) = state.reload_tx.lock().unwrap().send(PipelineMsg::ReloadConfig) {
        eprintln!("[corti] could not signal pipeline reload (worker gone?): {e}");
    }

    // Refresh the tray's read-only Backend:/Bucket: summary so it reflects the save.
    crate::tray::refresh_menu(&app);

    Ok(())
}

/// Trim a string and treat empty as absent.
fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ----- Path + Model sections (issue #24) -----

/// Storage paths corti uses, with on-disk sizes. `models_*` is `None` when the local backend isn't compiled
/// into this build (there are no local models to manage).
#[derive(Debug, Clone, Serialize)]
pub struct PathsDto {
    pub recordings_dir: String,
    pub recordings_bytes: u64,
    pub models_dir: Option<String>,
    pub models_bytes: Option<u64>,
}

/// One local model's install state for the Settings → Models table.
#[derive(Debug, Clone, Serialize)]
pub struct ModelStatus {
    pub id: String,
    pub label: String,
    pub present: bool,
    pub on_disk_bytes: u64,
    pub download_bytes: u64,
    pub diarize_only: bool,
}

/// Total bytes under a path: the file's length, or the recursive sum of a directory. Missing ⇒ 0.
fn path_size(p: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if meta.is_dir() {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                total += path_size(&entry.path());
            }
        }
        return total;
    }
    0
}

/// The recordings cache dir (honors `CORTI_RECORDINGS_DIR`); the fallback name is unreachable in practice.
fn recordings_dir() -> PathBuf {
    corti_capture::recordings_dir().unwrap_or_else(|_| PathBuf::from("recordings"))
}

/// Resolve the local models dir, or an error when the local backend isn't compiled into this build.
#[cfg(feature = "local")]
fn models_dir_or_err(state: &State<'_, ConfigState>) -> Result<PathBuf, String> {
    let override_dir = state.config.lock().unwrap().local_model_dir.clone();
    corti_transcribe_local::models::resolve_dir(override_dir).map_err(|e| format!("{e:#}"))
}
#[cfg(not(feature = "local"))]
fn models_dir_or_err(_state: &State<'_, ConfigState>) -> Result<PathBuf, String> {
    Err("local transcription backend is not compiled into this build".to_string())
}

/// Storage paths + sizes for the Settings → Storage section.
#[tauri::command]
pub fn get_paths(state: State<'_, ConfigState>) -> PathsDto {
    let recordings = recordings_dir();
    let recordings_bytes = path_size(&recordings);

    #[cfg(feature = "local")]
    let (models_dir, models_bytes) = {
        let override_dir = state.config.lock().unwrap().local_model_dir.clone();
        let dir = corti_transcribe_local::models::resolve_dir(override_dir).unwrap_or_default();
        (Some(dir.display().to_string()), Some(path_size(&dir)))
    };
    #[cfg(not(feature = "local"))]
    let (models_dir, models_bytes) = {
        let _ = &state;
        (None, None)
    };

    PathsDto {
        recordings_dir: recordings.display().to_string(),
        recordings_bytes,
        models_dir,
        models_bytes,
    }
}

/// Reveal a storage dir in Finder (creating it first if missing). `which` ∈ {"recordings","models"}.
#[tauri::command]
pub fn reveal_path(which: String, state: State<'_, ConfigState>) -> Result<(), String> {
    let dir = match which.as_str() {
        "recordings" => recordings_dir(),
        "models" => models_dir_or_err(&state)?,
        other => return Err(format!("unknown path {other:?}")),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::process::Command::new("open")
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("open {}: {e}", dir.display()))?;
    Ok(())
}

/// If `path` or any ancestor is an Obsidian vault root (has a `.obsidian/` dir), return that root. A
/// pragmatic guardrail-#5 check that keeps model/cache dirs out of the user's notes vault. (corti-vagus is
/// CLI-only — guardrail #1 — so we don't query it for vault roots; this heuristic stands in.)
#[cfg(feature = "local")]
fn vault_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.join(".obsidian").is_dir() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// Set the local models directory (persisted to config), rejecting a location inside a notes vault
/// (guardrail #5). Re-applies on the next recording.
#[tauri::command]
pub fn set_models_dir(dir: String, state: State<'_, ConfigState>, app: AppHandle) -> Result<(), String> {
    #[cfg(not(feature = "local"))]
    {
        let _ = (dir, state, app);
        Err("local transcription backend is not compiled into this build".to_string())
    }
    #[cfg(feature = "local")]
    {
        let trimmed = dir.trim();
        if trimmed.is_empty() {
            return Err("models directory path is empty".to_string());
        }
        if config::env_managed_fields().iter().any(|f| f == "local_model_dir") {
            return Err(
                "the models directory is pinned by CORTI_LOCAL_MODEL_DIR; unset it to edit here"
                    .to_string(),
            );
        }
        let path = PathBuf::from(trimmed);
        if let Some(vault) = vault_ancestor(&path) {
            return Err(format!(
                "refusing a models directory inside a vault ({}); pick a location outside your notes vault",
                vault.display()
            ));
        }

        let mut to_save = AppConfig::load_file().unwrap_or_default();
        to_save.local_model_dir = Some(path);
        to_save.save().map_err(|e| format!("saving config: {e:#}"))?;
        *state.config.lock().unwrap() = AppConfig::load();
        if let Err(e) = state.reload_tx.lock().unwrap().send(PipelineMsg::ReloadConfig) {
            eprintln!("[corti] could not signal pipeline reload (worker gone?): {e}");
        }
        crate::tray::refresh_menu(&app);
        Ok(())
    }
}

/// Install state of every local model (Settings → Models). Errors when the local backend isn't compiled in.
#[tauri::command]
pub fn get_models_status(state: State<'_, ConfigState>) -> Result<Vec<ModelStatus>, String> {
    #[cfg(not(feature = "local"))]
    {
        let _ = &state;
        Err("local transcription backend is not compiled into this build".to_string())
    }
    #[cfg(feature = "local")]
    {
        let dir = models_dir_or_err(&state)?;
        let statuses = corti_transcribe_local::models::model_catalog()
            .into_iter()
            .map(|spec| {
                let present = dir.join(spec.present_rel).exists();
                let on_disk_bytes = if present {
                    path_size(&dir.join(spec.install_rel))
                } else {
                    0
                };
                ModelStatus {
                    id: spec.id.to_string(),
                    label: spec.label.to_string(),
                    present,
                    on_disk_bytes,
                    download_bytes: spec.download_bytes,
                    diarize_only: spec.diarize_only,
                }
            })
            .collect();
        Ok(statuses)
    }
}

#[cfg(feature = "local")]
#[derive(Clone, Serialize)]
struct DownloadProgress {
    id: String,
    received: u64,
    total: u64,
}

#[cfg(feature = "local")]
#[derive(Clone, Serialize)]
struct DownloadDone {
    id: String,
}

/// Download + install a local model with SHA-256 verification, emitting `model-download-progress` /
/// `model-download-done` events to the webview. Async so it runs off the main thread.
#[cfg(feature = "local")]
#[tauri::command]
pub async fn download_model(
    id: String,
    state: State<'_, ConfigState>,
    app: AppHandle,
) -> Result<(), String> {
    let spec = corti_transcribe_local::models::model_catalog()
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("unknown model {id:?}"))?;
    let dir = models_dir_or_err(&state)?;
    download_and_install(&app, &dir, &spec)
}

#[cfg(not(feature = "local"))]
#[tauri::command]
pub fn download_model(id: String) -> Result<(), String> {
    let _ = id;
    Err("local transcription backend is not compiled into this build".to_string())
}

/// Stream the artifact to a `.part` file while hashing, verify the pinned SHA-256, then install: a bare file
/// is renamed into place; a tarball is extracted with the system `tar` (bzip2 via `-j`, matching
/// fetch-models.sh) after the prior install is removed. A failed checksum or extraction leaves any existing
/// install untouched — a truncated download can never replace a good model.
#[cfg(feature = "local")]
fn download_and_install(
    app: &AppHandle,
    dir: &Path,
    spec: &corti_transcribe_local::models::ModelSpec,
) -> Result<(), String> {
    use corti_transcribe_local::models::ArtifactKind;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use tauri::Emitter;

    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;

    let file_name = spec.url.rsplit('/').next().unwrap_or("model-download");
    let part = dir.join(format!("{file_name}.part"));

    let resp = ureq::get(spec.url)
        .call()
        .map_err(|e| format!("requesting {}: {e}", spec.url))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(spec.download_bytes);

    let mut reader = resp.into_reader();
    let mut out =
        std::fs::File::create(&part).map_err(|e| format!("creating {}: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("downloading {}: {e}", spec.id))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("writing {}: {e}", part.display()))?;
        received += n as u64;
        // Throttle progress events to ~every 1 MB.
        if received - last_emit >= 1_000_000 {
            last_emit = received;
            let _ = app.emit(
                "model-download-progress",
                DownloadProgress {
                    id: spec.id.to_string(),
                    received,
                    total,
                },
            );
        }
    }
    out.flush().ok();
    drop(out);
    // Final 100% tick.
    let _ = app.emit(
        "model-download-progress",
        DownloadProgress {
            id: spec.id.to_string(),
            received,
            total,
        },
    );

    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if hex != spec.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "checksum mismatch for {} — expected {}, got {hex}; download discarded",
            spec.id, spec.sha256
        ));
    }

    match spec.kind {
        ArtifactKind::File => {
            let dest = dir.join(spec.install_rel);
            std::fs::rename(&part, &dest)
                .map_err(|e| format!("installing {}: {e}", dest.display()))?;
        }
        ArtifactKind::Tarball => {
            // Remove any prior install for a clean replace, then extract with the system tar (bzip2 via -j).
            let install = dir.join(spec.install_rel);
            let _ = std::fs::remove_dir_all(&install);
            let status = std::process::Command::new("tar")
                .arg("xjf")
                .arg(&part)
                .arg("-C")
                .arg(dir)
                .status()
                .map_err(|e| format!("running tar: {e}"))?;
            let _ = std::fs::remove_file(&part);
            if !status.success() {
                return Err(format!(
                    "extracting {} failed (tar exit {:?})",
                    spec.id,
                    status.code()
                ));
            }
        }
    }

    let _ = app.emit(
        "model-download-done",
        DownloadDone {
            id: spec.id.to_string(),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_size_sums_a_directory_tree() {
        let dir = std::env::temp_dir().join(format!("corti-pathsize-{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.bin"), [0u8; 100]).unwrap();
        std::fs::write(sub.join("b.bin"), [0u8; 23]).unwrap();
        assert_eq!(path_size(&dir), 123);
        assert_eq!(path_size(&dir.join("a.bin")), 100);
        assert_eq!(path_size(&dir.join("missing")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "local")]
    #[test]
    fn vault_ancestor_detects_an_obsidian_vault() {
        let root = std::env::temp_dir().join(format!("corti-vault-{}", std::process::id()));
        let nested = root.join("attachments").join("corti-models");
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(vault_ancestor(&nested).as_deref(), Some(root.as_path()));

        let outside = std::env::temp_dir().join(format!("corti-nonvault-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        assert!(vault_ancestor(&outside).is_none());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
