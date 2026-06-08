import { type BackendInfo, type SettingsDto } from "../lib/api";

interface Props {
  cfg: SettingsDto;
  backends: BackendInfo[];
  onChange: (next: SettingsDto) => void;
}

// The Transcription section: a direct editor over the persisted AppConfig fields. A field pinned by a
// `CORTI_*` env var is shown disabled with a "managed by env" badge (editing it wouldn't take effect, and
// the override is never written to the file).
export function Transcription({ cfg, backends, onChange }: Props) {
  const isEnv = (field: string) => cfg.env_managed.includes(field);
  const set = <K extends keyof SettingsDto>(key: K, value: SettingsDto[K]) =>
    onChange({ ...cfg, [key]: value });

  const envBadge = (field: string) =>
    isEnv(field) ? <span className="env-badge">managed by env</span> : null;

  return (
    <section className="card">
      <h2>Transcription</h2>

      <div className="settings-field">
        <label>Backend{envBadge("transcribe_backend")}</label>
        <div className="radio-row">
          {backends.map((b) => (
            <label key={b.id} className="radio">
              <input
                type="radio"
                name="backend"
                value={b.id}
                checked={cfg.transcribe_backend === b.id}
                disabled={!b.compiled_in || isEnv("transcribe_backend")}
                onChange={() => set("transcribe_backend", b.id)}
              />
              {b.label}
              {!b.compiled_in && " (not in this build)"}
            </label>
          ))}
        </div>
      </div>

      {cfg.transcribe_backend === "aws" && (
        <>
          <div className="settings-field">
            <label>S3 bucket{envBadge("aws_bucket")}</label>
            <input
              type="text"
              value={cfg.aws_bucket ?? ""}
              placeholder="my-corti-bucket"
              disabled={isEnv("aws_bucket")}
              onChange={(e) => set("aws_bucket", e.target.value || null)}
            />
          </div>
          <div className="settings-field">
            <label>Language{envBadge("language")}</label>
            <input
              type="text"
              value={cfg.language}
              placeholder="en-US"
              disabled={isEnv("language")}
              onChange={(e) => set("language", e.target.value)}
            />
            <p className="muted small">BCP-47 tag, e.g. en-US, fr-FR.</p>
          </div>
        </>
      )}

      {cfg.transcribe_backend === "local" && (
        <>
          <div className="settings-field">
            <label>Compute provider{envBadge("local_provider")}</label>
            <select
              className="jselect"
              value={cfg.local_provider}
              disabled={isEnv("local_provider")}
              onChange={(e) => set("local_provider", e.target.value)}
            >
              <option value="cpu">CPU</option>
              <option value="coreml">CoreML / ANE (validating)</option>
            </select>
          </div>
          <div className="settings-field">
            <label>ONNX threads{envBadge("local_threads")}</label>
            <input
              type="number"
              min={1}
              value={cfg.local_threads}
              disabled={isEnv("local_threads")}
              onChange={(e) => set("local_threads", Math.max(1, Number(e.target.value) || 1))}
            />
          </div>
          <label className="radio">
            <input
              type="checkbox"
              checked={cfg.local_diarize_far_end}
              disabled={isEnv("local_diarize_far_end")}
              onChange={(e) => set("local_diarize_far_end", e.target.checked)}
            />
            Diarize the far end (split “Them” into per-speaker labels)
            {envBadge("local_diarize_far_end")}
          </label>
          <p className="muted small">
            Off by default — far-end diarization over-clusters on English audio today (issue #18).
          </p>
        </>
      )}

      <label className="radio">
        <input
          type="checkbox"
          checked={cfg.aec_enabled}
          disabled={isEnv("aec_enabled")}
          onChange={(e) => set("aec_enabled", e.target.checked)}
        />
        Acoustic echo cancellation
        {envBadge("aec_enabled")}
      </label>
      <p className="muted small">Clean speaker bleed from the mic track before transcription.</p>

      <p className="callout">Changes apply to the next recording.</p>
    </section>
  );
}
