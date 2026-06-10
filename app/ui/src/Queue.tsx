import { useCallback, useEffect, useState } from "react";
import {
  listRecordings,
  onQueueChanged,
  openNote,
  retryRecording,
  revealAudio,
  type RecordingDto,
} from "./lib/api";
import { rowState } from "./lib/queue";

function relativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return "";
  const mins = Math.round((Date.now() - then) / 60_000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins} min ago`;
  const hours = Math.round(mins / 60);
  if (hours < 48) return `${hours} h ago`;
  return new Date(iso).toLocaleDateString();
}

function mb(bytes: number | null): string {
  return bytes === null ? "" : `${(bytes / 1_000_000).toFixed(1)} MB`;
}

// The printer-queue window: every tracked recording, newest first, with its pipeline state and the
// one action that makes sense for it (open the note / retry). Refetches on the pipeline's
// `queue-changed` event plus a slow tick so relative times stay honest.
export default function Queue() {
  const [rows, setRows] = useState<RecordingDto[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState<string | null>(null); // id of an in-flight retry click

  const refresh = useCallback(() => {
    listRecordings()
      .then((r) => {
        setRows(r);
        setError("");
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    document.title = "Recording Queue — Corti";
    refresh();
    const unlisten = onQueueChanged(refresh);
    const tick = setInterval(refresh, 30_000);
    return () => {
      clearInterval(tick);
      unlisten.then((un) => un()).catch(() => {});
    };
  }, [refresh]);

  async function retry(id: string) {
    setBusy(id); // optimistic; queue-changed events drive the real state
    try {
      await retryRecording(id);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="app">
      <header className="app-header">
        <h1>Recording Queue</h1>
        <p className="subtitle">
          Every recording corti has tracked — audio expires on the retention sweep; notes live on in
          the brain.
        </p>
      </header>

      <main className="tab-content">
        {error && <p className="muted small">{error}</p>}
        {rows.length === 0 && !error && (
          <p className="muted">No recordings yet — join a call or start a webinar capture.</p>
        )}

        {rows.map((r) => {
          const s = rowState(r);
          return (
            <section className="card" key={r.id}>
              <div className="settings-field">
                <label>
                  {r.app} <span className="muted small">· {relativeTime(r.started_at)}</span>
                  {r.audio_exists && (
                    <span className="muted small"> · {mb(r.audio_bytes)}</span>
                  )}
                </label>
                <p className={s.tone === "error" ? "small" : "muted small"}>{s.label}</p>
                <div className="other-row">
                  {s.action === "open-note" && r.note_path && (
                    <button
                      className="btn-add"
                      onClick={() => openNote(r.note_path as string).catch((e) => setError(String(e)))}
                    >
                      Open note
                    </button>
                  )}
                  {s.action === "retry" && (
                    <button className="btn-add" disabled={busy === r.id} onClick={() => retry(r.id)}>
                      {busy === r.id ? "Retrying…" : "Retry"}
                    </button>
                  )}
                  {r.audio_exists && (
                    <button
                      className="btn-add"
                      onClick={() => revealAudio(r.id).catch((e) => setError(String(e)))}
                    >
                      Reveal audio
                    </button>
                  )}
                </div>
              </div>
            </section>
          );
        })}
      </main>
    </div>
  );
}
