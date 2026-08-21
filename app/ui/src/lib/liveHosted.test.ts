import { describe, expect, it } from "vitest";
import type {
  HostedAccountingEvent,
  HostedAssistantExchange,
  HostedCostEstimate,
  HostedNormalizedUsage,
  HostedTerminalEvent,
} from "./api";
import {
  MAX_VISIBLE_CALL_DETAILS,
  applyHostedCallEvent,
  boundAssistantExchanges,
  formatHostedCost,
  formatLatencyMicros,
  latencyEntries,
} from "./liveHosted";

const usage: HostedNormalizedUsage = {
  input_tokens: 120,
  output_tokens: 8,
  cached_read_tokens: 80,
  cached_write_tokens: null,
  reasoning_tokens: null,
  usage_complete: true,
};

const unknownCost: HostedCostEstimate = {
  billing_basis: "unknown",
  cost_micros: null,
  currency: null,
  pricing_catalog_version: null,
  tariff_id: null,
  tariff_effective_at_unix_ms: null,
};

function accounting(over: Partial<HostedAccountingEvent> = {}): HostedAccountingEvent {
  return {
    event: "accounting",
    call_id: "call-live-1",
    lane: "live",
    finality: "provisional",
    usage,
    cost: unknownCost,
    late: false,
    ...over,
  };
}

function terminal(over: Partial<HostedTerminalEvent> = {}): HostedTerminalEvent {
  return {
    event: "terminal",
    call_id: "call-live-1",
    recording_id: "session-a",
    request_group_id: "group-a",
    target_id: "target-a",
    lane: "live",
    attempt_no: 1,
    fence: {
      process_epoch: 7,
      session_generation: 3,
      transcript_revision: 9,
      control_revision: 2,
      lane_revision: 2,
      steering_revision: 1,
      bank_revision: 1,
      question_revision: null,
    },
    provider: "fixture-provider",
    transport: "fixture-api",
    model: "fixture-model-v1",
    support_tier: "documented",
    adapter_version: 1,
    prompt_version: 1,
    output_schema_version: 1,
    outcome: "completed",
    error: null,
    provider_request_sent: true,
    late_content_discarded: false,
    cache: "provider_read",
    usage,
    cost: {
      ...unknownCost,
      billing_basis: "metered_estimate",
      cost_micros: 1_234,
      currency: "USD",
      pricing_catalog_version: "fixture-tariff-v1",
    },
    latency: {
      queue_us: 5_000,
      auth_us: null,
      cache_lookup_us: 900,
      connect_us: 12_500,
      ttfb_us: 45_000,
      ttft_us: 80_000,
      stream_us: 250_000,
      parse_us: 1_000,
      cache_commit_us: 2_000,
      total_us: 401_000,
    },
    queued_at_unix_ms: 1,
    dispatched_at_unix_ms: 2,
    completed_at_unix_ms: 3,
    ...over,
  };
}

function exchange(index: number, bytes = 8): HostedAssistantExchange {
  return {
    call_id: `question-${index}`,
    as_of_revision: index,
    status: "completed",
    error: null,
    question: `q${index}`,
    answer: "x".repeat(bytes),
    cost_label: "Cost unavailable",
  };
}

describe("live hosted call state", () => {
  it("upgrades provisional accounting to final telemetry and ignores late provisional regressions", () => {
    const context = { sessionId: "session-a", sessionGeneration: 3 };
    const provisional = applyHostedCallEvent([], accounting(), context);
    expect(provisional[0].finality).toBe("provisional");
    const final = applyHostedCallEvent(provisional, terminal(), context);
    expect(final[0]).toMatchObject({
      finality: "final",
      model: "fixture-model-v1",
      cache: "provider_read",
      outcome: "completed",
    });
    expect(applyHostedCallEvent(final, accounting(), context)).toBe(final);
  });

  it("drops stale session/generation terminals and bounds visible calls", () => {
    const context = { sessionId: "session-a", sessionGeneration: 3 };
    const first = applyHostedCallEvent([], terminal(), context);
    expect(
      applyHostedCallEvent(first, terminal({ recording_id: "old-session", call_id: "late" }), context),
    ).toBe(first);
    expect(
      applyHostedCallEvent(
        first,
        terminal({ call_id: "late-generation", fence: { ...terminal().fence, session_generation: 2 } }),
        context,
      ),
    ).toBe(first);

    let calls = first;
    for (let index = 0; index < MAX_VISIBLE_CALL_DETAILS + 5; index += 1) {
      calls = applyHostedCallEvent(calls, accounting({ call_id: `bounded-${index}` }), context);
    }
    expect(calls).toHaveLength(MAX_VISIBLE_CALL_DETAILS);
    expect(calls[0].call_id).toBe(`bounded-${MAX_VISIBLE_CALL_DETAILS + 4}`);
  });

  it("renders compact phase timing and truthful costs", () => {
    expect(formatLatencyMicros(900)).toBe("900µs");
    expect(formatLatencyMicros(5_000)).toBe("5ms");
    expect(formatLatencyMicros(80_000)).toBe("80ms");
    expect(latencyEntries(terminal().latency)).toContainEqual(["TTFT", "80ms"]);
    expect(formatHostedCost(terminal().cost)).toBe("Estimated $0.001234");
    expect(formatHostedCost(unknownCost)).toBe("Cost unavailable");
    expect(formatHostedCost({ ...unknownCost, billing_basis: "included_subscription" })).toBe(
      "Included subscription · cost unavailable",
    );
    expect(formatHostedCost({ ...unknownCost, billing_basis: "no_provider_request" })).toBe(
      "Local cache · no provider request",
    );
  });
});

describe("assistant frontend bounds", () => {
  it("keeps the newest 20 exchanges", () => {
    const values = Array.from({ length: 25 }, (_, index) => exchange(index));
    const bounded = boundAssistantExchanges(values);
    expect(bounded).toHaveLength(20);
    expect(bounded[0].call_id).toBe("question-5");
    expect(bounded[bounded.length - 1]?.call_id).toBe("question-24");
  });

  it("strictly enforces a UTF-8 byte budget", () => {
    const bounded = boundAssistantExchanges([exchange(1, 20), exchange(2, 20), exchange(3, 20)], 20, 30);
    expect(bounded.map((item) => item.call_id)).toEqual(["question-3"]);
    expect(boundAssistantExchanges([exchange(1, 40)], 20, 30)).toEqual([]);
  });
});
