import type {
  HostedBillingBasis,
  HostedCredentialState,
  HostedErrorCode,
  HostedLane,
  HostedLocalCacheMode,
  HostedModelDescriptor,
  HostedProviderCacheMode,
  HostedProviderState,
  HostedSelectionInput,
  HostedSupportTier,
} from "./api";

/** The backend contract requires this warning to remain undecorated and exact. */
export const VERTEX_UNARMED_WARNING = "gcloud token isn't armed";

export interface ProviderPresentation {
  name: string;
  shortName: string;
  auth: string;
}

const PRESENTATION: Record<string, ProviderPresentation> = {
  vertex_api: {
    name: "Google Vertex direct API",
    shortName: "Vertex",
    auth: "Application Default Credentials (ADC)",
  },
  openai_api: {
    name: "OpenAI direct API",
    shortName: "OpenAI API",
    auth: "API key in macOS Keychain or workload identity",
  },
  codex_app_server: {
    name: "Codex app-server",
    shortName: "Codex",
    auth: "Broker-owned device code and OS keyring",
  },
  anthropic_api: {
    name: "Anthropic direct API",
    shortName: "Anthropic API",
    auth: "API key in macOS Keychain or workload identity",
  },
  claude_subscription: {
    name: "Claude subscription (Free / Pro / Max)",
    shortName: "Claude subscription",
    auth: "No credential import permitted",
  },
};

export function providerPresentation(provider: string, transport: string): ProviderPresentation {
  return (
    PRESENTATION[transport] ?? {
      name: `${provider} · ${transport}`,
      shortName: provider,
      auth: "Backend-managed credential",
    }
  );
}

export function supportTierLabel(tier: HostedSupportTier): string {
  switch (tier) {
    case "documented":
      return "Documented";
    case "experimental":
      return "Experimental";
    case "blocked":
      return "Blocked";
  }
}

export function errorLabel(code: HostedErrorCode): string {
  return code.replace(/_/gu, " ");
}

export interface CredentialSummary {
  label: string;
  detail: string;
  tone: "ok" | "caution" | "error" | "muted";
}

function credentialSourceLabel(source: Extract<HostedCredentialState, { state: "ready" }>["source"]): string {
  switch (source) {
    case "keychain":
      return "macOS Keychain";
    case "workload_identity":
      return "workload identity";
    case "application_default_credentials":
      return "Application Default Credentials";
    case "broker_keyring":
      return "broker-owned OS keyring";
  }
}

export function credentialSummary(
  credential: HostedCredentialState,
  transport: string,
): CredentialSummary {
  const vertex = transport === "vertex_api";
  const codex = transport === "codex_app_server";
  switch (credential.state) {
    case "absent":
      return {
        label: vertex ? "Unarmed" : codex ? "Disconnected" : "No credential",
        detail: vertex
          ? "No memory-only ADC access token is ready."
          : "No backend-managed credential is ready.",
        tone: "muted",
      };
    case "resolving":
      return {
        label: vertex ? "Arming" : "Resolving",
        detail: vertex ? "Resolving ADC without blocking transcript capture." : "Checking credential state.",
        tone: "caution",
      };
    case "ready":
      return {
        label: vertex ? "Armed · token only" : codex ? "Broker ready" : "Ready",
        detail: `Credential source: ${credentialSourceLabel(credential.source)}.`,
        tone: "ok",
      };
    case "awaiting_user":
      return {
        label: "Waiting for authorization",
        detail: "The backend is waiting for the user-owned authorization step.",
        tone: "caution",
      };
    case "device_authorization":
      return {
        label: "Device authorization pending",
        detail: "The local broker returned display-only verification details; no token enters React.",
        tone: "caution",
      };
    case "refreshing":
      return {
        label: "Refreshing",
        detail: "The existing credential is being refreshed in its backend owner.",
        tone: "caution",
      };
    case "rejected":
      return {
        label: "Rejected",
        detail: "Authentication was rejected. Service readiness is checked separately.",
        tone: "error",
      };
    case "unsupported":
      return {
        label: "Unsupported",
        detail: `Unavailable: ${errorLabel(credential.code)}.`,
        tone: "muted",
      };
    case "error":
      return {
        label: "Credential error",
        detail: errorLabel(credential.code),
        tone: "error",
      };
  }
}

export function providerKey(provider: string, transport: string): string {
  return JSON.stringify([provider, transport]);
}

export function parseProviderKey(value: string): { provider: string; transport: string } | null {
  try {
    const parsed: unknown = JSON.parse(value);
    if (
      Array.isArray(parsed) &&
      parsed.length === 2 &&
      parsed.every((part) => typeof part === "string" && part.length > 0)
    ) {
      return { provider: parsed[0], transport: parsed[1] };
    }
  } catch {
    // A malformed DOM value is treated as no selection; it never reaches an IPC command.
  }
  return null;
}

export function modelUnavailableReason(model: HostedModelDescriptor, lane: HostedLane): string | null {
  if (model.support_tier === "blocked") return "blocked by provider policy";
  if (!model.account_scoped_available) return "not available to this account or region";
  if (model.deprecated) return "deprecated";
  if (!model.capabilities.text_input || !model.capabilities.text_output) {
    return "text input/output capability is missing";
  }
  if (!model.capabilities.structured_output) return "structured output is not supported";
  if (lane === "live" && !model.benchmarked_for_live) return "not benchmarked for live latency";
  return null;
}

export function modelsForProvider(
  providers: HostedProviderState[],
  provider: string,
  transport: string,
): HostedModelDescriptor[] {
  return (
    providers.find(
      (state) =>
        state.descriptor.provider === provider && state.descriptor.transport === transport,
    )?.models ?? []
  );
}

export function findExactModel(
  providers: HostedProviderState[],
  provider: string | null,
  transport: string | null,
  model: string | null,
): HostedModelDescriptor | null {
  if (!provider || !transport || !model) return null;
  return (
    modelsForProvider(providers, provider, transport).find(
      (candidate) => candidate.exact_model_id === model,
    ) ?? null
  );
}

export function defaultProviderCache(model: HostedModelDescriptor): HostedProviderCacheMode {
  if (model.transport === "codex_app_server") return "unavailable";
  if (model.capabilities.implicit_cache_may_apply) return "unavoidable_implicit";
  return "off";
}

export function selectionForModel(
  model: HostedModelDescriptor,
  localCache: HostedLocalCacheMode = "reusable",
  providerCache: HostedProviderCacheMode = defaultProviderCache(model),
): HostedSelectionInput {
  return {
    provider: model.provider,
    transport: model.transport,
    model: model.exact_model_id,
    local_cache: localCache,
    provider_cache: providerCache,
  };
}

export function emptySelection(localCache: HostedLocalCacheMode = "reusable"): HostedSelectionInput {
  return {
    provider: null,
    transport: null,
    model: null,
    local_cache: localCache,
    provider_cache: "off",
  };
}

export function billingDisclosure(basis: HostedBillingBasis, tariffVersion: string | null): string {
  switch (basis) {
    case "metered_estimate":
      return tariffVersion
        ? `Metered API estimate · tariff ${tariffVersion}`
        : "Metered API · estimate unavailable until a reviewed tariff matches";
    case "included_subscription":
      return "Included subscription · no dollar amount";
    case "no_provider_request":
      return "Local cache · no provider request";
    case "unknown":
      return "Cost unknown · not $0.00";
  }
}

export function splitBulkEntries(value: string): string[] {
  return value
    .split(/\r?\n/u)
    .map((entry) => entry.trim())
    .filter(Boolean);
}

export function replaceWordEntry(entries: string[], existing: string, replacement: string): string[] {
  return entries.map((entry) => (entry === existing ? replacement : entry));
}

export function removeWordEntry(entries: string[], target: string): string[] {
  return entries.filter((entry) => entry !== target);
}

export function filterWordEntries(entries: string[], query: string): string[] {
  const needle = query.trim().normalize("NFC").toLocaleLowerCase();
  if (!needle) return entries;
  return entries.filter((entry) => entry.normalize("NFC").toLocaleLowerCase().includes(needle));
}
