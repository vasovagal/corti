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
  HostedSettingsDto,
  HostedSupportTier,
  PreferencesSection,
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
  switch (code) {
    case "auth_unarmed":
      return "provider sign-in is not ready";
    case "auth_rejected":
      return "provider sign-in was rejected";
    case "permission":
      return "provider permission was denied";
    case "quota":
      return "provider quota is exhausted";
    case "rate_limited":
      return "provider rate limit reached";
    case "model_unavailable":
      return "selected model is unavailable";
    case "network":
      return "network request failed";
    case "timeout":
      return "request deadline was exceeded";
    case "canceled":
      return "request was canceled";
    case "superseded":
      return "request was replaced by newer work";
    case "policy_blocked":
      return "setup or provider policy prevented the request";
    case "cache":
      return "protected cache is unavailable";
    case "malformed_output":
      return "model response could not be safely applied";
    case "provider":
      return "provider request failed";
    case "broker_exited":
      return "provider helper exited";
    case "ambiguous_dispatch":
      return "paid dispatch outcome is unknown";
    case "internal":
      return "internal hosted processing error";
  }
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

export function modelUnavailableReason(model: HostedModelDescriptor, _lane: HostedLane): string | null {
  if (model.support_tier === "blocked") return "blocked by provider policy";
  if (!model.account_scoped_available) return "not available to this account or region";
  if (model.deprecated) return "deprecated";
  if (!model.capabilities.text_input || !model.capabilities.text_output) {
    return "text input/output capability is missing";
  }
  if (!model.capabilities.structured_output) return "structured output is not supported";
  return null;
}

/** Benchmark state is advice, never an ownership gate: the user's exact catalog model may still run. */
export function modelAdvisory(model: HostedModelDescriptor, lane: HostedLane): string | null {
  if (lane === "live" && !model.benchmarked_for_live) {
    return "live speed not measured; raw text wins if the deadline is missed";
  }
  return null;
}

export interface HostedActionGuidance {
  message: string;
  actionLabel: string | null;
  section: PreferencesSection | null;
}

function controlForLane(settings: HostedSettingsDto, lane: HostedLane) {
  switch (lane) {
    case "live":
      return settings.control.live;
    case "final":
      return settings.control.final_lane;
    case "question":
      return settings.control.questions;
  }
}

function laneTitle(lane: HostedLane): string {
  switch (lane) {
    case "live":
      return "Live cleanup";
    case "final":
      return "Final rewrite";
    case "question":
      return "Questions";
  }
}

/** Explain the first configuration fact that prevents this lane from dispatching. */
export function laneConfigurationGuidance(
  settings: HostedSettingsDto,
  lane: HostedLane,
): HostedActionGuidance | null {
  const title = laneTitle(lane);
  const selection = controlForLane(settings, lane).selection;
  if (!selection.provider || !selection.transport || !selection.model) {
    return {
      message: `${title} needs an exact provider model before it can run.`,
      actionLabel: `Configure ${title}`,
      section: "hosted-routing",
    };
  }
  const provider = settings.providers.find(
    (candidate) =>
      candidate.descriptor.provider === selection.provider &&
      candidate.descriptor.transport === selection.transport,
  );
  if (!provider || provider.descriptor.support_tier === "blocked" || !provider.descriptor.adapter_available) {
    return {
      message: `${title}'s saved provider is unavailable. Choose a supported provider and refresh its catalog.`,
      actionLabel: "Review provider",
      section: "hosted-provider",
    };
  }
  if (provider.credential.state !== "ready") {
    return {
      message: `${title}'s provider is not connected. Sign in or fix its credential, then refresh the catalog.`,
      actionLabel: "Fix provider connection",
      section: "hosted-provider",
    };
  }
  const model = findExactModel(
    settings.providers,
    selection.provider,
    selection.transport,
    selection.model,
  );
  if (!model) {
    return {
      message: `${title}'s saved model is no longer in the current catalog. Refresh the provider or choose another model.`,
      actionLabel: "Choose another model",
      section: "hosted-routing",
    };
  }
  const unavailable = modelUnavailableReason(model, lane);
  if (unavailable) {
    return {
      message: `${title}'s model is ${unavailable}. Choose another exact catalog model.`,
      actionLabel: "Choose another model",
      section: "hosted-routing",
    };
  }
  return null;
}

/** One next step for the Live window instead of exposing controls that fail with policy jargon. */
export function hostedOnboardingGuidance(
  settings: HostedSettingsDto,
): HostedActionGuidance | null {
  const readyCatalog = settings.providers.some(
    (provider) =>
      provider.descriptor.support_tier !== "blocked" &&
      provider.descriptor.adapter_available &&
      provider.credential.state === "ready" &&
      provider.models.some((model) => modelUnavailableReason(model, "final") === null),
  );
  if (!readyCatalog) {
    return {
      message: "You don't have a ready hosted provider catalog yet. Connect one provider and refresh its models.",
      actionLabel: "Configure a provider",
      section: "hosted-provider",
    };
  }
  const lanes: HostedLane[] = ["final", "live", "question"];
  const configured = lanes.filter((lane) => laneConfigurationGuidance(settings, lane) === null);
  if (configured.length === 0) {
    return {
      message: "Your provider is ready. Choose an exact model for at least one rewrite mode next.",
      actionLabel: "Choose rewrite models",
      section: "hosted-routing",
    };
  }
  if (!configured.some((lane) => controlForLane(settings, lane).enabled)) {
    return {
      message: "Models are selected, but every rewrite mode is off. Enable the mode you want to try.",
      actionLabel: "Enable a rewrite mode",
      section: "hosted-routing",
    };
  }
  if (!settings.control.egress_acknowledged) {
    return {
      message: "Your provider and model are ready. Review the text-only privacy boundary before the first hosted request.",
      actionLabel: "Review privacy & enable",
      section: "hosted",
    };
  }
  if (!settings.control.master_enabled) {
    return {
      message: "Hosted rewrite is configured but paused. Turn on Master above when you're ready to send transcript text.",
      actionLabel: "Review Master",
      section: "hosted",
    };
  }
  return null;
}

/** User-facing recovery copy for typed terminal errors. */
export function hostedErrorGuidance(code: HostedErrorCode): HostedActionGuidance {
  switch (code) {
    case "auth_unarmed":
    case "auth_rejected":
    case "permission":
      return {
        message: "The provider could not authorize this request. Check its sign-in, account access, and connection scope, then refresh.",
        actionLabel: "Fix provider connection",
        section: "hosted-provider",
      };
    case "quota":
    case "rate_limited":
      return {
        message: "The provider refused more work right now. Check quota or billing, or wait and try again.",
        actionLabel: "Review provider",
        section: "hosted-provider",
      };
    case "model_unavailable":
    case "policy_blocked":
      return {
        message: "The saved model or rewrite mode is not currently usable. Review the exact model and lane setup.",
        actionLabel: "Review rewrite modes",
        section: "hosted-routing",
      };
    case "malformed_output":
      return {
        message: "The model returned text Corti could not safely apply. Try another exact model; raw transcript text was kept.",
        actionLabel: "Choose another model",
        section: "hosted-routing",
      };
    case "cache":
      return {
        message: "Corti could not use its protected hosted state or cache. Review diagnostics; raw transcript text was kept.",
        actionLabel: "Open diagnostics",
        section: "hosted-advanced",
      };
    case "network":
    case "timeout":
    case "provider":
    case "broker_exited":
    case "ambiguous_dispatch":
      return {
        message: "The provider call did not finish safely. Check the connection and try again; Corti kept raw text and will not blindly repeat a paid call.",
        actionLabel: "Review provider",
        section: "hosted-provider",
      };
    case "canceled":
    case "superseded":
      return {
        message: "This request was replaced or canceled. The current transcript remains available.",
        actionLabel: null,
        section: null,
      };
    case "internal":
      return {
        message: "Hosted processing hit an internal error. Raw transcript text was kept; diagnostics can help identify the cause.",
        actionLabel: "Open diagnostics",
        section: "hosted-advanced",
      };
  }
}

export function unknownHostedErrorGuidance(error: unknown): HostedActionGuidance {
  const text = String(error).toLocaleLowerCase();
  if (text.includes("auth") || text.includes("credential") || text.includes("permission")) {
    return hostedErrorGuidance("auth_rejected");
  }
  if (text.includes("quota")) return hostedErrorGuidance("quota");
  if (text.includes("rate")) return hostedErrorGuidance("rate_limited");
  if (text.includes("network")) return hostedErrorGuidance("network");
  if (text.includes("timeout") || text.includes("deadline")) return hostedErrorGuidance("timeout");
  if (text.includes("cache")) return hostedErrorGuidance("cache");
  if (text.includes("model")) return hostedErrorGuidance("model_unavailable");
  if (text.includes("policy") || text.includes("disabled")) return hostedErrorGuidance("policy_blocked");
  return hostedErrorGuidance("internal");
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
