import { describe, expect, it } from "vitest";
import type { HostedModelDescriptor, HostedProviderState } from "./api";
import { AWS_REGIONS, BEDROCK_REGIONS, regionOptions } from "./awsRegions";
import {
  VERTEX_UNARMED_WARNING,
  awsCredentialModeDescription,
  awsCredentialModeLabel,
  bedrockModeRequirements,
  billingDisclosure,
  credentialSummary,
  defaultProviderCache,
  filterWordEntries,
  findExactModel,
  modelUnavailableReason,
  parseProviderKey,
  providerKey,
  providerPresentation,
  sessionExpiryLabel,
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

  it("presents native ChatGPT device auth as subscription access, not an API key or server", () => {
    const presentation = providerPresentation("openai", "chatgpt_subscription");
    expect(presentation.name).toBe("ChatGPT subscription");
    expect(presentation.guidance).toContain("No Codex server");
    expect(credentialSummary({ state: "absent" }, "chatgpt_subscription").label).toBe(
      "Not signed in",
    );
    expect(
      credentialSummary(
        { state: "ready", expires_at_unix_ms: 10_000, source: "chat_gpt_device" },
        "chatgpt_subscription",
      ),
    ).toMatchObject({ label: "Signed in", tone: "ok" });
    expect(
      credentialSummary({ state: "error", code: "cache" }, "chatgpt_subscription"),
    ).toMatchObject({ label: "Sign-in not saved", tone: "error" });
    expect(
      defaultProviderCache(
        model({
          provider: "openai",
          transport: "chatgpt_subscription",
          billing_basis: "included_subscription",
        }),
      ),
    ).toBe("unavailable");
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

describe("bedrock credential helpers", () => {
  it("names every AWS credential source without leaking account detail", () => {
    const sources = [
      "aws_default_chain",
      "aws_profile",
      "aws_static_keychain",
      "aws_assumed_role",
      "aws_sso",
    ] as const;
    for (const source of sources) {
      const summary = credentialSummary(
        { state: "ready", expires_at_unix_ms: null, source },
        "bedrock_runtime",
      );
      expect(summary.tone).toBe("ok");
      expect(summary.detail).not.toContain("arn:");
      expect(summary.detail).not.toContain("undefined");
    }
  });

  it("explains a Bedrock rejection as an expired session rather than a generic failure", () => {
    expect(credentialSummary({ state: "rejected" }, "bedrock_runtime").detail).toContain("SSO");
    expect(credentialSummary({ state: "refreshing" }, "bedrock_runtime").detail).toContain("renewed");
    expect(credentialSummary({ state: "absent" }, "bedrock_runtime").detail).toContain("mode");
    // Other transports keep their existing wording.
    expect(credentialSummary({ state: "rejected" }, "openai_api").detail).not.toContain("SSO");
  });

  it("labels and describes each credential mode", () => {
    for (const mode of ["default_chain", "profile", "static_keychain", "assume_role", "sso"] as const) {
      expect(awsCredentialModeLabel(mode).length).toBeGreaterThan(0);
      expect(awsCredentialModeDescription(mode).length).toBeGreaterThan(0);
    }
    expect(awsCredentialModeLabel("assume_role")).toBe("Assume role");
  });

  it("requires each mode's own companion field before it can be saved", () => {
    const ready = { profile: "work", roleArn: "arn:aws:iam::1:role/x", region: "us-east-1" };
    const keys = { hasAccessKeyId: true, hasSecretAccessKey: true };

    expect(bedrockModeRequirements("default_chain", ready, keys)).toEqual([]);
    expect(bedrockModeRequirements("profile", ready, keys)).toEqual([]);
    expect(bedrockModeRequirements("assume_role", ready, keys)).toEqual([]);
    expect(bedrockModeRequirements("static_keychain", ready, keys)).toEqual([]);

    expect(bedrockModeRequirements("profile", { ...ready, profile: "  " }, keys)).toContain(
      "a profile name",
    );
    expect(bedrockModeRequirements("sso", { ...ready, profile: "" }, keys)).toContain(
      "a profile name",
    );
    expect(bedrockModeRequirements("assume_role", { ...ready, roleArn: "" }, keys)).toContain(
      "a role ARN",
    );
    expect(
      bedrockModeRequirements("assume_role", { ...ready, roleArn: "arn:aws:iam::1:user/x" }, keys),
    ).toContain("a valid IAM role ARN");
    expect(
      bedrockModeRequirements("static_keychain", ready, {
        hasAccessKeyId: false,
        hasSecretAccessKey: false,
      }),
    ).toEqual(["an access key ID", "a secret access key"]);
    // The connection is regional in every mode.
    expect(bedrockModeRequirements("default_chain", { ...ready, region: "" }, keys)).toEqual([
      "a Bedrock region",
    ]);
  });

  it("counts down an assumed-role session and reports a lapsed one", () => {
    const now = 1_787_000_000_000;
    expect(sessionExpiryLabel(null, now)).toBeNull();
    expect(sessionExpiryLabel(undefined, now)).toBeNull();
    expect(sessionExpiryLabel(now - 1, now)).toBe("Session expired");
    expect(sessionExpiryLabel(now + 30_000, now)).toBe("Session renews in under a minute");
    expect(sessionExpiryLabel(now + 25 * 60_000, now)).toBe("Session renews in 25 min");
    expect(sessionExpiryLabel(now + 120 * 60_000, now)).toBe("Session renews in 2 h");
    expect(sessionExpiryLabel(now + 95 * 60_000, now)).toBe("Session renews in 1 h 35 min");
  });

  it("presents Bedrock as its own transport", () => {
    const presentation = providerPresentation("amazon", "bedrock_runtime");
    expect(presentation.name).toBe("Amazon Bedrock");
    expect(presentation.shortName).toBe("Bedrock");
    expect(presentation.auth).toContain("SSO");
  });
});

describe("aws region lists", () => {
  it("offers a narrower Bedrock list and injects an unknown current value", () => {
    // Every Bedrock region must be a real AWS region, but not every Transcribe region has Bedrock.
    expect(BEDROCK_REGIONS.length).toBeLessThan(AWS_REGIONS.length);
    for (const region of ["us-west-1", "ca-central-1", "eu-north-1", "ap-northeast-2"]) {
      expect(AWS_REGIONS).toContain(region);
      expect(BEDROCK_REGIONS).not.toContain(region);
    }

    expect(regionOptions(BEDROCK_REGIONS, "us-east-1")).toEqual(BEDROCK_REGIONS);
    expect(regionOptions(BEDROCK_REGIONS, null)).toEqual(BEDROCK_REGIONS);
    expect(regionOptions(BEDROCK_REGIONS, "us-gov-west-1")[0]).toBe("us-gov-west-1");
  });
});
