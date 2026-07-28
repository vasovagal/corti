import { describe, expect, it } from "vitest";
import type { RecordingDto } from "./api";
import { fmtDuration, rowState } from "./queue";

// A happy-path Done row; tests override the fields under scrutiny.
function dto(over: Partial<RecordingDto>): RecordingDto {
  return {
    id: "20260609-140500-zoom",
    app: "Zoom",
    mode: "call",
    started_at: "2026-06-09T14:05:00Z",
    ended_at: "2026-06-09T15:00:00Z",
    duration_secs: 55 * 60,
    status: "done",
    error: null,
    transcribe_secs: 30,
    note_path: "/brain/00-Inbox/zoom.md",
    note_exists: true,
    audio_exists: true,
    recovery_exists: true,
    audio_bytes: 22_000_000,
    retry_pending: false,
    retry_attempts: null,
    ...over,
  };
}

describe("rowState — the printer-queue truth table", () => {
  it("shows live progress states", () => {
    expect(rowState(dto({ status: "recording" }))).toMatchObject({
      label: "Recording…",
      tone: "progress",
    });
    expect(rowState(dto({ status: "transcribing" }))).toMatchObject({
      label: "Transcribing…",
    });
    expect(rowState(dto({ status: "pending_note" }))).toMatchObject({
      label: "Filing note…",
    });
    expect(rowState(dto({ status: "pending_transcription" }))).toMatchObject({
      label: "Queued",
    });
  });

  it("marks a backing-off transcription failure as will-retry, not failed", () => {
    const s = rowState(
      dto({
        status: "pending_transcription",
        error: "transcription failed: connection reset",
        retry_pending: true,
        retry_attempts: 2,
      }),
    );
    expect(s.label).toBe("Will retry (attempt 2/5): transcription failed: connection reset");
    expect(s.tone).toBe("progress");
    expect(s.action).toBeNull();
  });

  it("keeps a backing-off filing failure at the durable PendingNote checkpoint", () => {
    const s = rowState(
      dto({
        status: "pending_note",
        error: "vagus unavailable",
        retry_pending: true,
        retry_attempts: 1,
      }),
    );
    expect(s.label).toBe("Will retry filing (attempt 1/5): vagus unavailable");
    expect(s.tone).toBe("progress");
    expect(s.action).toBeNull();
  });

  it("celebrates a filed note with duration + transcription time and an open action", () => {
    const s = rowState(dto({}));
    expect(s.label).toBe("Transcribed 55 min call in 30 s");
    expect(s.tone).toBe("ok");
    expect(s.action).toBe("open-note");
  });

  it("shows 'Filed in brain' (info, no dead link) once vagus moves the note out of the inbox", () => {
    const s = rowState(dto({ note_exists: false }));
    expect(s.label).toBe("Filed in brain");
    expect(s.tone).toBe("info");
    expect(s.action).toBeNull();
  });

  it("offers Retry on a failure while the audio survives", () => {
    const s = rowState(
      dto({ status: "failed", error: "gave up after 5 attempts: no models", note_path: null }),
    );
    expect(s.label).toBe("Could not transcribe 55 min call: gave up after 5 attempts: no models");
    expect(s.tone).toBe("error");
    expect(s.action).toBe("retry");
  });

  it("offers Retry from a checkpoint after raw audio expires", () => {
    const s = rowState(
      dto({ status: "failed", error: "filing failed", audio_exists: false, recovery_exists: true }),
    );
    expect(s.label).toContain("Could not transcribe");
    expect(s.action).toBe("retry");
  });

  it("withdraws Retry once every recovery input expired", () => {
    const s = rowState(
      dto({ status: "failed", error: "x", audio_exists: false, recovery_exists: false }),
    );
    expect(s.label).toBe("Failed (recovery expired)");
    expect(s.action).toBeNull();
  });

  it("labels webinars as webinars", () => {
    const s = rowState(dto({ mode: "webinar", duration_secs: 30 * 60 }));
    expect(s.label).toBe("Transcribed 30 min webinar in 30 s");
  });
});

describe("fmtDuration", () => {
  it("rounds sensibly across scales", () => {
    expect(fmtDuration(30)).toBe("30 s");
    expect(fmtDuration(55 * 60)).toBe("55 min");
    expect(fmtDuration(65 * 60)).toBe("1 h 05 min");
    expect(fmtDuration(120 * 60)).toBe("2 h");
    expect(fmtDuration(null)).toBe("");
  });
});
