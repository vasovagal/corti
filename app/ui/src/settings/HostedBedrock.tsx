import { useEffect, useMemo, useState, type FormEvent } from "react";
import type {
  AwsCredentialMode,
  AwsCredentialOptionsDto,
  AwsKeySlot,
  BedrockCredentialDto,
  HostedCredentialState,
  HostedModelDescriptor,
  HostedMutationInvalidField,
  HostedMutationInvalidReason,
  HostedProviderScope,
} from "../lib/api";
import {
  BEDROCK_CREDENTIAL_EXPIRY_SKEW_MS,
  awsCredentialModeDescription,
  awsCredentialModeLabel,
  bedrockCredentialGuidance,
  bedrockInvalidMessage,
  bedrockModeRequirements,
  bedrockSetupChanged,
  bedrockSetupStatusLabel,
  deriveBedrockSetupStatus,
  normalizeBedrockSetup,
  sessionExpiryLabel,
  type BedrockSetupDraft,
  type NormalizedBedrockSetup,
} from "../lib/hosted";
import { BEDROCK_REGIONS, regionOptions } from "../lib/awsRegions";

const MODES: AwsCredentialMode[] = [
  "default_chain",
  "profile",
  "static_keychain",
  "assume_role",
  "sso",
];

const KEY_SLOTS: { slot: AwsKeySlot; label: string; optional?: boolean }[] = [
  { slot: "access_key_id", label: "Access key ID" },
  { slot: "secret_access_key", label: "Secret access key" },
  { slot: "session_token", label: "Session token", optional: true },
];

const SETUP_NAME_HELP =
  "A name stored by Corti for this Bedrock setup. It is not Ollama, a local model, or an OpenAI-compatible endpoint. Corti uses a separate internal identity to keep provider catalogs and cached results for different provider configurations apart.";

export type BedrockActionOutcome =
  | { status: "accepted" }
  | {
      status: "invalid";
      field: HostedMutationInvalidField;
      reason: HostedMutationInvalidReason;
    }
  | { status: "conflict" }
  | { status: "failed" };

export type AwsProfileDiscoveryState = "loading" | "loaded" | "error";

export interface BedrockActions {
  busy: boolean;
  onSave: (setup: NormalizedBedrockSetup) => Promise<BedrockActionOutcome>;
  onClear: () => Promise<BedrockActionOutcome>;
  onPromptKey: (slot: AwsKeySlot) => Promise<boolean>;
  onClearKey: (slot: AwsKeySlot) => Promise<boolean>;
  onReloadProfiles: () => Promise<boolean>;
  onRefresh: () => Promise<boolean>;
}

function persistedDraft(
  bedrock: BedrockCredentialDto,
  scope: HostedProviderScope | undefined,
): BedrockSetupDraft {
  return {
    mode: bedrock.mode,
    profile: bedrock.profile ?? "",
    roleArn: bedrock.role_arn ?? "",
    region: scope?.region ?? "",
    setupName: scope?.alias ?? "",
  };
}

/**
 * One secret-free Bedrock setup form. Key values are still owned by native secure sheets; this
 * component receives presence flags only and submits non-secret configuration atomically.
 */
export function HostedBedrockCredentials({
  bedrock,
  scope,
  credential,
  options,
  profileDiscoveryState,
  resetToken,
  adapterAvailable,
  models,
  actions,
}: {
  bedrock: BedrockCredentialDto;
  scope?: HostedProviderScope;
  credential: HostedCredentialState;
  options: AwsCredentialOptionsDto | null;
  profileDiscoveryState: AwsProfileDiscoveryState;
  /** Changes only after this form's accepted mutation or conflict snapshot. */
  resetToken: number;
  adapterAvailable: boolean;
  models: HostedModelDescriptor[];
  actions: BedrockActions;
}) {
  const savedDraft = useMemo(
    () => persistedDraft(bedrock, scope),
    [
      bedrock.mode,
      bedrock.profile,
      bedrock.role_arn,
      scope?.alias,
      scope?.configured,
      scope?.region,
    ],
  );
  const [draft, setDraft] = useState<BedrockSetupDraft>(savedDraft);
  const [serverIssue, setServerIssue] = useState<{
    field: HostedMutationInvalidField;
    reason: HostedMutationInvalidReason;
  } | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [reloadingProfiles, setReloadingProfiles] = useState(false);
  const [nowUnixMs, setNowUnixMs] = useState(Date.now());

  // Canonical fields change only after an accepted backend snapshot (including a conflict snapshot).
  // Credential/catalog events leave these dependencies unchanged and therefore never erase a dirty draft.
  useEffect(() => {
    setDraft(savedDraft);
    setServerIssue(null);
  }, [resetToken, savedDraft]);

  useEffect(() => {
    setServerIssue(null);
  }, [
    bedrock.has_access_key_id,
    bedrock.has_secret_access_key,
    options,
  ]);

  const profiles = useMemo(
    () => Array.from(new Set(options?.profiles ?? [])),
    [options?.profiles],
  );
  const keys = useMemo(
    () => ({
      hasAccessKeyId: bedrock.has_access_key_id,
      hasSecretAccessKey: bedrock.has_secret_access_key,
      hasSessionToken: bedrock.has_session_token,
    }),
    [
      bedrock.has_access_key_id,
      bedrock.has_secret_access_key,
      bedrock.has_session_token,
    ],
  );
  const issues = useMemo(
    () => bedrockModeRequirements(
      draft,
      profileDiscoveryState === "loaded" ? profiles : null,
      keys,
    ),
    [draft, keys, profileDiscoveryState, profiles],
  );
  const changed = bedrockSetupChanged(draft, savedDraft);
  const backendIssue = serverIssue
    ? {
        ...serverIssue,
        message: bedrockInvalidMessage(serverIssue.field, serverIssue.reason),
      }
    : null;
  const effectiveIssues = backendIssue && !issues.some((issue) => issue.field === backendIssue.field)
    ? [...issues, backendIssue]
    : issues;
  const issueFor = (field: HostedMutationInvalidField) =>
    effectiveIssues.find((issue) => issue.field === field);
  const status = deriveBedrockSetupStatus({
    changed,
    issues: effectiveIssues,
    scopeConfigured: Boolean(scope?.configured),
    credential,
    nowUnixMs,
  });
  const profileIssue = issueFor("profile");
  const roleIssue = issueFor("role_arn");
  const keyIssue = issueFor("key_pair");
  const regionIssue = issueFor("region");
  const setupNameIssue = issueFor("setup_name");
  const regions = useMemo(() => regionOptions(BEDROCK_REGIONS, draft.region), [draft.region]);
  const expiry =
    credential.state === "ready" && !changed
      ? sessionExpiryLabel(credential.expires_at_unix_ms, nowUnixMs)
      : null;
  const showsProfile =
    draft.mode === "profile" || draft.mode === "sso" || draft.mode === "assume_role";
  const profileLabel = draft.mode === "assume_role" ? "Base AWS profile (optional)" : "AWS profile";
  const missingSelectedProfile = Boolean(
    profileDiscoveryState === "loaded" &&
      draft.profile.trim() &&
      !profiles.includes(draft.profile.trim()),
  );
  const hasSavedSetup = Boolean(
    scope?.configured ||
      scope?.alias ||
      scope?.region ||
      bedrock.mode !== "default_chain" ||
      bedrock.profile ||
      bedrock.role_arn,
  );
  const operationBusy =
    actions.busy || submitting || clearing || refreshing || reloadingProfiles;
  const canSave = changed && effectiveIssues.length === 0 && !operationBusy;
  const canRefresh =
    !changed &&
    effectiveIssues.length === 0 &&
    Boolean(scope?.configured) &&
    adapterAvailable &&
    !operationBusy;

  useEffect(() => {
    if (credential.state !== "ready" || credential.expires_at_unix_ms == null) return;
    const renewalAt = credential.expires_at_unix_ms - BEDROCK_CREDENTIAL_EXPIRY_SKEW_MS;
    const delay = renewalAt > Date.now()
      ? Math.min(30_000, Math.max(10, renewalAt - Date.now() + 10))
      : 30_000;
    const timer = window.setTimeout(() => setNowUnixMs(Date.now()), delay);
    return () => window.clearTimeout(timer);
  }, [credential, nowUnixMs]);

  const credentialGuidance = !changed
    ? bedrockCredentialGuidance(bedrock.mode, credential, bedrock.profile)
    : null;
  const statusDetail = submitting
    ? "Saving the complete Bedrock setup…"
    : status === "unsaved_changes"
      ? effectiveIssues[0]?.message ?? "All required fields are complete. Save to apply this setup."
      : status === "saved_not_ready"
        ? effectiveIssues[0]?.message ??
          (!scope?.configured
            ? "No complete Bedrock setup is saved yet."
            : credentialGuidance ?? "The saved setup still needs an AWS credential check.")
        : "The saved setup and its AWS credential are ready.";
  const saveGuidance = submitting
    ? "Saving the complete setup in one update."
    : actions.busy || clearing || refreshing
      ? "Wait for the current provider update to finish."
      : effectiveIssues.length > 0
        ? `Before saving: ${effectiveIssues.map((issue) => issue.message).join(" ")}`
        : !changed
          ? "Make a valid change to enable Save Bedrock setup."
          : "All required setup fields are complete.";
  const clearGuidance = operationBusy
    ? "Wait for the current provider update to finish."
    : hasSavedSetup
      ? "Clear removes the saved non-secret setup. Stored key values remain available in the key management section."
      : "There is no saved non-secret Bedrock setup to clear.";
  const keyActionGuidance = operationBusy
    ? "Wait for the current provider update to finish."
    : "Add, replace, or remove stored AWS values using Corti's native secure sheet.";
  const refreshGuidance = actions.busy || submitting || clearing || refreshing || reloadingProfiles
    ? "Wait for the current provider update to finish."
    : changed
      ? "Save these setup changes before refreshing models."
      : effectiveIssues.length > 0
        ? `Before refreshing: ${effectiveIssues.map((issue) => issue.message).join(" ")}`
        : !scope?.configured
          ? "Save a complete Bedrock setup before refreshing models."
          : !adapterAvailable
            ? "Bedrock support is unavailable in this build."
            : models.length === 0
              ? "No models loaded yet. Refresh to check this AWS account and region."
              : `${models.length} exact catalog ${models.length === 1 ? "model" : "models"} loaded.`;

  function updateDraft(patch: Partial<BedrockSetupDraft>) {
    setServerIssue(null);
    setDraft((current) => ({ ...current, ...patch }));
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    if (!canSave) return;
    setSubmitting(true);
    setServerIssue(null);
    try {
      const outcome = await actions.onSave(normalizeBedrockSetup(draft));
      if (outcome.status === "invalid") {
        setServerIssue({ field: outcome.field, reason: outcome.reason });
        if (outcome.field === "profile" && outcome.reason === "not_found") {
          await actions.onReloadProfiles();
        }
      }
    } finally {
      setSubmitting(false);
    }
  }

  async function clearSetup() {
    if (!hasSavedSetup || operationBusy) return;
    setClearing(true);
    setServerIssue(null);
    try {
      const outcome = await actions.onClear();
      if (outcome.status === "invalid") {
        setServerIssue({ field: outcome.field, reason: outcome.reason });
      }
    } finally {
      setClearing(false);
    }
  }

  async function reloadProfiles() {
    if (operationBusy) return;
    setReloadingProfiles(true);
    try {
      await actions.onReloadProfiles();
    } finally {
      setReloadingProfiles(false);
    }
  }

  async function refreshModels() {
    if (!canRefresh) return;
    setRefreshing(true);
    try {
      await actions.onRefresh();
    } finally {
      setRefreshing(false);
    }
  }

  return (
    <div className="hosted-bedrock">
      <form className="hosted-bedrock-form" onSubmit={(event) => void save(event)} noValidate>
        <div className="hosted-bedrock-heading hosted-bedrock-full">
          <div>
            <h4>Bedrock setup</h4>
            <p>Save the authentication method, account routing, region, and name together.</p>
          </div>
          <div
            className={`hosted-bedrock-status hosted-bedrock-status-${status}`}
            role="status"
            aria-live="polite"
            aria-atomic="true"
          >
            <strong>{bedrockSetupStatusLabel(status)}</strong>
            <span>{statusDetail}</span>
          </div>
        </div>

        <fieldset
          className="hosted-bedrock-modes hosted-bedrock-full"
          aria-describedby="bedrock-auth-method-help"
        >
          <legend>Authentication method</legend>
          <div className="hosted-segmented" role="radiogroup" aria-label="AWS authentication method">
            {MODES.map((candidate) => (
              <label key={candidate} className={draft.mode === candidate ? "is-selected" : undefined}>
                <input
                  type="radio"
                  name="bedrock-credential-mode"
                  value={candidate}
                  checked={draft.mode === candidate}
                  disabled={operationBusy}
                  onChange={() => updateDraft({ mode: candidate })}
                />
                <span>{awsCredentialModeLabel(candidate)}</span>
              </label>
            ))}
          </div>
          <p id="bedrock-auth-method-help" className="muted small">
            {awsCredentialModeDescription(draft.mode)}
          </p>
        </fieldset>

        {showsProfile && (
          <div className="hosted-bedrock-field">
            <label htmlFor="bedrock-profile">{profileLabel}</label>
            <select
              id="bedrock-profile"
              value={draft.profile}
              disabled={operationBusy}
              aria-invalid={profileIssue ? "true" : undefined}
              aria-describedby={`bedrock-profile-discovery${profileIssue ? " bedrock-profile-error" : ""}`}
              onChange={(event) => updateDraft({ profile: event.target.value })}
            >
              <option value="">
                {draft.mode === "assume_role" ? "Default chain" : "Select a profile…"}
              </option>
              {missingSelectedProfile && (
                <option value={draft.profile}>Previously selected — not found</option>
              )}
              {profiles.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
            <div className="hosted-profile-discovery">
              <span id="bedrock-profile-discovery" className="muted small">
                {profileDiscoveryState === "loading"
                  ? "Loading AWS profiles…"
                  : profileDiscoveryState === "error"
                    ? "AWS profiles could not be loaded; Corti has not classified this selection as missing."
                    : `${profiles.length} AWS ${profiles.length === 1 ? "profile" : "profiles"} loaded.`}
              </span>
              <button
                className="btn-secondary"
                type="button"
                disabled={operationBusy}
                aria-describedby="bedrock-profile-discovery"
                onClick={() => void reloadProfiles()}
              >
                {reloadingProfiles ? "Reloading AWS profiles…" : "Reload AWS profiles"}
              </button>
            </div>
            {profileIssue && (
              <span id="bedrock-profile-error" className="hosted-field-error">
                {profileIssue.message}
              </span>
            )}
          </div>
        )}

        {draft.mode === "assume_role" && (
          <label className="hosted-bedrock-field" htmlFor="bedrock-role-arn">
            <span>Role ARN</span>
            <input
              id="bedrock-role-arn"
              type="text"
              value={draft.roleArn}
              maxLength={1024}
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              placeholder="arn:aws:iam::123456789012:role/example"
              disabled={operationBusy}
              aria-invalid={roleIssue ? "true" : undefined}
              aria-describedby={roleIssue ? "bedrock-role-arn-error" : undefined}
              onChange={(event) => updateDraft({ roleArn: event.target.value })}
            />
            {roleIssue && (
              <span id="bedrock-role-arn-error" className="hosted-field-error">
                {roleIssue.message}
              </span>
            )}
          </label>
        )}

        {(draft.mode === "static_keychain" ||
          keys.hasAccessKeyId ||
          keys.hasSecretAccessKey ||
          keys.hasSessionToken) && (
          <div
            className="hosted-bedrock-secrets hosted-bedrock-full"
            aria-describedby={keyIssue ? "bedrock-key-pair-error" : "bedrock-key-pair-help"}
          >
            <strong>Stored AWS key values</strong>
            <p id="bedrock-key-pair-help" className="muted small">
              {draft.mode === "static_keychain"
                ? "Key pair authentication requires both primary slots. Add or replace values in a native macOS sheet."
                : "These retained values are not used by the selected authentication method, but you can manage or remove them here."}{" "}
              Values go straight to Corti&apos;s private, owner-only secret store; this window receives presence only.
            </p>
            <ul className="hosted-key-slots">
              {KEY_SLOTS.map(({ slot, label, optional }) => {
                const present =
                  slot === "access_key_id"
                    ? keys.hasAccessKeyId
                    : slot === "secret_access_key"
                      ? keys.hasSecretAccessKey
                      : keys.hasSessionToken;
                return (
                  <li key={slot}>
                    <div>
                      <strong>{label}</strong>
                      <span className={present ? "hosted-configured" : "muted"}>
                        {present ? "Stored" : optional ? "Not set (optional)" : "Not set"}
                      </span>
                    </div>
                    <div className="other-row">
                      <button
                        className="btn-secondary"
                        type="button"
                        disabled={operationBusy}
                        aria-describedby="bedrock-key-action-guidance"
                        onClick={() => void actions.onPromptKey(slot)}
                      >
                        {present ? "Replace…" : "Add…"}
                      </button>
                      {present && (
                        <button
                          className="btn-secondary"
                          type="button"
                          disabled={operationBusy}
                          aria-describedby="bedrock-key-action-guidance"
                          onClick={() => void actions.onClearKey(slot)}
                        >
                          Remove
                        </button>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
            <p id="bedrock-key-action-guidance" className="muted small">
              {keyActionGuidance}
            </p>
            {keyIssue && (
              <p id="bedrock-key-pair-error" className="hosted-field-error">
                {keyIssue.message}
              </p>
            )}
          </div>
        )}

        <label className="hosted-bedrock-field" htmlFor="bedrock-region">
          <span>AWS region</span>
          <select
            id="bedrock-region"
            value={draft.region}
            disabled={operationBusy}
            aria-invalid={regionIssue ? "true" : undefined}
            aria-describedby={regionIssue ? "bedrock-region-error" : "bedrock-region-help"}
            onChange={(event) => updateDraft({ region: event.target.value })}
          >
            <option value="">Select a region…</option>
            {regions.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          <span id="bedrock-region-help" className="muted small">
            Model availability and permissions differ by AWS region.
          </span>
          {regionIssue && (
            <span id="bedrock-region-error" className="hosted-field-error">
              {regionIssue.message}
            </span>
          )}
        </label>

        <label className="hosted-bedrock-field" htmlFor="bedrock-setup-name">
          <span>Setup name</span>
          <input
            id="bedrock-setup-name"
            type="text"
            value={draft.setupName}
            maxLength={1024}
            disabled={operationBusy}
            aria-invalid={setupNameIssue ? "true" : undefined}
            aria-describedby={`bedrock-setup-name-help${setupNameIssue ? " bedrock-setup-name-error" : ""}`}
            onChange={(event) => updateDraft({ setupName: event.target.value })}
          />
          {setupNameIssue && (
            <span id="bedrock-setup-name-error" className="hosted-field-error">
              {setupNameIssue.message}
            </span>
          )}
        </label>

        <p id="bedrock-setup-name-help" className="muted small hosted-bedrock-help hosted-bedrock-full">
          {SETUP_NAME_HELP}
        </p>

        {expiry && <p className="muted small hosted-bedrock-expiry hosted-bedrock-full">{expiry}</p>}

        <div className="hosted-bedrock-actions hosted-bedrock-full">
          <div className="other-row">
            <button
              className="btn-primary"
              type="submit"
              disabled={!canSave}
              aria-describedby="bedrock-save-guidance"
            >
              {submitting ? "Saving Bedrock setup…" : "Save Bedrock setup"}
            </button>
            <button
              className="btn-secondary"
              type="button"
              disabled={!hasSavedSetup || operationBusy}
              aria-describedby="bedrock-clear-guidance"
              onClick={() => void clearSetup()}
            >
              {clearing ? "Clearing Bedrock setup…" : "Clear Bedrock setup"}
            </button>
          </div>
          <p id="bedrock-save-guidance" className="muted small">
            {saveGuidance}
          </p>
          <p id="bedrock-clear-guidance" className="muted small">
            {clearGuidance}
          </p>
        </div>
      </form>

      <section className="hosted-bedrock-models" aria-labelledby="bedrock-models-heading">
        <div>
          <h4 id="bedrock-models-heading">Models</h4>
          <p className="muted small">Load the authenticated catalog for the saved AWS account and region.</p>
        </div>
        <button
          className="btn-secondary"
          type="button"
          disabled={!canRefresh}
          aria-describedby="bedrock-refresh-guidance"
          onClick={() => void refreshModels()}
        >
          {refreshing ? "Refreshing models…" : "Refresh models"}
        </button>
        <p id="bedrock-refresh-guidance" className="muted small">
          {refreshGuidance}
        </p>
        {models.length > 0 && (
          <details className="hosted-catalog">
            <summary>Authenticated model catalog</summary>
            <ul>
              {models.map((model) => (
                <li key={`${model.exact_model_id}:${model.region ?? ""}`}>
                  <code>{model.exact_model_id}</code>
                  <span>
                    {model.region ? `${model.region} · ` : ""}
                    {model.benchmarked_for_live
                      ? "live speed measured"
                      : "live speed not measured · still selectable"}
                    {model.deprecated ? " · deprecated" : ""}
                  </span>
                </li>
              ))}
            </ul>
          </details>
        )}
      </section>
    </div>
  );
}
