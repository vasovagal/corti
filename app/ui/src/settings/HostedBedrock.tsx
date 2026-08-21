import { useEffect, useMemo, useState, type FormEvent } from "react";
import type {
  AwsCredentialMode,
  AwsCredentialOptionsDto,
  AwsKeySlot,
  BedrockCredentialDto,
  HostedCredentialState,
  HostedProviderScope,
} from "../lib/api";
import {
  awsCredentialModeDescription,
  awsCredentialModeLabel,
  bedrockModeRequirements,
  sessionExpiryLabel,
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

export interface BedrockActions {
  busy: boolean;
  onMode: (
    mode: AwsCredentialMode,
    profile: string | null,
    roleArn: string | null,
  ) => Promise<boolean>;
  onScopeRegion: (region: string | null, alias: string | null) => Promise<boolean>;
  onPromptKey: (slot: AwsKeySlot) => Promise<boolean>;
  onClearKey: (slot: AwsKeySlot) => Promise<boolean>;
}

/**
 * Bedrock's credential pane. Every field here is non-secret configuration; the key pair is reached only
 * through the native secure-entry sheet, so no input on this page ever holds a credential.
 */
export function HostedBedrockCredentials({
  bedrock,
  scope,
  credential,
  options,
  actions,
}: {
  bedrock: BedrockCredentialDto;
  scope?: HostedProviderScope;
  credential: HostedCredentialState;
  options: AwsCredentialOptionsDto | null;
  actions: BedrockActions;
}) {
  const [mode, setMode] = useState<AwsCredentialMode>(bedrock.mode);
  const [profile, setProfile] = useState(bedrock.profile ?? "");
  const [roleArn, setRoleArn] = useState(bedrock.role_arn ?? "");
  const [region, setRegion] = useState(scope?.region ?? "");

  useEffect(() => {
    setMode(bedrock.mode);
    setProfile(bedrock.profile ?? "");
    setRoleArn(bedrock.role_arn ?? "");
  }, [bedrock.mode, bedrock.profile, bedrock.role_arn]);

  useEffect(() => {
    setRegion(scope?.region ?? "");
  }, [scope?.region]);

  const keys = {
    hasAccessKeyId: bedrock.has_access_key_id,
    hasSecretAccessKey: bedrock.has_secret_access_key,
    hasSessionToken: bedrock.has_session_token,
  };
  const missing = bedrockModeRequirements(mode, { profile, roleArn, region }, keys);
  const changed =
    mode !== bedrock.mode ||
    profile.trim() !== (bedrock.profile ?? "") ||
    roleArn.trim() !== (bedrock.role_arn ?? "");
  const regionChanged = region.trim() !== (scope?.region ?? "");
  const regions = useMemo(() => regionOptions(BEDROCK_REGIONS, region), [region]);
  const expiry =
    credential.state === "ready"
      ? sessionExpiryLabel(credential.expires_at_unix_ms, Date.now())
      : null;
  const profiles = options?.profiles ?? [];
  // Assume-role resolves a base credential first, and that base can be a named profile, so the field is
  // offered there too — optional rather than required.
  const showsProfile = mode === "profile" || mode === "sso" || mode === "assume_role";
  const profileLabel = mode === "assume_role" ? "Base AWS profile (optional)" : "AWS profile";

  function save(event: FormEvent) {
    event.preventDefault();
    if (!changed || missing.length > 0 || actions.busy) return;
    void actions.onMode(mode, profile.trim() || null, roleArn.trim() || null);
  }

  return (
    <div className="hosted-bedrock">
      <fieldset className="hosted-bedrock-modes">
        <legend>AWS credentials</legend>
        <div className="hosted-segmented" role="radiogroup" aria-label="AWS credential mode">
          {MODES.map((candidate) => (
            <label key={candidate} className={mode === candidate ? "is-selected" : undefined}>
              <input
                type="radio"
                name="bedrock-credential-mode"
                value={candidate}
                checked={mode === candidate}
                disabled={actions.busy}
                onChange={() => setMode(candidate)}
              />
              <span>{awsCredentialModeLabel(candidate)}</span>
            </label>
          ))}
        </div>
        <p className="muted small">{awsCredentialModeDescription(mode)}</p>
      </fieldset>

      <form className="hosted-bedrock-fields" onSubmit={save}>
        {showsProfile &&
          (profiles.length > 0 ? (
            <label>
              <span>{profileLabel}</span>
              <select
                value={profiles.includes(profile) ? profile : ""}
                disabled={actions.busy}
                onChange={(event) => setProfile(event.target.value)}
              >
                <option value="">
                  {mode === "assume_role" ? "Default chain" : "Select a profile…"}
                </option>
                {profiles.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
          ) : (
            <label>
              <span>{profileLabel}</span>
              <input
                type="text"
                value={profile}
                maxLength={256}
                autoCapitalize="none"
                autoCorrect="off"
                placeholder="No profiles found in ~/.aws"
                disabled={actions.busy}
                onChange={(event) => setProfile(event.target.value)}
              />
            </label>
          ))}

        {mode === "assume_role" && (
          <label>
            <span>Role ARN</span>
            <input
              type="text"
              value={roleArn}
              maxLength={1024}
              autoCapitalize="none"
              autoCorrect="off"
              placeholder="arn:aws:iam::123456789012:role/example"
              disabled={actions.busy}
              onChange={(event) => setRoleArn(event.target.value)}
            />
          </label>
        )}

        {missing.length > 0 && (
          <p className="hosted-field-error">Still needs {missing.join(", ")}.</p>
        )}

        <button
          className="btn-secondary"
          type="submit"
          disabled={!changed || missing.length > 0 || actions.busy}
        >
          Save credential mode
        </button>
      </form>

      {mode === "static_keychain" && (
        <div className="hosted-native-auth">
          <p className="muted small">
            Values are typed into a native macOS sheet and written straight to the Keychain. This window
            never receives them, and no browser field accepts a key.
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
                      disabled={actions.busy}
                      onClick={() => void actions.onPromptKey(slot)}
                    >
                      {present ? "Replace…" : "Add…"}
                    </button>
                    {present && (
                      <button
                        className="btn-secondary"
                        type="button"
                        disabled={actions.busy}
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
        </div>
      )}

      {credential.state === "rejected" && mode === "sso" && (
        <p className="callout small">
          The cached SSO token looks expired. Run{" "}
          <code>aws sso login --profile {bedrock.profile ?? "your-profile"}</code>, then refresh.
        </p>
      )}

      {expiry && <p className="muted small hosted-bedrock-expiry">{expiry}</p>}

      <BedrockRegionField
        region={region}
        regions={regions}
        changed={regionChanged}
        alias={scope?.alias ?? null}
        busy={actions.busy}
        onRegion={setRegion}
        onSave={() => void actions.onScopeRegion(region.trim() || null, scope?.alias ?? null)}
      />
    </div>
  );
}

/** Region lives on the connection scope, not on the credential, because the catalog is region-scoped. */
function BedrockRegionField({
  region,
  regions,
  changed,
  alias,
  busy,
  onRegion,
  onSave,
}: {
  region: string;
  regions: string[];
  changed: boolean;
  alias: string | null;
  busy: boolean;
  onRegion: (value: string) => void;
  onSave: () => void;
}) {
  return (
    <form
      className="hosted-bedrock-region"
      onSubmit={(event) => {
        event.preventDefault();
        if (changed && !busy) onSave();
      }}
    >
      <label>
        <span>Bedrock region</span>
        <select value={region} disabled={busy} onChange={(event) => onRegion(event.target.value)}>
          <option value="">Select a region…</option>
          {regions.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
      </label>
      <p className="muted small">
        Model availability differs per region. {alias ? `Saved as “${alias}”.` : "Add a connection label below."}
      </p>
      <button className="btn-secondary" type="submit" disabled={!changed || busy}>
        Save region
      </button>
    </form>
  );
}
