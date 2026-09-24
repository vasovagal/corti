import { describe, expect, it } from "vitest";
import type { HostedAssistantSubscription } from "./api";
import {
  displayedAnswer,
  newSubscriptionId,
  orderAssistantCards,
  parseNameHints,
  presetDefaults,
  splitAnswerLines,
  subscriptionProblem,
  subscriptionSetProblem,
} from "./subscriptions";

function card(over: Partial<HostedAssistantSubscription> = {}): HostedAssistantSubscription {
  return {
    id: "summary",
    title: "Running summary",
    preset: "running_summary",
    format: "bullets",
    enabled: true,
    run_count: 1,
    in_flight: false,
    pending: false,
    exchange: null,
    ...over,
  };
}

describe("question subscriptions", () => {
  it("mirrors the backend preset defaults", () => {
    const asked = presetDefaults("asked_of_me", "asked-of-me");
    expect(asked.trigger.on_speakers).toBe("them");
    expect(asked.trigger.quiet_ms).toBe(1_000);
    expect(asked.context).toEqual({ window: "last_minutes", minutes: 5, rows: 0 });
    expect(asked.output).toBe("bullets");
    const custom = presetDefaults("none", "question");
    expect(custom.output).toBe("paragraph");
    expect(custom.trigger.min_new_words).toBe(40);
  });

  it("mints non-colliding ids and validates like the backend", () => {
    expect(newSubscriptionId("asked_of_me", [])).toBe("asked-of-me");
    expect(newSubscriptionId("asked_of_me", ["asked-of-me"])).toBe("asked-of-me-2");
    expect(newSubscriptionId("none", ["question", "question-2"])).toBe("question-3");

    expect(subscriptionProblem(presetDefaults("running_summary", "summary"))).toBeNull();
    expect(subscriptionProblem(presetDefaults("none", "question"))).toMatch(/template/u);
    expect(subscriptionProblem({ ...presetDefaults("none", "Bad Id"), template: "x" })).toMatch(/Id/u);
    const twice = [presetDefaults("running_summary", "summary"), presetDefaults("topic_watch", "summary")];
    expect(subscriptionSetProblem(twice)).toMatch(/Duplicate/u);
    expect(subscriptionSetProblem([])).toBeNull();
  });

  it("puts asked-of-me first and splits bullet answers", () => {
    const ordered = orderAssistantCards([
      card({ id: "summary", preset: "running_summary" }),
      card({ id: "asked", preset: "asked_of_me" }),
    ]);
    expect(ordered.map((item) => item.id)).toEqual(["asked", "summary"]);

    expect(splitAnswerLines("- one\n- two\n3. three")).toEqual({
      bullets: true,
      lines: ["one", "two", "three"],
    });
    expect(splitAnswerLines("Just a paragraph.\nAnd another.")).toEqual({
      bullets: false,
      lines: ["Just a paragraph.", "And another."],
    });
  });

  it("shows the partial answer while running and the previous one before that", () => {
    const base = {
      call_id: "c-1",
      as_of_revision: 4,
      status: "running" as const,
      error: null,
      question: "q",
      answer: null,
      cost_label: null,
      format: "bullets",
      partial_answer: null,
      previous_answer: null,
    };
    expect(displayedAnswer(card({ exchange: base }))).toEqual({ text: null, kind: "none" });
    expect(displayedAnswer(card({ exchange: { ...base, previous_answer: "old" } }))).toEqual({
      text: "old",
      kind: "previous",
    });
    expect(
      displayedAnswer(card({ exchange: { ...base, previous_answer: "old", partial_answer: "- new" } })),
    ).toEqual({ text: "- new", kind: "partial" });
    expect(
      displayedAnswer(card({ exchange: { ...base, status: "completed", answer: "- done", previous_answer: "old" } })),
    ).toEqual({ text: "- done", kind: "answer" });
  });

  it("parses name hints from commas or lines", () => {
    expect(parseNameHints("Xavier, Xav\n X-ray ")).toEqual(["Xavier", "Xav", "X-ray"]);
    expect(parseNameHints("")).toEqual([]);
  });
});
