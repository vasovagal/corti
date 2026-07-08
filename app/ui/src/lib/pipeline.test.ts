import { describe, expect, it } from "vitest";
import { PIPELINE_BOXES, activeBoxKeys, activeBoxKeysForActivity } from "./pipeline";

describe("activeBoxKeys", () => {
  it("idle lights nothing", () => {
    expect(activeBoxKeys("idle")).toEqual([]);
  });

  it("recording lights detect + capture together", () => {
    expect(activeBoxKeys("recording")).toEqual(["detect", "capture"]);
  });

  it("transcribing lights echo-cancel + transcribe (echo folded into the transcribe step)", () => {
    expect(activeBoxKeys("transcribing")).toEqual(["echo", "transcribe"]);
  });

  it("cancelling_echo lights the echo-cancel box only", () => {
    expect(activeBoxKeys("cancelling_echo")).toEqual(["echo"]);
  });

  it("filing lights the file box", () => {
    expect(activeBoxKeys("filing")).toEqual(["file"]);
  });

  it("an unknown stage id lights nothing", () => {
    expect(activeBoxKeys("bogus")).toEqual([]);
  });

  it("every box is reachable by at least one stage", () => {
    for (const box of PIPELINE_BOXES) {
      expect(box.stages.length).toBeGreaterThan(0);
    }
  });
});

describe("activeBoxKeysForActivity", () => {
  it("matches the stage mapping when not recording", () => {
    expect(activeBoxKeysForActivity("filing", false)).toEqual(["file"]);
    expect(activeBoxKeysForActivity("idle", false)).toEqual([]);
  });

  it("a live recording lights detect + capture regardless of stage", () => {
    // Idle stage but capture live: still pulse detect + capture.
    expect(activeBoxKeysForActivity("idle", true)).toEqual(["detect", "capture"]);
    // An older job's stage clobbered a live capture to filing: detect + capture still light, plus file.
    expect(activeBoxKeysForActivity("filing", true)).toEqual(["detect", "capture", "file"]);
  });

  it("does not double-count when the stage already lights capture", () => {
    expect(activeBoxKeysForActivity("recording", true)).toEqual(["detect", "capture"]);
  });
});
