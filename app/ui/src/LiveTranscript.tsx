import { useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  getLiveTranscript,
  onLiveTranscriptChanged,
  startLiveTest,
  stopLiveTest,
  type LiveTranscriptSnapshot,
} from "./lib/api";
import {
  applyLiveEvent,
  applyLiveSnapshot,
  formatLiveRange,
} from "./lib/liveTranscript";

export default function LiveTranscript() {
  const [snapshot, setSnapshot] = useState<LiveTranscriptSnapshot | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);
  const follow = useRef(true);

  useEffect(() => {
    document.title = "Live Transcript — Corti";
    let cancelled = false;
    const refresh = () =>
      getLiveTranscript()
        .then((incoming) => {
          if (!cancelled) setSnapshot((current) => applyLiveSnapshot(current, incoming));
        })
        .catch((reason) => {
          if (!cancelled) setError(String(reason));
        });

    // Subscribe first: a line emitted while the initial snapshot invoke is in flight cannot be missed.
    const unlisten = onLiveTranscriptChanged((event) => {
      if (!cancelled) setSnapshot((current) => applyLiveEvent(current, event));
    });
    refresh();
    const reconciliation = setInterval(refresh, 30_000);
    return () => {
      cancelled = true;
      clearInterval(reconciliation);
      unlisten.then((remove) => remove()).catch(() => {});
    };
  }, []);

  useLayoutEffect(() => {
    const node = scroller.current;
    if (node && follow.current) node.scrollTop = node.scrollHeight;
  }, [snapshot?.revision]);

  async function startTest() {
    setBusy(true);
    setError("");
    try {
      await startLiveTest();
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function stopTest() {
    setBusy(true);
    setError("");
    try {
      await stopLiveTest();
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }

  const mode = snapshot?.mode ?? "idle";
  const status = snapshot?.status ?? "idle";
  const canStart = mode !== "call" && !snapshot?.active && status !== "stopping";
  const canStop = mode === "test" && snapshot?.active;
  const rows = snapshot?.lines ?? [];

  return (
    <div className="app live-app">
      <header className="app-header live-header">
        <div>
          <h1>{snapshot?.title ?? "Live transcript"}</h1>
          <p className="subtitle" aria-live="polite">
            <span className={`live-status live-status-${status}`} />
            {snapshot?.detail ?? "Start a microphone test or join a call."}
          </p>
        </div>
        <div className="live-actions">
          {canStart && (
            <button className="btn-add" disabled={busy} onClick={startTest}>
              {busy ? "Starting…" : "Test microphone"}
            </button>
          )}
          {canStop && (
            <button className="btn-add live-stop" disabled={busy || status === "stopping"} onClick={stopTest}>
              {status === "stopping" ? "Stopping…" : "Stop test"}
            </button>
          )}
        </div>
      </header>

      {mode === "test" && (
        <p className="callout small">
          Test mode listens only to your default microphone. It does not save audio, file a note, or add a
          recording to the queue. Automatic call detection resumes when you stop the test.
        </p>
      )}
      {error && <p className="live-error">{error}</p>}
      {(snapshot?.evicted_lines ?? 0) > 0 && (
        <p className="muted small live-trimmed">
          {snapshot?.evicted_lines.toLocaleString()} earlier line(s) omitted from this bounded live view. The
          durable call note is unaffected.
        </p>
      )}

      <div
        className="live-scroll"
        ref={scroller}
        onScroll={(event) => {
          const node = event.currentTarget;
          follow.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
        }}
      >
        {rows.length === 0 ? (
          <div className="live-empty">
            <p>{emptyMessage(status)}</p>
            {status === "listening" && <p className="muted small">Rows appear after you pause at the end of a phrase.</p>}
          </div>
        ) : (
          <ol className="live-lines">
            {rows.map((line) => (
              <li className={`live-line live-line-${line.speaker === "Me" ? "me" : "them"}`} key={line.seq}>
                <div className="live-line-meta">
                  <time>{formatLiveRange(line)}</time>
                  <strong>{line.speaker}</strong>
                </div>
                <p>{line.text}</p>
              </li>
            ))}
          </ol>
        )}
      </div>
    </div>
  );
}

function emptyMessage(status: LiveTranscriptSnapshot["status"]): string {
  switch (status) {
    case "loading":
      return "Preparing live transcription…";
    case "listening":
      return "Listening for speech…";
    case "stopping":
      return "Flushing the final speech region…";
    case "unavailable":
    case "error":
      return "No live transcript is available.";
    case "complete":
      return "The session completed without recognized speech.";
    default:
      return "Join a call, or run a microphone transcription test.";
  }
}
