import { describe, expect, it } from "vitest";
import type { LiveTranscriptEvent, LiveTranscriptSnapshot } from "./api";
import {
  applyLiveEvent,
  applyLiveSnapshot,
  reduceLiveEvent,
  formatLiveRange,
  formatLiveTimestamp,
} from "./liveTranscript";

function snapshot(over: Partial<LiveTranscriptSnapshot> = {}): LiveTranscriptSnapshot {
  return {
    revision: 1,
    session_id: "call-a",
    mode: "call",
    status: "listening",
    title: "Zoom",
    detail: "Listening",
    active: true,
    evicted_lines: 0,
    retained_from_seq: 1,
    lines: [],
    ...over,
  };
}

function event(over: Partial<LiveTranscriptEvent> = {}): LiveTranscriptEvent {
  return {
    revision: 2,
    session_id: "call-a",
    mode: "call",
    status: "listening",
    title: "Zoom",
    detail: "Listening",
    active: true,
    evicted_lines: 0,
    retained_from_seq: 1,
    reset: false,
    line: { seq: 1, speaker: "Me", start_sec: 2, end_sec: 3, text: "hello" },
    ...over,
  };
}

describe("live transcript race/retention reducer", () => {
  it("merges a stale open-late baseline under the newer event without regressing", () => {
    const current = applyLiveEvent(
      null,
      event({
        revision: 5,
        line: { seq: 2, speaker: "Me", start_sec: 2, end_sec: 3, text: "new delta" },
      }),
    );
    const merged = applyLiveSnapshot(
      current,
      snapshot({
        revision: 4,
        lines: [{ seq: 1, speaker: "Them", start_sec: 0, end_sec: 1, text: "baseline" }],
      }),
    );
    expect(merged.revision).toBe(5);
    expect(merged.lines.map((line) => line.text)).toEqual(["baseline", "new delta"]);
  });

  it("deduplicates rows and ignores duplicate/out-of-order revisions", () => {
    const first = applyLiveEvent(null, event());
    const duplicate = applyLiveEvent(first, event({ revision: 2 }));
    expect(duplicate).toBe(first);
    const next = applyLiveEvent(
      duplicate,
      event({
        revision: 3,
        line: { seq: 1, speaker: "Me", start_sec: 2, end_sec: 3, text: "hello" },
      }),
    );
    expect(next.lines).toHaveLength(1);
  });

  it("resets on a new session and obeys the server's eviction floor", () => {
    const old = snapshot({
      revision: 5,
      lines: [
        { seq: 2, speaker: "Them", start_sec: 2, end_sec: 3, text: "old" },
        { seq: 3, speaker: "Me", start_sec: 4, end_sec: 5, text: "kept" },
      ],
    });
    const trimmed = applyLiveEvent(
      old,
      event({ revision: 6, retained_from_seq: 3, line: null }),
    );
    expect(trimmed.lines.map((line) => line.seq)).toEqual([3]);

    const reset = applyLiveEvent(
      trimmed,
      event({
        revision: 7,
        session_id: "test-b",
        mode: "test",
        reset: true,
        line: null,
      }),
    );
    expect(reset.lines).toEqual([]);
    expect(reset.session_id).toBe("test-b");
  });

  it("orders independently recognized Me/Them rows by call-relative timestamp", () => {
    const current = snapshot({
      revision: 8,
      lines: [{ seq: 8, speaker: "Me", start_sec: 8, end_sec: 9, text: "later" }],
    });
    const next = applyLiveEvent(
      current,
      event({
        revision: 9,
        line: { seq: 9, speaker: "Them", start_sec: 4, end_sec: 5, text: "earlier" },
      }),
    );
    expect(next.lines.map((line) => line.text)).toEqual(["earlier", "later"]);
  });

  it("retains raw rows and requests repair on revision gaps or process changes", () => {
    const current = snapshot({
      process_epoch: 10,
      revision: 8,
      lines: [{ seq: 8, speaker: "Me", start_sec: 1, end_sec: 2, text: "raw retained" }],
    });
    const gap = reduceLiveEvent(current, event({ revision: 10, process_epoch: 10 }));
    expect(gap.outcome).toBe("gap");
    expect(gap.snapshot).toBe(current);
    expect(gap.snapshot?.lines[0].text).toBe("raw retained");

    const process = reduceLiveEvent(current, event({ revision: 1, process_epoch: 11 }));
    expect(process.outcome).toBe("process_change");
    expect(process.snapshot).toBe(current);
  });

  it("uses from_revision when supplied and never lets a clean upsert mutate immutable raw text", () => {
    const current = snapshot({
      revision: 8,
      lines: [
        {
          seq: 1,
          row_id: "row-1",
          speaker: "Me",
          start_sec: 1,
          end_sec: 2,
          text: "immutable raw",
        },
      ],
    });
    const reduced = reduceLiveEvent(
      current,
      event({
        from_revision: 8,
        revision: 10,
        line: {
          seq: 1,
          row_id: "row-1",
          speaker: "Changed speaker",
          start_sec: 9,
          end_sec: 10,
          text: "attempted replacement",
          clean_text: "accepted clean",
          rewrite_state: "clean",
          commit_epoch: 10,
        },
      }),
    );
    expect(reduced.outcome).toBe("applied");
    expect(reduced.snapshot?.lines[0]).toMatchObject({
      speaker: "Me",
      start_sec: 1,
      end_sec: 2,
      text: "immutable raw",
      clean_text: "accepted clean",
      commit_epoch: 10,
    });
  });
});

describe("live transcript timestamp formatting", () => {
  it("covers minutes, hours, invalid values, and ranges", () => {
    expect(formatLiveTimestamp(65.9)).toBe("01:05");
    expect(formatLiveTimestamp(3_661)).toBe("1:01:01");
    expect(formatLiveTimestamp(Number.NaN)).toBe("00:00");
    expect(
      formatLiveRange({ seq: 1, speaker: "Me", start_sec: 65, end_sec: 67, text: "x" }),
    ).toBe("01:05–01:07");
  });
});
