import { describe, expect, it } from "vitest";
import type { HostedModelDescriptor, HostedProviderState } from "./api";
import {
  VERTEX_UNARMED_WARNING,
  billingDisclosure,
  credentialSummary,
  defaultProviderCache,
  filterWordEntries,
  findExactModel,
  modelUnavailableReason,
  parseProviderKey,
  providerKey,
  removeWordEntry,
  replaceWordEntry,
  selectionForModel,
  splitBulkEntries,
  supportTierLabel,
} from "./hosted";

function model(over: Partial<HostedModelDescriptor> = {}): HostedModelDescriptor {
  return {
    provider: "fixture-provider",
    transport: "fixture-api",
    support_tier: "documented",
    exact_model_id: "catalog-fixture-v1",
    account_scoped_available: true,
    region: null,
    max_context_tokens: 100_000,
    max_output_tokens: 4_000,
    capabilities: {
      text_input: true,
      text_output: true,
      streaming: true,
      structured_output: true,
      explicit_prefix_cache: false,
      implicit_cache_may_apply: false,
    },
    billing_basis: "metered_estimate",
    tariff_version: null,
    deprecated: false,
    benchmarked_for_live: false,
    ...over,
  };
}

function provider(models: HostedModelDescriptor[]): HostedProviderState {
  return {
    descriptor: {
      provider: "fixture-provider",
      transport: "fixture-api",
      support_tier: "documented",
      billing_basis: "metered_estimate",
      adapter_available: true,
    },
    credential: { state: "ready", expires_at_unix_ms: null, source: "keychain" },
    models,
    service_error: null,
  };
}

describe("hosted provider and catalog truth", () => {
  it("uses backend tiers verbatim and preserves the exact Vertex warning", () => {
    expect(supportTierLabel("documented")).toBe("Documented");
    expect(supportTierLabel("experimental")).toBe("Experimental");
    expect(supportTierLabel("blocked")).toBe("Blocked");
    expect(VERTEX_UNARMED_WARNING).toBe("gcloud token isn't armed");
    expect(credentialSummary({ state: "absent" }, "vertex_api")).toMatchObject({
      label: "Unarmed",
      tone: "muted",
    });
  });

  it("never invents a model and applies the live benchmark gate", () => {
    const finalOnly = model();
    const liveReady = model({ exact_model_id: "catalog-fixture-live", benchmarked_for_live: true });
    const states = [provider([finalOnly, liveReady])];

    expect(modelUnavailableReason(finalOnly, "live")).toBe("not benchmarked for live latency");
    expect(modelUnavailableReason(finalOnly, "final")).toBeNull();
    expect(findExactModel(states, "fixture-provider", "fixture-api", "catalog-fixture-live")).toBe(
      liveReady,
    );
    expect(findExactModel(states, "fixture-provider", "fixture-api", "not-in-catalog")).toBeNull();
  });

  it("derives selections only from an exact catalog descriptor and discloses cache behavior", () => {
    const implicit = model({
      provider: "google",
      transport: "vertex_api",
      exact_model_id: "catalog-fixture-vertex",
      capabilities: {
        ...model().capabilities,
        implicit_cache_may_apply: true,
      },
    });
    expect(defaultProviderCache(implicit)).toBe("unavoidable_implicit");
    expect(selectionForModel(implicit)).toEqual({
      provider: "google",
      transport: "vertex_api",
      model: "catalog-fixture-vertex",
      local_cache: "reusable",
      provider_cache: "unavoidable_implicit",
    });
  });

  it("never renders unknown or subscription cost as zero", () => {
    expect(billingDisclosure("included_subscription", null)).toBe(
      "Included subscription · no dollar amount",
    );
    expect(billingDisclosure("no_provider_request", null)).toBe(
      "Local cache · no provider request",
    );
    expect(billingDisclosure("unknown", null)).toContain("not $0.00");
    expect(billingDisclosure("metered_estimate", null)).toContain("estimate unavailable");
  });

  it("round-trips provider selector values without delimiter assumptions", () => {
    const encoded = providerKey("fixture:provider", "fixture/transport");
    expect(parseProviderKey(encoded)).toEqual({
      provider: "fixture:provider",
      transport: "fixture/transport",
    });
    expect(parseProviderKey("not-json")).toBeNull();
  });
});

describe("word-bank editor operations", () => {
  it("supports bulk lines, search, edit, and remove without client-side canonical claims", () => {
    expect(splitBulkEntries("  Corti\n\nParakeet\r\n Vagus ")).toEqual([
      "Corti",
      "Parakeet",
      "Vagus",
    ]);
    const entries = ["Corti", "Parakeet", "Vagus"];
    expect(filterWordEntries(entries, "keet")).toEqual(["Parakeet"]);
    expect(replaceWordEntry(entries, "Parakeet", "Parakeet TDT")).toEqual([
      "Corti",
      "Parakeet TDT",
      "Vagus",
    ]);
    expect(removeWordEntry(entries, "Corti")).toEqual(["Parakeet", "Vagus"]);
  });
});
