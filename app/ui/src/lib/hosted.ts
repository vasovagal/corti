import type {
  AwsCredentialMode,
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
  guidanceTitle: string;
  guidance: string;
}

const PRESENTATION: Record<string, ProviderPresentation> = {
  vertex_api: {
    name: "Google Vertex direct API",
    shortName: "Vertex",
    auth: "Application Default Credentials (ADC)",
    guidanceTitle: "For Google Cloud organizations",
    guidance: "Best when project, region, IAM, quota, and billing are already managed in Google Cloud.",
  },
  chatgpt_subscription: {
    name: "ChatGPT subscription",
    shortName: "ChatGPT",
    auth: "Corti-owned OpenAI device authorization",
    guidanceTitle: "Use your existing ChatGPT plan",
    guidance: "Signs Corti in directly and uses the included subscription quota. No Codex server or OpenAI API key is involved.",
  },
  openai_api: {
    name: "OpenAI direct API",
    shortName: "OpenAI API",
    auth: "API key in Corti's private secret store or workload identity",
    guidanceTitle: "For an OpenAI API account",
    guidance: "Uses separate API billing and data controls. A ChatGPT subscription does not include API usage.",
  },
  anthropic_api: {
    name: "Anthropic direct API",
    shortName: "Anthropic API",
    auth: "API key in Corti's private secret store or workload identity",
    guidanceTitle: "For an Anthropic API account",
    guidance: "Uses metered API billing. Claude Free, Pro, and Max subscriptions are separate products.",
  },
  claude_subscription: {
    name: "Claude subscription (Free / Pro / Max)",
    shortName: "Claude subscription",
    auth: "No credential import permitted",
    guidanceTitle: "Consumer subscription credentials are blocked",
    guidance: "Corti cannot import or route Claude subscription credentials without written provider permission.",
  },
  bedrock_runtime: {
    name: "Amazon Bedrock",
    shortName: "Bedrock",
    auth: "AWS credentials: default chain, profile, key pair, assumed role, or SSO",
    guidanceTitle: "For AWS organizations",
    guidance: "Uses a region-scoped Bedrock catalog and your chosen AWS credential chain, profile, role, or SSO session.",
  },
};

export function providerPresentation(provider: string, transport: string): ProviderPresentation {
  return (
    PRESENTATION[transport] ?? {
      name: `${provider} · ${transport}`,
      shortName: provider,
      auth: "Backend-managed credential",
      guidanceTitle: "Backend-managed provider",
      guidance: "Review the authenticated connection scope and catalog before selecting an exact model.",
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
      return "Corti's private secret store";
    case "workload_identity":
      return "workload identity";
    case "application_default_credentials":
      return "Application Default Credentials";
    case "broker_keyring":
      return "broker-owned OS keyring";
    case "chat_gpt_device":
      return "Corti-owned ChatGPT device login";
    case "aws_default_chain":
      return "the default AWS credential chain";
    case "aws_profile":
      return "a named AWS profile";
    case "aws_static_keychain":
      return "an AWS key pair in Corti's private secret store";
    case "aws_assumed_role":
      return "an assumed IAM role";
    case "aws_sso":
      return "AWS IAM Identity Center (SSO)";
  }
}

/** Human wording for one AWS credential mode, used by the mode picker and the readiness line. */
export function awsCredentialModeLabel(mode: AwsCredentialMode): string {
  switch (mode) {
    case "default_chain":
      return "Default chain";
    case "profile":
      return "Named profile";
    case "static_keychain":
      return "Key pair";
    case "assume_role":
      return "Assume role";
    case "sso":
      return "SSO";
  }
}

export function awsCredentialModeDescription(mode: AwsCredentialMode): string {
  switch (mode) {
    case "default_chain":
      return "Whatever the AWS SDK would resolve: environment variables, then ~/.aws, then any instance role.";
    case "profile":
      return "A named profile from ~/.aws/config or ~/.aws/credentials.";
    case "static_keychain":
      return "An access key ID and secret stored in Corti's private secret store. A session token is optional.";
    case "assume_role":
      return "Resolve a base credential, then assume this role. Corti renews the session before it lapses.";
    case "sso":
      return "An IAM Identity Center profile. Corti reads the token the AWS CLI cached; it never performs the login.";
  }
}

/** What still has to be filled in before this Bedrock mode can be saved. Empty means the mode is ready. */
export function bedrockModeRequirements(
  mode: AwsCredentialMode,
  draft: { profile: string; roleArn: string; region: string },
  keys: { hasAccessKeyId: boolean; hasSecretAccessKey: boolean },
): string[] {
  const missing: string[] = [];
  if (!draft.region.trim()) missing.push("a Bedrock region");
  if ((mode === "profile" || mode === "sso") && !draft.profile.trim()) {
    missing.push("a profile name");
  }
  if (mode === "assume_role") {
    const arn = draft.roleArn.trim();
    if (!arn) missing.push("a role ARN");
    else if (!arn.startsWith("arn:") || !arn.includes(":role/")) missing.push("a valid IAM role ARN");
  }
  if (mode === "static_keychain") {
    if (!keys.hasAccessKeyId) missing.push("an access key ID");
    if (!keys.hasSecretAccessKey) missing.push("a secret access key");
  }
  return missing;
}

/** Countdown wording for an assumed-role or SSO session. Returns null when there is no expiry. */
export function sessionExpiryLabel(
  expiresAtUnixMs: number | null | undefined,
  nowUnixMs: number,
): string | null {
  if (expiresAtUnixMs == null) return null;
  const remainingMs = expiresAtUnixMs - nowUnixMs;
  if (remainingMs <= 0) return "Session expired";
  const minutes = Math.floor(remainingMs / 60_000);
  if (minutes < 1) return "Session renews in under a minute";
  if (minutes < 60) return `Session renews in ${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0
    ? `Session renews in ${hours} h`
    : `Session renews in ${hours} h ${rest} min`;
}

export function credentialSummary(
  credential: HostedCredentialState,
  transport: string,
): CredentialSummary {
  const vertex = transport === "vertex_api";
  const chatgpt = transport === "chatgpt_subscription";
  const bedrock = transport === "bedrock_runtime";
  switch (credential.state) {
    case "absent":
      return {
        label: vertex ? "Unarmed" : chatgpt ? "Not signed in" : "No credential",
        detail: vertex
          ? "No memory-only ADC access token is ready."
          : chatgpt
            ? "Authorize Corti with the ChatGPT account whose included quota you want to use."
            : bedrock
              ? "No AWS credential resolved for the selected mode."
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
        label: vertex ? "Armed · token only" : chatgpt ? "Signed in" : "Ready",
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
        detail: chatgpt
          ? "Corti is waiting for approval in your browser; no token enters this window."
          : "The backend returned display-only verification details; no token enters React.",
        tone: "caution",
      };
    case "refreshing":
      return {
        label: "Refreshing",
        detail: bedrock
          ? "The session is close to expiry and is being renewed before the next request."
          : "The existing credential is being refreshed in its backend owner.",
        tone: "caution",
      };
    case "rejected":
      return {
        label: "Rejected",
        detail: bedrock
          ? "AWS refused the credential. An expired SSO session is the usual cause."
          : "Authentication was rejected. Service readiness is checked separately.",
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
        label: chatgpt && credential.code === "cache" ? "Sign-in not saved" : "Credential error",
        detail:
          chatgpt && credential.code === "cache"
            ? "OpenAI authorized Corti, but the rotating credential could not be persisted in Corti's private secret store. Sign in again after fixing its permissions."
            : errorLabel(credential.code),
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
  if (model.transport === "chatgpt_subscription") {
    return "unavailable";
  }
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
