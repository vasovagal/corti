import { useEffect, useState } from "react";
import {
  downloadModel,
  getModelsStatus,
  onDownloadProgress,
  type ModelStatus,
} from "../lib/api";

function mb(bytes: number): string {
  return `${(bytes / 1_000_000).toFixed(bytes < 10_000_000 ? 1 : 0)} MB`;
}

// Settings → Models: per-model install state with verified downloads. Errors gracefully (e.g. "local backend
// not compiled in") since the underlying commands are gated on the `local` feature.
export function Models() {
  const [models, setModels] = useState<ModelStatus[] | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [pct, setPct] = useState<Record<string, number>>({});

  const refresh = () =>
    getModelsStatus()
      .then((m) => {
        setModels(m);
        setError("");
      })
      .catch((e) => setError(String(e)));

  useEffect(() => {
    refresh();
    const unlisten = onDownloadProgress((p) =>
      setPct((cur) => ({ ...cur, [p.id]: p.total ? Math.round((p.received / p.total) * 100) : 0 })),
    );
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  async function download(id: string) {
    setBusy(id);
    setError("");
    setPct((cur) => ({ ...cur, [id]: 0 }));
    try {
      await downloadModel(id);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      setPct((cur) => {
        const next = { ...cur };
        delete next[id];
        return next;
      });
    }
  }

  // No models list yet: distinguish "loading" from an error (e.g. local backend not in this build).
  if (!models) {
    return (
      <section className="card">
        <h2>Models</h2>
        <p className="muted small">{error || "Loading…"}</p>
      </section>
    );
  }

  return (
    <section className="card">
      <h2>Models</h2>
      <p className="muted small">
        Local transcription models (sherpa-onnx). Diarization models are only needed when far-end
        diarization is on.
      </p>
      <table className="jtable">
        <tbody>
          {models.map((m) => (
            <tr key={m.id}>
              <td className="cell-name">
                {m.label}
                {m.diarize_only && <span className="env-badge">diarization</span>}
              </td>
              <td className="muted small">
                {m.present
                  ? `installed · ${mb(m.on_disk_bytes)}`
                  : `not installed · ~${mb(m.download_bytes)}`}
              </td>
              <td>
                {busy === m.id ? (
                  <span className="muted small">{pct[m.id] ?? 0}%</span>
                ) : (
                  <button className="btn-add" disabled={busy !== null} onClick={() => download(m.id)}>
                    {m.present ? "Re-download" : "Download"}
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {error && <p className="muted small">{error}</p>}
    </section>
  );
}
