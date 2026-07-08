// Typed wrappers over the Tauri `invoke` bridge — the single place the settings DTO shapes are mirrored
// from the Rust `app/src/settings.rs` commands. Keep these in sync with that file.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Mirror of Rust `settings::SettingsDto`. `env_managed` lists the fields a `CORTI_*` env var pins. */
export interface SettingsDto {
  transcribe_backend: string; // "aws" | "local"
  aws_bucket: string | null;
  language: string;
  aws_profile: string | null;
  aws_region: string | null;
  local_threads: number;
  local_diarize_far_end: boolean;
  local_embedding_model: string; // a corti-transcribe-local EMBEDDING_IDS id, e.g. "titanet"
  aec_enabled: boolean;
  retention_days: number;
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

// ----- AWS credential-chain status + verification -----

/** Mirror of Rust `settings::AwsStatus`. Never carries the secret/session-token values — booleans only. */
export interface AwsStatus {
  profiles: string[];
  selected_profile: string | null;
  configured_region: string | null;
  profile_locked: boolean;
  region_locked: boolean;
  env_access_key_id: string | null;
  env_has_secret: boolean;
  env_session_token: boolean;
  env_profile: string | null;
  env_region: string | null;
  source: string;
}

/** Mirror of Rust `settings::AwsIdentity` (STS GetCallerIdentity). */
export interface AwsIdentity {
  account: string | null;
  arn: string | null;
  user_id: string | null;
}

export const getAwsStatus = (): Promise<AwsStatus> => invoke<AwsStatus>("get_aws_status");

export const verifyAws = (): Promise<AwsIdentity> => invoke<AwsIdentity>("verify_aws");

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

/** Install state of the selectable English speaker-embedding models (the Transcription dropdown). */
export const getEmbeddingModels = (): Promise<ModelStatus[]> =>
  invoke<ModelStatus[]>("get_embedding_models");

export const downloadModel = (id: string): Promise<void> => invoke<void>("download_model", { id });

/** Payload of the `model-download-progress` event emitted while a model downloads. */
export interface DownloadProgress {
  id: string;
  received: number;
  total: number;
}

export const onDownloadProgress = (cb: (p: DownloadProgress) => void): Promise<UnlistenFn> =>
  listen<DownloadProgress>("model-download-progress", (e) => cb(e.payload));

// ----- Diagnostics console (mirror of Rust `console::ConsoleEntry`) -----

/** Mirror of Rust `console::ConsoleEntry`. One captured tracing event. Field names match the Rust struct
 * verbatim (serde serializes them as-is — no specta binding generation in corti). */
export interface ConsoleEntry {
  timestamp: string; // ISO-8601 UTC, e.g. "2026-06-18T17:04:05.123Z"
  level: string; // "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"
  target: string; // event metadata target (usually the module path)
  message: string; // the `message` field, else a " field=value" concatenation
}

/** Snapshot of the in-memory console ring buffer, oldest entry first. */
export const getConsoleLogs = (): Promise<ConsoleEntry[]> =>
  invoke<ConsoleEntry[]>("get_console_logs");

/** The same buffer rendered as plain text (one entry per line). */
export const getConsoleLogsText = (): Promise<string> => invoke<string>("get_console_logs_text");

/** Opens a native save dialog and writes the buffer text. Resolves true if written, false if cancelled;
 * rejects with a string on write failure. */
export const saveConsoleLogs = (): Promise<boolean> => invoke<boolean>("save_console_logs");

// ----- Resource stats (mirror of Rust `stats::*`; serde field names verbatim — no specta in corti) -----

/** Mirror of Rust `stats::ThreadStat`. One OS thread's scheduler-averaged CPU share. */
export interface ThreadStat {
  name: string; // thread name (e.g. "corti-pipeline", "corti-stats"); may be "" for unnamed threads
  cpu_pct: number; // scheduler-averaged CPU %, per-core (Mach cpu_usage permille / 10; decays, not exact)
}

/** Mirror of Rust `stats::StageSample`. One completed coarse pipeline stage. */
export interface StageSample {
  timestamp: string; // ISO-8601 UTC
  stage: string; // "transcribe" | "file"
  backend: string; // "aws" | "local" | "none" | "vagus"
  duration_ms: number;
}

/** Mirror of Rust `stats::StatsSnapshot`. One 1 Hz sample of process health. */
export interface StatsSnapshot {
  timestamp: string; // ISO-8601 UTC, same format as ConsoleEntry.timestamp
  backend: string; // active transcription backend label ("AWS Transcribe" | "Parakeet (local)" | "none")
  detector_recording: boolean;
  webinar_recording: boolean;
  phys_mb: number; // phys_footprint, mebibytes (MiB)
  rss_mb: number; // resident set size, mebibytes (MiB)
  threads: ThreadStat[];
}

/** Mirror of Rust `stats::StatsReport`. One coherent read of the stats buffer. */
export interface StatsReport {
  history: StatsSnapshot[]; // 1 Hz memory/thread samples, oldest first (~5 min ring)
  stages: StageSample[]; // process-global completed pipeline-stage timings, oldest first (not per-sample)
}

/** One coherent snapshot of the stats buffer: memory/thread history + global stage timings. */
export const getStats = (): Promise<StatsReport> => invoke<StatsReport>("get_stats");

// ----- Recording Queue window -----

/** Mirror of Rust `queue_ui::RecordingDto`. */
export interface RecordingDto {
  id: string;
  app: string;
  mode: string; // "call" | "webinar"
  started_at: string;
  ended_at: string | null;
  duration_secs: number | null;
  status: string; // JobStatus wire form: "recording" | "pending_transcription" | ... | "done" | "failed"
  error: string | null;
  transcribe_secs: number | null;
  note_path: string | null;
  note_exists: boolean;
  audio_exists: boolean;
  audio_bytes: number | null;
  retry_pending: boolean;
  retry_attempts: number | null;
}

export const listRecordings = (): Promise<RecordingDto[]> =>
  invoke<RecordingDto[]>("list_recordings");

export const retryRecording = (id: string): Promise<void> =>
  invoke<void>("retry_recording", { id });

export const openNote = (path: string): Promise<void> => invoke<void>("open_note", { path });

export const revealAudio = (id: string): Promise<void> => invoke<void>("reveal_audio", { id });

/** Subscribe to the pipeline's coarse "something changed" signal; returns the unlisten fn. */
export const onQueueChanged = (handler: () => void): Promise<UnlistenFn> =>
  listen("queue-changed", handler);
