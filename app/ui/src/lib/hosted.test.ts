import { describe, expect, it } from "vitest";
import type { HostedModelDescriptor, HostedProviderState, HostedSettingsDto } from "./api";
import { AWS_REGIONS, BEDROCK_REGIONS, regionOptions } from "./awsRegions";
import {
  VERTEX_UNARMED_WARNING,
  awsCredentialModeDescription,
  awsCredentialModeLabel,
  bedrockCredentialGuidance,
  bedrockCredentialReady,
  bedrockInvalidMessage,
  bedrockModeRequirements,
  bedrockSetupChanged,
  bedrockRefreshFailureGuidance,
  bedrockSetupStatusLabel,
  billingDisclosure,
  credentialSummary,
  defaultProviderCache,
  deriveBedrockSetupStatus,
  errorLabel,
  filterWordEntries,
  findExactModel,
  hostedErrorGuidance,
  hostedOnboardingGuidance,
  laneConfigurationGuidance,
  modelAdvisory,
  modelUnavailableReason,
  normalizeBedrockSetup,
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

function settings(): HostedSettingsDto {
  const selection = {
    provider: null,
    transport: null,
    model: null,
    cache_policy: { local: "reusable" as const, provider: "off" as const },
  };
  return {
    state_revision: 1,
    preferences_revision: 1,
    control: {
      process_epoch: 1,
      session_generation: 1,
      control_revision: 1,
      steering_revision: 1,
      bank_revision: 1,
      pinned_question_revision: 0,
      master_enabled: false,
      egress_acknowledged: false,
      pinned_auto_enabled: false,
      live: { enabled: false, revision: 1, selection: { ...selection } },
      final_lane: { enabled: false, revision: 1, selection: { ...selection } },
      questions: { enabled: false, revision: 1, selection: { ...selection } },
    },
    providers: [provider([model()])],
    scopes: [],
    bedrock: {
      mode: "default_chain",
      profile: null,
      role_arn: null,
      has_access_key_id: false,
      has_secret_access_key: false,
      has_session_token: false,
    },
    vertex_models: [],
    default_steering: "",
    word_bank: { revision: 1, entries: [] },
    final_deadline_seconds: 90,
    show_history_diagnostics: false,
    show_live_metrics_by_default: false,
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

  it("never invents a model and treats live benchmarks as advice rather than a gate", () => {
    const unmeasured = model();
    const measured = model({ exact_model_id: "catalog-fixture-live", benchmarked_for_live: true });
    const states = [provider([unmeasured, measured])];

    expect(modelUnavailableReason(unmeasured, "live")).toBeNull();
    expect(modelAdvisory(unmeasured, "live")).toContain("raw text wins");
    expect(modelAdvisory(measured, "live")).toBeNull();
    expect(findExactModel(states, "fixture-provider", "fixture-api", "catalog-fixture-live")).toBe(
      measured,
    );
    expect(findExactModel(states, "fixture-provider", "fixture-api", "not-in-catalog")).toBeNull();
  });

  it("routes each incomplete onboarding state to an actionable Preferences section", () => {
    const noProvider = settings();
    noProvider.providers = [{ ...noProvider.providers[0], credential: { state: "absent" }, models: [] }];
    expect(hostedOnboardingGuidance(noProvider)).toMatchObject({ section: "hosted-provider" });

    const noLane = settings();
    expect(hostedOnboardingGuidance(noLane)).toMatchObject({ section: "hosted-routing" });
    expect(laneConfigurationGuidance(noLane, "question")).toMatchObject({
      section: "hosted-routing",
    });

    const ready = settings();
    ready.control.final_lane = {
      enabled: true,
      revision: 2,
      selection: {
        provider: "fixture-provider",
        transport: "fixture-api",
        model: "catalog-fixture-v1",
        cache_policy: { local: "reusable", provider: "off" },
      },
    };
    expect(hostedOnboardingGuidance(ready)).toMatchObject({ section: "hosted" });
    ready.control.egress_acknowledged = true;
    ready.control.master_enabled = true;
    expect(hostedOnboardingGuidance(ready)).toBeNull();
  });

  it("turns typed hosted failures into safe repair advice", () => {
    expect(hostedErrorGuidance("policy_blocked")).toMatchObject({
      section: "hosted-routing",
    });
    expect(hostedErrorGuidance("auth_rejected")).toMatchObject({
      section: "hosted-provider",
    });
    expect(hostedErrorGuidance("malformed_output").message).toContain("raw transcript");
    expect(errorLabel("policy_blocked")).toContain("setup");
    expect(errorLabel("policy_blocked")).not.toBe("policy blocked");
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

  it("uses method-specific Bedrock rejection guidance and mentions SSO login only for SSO", () => {
    const modes = ["default_chain", "profile", "static_keychain", "assume_role", "sso"] as const;
    for (const mode of modes) {
      const guidance = bedrockCredentialGuidance(mode, { state: "rejected" }, "work");
      expect(guidance).not.toBeNull();
      if (mode === "sso") expect(guidance).toContain("aws sso login --profile work");
      else expect(guidance).not.toContain("sso login");
    }
    expect(credentialSummary({ state: "rejected" }, "bedrock_runtime").detail).toContain(
      "method-specific",
    );
    expect(credentialSummary({ state: "refreshing" }, "bedrock_runtime").detail).toContain("renewed");
    expect(credentialSummary({ state: "absent" }, "bedrock_runtime").detail).toContain("mode");

    expect(
      bedrockCredentialGuidance(
        "sso",
        { state: "error", code: "network" },
        "work",
      ),
    ).toContain("network");
    expect(
      bedrockCredentialGuidance(
        "sso",
        { state: "error", code: "network" },
        "work",
      ),
    ).not.toContain("sso login");
    expect(bedrockRefreshFailureGuidance("profile", "timeout", "work")).toContain("timed out");
    expect(bedrockRefreshFailureGuidance("sso", "auth_rejected", "work")).toContain(
      "aws sso login --profile work",
    );
  });

  it("labels and describes each credential mode", () => {
    for (const mode of ["default_chain", "profile", "static_keychain", "assume_role", "sso"] as const) {
      expect(awsCredentialModeLabel(mode).length).toBeGreaterThan(0);
      expect(awsCredentialModeDescription(mode).length).toBeGreaterThan(0);
    }
    expect(awsCredentialModeLabel("assume_role")).toBe("Assume role");
  });

  it("validates the complete atomic draft for every credential mode", () => {
    const ready = {
      mode: "default_chain" as const,
      profile: "work",
      roleArn: "arn:aws:iam::123456789012:role/corti",
      region: "us-east-1",
      setupName: "Clinical Bedrock",
    };
    const profiles = ["work"];
    const keys = { hasAccessKeyId: true, hasSecretAccessKey: true };

    for (const mode of ["default_chain", "profile", "static_keychain", "assume_role", "sso"] as const) {
      expect(bedrockModeRequirements({ ...ready, mode }, profiles, keys)).toEqual([]);
    }

    expect(
      bedrockModeRequirements({ ...ready, mode: "profile", profile: "" }, profiles, keys),
    ).toContainEqual(expect.objectContaining({ field: "profile", reason: "required" }));
    expect(
      bedrockModeRequirements({ ...ready, mode: "sso", profile: "retired" }, profiles, keys),
    ).toContainEqual(expect.objectContaining({ field: "profile", reason: "not_found" }));
    expect(
      bedrockModeRequirements({ ...ready, mode: "sso", profile: "retired" }, null, keys),
    ).not.toContainEqual(expect.objectContaining({ field: "profile", reason: "not_found" }));
    expect(
      bedrockModeRequirements({ ...ready, mode: "assume_role", roleArn: "" }, profiles, keys),
    ).toContainEqual(expect.objectContaining({ field: "role_arn", reason: "required" }));
    expect(
      bedrockModeRequirements(
        { ...ready, mode: "assume_role", roleArn: "arn:aws:iam::123456789012:user/x" },
        profiles,
        keys,
      ),
    ).toContainEqual(expect.objectContaining({ field: "role_arn", reason: "invalid" }));
    expect(
      bedrockModeRequirements(
        { ...ready, mode: "static_keychain" },
        profiles,
        { hasAccessKeyId: false, hasSecretAccessKey: false },
      ),
    ).toContainEqual(expect.objectContaining({ field: "key_pair", reason: "keys_missing" }));
    expect(
      bedrockModeRequirements({ ...ready, region: "", setupName: "" }, profiles, keys),
    ).toEqual([
      expect.objectContaining({ field: "region", reason: "required" }),
      expect.objectContaining({ field: "setup_name", reason: "required" }),
    ]);
  });

  it("normalizes one payload and derives pristine, dirty, saved-not-ready, and ready states", () => {
    const saved = {
      mode: "profile" as const,
      profile: "work",
      roleArn: "hidden and ignored",
      region: "us-east-1",
      setupName: "Clinical Bedrock",
    };
    expect(normalizeBedrockSetup({ ...saved, setupName: "  Clinical Bedrock  " })).toEqual({
      mode: "profile",
      profile: "work",
      roleArn: null,
      region: "us-east-1",
      setupName: "Clinical Bedrock",
    });
    expect(bedrockSetupChanged({ ...saved, setupName: " Clinical Bedrock " }, saved)).toBe(false);
    expect(bedrockSetupChanged({ ...saved, region: "us-west-2" }, saved)).toBe(true);

    const readyCredential = {
      state: "ready" as const,
      expires_at_unix_ms: null,
      source: "aws_profile" as const,
    };
    expect(
      deriveBedrockSetupStatus({
        changed: true,
        issues: [],
        scopeConfigured: true,
        credential: readyCredential,
      }),
    ).toBe("unsaved_changes");
    expect(
      deriveBedrockSetupStatus({
        changed: false,
        issues: [{ field: "setup_name", reason: "required", message: "Enter a setup name." }],
        scopeConfigured: true,
        credential: readyCredential,
      }),
    ).toBe("saved_not_ready");
    expect(
      deriveBedrockSetupStatus({
        changed: false,
        issues: [],
        scopeConfigured: true,
        credential: { state: "rejected" },
      }),
    ).toBe("saved_not_ready");
    expect(
      deriveBedrockSetupStatus({
        changed: false,
        issues: [],
        scopeConfigured: true,
        credential: readyCredential,
        nowUnixMs: 1_000,
      }),
    ).toBe("ready");
    const expiringCredential = {
      ...readyCredential,
      expires_at_unix_ms: 60_000,
    };
    expect(bedrockCredentialReady(expiringCredential, 1)).toBe(false);
    expect(
      deriveBedrockSetupStatus({
        changed: false,
        issues: [],
        scopeConfigured: true,
        credential: expiringCredential,
        nowUnixMs: 1,
      }),
    ).toBe("saved_not_ready");
    expect(bedrockSetupStatusLabel("saved_not_ready")).toBe("Saved—not ready");
  });

  it("maps backend validation to field-local copy", () => {
    expect(bedrockInvalidMessage("profile", "not_found")).toContain("currently available");
    expect(bedrockInvalidMessage("key_pair", "keys_missing")).toContain("both");
    expect(bedrockInvalidMessage("setup_name", "required")).toBe("Enter a setup name.");
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
