import type {
  LiveTranscriptEvent,
  LiveTranscriptLine,
  LiveTranscriptSnapshot,
} from "./api";

/** Apply an open-late snapshot without letting an older invoke response overwrite a newer event. */
export function applyLiveSnapshot(
  current: LiveTranscriptSnapshot | null,
  incoming: LiveTranscriptSnapshot,
): LiveTranscriptSnapshot {
  if (current && incoming.revision < current.revision) {
    // Subscribe-before-snapshot race: the invoke may have cloned revision N just before event N+1 arrived.
    // Merge that older baseline with the newer delta instead of either losing history or regressing metadata.
    if (current.session_id !== incoming.session_id) return current;
    return {
      ...current,
      lines: orderedUnique(
        [...incoming.lines, ...current.lines],
        current.retained_from_seq,
      ),
    };
  }
  return { ...incoming, lines: orderedUnique(incoming.lines, incoming.retained_from_seq) };
}

/** Apply a small Tauri delta. Revision + session ids make subscribe-before-snapshot races harmless. */
export function applyLiveEvent(
  current: LiveTranscriptSnapshot | null,
  event: LiveTranscriptEvent,
): LiveTranscriptSnapshot {
  if (current && event.revision <= current.revision) return current;
  const sameSession = current?.session_id === event.session_id;
  const prior = event.reset || !sameSession ? [] : (current?.lines ?? []);
  const lines = event.line === null ? prior : [...prior, event.line];
  return {
    revision: event.revision,
    session_id: event.session_id,
    mode: event.mode,
    status: event.status,
    title: event.title,
    detail: event.detail,
    active: event.active,
    evicted_lines: event.evicted_lines,
    retained_from_seq: event.retained_from_seq,
    lines: orderedUnique(lines, event.retained_from_seq),
  };
}

function orderedUnique(lines: LiveTranscriptLine[], retainedFrom: number): LiveTranscriptLine[] {
  const bySeq = new Map<number, LiveTranscriptLine>();
  for (const line of lines) {
    if (line.seq >= retainedFrom) bySeq.set(line.seq, line);
  }
  return [...bySeq.values()].sort(
    (a, b) => a.start_sec - b.start_sec || a.end_sec - b.end_sec || a.seq - b.seq,
  );
}

export function formatLiveTimestamp(seconds: number): string {
  const total = Number.isFinite(seconds) ? Math.max(0, Math.floor(seconds)) : 0;
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, "0")}:${String(secs).padStart(2, "0")}`
    : `${String(minutes).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
}

export function formatLiveRange(line: LiveTranscriptLine): string {
  const start = formatLiveTimestamp(line.start_sec);
  const end = formatLiveTimestamp(line.end_sec);
  return start === end ? start : `${start}–${end}`;
}
