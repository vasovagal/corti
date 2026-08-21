import type {
  LiveTranscriptEvent,
  LiveTranscriptLine,
  LiveTranscriptSnapshot,
} from "./api";

export type LiveEventOutcome = "applied" | "duplicate" | "gap" | "process_change";

export interface LiveEventReduction {
  snapshot: LiveTranscriptSnapshot | null;
  outcome: LiveEventOutcome;
}

/** Apply an open-late snapshot without letting an older invoke response overwrite a newer event. */
export function applyLiveSnapshot(
  current: LiveTranscriptSnapshot | null,
  incoming: LiveTranscriptSnapshot,
): LiveTranscriptSnapshot {
  const processChanged =
    current?.process_epoch !== undefined &&
    incoming.process_epoch !== undefined &&
    current.process_epoch !== incoming.process_epoch;
  if (processChanged) return normalizeSnapshot(incoming);

  if (current && incoming.revision < current.revision) {
    // Subscribe-before-snapshot race: the invoke may have cloned revision N just before event N+1 arrived.
    // Merge that older baseline with the newer delta instead of either losing history or regressing metadata.
    if (current.session_id !== incoming.session_id) return current;
    return {
      ...current,
      lines: orderedUnique([...incoming.lines, ...current.lines], current.retained_from_seq),
    };
  }
  return normalizeSnapshot(incoming);
}

/**
 * Reduce one delta only when its revision edge is contiguous. A caller must refetch on `gap` or
 * `process_change`; the last snapshot (and therefore every immutable raw row) is deliberately retained.
 */
export function reduceLiveEvent(
  current: LiveTranscriptSnapshot | null,
  event: LiveTranscriptEvent,
): LiveEventReduction {
  if (!current) return { snapshot: snapshotFromEvent(null, event), outcome: "applied" };

  if (
    current.process_epoch !== undefined &&
    event.process_epoch !== undefined &&
    current.process_epoch !== event.process_epoch
  ) {
    return { snapshot: current, outcome: "process_change" };
  }

  if (event.revision <= current.revision) {
    return { snapshot: current, outcome: "duplicate" };
  }

  const sameSession = current.session_id === event.session_id;
  const sameGeneration =
    current.session_generation === undefined ||
    event.session_generation === undefined ||
    current.session_generation === event.session_generation;
  const expectedFrom = event.from_revision ?? event.revision - 1;
  const contiguous = expectedFrom === current.revision;

  if (!sameSession || !sameGeneration) {
    if (!event.reset || !contiguous) return { snapshot: current, outcome: "gap" };
  } else if (!contiguous) {
    return { snapshot: current, outcome: "gap" };
  }

  return { snapshot: snapshotFromEvent(current, event), outcome: "applied" };
}

/** Backwards-compatible convenience for callers that do not need to inspect repair outcomes. */
export function applyLiveEvent(
  current: LiveTranscriptSnapshot | null,
  event: LiveTranscriptEvent,
): LiveTranscriptSnapshot {
  return reduceLiveEvent(current, event).snapshot ?? snapshotFromEvent(null, event);
}

function normalizeSnapshot(snapshot: LiveTranscriptSnapshot): LiveTranscriptSnapshot {
  return {
    ...snapshot,
    lines: orderedUnique(snapshot.lines, snapshot.retained_from_seq),
  };
}

function snapshotFromEvent(
  current: LiveTranscriptSnapshot | null,
  event: LiveTranscriptEvent,
): LiveTranscriptSnapshot {
  const sameSession =
    current?.session_id === event.session_id &&
    (current.session_generation === undefined ||
      event.session_generation === undefined ||
      current.session_generation === event.session_generation);
  const prior = event.reset || !sameSession ? [] : (current?.lines ?? []);
  const lines = event.line === null ? prior : [...prior, event.line];
  return {
    protocol_version: event.protocol_version ?? current?.protocol_version,
    process_epoch: event.process_epoch ?? current?.process_epoch,
    session_generation: event.session_generation ?? current?.session_generation,
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
    if (line.seq < retainedFrom) continue;
    const existing = bySeq.get(line.seq);
    bySeq.set(line.seq, existing ? mergeLine(existing, line) : { ...line });
  }
  return [...bySeq.values()].sort(
    (a, b) => a.start_sec - b.start_sec || a.end_sec - b.end_sec || a.seq - b.seq,
  );
}

/** A clean upsert may add presentation fields, but it can never mutate the raw row identity or text. */
function mergeLine(existing: LiveTranscriptLine, incoming: LiveTranscriptLine): LiveTranscriptLine {
  return {
    ...existing,
    ...incoming,
    row_id: existing.row_id ?? incoming.row_id,
    speaker: existing.speaker,
    start_sec: existing.start_sec,
    end_sec: existing.end_sec,
    text: existing.text,
    clean_text:
      incoming.clean_text !== undefined ? incoming.clean_text : existing.clean_text,
    rewrite_state:
      incoming.rewrite_state !== undefined ? incoming.rewrite_state : existing.rewrite_state,
    commit_epoch:
      incoming.commit_epoch !== undefined ? incoming.commit_epoch : existing.commit_epoch,
  };
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
