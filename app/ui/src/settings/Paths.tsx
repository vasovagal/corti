import { useEffect, useState } from "react";
import {
  getPaths,
  revealPath,
  setModelsDir,
  type PathsDto,
  type SettingsDto,
} from "../lib/api";

function mb(bytes: number): string {
  return `${(bytes / 1_000_000).toFixed(bytes < 10_000_000 ? 1 : 0)} MB`;
}

// Settings → Storage: the recordings cache, the retention window for its audio, and (when the local
// backend is compiled in) the models dir. Changing the models dir validates it sits outside any notes
// vault, server-side (guardrail #5). The retention edit rides the shared Save button via `onChange`.
export function Paths({
  cfg,
  onChange,
}: {
  cfg: SettingsDto;
  onChange: (cfg: SettingsDto) => void;
}) {
  const [paths, setPaths] = useState<PathsDto | null>(null);
  const [modelsDirEdit, setModelsDirEdit] = useState("");
  const [error, setError] = useState("");
  const retentionPinned = cfg.env_managed.includes("retention_days");

  const refresh = () =>
    getPaths()
      .then((p) => {
        setPaths(p);
        setModelsDirEdit(p.models_dir ?? "");
        setError("");
      })
      .catch((e) => setError(String(e)));

  useEffect(() => {
    refresh();
  }, []);

  if (!paths) {
    return (
      <section className="card">
        <h2>Storage</h2>
        <p className="muted">{error || "Loading…"}</p>
      </section>
    );
  }

  const reveal = (which: "recordings" | "models") =>
    revealPath(which).catch((e) => setError(String(e)));

  async function changeModelsDir() {
    setError("");
    try {
      await setModelsDir(modelsDirEdit);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <section className="card">
      <h2>Storage</h2>

      <div className="settings-field">
        <label>Recordings cache</label>
        <p className="muted small">
          {paths.recordings_dir} · {mb(paths.recordings_bytes)} on disk
        </p>
        <div className="other-row">
          <button className="btn-add" onClick={() => reveal("recordings")}>
            Reveal in Finder
          </button>
        </div>
      </div>

      <div className="settings-field">
        <label>Keep recordings for</label>
        <div className="other-row">
          <input
            type="number"
            min={1}
            max={365}
            value={cfg.retention_days}
            disabled={retentionPinned}
            onChange={(e) =>
              onChange({ ...cfg, retention_days: Number(e.target.value) })
            }
            style={{ width: "5em" }}
          />
          <span className="muted small">days</span>
          {retentionPinned && (
            <span className="muted small">set by CORTI_RETENTION_DAYS</span>
          )}
        </div>
        <p className="muted small">
          Audio older than this is deleted by the hourly sweep. Notes and
          transcripts are never touched, and history stays visible in the
          Recording Queue.
        </p>
      </div>

      {paths.models_dir !== null && (
        <div className="settings-field">
          <label>Models directory</label>
          <p className="muted small">{mb(paths.models_bytes ?? 0)} on disk</p>
          <input
            type="text"
            value={modelsDirEdit}
            placeholder="~/Library/Caches/corti/models"
            onChange={(e) => setModelsDirEdit(e.target.value)}
          />
          <div className="other-row">
            <button
              className="btn-add"
              onClick={changeModelsDir}
              disabled={modelsDirEdit.trim() === (paths.models_dir ?? "")}
            >
              Set
            </button>
            <button className="btn-add" onClick={() => reveal("models")}>
              Reveal in Finder
            </button>
          </div>
        </div>
      )}

      {error && <p className="muted small">{error}</p>}
    </section>
  );
}
