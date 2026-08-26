import { useEffect, useMemo, useState, type FormEvent } from "react";
import type {
  AwsCredentialOptionsDto,
  BedrockCredentialDto,
  HostedPatchInput,
  HostedProviderScope,
  HostedProviderScopeUpdate,
  HostedProviderState,
  SecretSlotRequest,
} from "../lib/api";
import {
  VERTEX_UNARMED_WARNING,
  billingDisclosure,
  credentialSummary,
  errorLabel,
  providerKey,
  providerPresentation,
  supportTierLabel,
} from "../lib/hosted";
import { HostedBedrockCredentials, type BedrockActions } from "./HostedBedrock";
import { HostedSwitch } from "./HostedCommon";

interface ProviderActions {
  busy: boolean;
  codexApproved: boolean;
  onRefresh: (provider: string, transport: string) => Promise<boolean>;
  onScope: (update: HostedProviderScopeUpdate) => Promise<boolean>;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
  /** Opens the native secure-entry sheet; the key never returns through this callback. */
  onPromptSecret: (request: SecretSlotRequest) => Promise<boolean>;
  onClearSecret: (request: SecretSlotRequest) => Promise<boolean>;
  bedrock: BedrockActions;
}

type PreferredProvider = { provider: string | null; transport: string | null };

const PROVIDER_ORDER: Record<string, number> = {
  openai_api: 0,
  anthropic_api: 1,
  vertex_api: 2,
  bedrock_runtime: 3,
  codex_app_server: 4,
  claude_subscription: 5,
};

export function HostedProviders({
  providers,
  scopes,
  bedrock,
  awsOptions,
  preferredSelection,
  actions,
}: {
  providers: HostedProviderState[];
  scopes: HostedProviderScope[];
  bedrock: BedrockCredentialDto;
  awsOptions: AwsCredentialOptionsDto | null;
  preferredSelection: PreferredProvider;
  actions: ProviderActions;
}) {
  const orderedProviders = useMemo(
    () =>
      [...providers].sort(
        (left, right) =>
          (PROVIDER_ORDER[left.descriptor.transport] ?? 99) -
          (PROVIDER_ORDER[right.descriptor.transport] ?? 99),
      ),
    [providers],
  );
  const preferredKey =
    preferredSelection.provider && preferredSelection.transport
      ? providerKey(preferredSelection.provider, preferredSelection.transport)
      : null;
  const fallback =
    orderedProviders.find((provider) => provider.credential.state === "ready") ??
    orderedProviders.find((provider) => provider.descriptor.support_tier === "documented") ??
    orderedProviders[0];
  const initialKey =
    preferredKey &&
    orderedProviders.some(
      (provider) =>
        providerKey(provider.descriptor.provider, provider.descriptor.transport) === preferredKey,
    )
      ? preferredKey
      : fallback
        ? providerKey(fallback.descriptor.provider, fallback.descriptor.transport)
        : "";
  const [selectedKey, setSelectedKey] = useState(initialKey);

  useEffect(() => {
    if (
      orderedProviders.some(
        (provider) =>
          providerKey(provider.descriptor.provider, provider.descriptor.transport) === selectedKey,
      )
    ) {
      return;
    }
    setSelectedKey(initialKey);
  }, [initialKey, orderedProviders, selectedKey]);

  const selected = orderedProviders.find(
    (provider) =>
      providerKey(provider.descriptor.provider, provider.descriptor.transport) === selectedKey,
  );
  const selectedScope = selected
    ? scopes.find(
        (candidate) =>
          candidate.provider === selected.descriptor.provider &&
          candidate.transport === selected.descriptor.transport,
      )
    : undefined;
  const guidance = selected
    ? providerPresentation(selected.descriptor.provider, selected.descriptor.transport)
    : null;

  return (
    <section aria-labelledby="hosted-providers-heading">
      <div className="hosted-section-heading">
        <div>
          <p className="hosted-eyebrow">Connection</p>
          <h2 id="hosted-providers-heading">Choose one provider to configure</h2>
        </div>
        <p>Switching this view never changes the provider or model already saved for a rewrite mode.</p>
      </div>

      <div className="hosted-provider-picker">
        <label htmlFor="hosted-provider-picker">
          <span>Provider to configure</span>
          <select
            id="hosted-provider-picker"
            value={selectedKey}
            disabled={actions.busy}
            onChange={(event) => setSelectedKey(event.target.value)}
          >
            {orderedProviders.map((provider) => {
              const presentation = providerPresentation(
                provider.descriptor.provider,
                provider.descriptor.transport,
              );
              const auth = credentialSummary(provider.credential, provider.descriptor.transport);
              const key = providerKey(
                provider.descriptor.provider,
                provider.descriptor.transport,
              );
              return (
                <option key={key} value={key}>
                  {presentation.shortName} — {auth.label}
                </option>
              );
            })}
          </select>
        </label>
        {guidance && (
          <div className="hosted-provider-guidance">
            <strong>{guidance.guidanceTitle}</strong>
            <p>{guidance.guidance}</p>
          </div>
        )}
      </div>

      <ol className="hosted-provider-steps" aria-label="Provider setup steps">
        <li><span>1</span> Choose the account where you already manage billing and data controls.</li>
        <li><span>2</span> Connect it here, then refresh the authenticated model catalog.</li>
        <li><span>3</span> Select an exact model under Rewrite modes; nothing is enabled automatically.</li>
      </ol>

      {selected && (
        <div className="hosted-provider-grid">
          <HostedProviderCard
            key={`${selected.descriptor.provider}:${selected.descriptor.transport}`}
            state={selected}
            scope={selectedScope}
            bedrock={bedrock}
            awsOptions={awsOptions}
            actions={actions}
          />
        </div>
      )}
    </section>
  );
}

function HostedProviderCard({
  state,
  scope,
  bedrock,
  awsOptions,
  actions,
}: {
  state: HostedProviderState;
  scope?: HostedProviderScope;
  bedrock: BedrockCredentialDto;
  awsOptions: AwsCredentialOptionsDto | null;
  actions: ProviderActions;
}) {
  const { descriptor, credential } = state;
  const presentation = providerPresentation(descriptor.provider, descriptor.transport);
  const auth = credentialSummary(credential, descriptor.transport);
  const isVertex = descriptor.transport === "vertex_api";
  const isDirectKey = descriptor.transport === "openai_api" || descriptor.transport === "anthropic_api";
  const isCodex = descriptor.transport === "codex_app_server";
  const isClaudeSubscription = descriptor.transport === "claude_subscription";
  const isBedrock = descriptor.transport === "bedrock_runtime";
  const directSlot: SecretSlotRequest =
    descriptor.transport === "openai_api" ? { provider: "open_ai" } : { provider: "anthropic" };
  const canRefresh =
    Boolean(scope?.configured) &&
    (descriptor.support_tier === "documented" ||
      (descriptor.support_tier === "experimental" &&
        descriptor.adapter_available &&
        actions.codexApproved));

  return (
    <article className={`card hosted-provider-card hosted-tier-${descriptor.support_tier}`}>
      <header className="hosted-card-head">
        <div>
          <h3>{presentation.name}</h3>
          <p>{presentation.auth}</p>
        </div>
        <span className={`hosted-tier-badge hosted-tier-${descriptor.support_tier}`}>
          {supportTierLabel(descriptor.support_tier)}
        </span>
      </header>

      {isClaudeSubscription && (
        <div className="hosted-policy-block">
          <strong>Blocked — use Anthropic API billing</strong>
          <p>
            Claude Free / Pro / Max credentials cannot be imported or routed without written Anthropic
            permission. Direct Anthropic API access is a separate, metered product.
          </p>
        </div>
      )}

      {isCodex && (
        <div className="hosted-policy-block hosted-policy-experimental">
          <strong>Experimental · unavailable in production</strong>
          <p>
            App-server support is local-stdio, broker/keyring owned, denied tools, and off by default.
            Subscription access is not a direct API credential and carries no dollar estimate.
          </p>
          <HostedSwitch
            compact
            label="Allow experimental Codex app-server"
            description={
              descriptor.adapter_available
                ? "Approval alone does not enable egress or select a model."
                : "This build has no approved app-server adapter."
            }
            checked={actions.codexApproved}
            disabled={!descriptor.adapter_available || actions.busy}
            onChange={(approved) =>
              void actions.onPatch(
                { kind: "set_codex_experimental_approved", approved },
                approved ? "Experimental Codex approval saved." : "Experimental Codex access is off.",
              )
            }
          />
        </div>
      )}

      {!isClaudeSubscription && (
        <div className={`hosted-auth-state hosted-tone-${auth.tone}`}>
          <span aria-hidden="true" />
          <div>
            <strong>{auth.label}</strong>
            <p>{auth.detail}</p>
          </div>
        </div>
      )}

      {isVertex && credential.state === "absent" && (
        <>
          <p className="hosted-vertex-warning" role="alert">
            {VERTEX_UNARMED_WARNING}
          </p>
          <p className="muted small">
            Run <code>gcloud auth application-default login</code>. Ordinary <code>gcloud auth login</code>
            is not ADC. Corti checks every five seconds; once armed, only the newest still-valid automatic
            work catches up.
          </p>
        </>
      )}

      {isVertex && credential.state === "resolving" && (
        <p className="callout small">
          Arming with ADC now. Newest automatic lane state is retained; older work is not replayed.
        </p>
      )}

      {isVertex && credential.state === "ready" && (
        <p className="muted small">
          Armed means an in-memory token is ready. It does not prove project, IAM, API enablement, billing,
          quota, region, or model access.
        </p>
      )}

      {credential.state === "device_authorization" && (
        <div className="hosted-device-code">
          <p>
            Open <code>{credential.verification_url}</code>
          </p>
          <p>
            User code <code>{credential.user_code}</code>
          </p>
          <p className="muted small">Login id is broker-owned; tokens never enter this window.</p>
        </div>
      )}

      {state.service_error && (
        <p className="hosted-service-error" role="status">
          <strong>Service error</strong> · {errorLabel(state.service_error)}. Credential readiness is unchanged.
        </p>
      )}

      {isDirectKey && (
        <div className="hosted-native-auth">
          <div className="other-row">
            <button
              className="btn-secondary"
              type="button"
              disabled={actions.busy}
              onClick={() => void actions.onPromptSecret(directSlot)}
            >
              {credential.state === "ready" ? "Replace key…" : "Add key…"}
            </button>
            {credential.state === "ready" && (
              <button
                className="btn-secondary"
                type="button"
                disabled={actions.busy}
                onClick={() => void actions.onClearSecret(directSlot)}
              >
                Remove key
              </button>
            )}
          </div>
          <p className="muted small">
            The key is typed into a native macOS sheet and written straight to the Keychain. This window
            never receives it, and no browser field accepts a key.
          </p>
        </div>
      )}

      {isBedrock && (
        <HostedBedrockCredentials
          bedrock={bedrock}
          scope={scope}
          credential={credential}
          options={awsOptions}
          actions={actions.bedrock}
        />
      )}

      {scope && descriptor.support_tier === "documented" && (
        <ProviderScopeEditor scope={scope} transport={descriptor.transport} actions={actions} />
      )}

      {!isClaudeSubscription && (
        <div className="hosted-provider-actions">
          <button
            className="btn-secondary"
            type="button"
            disabled={!canRefresh || actions.busy}
            onClick={() => void actions.onRefresh(descriptor.provider, descriptor.transport)}
          >
            Refresh status &amp; catalog
          </button>
          <span className="muted small">
            {scope && !scope.configured
              ? "Save a connection scope first."
              : state.models.length === 0
                ? "No selectable models returned."
                : `${state.models.length} exact catalog ${state.models.length === 1 ? "model" : "models"}.`}
          </span>
        </div>
      )}

      {state.models.length > 0 && (
        <details className="hosted-catalog">
          <summary>Authenticated model catalog</summary>
          <ul>
            {state.models.map((model) => (
              <li key={`${model.exact_model_id}:${model.region ?? ""}`}>
                <code>{model.exact_model_id}</code>
                <span>
                  {model.region ? `${model.region} · ` : ""}
                  {model.benchmarked_for_live ? "live benchmark passed" : "live latency unbenchmarked"}
                  {model.deprecated ? " · deprecated" : ""}
                </span>
              </li>
            ))}
          </ul>
        </details>
      )}

      <p className="hosted-cost-line">
        <strong>Cost</strong> · {billingDisclosure(descriptor.billing_basis, null)}
      </p>
      <p className="muted small hosted-connection-footnote">
        Status and catalog are connection facts only. Transcript egress still requires Master, a lane, and an
        exact model selection.
      </p>
    </article>
  );
}

function ProviderScopeEditor({
  scope,
  transport,
  actions,
}: {
  scope: HostedProviderScope;
  transport: string;
  actions: ProviderActions;
}) {
  const [alias, setAlias] = useState(scope.alias ?? "");
  const [project, setProject] = useState(scope.project ?? "");
  const [region, setRegion] = useState(scope.region ?? "");
  const [quotaProject, setQuotaProject] = useState(scope.quota_project ?? "");
  const vertex = transport === "vertex_api";
  const anthropic = transport === "anthropic_api";

  useEffect(() => {
    setAlias(scope.alias ?? "");
    setProject(scope.project ?? "");
    setRegion(scope.region ?? "");
    setQuotaProject(scope.quota_project ?? "");
  }, [scope.alias, scope.project, scope.quota_project, scope.region]);

  const changed = useMemo(
    () =>
      alias !== (scope.alias ?? "") ||
      project !== (scope.project ?? "") ||
      region !== (scope.region ?? "") ||
      quotaProject !== (scope.quota_project ?? ""),
    [alias, project, quotaProject, region, scope.alias, scope.project, scope.quota_project, scope.region],
  );
  const allEmpty = !alias.trim() && !project.trim() && !region.trim() && !quotaProject.trim();
  const valid = vertex ? allEmpty || Boolean(project.trim() && region.trim()) : allEmpty || Boolean(alias.trim());

  function save(event: FormEvent) {
    event.preventDefault();
    if (!changed || !valid) return;
    void actions.onScope({
      provider: scope.provider,
      transport: scope.transport,
      alias: alias.trim() || null,
      project: vertex ? project.trim() || null : null,
      region: vertex || anthropic ? region.trim() || null : null,
      quota_project: vertex ? quotaProject.trim() || null : null,
    });
  }

  return (
    <form className="hosted-scope" onSubmit={save}>
      <div className="hosted-scope-head">
        <strong>Connection scope</strong>
        <span className={scope.configured ? "hosted-configured" : "muted"}>
          {scope.configured ? "Configured" : "Not configured"}
        </span>
      </div>
      <label>
        <span>Connection label</span>
        <input
          type="text"
          value={alias}
          maxLength={1024}
          placeholder={vertex ? "Optional local label" : "Required local label"}
          onChange={(event) => setAlias(event.target.value)}
        />
      </label>
      {vertex && (
        <label>
          <span>Vertex project</span>
          <input
            type="text"
            value={project}
            maxLength={1024}
            autoCapitalize="none"
            autoCorrect="off"
            onChange={(event) => setProject(event.target.value)}
          />
        </label>
      )}
      {(vertex || anthropic) && (
        <label>
          <span>{vertex ? "Vertex region" : "Inference geography (optional)"}</span>
          <input
            type="text"
            value={region}
            maxLength={1024}
            autoCapitalize="none"
            autoCorrect="off"
            placeholder={vertex ? "Required, for example global" : "Provider-supported value"}
            onChange={(event) => setRegion(event.target.value)}
          />
        </label>
      )}
      {vertex && (
        <label>
          <span>Quota project (optional)</span>
          <input
            type="text"
            value={quotaProject}
            maxLength={1024}
            autoCapitalize="none"
            autoCorrect="off"
            onChange={(event) => setQuotaProject(event.target.value)}
          />
        </label>
      )}
      {!valid && (
        <p className="hosted-field-error">
          {vertex ? "Project and region are required together." : "Add a local connection label."}
        </p>
      )}
      <button className="btn-secondary" type="submit" disabled={!changed || !valid || actions.busy}>
        {allEmpty && scope.configured ? "Clear scope" : "Save scope"}
      </button>
    </form>
  );
}
