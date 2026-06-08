import { useEffect, useState } from "react";
import { getPaths, revealPath, setModelsDir, type PathsDto } from "../lib/api";

function mb(bytes: number): string {
  return `${(bytes / 1_000_000).toFixed(bytes < 10_000_000 ? 1 : 0)} MB`;
}

// Settings → Storage: the recordings cache and (when the local backend is compiled in) the models dir.
// Changing the models dir validates it sits outside any notes vault, server-side (guardrail #5).
export function Paths() {
  const [paths, setPaths] = useState<PathsDto | null>(null);
  const [modelsDirEdit, setModelsDirEdit] = useState("");
  const [error, setError] = useState("");

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
