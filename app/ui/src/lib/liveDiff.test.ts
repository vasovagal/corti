import { describe, expect, it } from "vitest";
import { diffLiveText, tokenizeLiveText } from "./liveDiff";

describe("live token diff", () => {
  it("keeps Unicode words, punctuation, and whitespace lossless", () => {
    const value = "Café ☕ — ship it, please.\n";
    expect(tokenizeLiveText(value).join("")).toBe(value);
  });

  it("emits stable word and punctuation replacement spans", () => {
    const spans = diffLiveText(
      "We shipped teh fixture, today.",
      "We shipped the fixture today!",
    );
    expect(spans.map((span) => [span.kind, span.text])).toEqual([
      ["equal", "We shipped "],
      ["delete", "teh"],
      ["insert", "the"],
      ["equal", " fixture"],
      ["delete", ","],
      ["equal", " today"],
      ["delete", "."],
      ["insert", "!"],
    ]);
    expect(spans.filter((span) => span.kind !== "insert").map((span) => span.text).join("")).toBe(
      "We shipped teh fixture, today.",
    );
    expect(spans.filter((span) => span.kind !== "delete").map((span) => span.text).join("")).toBe(
      "We shipped the fixture today!",
    );
  });

  it("handles insertions, deletions, emoji, and empty rows", () => {
    expect(diffLiveText("", "Ready ✅")).toEqual([{ kind: "insert", text: "Ready ✅" }]);
    expect(diffLiveText("remove me", "")).toEqual([{ kind: "delete", text: "remove me" }]);
    expect(diffLiveText("same", "same")).toEqual([{ kind: "equal", text: "same" }]);
  });

  it("uses a bounded fallback for pathological rows", () => {
    const raw = Array.from({ length: 500 }, (_, index) => `raw-${index}`).join(" ");
    const clean = Array.from({ length: 500 }, (_, index) => `clean-${index}`).join(" ");
    const spans = diffLiveText(raw, clean);
    expect(spans.some((span) => span.kind === "delete")).toBe(true);
    expect(spans.some((span) => span.kind === "insert")).toBe(true);
  });
});
