import {
  useState,
  useEffect,
  useRef,
  useCallback,
  useMemo,
  type ReactNode,
  type CSSProperties,
} from "react";
import {
  getConsoleLogs,
  getConsoleLogsText,
  saveConsoleLogs,
  getStats,
  type ConsoleEntry,
  type StatsSnapshot,
  type StageSample,
} from "./lib/api";

const LEVELS = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] as const;
type Level = (typeof LEVELS)[number];

// Per-level line color, drawn from the shared CSS variables so it tracks light/dark mode.
function levelColor(level: string): string {
  switch (level) {
    case "ERROR":
      return "var(--warn-fg)";
    case "WARN":
      return "var(--caution-fg)";
    case "DEBUG":
    case "TRACE":
      return "var(--fg-muted)";
    default:
      return "var(--fg)";
  }
}

// The toggle-button colors for an enabled level. Disabled levels fall back to a muted style inline.
function levelToggleStyle(level: Level, enabled: boolean): CSSProperties {
  if (!enabled) {
    return { background: "var(--bg-soft)", color: "var(--fg-muted)", opacity: 0.5 };
  }
  switch (level) {
    case "ERROR":
      return { background: "var(--warn-bg)", color: "var(--warn-fg)" };
    case "WARN":
      return { background: "var(--caution-bg)", color: "var(--caution-fg)" };
    case "INFO":
      return { background: "var(--ok-bg)", color: "var(--ok-fg)" };
    default:
      return { background: "var(--bg-soft)", color: "var(--fg)" };
  }
}

// Split `text` into plain segments and <mark> highlights for each case-insensitive `query` match.
function highlightMatch(text: string, query: string): (string | ReactNode)[] {
  if (!query) return [text];
  const lower = text.toLowerCase();
  const q = query.toLowerCase();
  const parts: (string | ReactNode)[] = [];
  let last = 0;
  let idx = lower.indexOf(q, last);
  while (idx !== -1) {
    if (idx > last) parts.push(text.slice(last, idx));
    parts.push(
      <mark
        key={idx}
        style={{ background: "var(--caution-bg)", color: "var(--caution-fg)", borderRadius: 3 }}
      >
        {text.slice(idx, idx + query.length)}
      </mark>,
    );
    last = idx + query.length;
    idx = lower.indexOf(q, last);
  }
  if (last < text.length) parts.push(text.slice(last));
  return parts;
}

export default function Console() {
  const [entries, setEntries] = useState<ConsoleEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState("");
  const [enabledLevels, setEnabledLevels] = useState<Set<string>>(() => new Set(LEVELS));
  const [copied, setCopied] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const [stats, setStats] = useState<StatsSnapshot | null>(null);
  const [stages, setStages] = useState<StageSample[]>([]);
  const logRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    document.title = "Diagnostics — Corti";
  }, []);

  const fetchLogs = useCallback(async () => {
    setLoading(true);
    try {
      setEntries(await getConsoleLogs());
    } catch {
      // If fetch fails, keep existing entries.
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchLogs();
  }, [fetchLogs]);

  // Poll for new logs every 500ms while the window is open.
  useEffect(() => {
    const id = setInterval(async () => {
      try {
        const latest = await getConsoleLogs();
        // Skip the re-render only when nothing actually changed. Length alone is a bad signal: once the ring
        // buffer is full it evicts FIFO, so length stays pinned at the cap while the tail churns — comparing
        // the last entry's timestamp too keeps a busy console live instead of freezing.
        setEntries((prev) =>
          latest.length === prev.length &&
          latest[latest.length - 1]?.timestamp === prev[prev.length - 1]?.timestamp
            ? prev
            : latest,
        );
      } catch {
        // Ignore poll failures.
      }
    }, 500);
    return () => clearInterval(id);
  }, []);

  // Poll the dedicated stats buffer at 1 Hz (the sampler runs at 1 Hz). Keeps the newest memory/thread
  // snapshot plus the process-global stage timings (returned once, not duplicated per snapshot).
  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const report = await getStats();
        if (alive) {
          setStats(report.history[report.history.length - 1] ?? null);
          setStages(report.stages);
        }
      } catch {
        // Ignore poll failures; keep the last snapshot.
      }
    };
    tick(); // prime immediately
    const id = setInterval(tick, 1000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Auto-scroll to the bottom when new entries arrive, unless the user scrolled up.
  useEffect(() => {
    if (autoScroll && logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [entries, autoScroll]);

  // Pause auto-scroll when the user scrolls away from the bottom; resume when near it again.
  const handleScroll = useCallback(() => {
    if (!logRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = logRef.current;
    setAutoScroll(scrollHeight - scrollTop - clientHeight < 40);
  }, []);

  // Cmd/Ctrl+F focuses the search input.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "f") {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  const filtered = useMemo(() => {
    const q = search.toLowerCase();
    return entries.filter((e) => {
      if (!enabledLevels.has(e.level)) return false;
      if (q) {
        const line = `${e.timestamp} ${e.level} ${e.target}: ${e.message}`;
        if (!line.toLowerCase().includes(q)) return false;
      }
      return true;
    });
  }, [entries, search, enabledLevels]);

  const toggleLevel = (level: string) => {
    setEnabledLevels((prev) => {
      const next = new Set(prev);
      if (next.has(level)) next.delete(level);
      else next.add(level);
      return next;
    });
  };

  const handleCopy = async () => {
    try {
      const text = await getConsoleLogsText();
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard API may fail in some contexts.
    }
  };

  const handleSave = async () => {
    try {
      await saveConsoleLogs();
    } catch {
      // Save may fail or be cancelled.
    }
  };

  // Threads to show: always the named corti-* threads (the meaningful ones), then the hottest few of the
  // 30+ OS/WebKit/CoreAudio/ORT threads — sorted by CPU desc so the busy ones surface instead of kernel
  // tid order. Caps the table height without hiding the pipeline/detect/stats rows even when idle.
  const shownThreads = stats
    ? [
        ...stats.threads
          .filter((t) => t.name.startsWith("corti-"))
          .sort((a, b) => b.cpu_pct - a.cpu_pct),
        ...stats.threads
          .filter((t) => !t.name.startsWith("corti-"))
          .sort((a, b) => b.cpu_pct - a.cpu_pct)
          .slice(0, 8),
      ]
    : [];

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100vh",
        background: "var(--bg)",
        color: "var(--fg)",
      }}
    >
      {/* Header: title + entry count + actions */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 12,
          padding: "14px 20px",
          borderBottom: "1px solid var(--border)",
        }}
      >
        <h1 style={{ margin: 0, fontSize: 18 }}>Diagnostics</h1>
        <span className="muted small">{entries.length} entries</span>
        <div style={{ marginLeft: "auto", display: "flex", gap: 8 }}>
          <button className="btn-add" onClick={fetchLogs}>
            Refresh
          </button>
          <button className="btn-add" onClick={handleCopy}>
            {copied ? "Copied!" : "Copy"}
          </button>
          <button className="btn-add" onClick={handleSave}>
            Save As…
          </button>
        </div>
      </div>

      {stats && (
        <div
          style={{
            padding: "12px 20px",
            borderBottom: "1px solid var(--border)",
            display: "flex",
            gap: 24,
            flexWrap: "wrap",
            alignItems: "flex-start",
          }}
        >
          <div style={{ minWidth: 160 }}>
            <div
              className="muted small"
              style={{ textTransform: "uppercase", letterSpacing: "0.03em" }}
            >
              Resources
            </div>
            <div
              style={{
                marginTop: 6,
                fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                fontSize: 13,
              }}
            >
              <div>phys {stats.phys_mb.toFixed(1)} MiB</div>
              <div>rss&nbsp; {stats.rss_mb.toFixed(1)} MiB</div>
              <div className="muted">backend: {stats.backend}</div>
              {(stats.detector_recording || stats.webinar_recording) && (
                <div className="muted">
                  recording: {stats.detector_recording ? "detector" : ""}
                  {stats.detector_recording && stats.webinar_recording ? " + " : ""}
                  {stats.webinar_recording ? "webinar" : ""}
                </div>
              )}
            </div>
          </div>

          {shownThreads.length > 0 && (
            <table className="jtable" style={{ width: "auto", minWidth: 200 }}>
              <thead>
                <tr>
                  <th>Thread</th>
                  <th style={{ textAlign: "right" }}>CPU % (avg)</th>
                </tr>
              </thead>
              <tbody>
                {shownThreads.map((t, i) => (
                  <tr key={t.name || `thread-${i}`}>
                    <td>{t.name || "(unnamed)"}</td>
                    <td style={{ textAlign: "right", fontVariantNumeric: "tabular-nums" }}>
                      {t.cpu_pct.toFixed(1)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}

          {stages.length > 0 && (
            <table className="jtable" style={{ width: "auto", minWidth: 260 }}>
              <thead>
                <tr>
                  <th>Stage</th>
                  <th>backend</th>
                  <th style={{ textAlign: "right" }}>ms</th>
                </tr>
              </thead>
              <tbody>
                {stages
                  .slice(-8)
                  .reverse()
                  .map((s, i) => (
                    <tr key={`${s.timestamp}-${i}`}>
                      <td>{s.stage}</td>
                      <td className="muted">{s.backend}</td>
                      <td style={{ textAlign: "right", fontVariantNumeric: "tabular-nums" }}>
                        {s.duration_ms}
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          )}
        </div>
      )}

      {/* Toolbar: search + level toggles */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 12,
          padding: "10px 20px",
          borderBottom: "1px solid var(--border)",
        }}
      >
        <input
          ref={searchRef}
          type="text"
          placeholder="Search logs… (⌘F)"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="search"
          style={{ flex: 1, margin: 0 }}
        />
        <div style={{ display: "flex", gap: 4 }}>
          {LEVELS.map((level) => {
            const enabled = enabledLevels.has(level);
            return (
              <button
                key={level}
                onClick={() => toggleLevel(level)}
                style={{
                  border: "none",
                  borderRadius: 6,
                  padding: "4px 8px",
                  fontSize: 12,
                  fontWeight: 600,
                  fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                  cursor: "pointer",
                  ...levelToggleStyle(level, enabled),
                }}
              >
                {level}
              </button>
            );
          })}
        </div>
      </div>

      {/* Log viewer */}
      <div
        ref={logRef}
        onScroll={handleScroll}
        style={{
          flex: 1,
          overflow: "auto",
          padding: "12px 20px",
          fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
          fontSize: 12,
          lineHeight: 1.5,
        }}
      >
        {loading ? (
          <p className="muted" style={{ textAlign: "center", marginTop: 32 }}>
            Loading…
          </p>
        ) : filtered.length === 0 ? (
          <p className="muted" style={{ textAlign: "center", marginTop: 32 }}>
            {entries.length === 0 ? "No log entries yet." : "No matching entries."}
          </p>
        ) : (
          filtered.map((entry, i) => {
            const line = `${entry.timestamp} ${entry.level} ${entry.target}: ${entry.message}`;
            return (
              <div
                key={i}
                style={{
                  color: levelColor(entry.level),
                  whiteSpace: "pre-wrap",
                  wordBreak: "break-all",
                }}
              >
                {search ? highlightMatch(line, search) : line}
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
