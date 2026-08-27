import { useEffect, useMemo, useState, type FormEvent } from "react";
import type {
  AwsCredentialOptionsDto,
  BedrockCredentialDto,
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

interface ProviderActions {
  busy: boolean;
  onRefresh: (provider: string, transport: string) => Promise<boolean>;
  onScope: (update: HostedProviderScopeUpdate) => Promise<boolean>;
  /** Opens the native secure-entry sheet; the key never returns through this callback. */
  onPromptSecret: (request: SecretSlotRequest) => Promise<boolean>;
  onClearSecret: (request: SecretSlotRequest) => Promise<boolean>;
  onStartChatGpt: () => Promise<boolean>;
  onCancelChatGpt: () => Promise<boolean>;
  onSignOutChatGpt: () => Promise<boolean>;
  onOpenChatGptLogin: () => Promise<boolean>;
  onVertexModels: (models: string[]) => Promise<boolean>;
  bedrock: BedrockActions;
}

type PreferredProvider = { provider: string | null; transport: string | null };

const PROVIDER_ORDER: Record<string, number> = {
  chatgpt_subscription: 0,
  openai_api: 1,
  anthropic_api: 2,
  vertex_api: 3,
  bedrock_runtime: 4,
  claude_subscription: 5,
};

export function HostedProviders({
  providers,
  scopes,
  bedrock,
  vertexModels,
  awsOptions,
  preferredSelection,
  actions,
}: {
  providers: HostedProviderState[];
  scopes: HostedProviderScope[];
  bedrock: BedrockCredentialDto;
  vertexModels: string[];
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
    orderedProviders.find((provider) => provider.descriptor.transport === "chatgpt_subscription") ??
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
            vertexModels={vertexModels}
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
  vertexModels,
  awsOptions,
  actions,
}: {
  state: HostedProviderState;
  scope?: HostedProviderScope;
  bedrock: BedrockCredentialDto;
  vertexModels: string[];
  awsOptions: AwsCredentialOptionsDto | null;
  actions: ProviderActions;
}) {
  const { descriptor, credential } = state;
  const presentation = providerPresentation(descriptor.provider, descriptor.transport);
  const auth = credentialSummary(credential, descriptor.transport);
  const isVertex = descriptor.transport === "vertex_api";
  const isChatGpt = descriptor.transport === "chatgpt_subscription";
  const isDirectKey = descriptor.transport === "openai_api" || descriptor.transport === "anthropic_api";
  const isClaudeSubscription = descriptor.transport === "claude_subscription";
  const isBedrock = descriptor.transport === "bedrock_runtime";
  const directSlot: SecretSlotRequest =
    descriptor.transport === "openai_api" ? { provider: "open_ai" } : { provider: "anthropic" };
  const canRefresh =
    Boolean(scope?.configured) &&
    descriptor.adapter_available &&
    (!isChatGpt || credential.state === "ready");

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
          <p className="muted small">
            {isChatGpt
              ? "Corti is polling OpenAI directly. Access and refresh tokens stay in Corti's private secret store."
              : "The login id is backend-owned; tokens never enter this window."}
          </p>
          {isChatGpt && (
            <div className="other-row">
              <button
                className="btn-primary"
                type="button"
                disabled={actions.busy}
                onClick={() => void actions.onOpenChatGptLogin()}
              >
                Open authorization page
              </button>
              <button
                className="btn-secondary"
                type="button"
                disabled={actions.busy}
                onClick={() => void actions.onCancelChatGpt()}
              >
                Cancel sign-in
              </button>
            </div>
          )}
        </div>
      )}

      {isChatGpt && credential.state !== "device_authorization" && (
        <div className="hosted-native-auth">
          <div className="other-row">
            {credential.state === "ready" ? (
              <button
                className="btn-secondary"
                type="button"
                disabled={actions.busy}
                onClick={() => void actions.onSignOutChatGpt()}
              >
                Sign out
              </button>
            ) : (
              <button
                className="btn-primary"
                type="button"
                disabled={actions.busy || credential.state === "resolving" || credential.state === "refreshing"}
                onClick={() => void actions.onStartChatGpt()}
              >
                Sign in with ChatGPT…
              </button>
            )}
          </div>
          <p className="muted small">
            Corti uses OpenAI device authorization and the fixed ChatGPT Responses endpoint directly. It
            does not launch a Codex server, import another app&apos;s login, or use OpenAI API billing.
          </p>
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
            The key is typed into a native macOS sheet and written straight to Corti's private secret store
            (owner-only files under ~/.local/share/corti/hosted-secrets). This window
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

      {scope && descriptor.support_tier === "documented" && !isChatGpt && (
        <ProviderScopeEditor scope={scope} transport={descriptor.transport} actions={actions} />
      )}

      {isVertex && <VertexModelEditor models={vertexModels} actions={actions} />}

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
                ? isVertex
                  ? "No models yet — add one above."
                  : "No selectable models returned."
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
                  {model.benchmarked_for_live ? "live speed measured" : "live speed not measured · still selectable"}
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

const MAX_VERTEX_MODELS = 32;
const VERTEX_MODEL_ID = /^[A-Za-z0-9\-_.@]{1,256}$/u;

/** Vertex has no per-project listing of the models a caller may invoke, so the catalog is whatever the
 * operator types here plus the curated ids. A wrong id is only found out on the first call. */
function VertexModelEditor({ models, actions }: { models: string[]; actions: ProviderActions }) {
  const [draft, setDraft] = useState("");
  const trimmed = draft.trim();
  const duplicate = models.includes(trimmed);
  const malformed = trimmed.length > 0 && !VERTEX_MODEL_ID.test(trimmed);
  const full = models.length >= MAX_VERTEX_MODELS;
  const canAdd = trimmed.length > 0 && !duplicate && !malformed && !full;

  function add(event: FormEvent) {
    event.preventDefault();
    if (!canAdd) return;
    void actions.onVertexModels([...models, trimmed]).then((accepted) => {
      if (accepted) setDraft("");
    });
  }

  return (
    <form className="hosted-scope hosted-vertex-models" onSubmit={add}>
      <div className="hosted-scope-head">
        <strong>Vertex models</strong>
        <span className="muted">{models.length ? `${models.length} added` : "Curated only"}</span>
      </div>
      <p className="muted small">
        Google publishes no API that lists the models a project may call, so type the exact publisher model
        id — for example <code>gemini-2.5-flash-lite</code>. <code>gemini-2.5-flash</code>,{" "}
        <code>gemini-2.5-pro</code>, and <code>claude-sonnet-4-5</code> are always available. An id starting
        with <code>claude-</code> is served by the Anthropic publisher and must be enabled for the project in
        Model Garden. Added ids appear in the catalog after the next refresh; Vertex rejects a wrong one on
        the first request.
      </p>
      {models.length > 0 && (
        <ul className="hosted-vertex-model-list">
          {models.map((model) => (
            <li key={model}>
              <code>{model}</code>
              <button
                className="btn-secondary"
                type="button"
                disabled={actions.busy}
                onClick={() => void actions.onVertexModels(models.filter((other) => other !== model))}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      <label>
        <span>Add model id</span>
        <input
          type="text"
          value={draft}
          maxLength={256}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          placeholder="publisher model id"
          onChange={(event) => setDraft(event.target.value)}
        />
      </label>
      {(duplicate || malformed || full) && (
        <p className="hosted-field-error">
          {full
            ? `At most ${MAX_VERTEX_MODELS} models can be pinned.`
            : duplicate
              ? "That model is already listed."
              : "Model ids use letters, digits, and - _ . @ only."}
        </p>
      )}
      <button className="btn-secondary" type="submit" disabled={!canAdd || actions.busy}>
        Add model
      </button>
    </form>
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
