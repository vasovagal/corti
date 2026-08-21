import type {
  HostedAccountingEvent,
  HostedAssistantExchange,
  HostedCacheObservation,
  HostedCallLane,
  HostedCostEstimate,
  HostedLaneState,
  HostedLatencyFields,
  HostedNormalizedUsage,
  HostedTerminalEvent,
} from "./api";

export const MAX_VISIBLE_CALL_DETAILS = 12;
export const MAX_ASSISTANT_EXCHANGES = 20;
export const MAX_ASSISTANT_BYTES = 256 * 1024;

export interface LiveCallDetail {
  call_id: string;
  lane: HostedCallLane;
  finality: "provisional" | "final";
  usage: HostedNormalizedUsage;
  cost: HostedCostEstimate;
  late: boolean;
  provider?: string;
  transport?: string;
  model?: string;
  cache?: HostedCacheObservation;
  latency?: HostedLatencyFields;
  outcome?: HostedTerminalEvent["outcome"];
  error?: HostedTerminalEvent["error"];
  provider_request_sent?: boolean;
  late_content_discarded?: boolean;
  completed_at_unix_ms?: number;
}

export interface HostedCallContext {
  sessionId: string | null;
  sessionGeneration: number | null;
}

/** Content-free event reducer. Final accounting wins over late provisional events for the same call. */
export function applyHostedCallEvent(
  current: LiveCallDetail[],
  event: HostedAccountingEvent | HostedTerminalEvent,
  context: HostedCallContext,
): LiveCallDetail[] {
  const existing = current.find((call) => call.call_id === event.call_id);
  if (event.event === "accounting") {
    if (existing?.finality === "final" && event.finality === "provisional") return current;
    const next: LiveCallDetail = {
      ...existing,
      call_id: event.call_id,
      lane: event.lane,
      finality: event.finality,
      usage: event.usage,
      cost: event.cost,
      late: event.late,
    };
    return prependBounded(current, next);
  }

  if (context.sessionId && event.recording_id !== context.sessionId) return current;
  if (
    context.sessionGeneration !== null &&
    event.fence.session_generation !== context.sessionGeneration
  ) {
    return current;
  }
  const next: LiveCallDetail = {
    ...existing,
    call_id: event.call_id,
    lane: event.lane,
    finality: "final",
    usage: event.usage,
    cost: event.cost,
    late: event.late_content_discarded,
    provider: event.provider,
    transport: event.transport,
    model: event.model,
    cache: event.cache,
    latency: event.latency,
    outcome: event.outcome,
    error: event.error,
    provider_request_sent: event.provider_request_sent,
    late_content_discarded: event.late_content_discarded,
    completed_at_unix_ms: event.completed_at_unix_ms,
  };
  return prependBounded(current, next);
}

function prependBounded(current: LiveCallDetail[], next: LiveCallDetail): LiveCallDetail[] {
  return [next, ...current.filter((call) => call.call_id !== next.call_id)].slice(
    0,
    MAX_VISIBLE_CALL_DETAILS,
  );
}

/** Keep the newest exchanges while independently enforcing item and UTF-8 content bounds. */
export function boundAssistantExchanges(
  exchanges: HostedAssistantExchange[],
  maxItems = MAX_ASSISTANT_EXCHANGES,
  maxBytes = MAX_ASSISTANT_BYTES,
): HostedAssistantExchange[] {
  const encoder = new TextEncoder();
  const kept: HostedAssistantExchange[] = [];
  let bytes = 0;
  for (let index = exchanges.length - 1; index >= 0 && kept.length < maxItems; index -= 1) {
    const exchange = exchanges[index];
    const size =
      encoder.encode(exchange.question).byteLength +
      encoder.encode(exchange.answer ?? "").byteLength;
    if (bytes + size > maxBytes) break;
    kept.push(exchange);
    bytes += size;
  }
  return kept.reverse();
}

export function formatLatencyMicros(microseconds: number | null | undefined): string {
  if (microseconds === null || microseconds === undefined || !Number.isFinite(microseconds)) return "—";
  const value = Math.max(0, microseconds);
  if (value < 1_000) return `${Math.round(value)}µs`;
  if (value < 1_000_000) {
    const milliseconds = value / 1_000;
    return `${Number.isInteger(milliseconds) ? milliseconds.toFixed(0) : milliseconds.toFixed(1)}ms`;
  }
  const seconds = value / 1_000_000;
  return `${seconds >= 10 ? seconds.toFixed(0) : seconds.toFixed(1)}s`;
}

export function latencyEntries(latency: HostedLatencyFields): Array<[string, string]> {
  const fields: Array<[string, keyof HostedLatencyFields]> = [
    ["Queue", "queue_us"],
    ["Auth", "auth_us"],
    ["Cache", "cache_lookup_us"],
    ["Connect", "connect_us"],
    ["TTFB", "ttfb_us"],
    ["TTFT", "ttft_us"],
    ["Stream", "stream_us"],
    ["Parse", "parse_us"],
    ["Cache commit", "cache_commit_us"],
    ["Total", "total_us"],
  ];
  return fields
    .filter(([, key]) => latency[key] !== null && latency[key] !== undefined)
    .map(([label, key]) => [label, formatLatencyMicros(latency[key])]);
}

export function tokenEntries(usage: HostedNormalizedUsage): Array<[string, string]> {
  const values: Array<[string, number | null]> = [
    ["Input", usage.input_tokens],
    ["Output", usage.output_tokens],
    ["Cache read", usage.cached_read_tokens],
    ["Cache write", usage.cached_write_tokens],
    ["Reasoning", usage.reasoning_tokens],
  ];
  return values
    .filter(([, value]) => value !== null)
    .map(([label, value]) => [label, (value ?? 0).toLocaleString("en-US")]);
}

/** Mirrors Rust's truthful one-call labels; null and subscription values are never coerced to zero. */
export function formatHostedCost(cost: HostedCostEstimate): string {
  switch (cost.billing_basis) {
    case "included_subscription":
      return "Included subscription · cost unavailable";
    case "no_provider_request":
      return "Local cache · no provider request";
    case "unknown":
      return "Cost unavailable";
    case "metered_estimate": {
      if (cost.cost_micros === null || cost.currency === null) return "Cost unavailable";
      const amount = formatCurrencyMicros(cost.cost_micros);
      return cost.currency === "USD"
        ? `Estimated $${amount}`
        : `Estimated ${cost.currency} ${amount}`;
    }
  }
}

function formatCurrencyMicros(micros: number): string {
  const safe = Math.max(0, Math.trunc(micros));
  const whole = Math.floor(safe / 1_000_000);
  let fractional = String(safe % 1_000_000).padStart(6, "0");
  while (fractional.endsWith("0") && fractional.length > 4) fractional = fractional.slice(0, -1);
  return `${whole}.${fractional}`;
}

export function cacheObservationLabel(cache: HostedCacheObservation | undefined): string {
  switch (cache) {
    case "local":
      return "Local exact cache";
    case "provider_read":
      return "Provider cache read";
    case "provider_write":
      return "Provider cache write";
    case "provider_implicit":
      return "Provider implicit cache";
    case "none":
    case undefined:
      return "No cache observed";
  }
}

export function callLaneLabel(lane: HostedCallLane): string {
  switch (lane) {
    case "live":
      return "Live";
    case "final":
      return "Final";
    case "ad_hoc_question":
      return "Question";
    case "pinned_question":
      return "Pinned";
  }
}

export function laneStateLabel(state: HostedLaneState): string {
  switch (state) {
    case "disabled":
      return "Disabled";
    case "waiting_for_phrase":
      return "Waiting for phrase";
    case "debouncing":
    case "queued":
      return "Queued";
    case "arming":
      return "Arming";
    case "catching_up":
      return "Catching up";
    case "rewriting":
      return "Rewriting";
    case "finalizing":
      return "Finalizing";
    case "clean":
      return "Clean";
    case "using_raw":
      return "Using raw";
    case "failed":
      return "Failed";
  }
}

export function questionStatusLabel(status: HostedAssistantExchange["status"]): string {
  switch (status) {
    case "queued":
      return "Queued";
    case "waiting_for_credential":
      return "Waiting for provider";
    case "running":
      return "Running";
    case "completed":
      return "Answered";
    case "canceled":
      return "Canceled";
    case "failed":
      return "Failed";
  }
}
