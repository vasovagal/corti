// The Recording Queue's state-label mapping: one pure function from a RecordingDto to what the row
// shows and which action it offers. Kept free of React/Tauri so the whole printer-queue truth table
// is unit-testable (see queue.test.ts).

import type { RecordingDto } from "./api";

export type RowAction = "open-note" | "retry" | null;

export interface RowState {
  /** The status line, e.g. "Transcribed 55 min in 30 s" or "Could not transcribe 30 min call: …". */
  label: string;
  /** Visual tone: progress (spinner-ish), ok, info (the "Filed in brain" chip), error. */
  tone: "progress" | "ok" | "info" | "error";
  action: RowAction;
}

/** "55 min", "1 h 05 min", "30 s" — coarse human duration. */
export function fmtDuration(secs: number | null): string {
  if (secs === null || !Number.isFinite(secs) || secs < 0) return "";
  if (secs < 60) return `${Math.round(secs)} s`;
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins} min`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m > 0 ? `${h} h ${String(m).padStart(2, "0")} min` : `${h} h`;
}

export function rowState(r: RecordingDto): RowState {
  const dur = fmtDuration(r.duration_secs);
  const call = `${dur ? `${dur} ` : ""}${r.mode === "webinar" ? "webinar" : "call"}`;
  switch (r.status) {
    case "recording":
      return { label: "Recording…", tone: "progress", action: null };
    case "pending_transcription":
      // An error + an active retry job = a transient failure backing off, not a terminal one.
      if (r.retry_pending && r.error) {
        const attempt = r.retry_attempts ? ` (attempt ${r.retry_attempts}/5)` : "";
        return { label: `Will retry${attempt}: ${r.error}`, tone: "progress", action: null };
      }
      return { label: "Queued", tone: "progress", action: null };
    case "transcribing":
      return { label: "Transcribing…", tone: "progress", action: null };
    case "pending_note":
      return { label: "Filing note…", tone: "progress", action: null };
    case "done": {
      if (r.note_path && r.note_exists) {
        const took = r.transcribe_secs !== null ? ` in ${fmtDuration(r.transcribe_secs)}` : "";
        return { label: `Transcribed ${call}${took}`, tone: "ok", action: "open-note" };
      }
      if (r.note_path && !r.note_exists) {
        // vagus reorganized the note out of the inbox — the work is done, the link just moved on.
        return { label: "Filed in brain", tone: "info", action: null };
      }
      return { label: "Done", tone: "ok", action: null };
    }
    case "failed":
      if (r.audio_exists) {
        return {
          label: `Could not transcribe ${call}: ${r.error ?? "unknown error"}`,
          tone: "error",
          action: "retry",
        };
      }
      return { label: "Failed (audio expired)", tone: "error", action: null };
    default:
      return { label: r.status, tone: "info", action: null };
  }
}
