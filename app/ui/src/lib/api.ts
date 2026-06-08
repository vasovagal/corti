// Typed wrappers over the Tauri `invoke` bridge — the single place the settings DTO shapes are mirrored
// from the Rust `app/src/settings.rs` commands. Keep these in sync with that file.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Mirror of Rust `settings::SettingsDto`. `env_managed` lists the fields a `CORTI_*` env var pins. */
export interface SettingsDto {
  transcribe_backend: string; // "aws" | "local"
  aws_bucket: string | null;
  language: string;
  local_model_dir: string | null;
  local_provider: string; // "cpu" | "coreml"
  local_threads: number;
  local_diarize_far_end: boolean;
  aec_enabled: boolean;
  env_managed: string[];
}

/** Mirror of Rust `settings::BackendInfo`. */
export interface BackendInfo {
  id: string;
  label: string;
  compiled_in: boolean;
}

export const getConfig = (): Promise<SettingsDto> => invoke<SettingsDto>("get_config");

export const getBackends = (): Promise<BackendInfo[]> => invoke<BackendInfo[]>("get_backends");

export const setConfig = (dto: SettingsDto): Promise<void> => invoke<void>("set_config", { dto });

// ----- Path + Model sections -----

/** Mirror of Rust `settings::PathsDto`. `models_*` is null when the local backend isn't compiled in. */
export interface PathsDto {
  recordings_dir: string;
  recordings_bytes: number;
  models_dir: string | null;
  models_bytes: number | null;
}

/** Mirror of Rust `settings::ModelStatus`. */
export interface ModelStatus {
  id: string;
  label: string;
  present: boolean;
  on_disk_bytes: number;
  download_bytes: number;
  diarize_only: boolean;
}

export const getPaths = (): Promise<PathsDto> => invoke<PathsDto>("get_paths");

export const revealPath = (which: "recordings" | "models"): Promise<void> =>
  invoke<void>("reveal_path", { which });

export const setModelsDir = (dir: string): Promise<void> => invoke<void>("set_models_dir", { dir });

export const getModelsStatus = (): Promise<ModelStatus[]> =>
  invoke<ModelStatus[]>("get_models_status");

export const downloadModel = (id: string): Promise<void> => invoke<void>("download_model", { id });

/** Payload of the `model-download-progress` event emitted while a model downloads. */
export interface DownloadProgress {
  id: string;
  received: number;
  total: number;
}

export const onDownloadProgress = (cb: (p: DownloadProgress) => void): Promise<UnlistenFn> =>
  listen<DownloadProgress>("model-download-progress", (e) => cb(e.payload));
