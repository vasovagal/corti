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
  type ConsoleEntry,
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
