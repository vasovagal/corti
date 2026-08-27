import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelChatGptDeviceLogin,
  clearBedrockSetup,
  clearProviderSecret,
  getHostedSettings,
  listAwsCredentialOptions,
  onHostedStateChanged,
  openChatGptDeviceLogin,
  patchHostedSettings,
  promptForProviderSecret,
  refreshHostedProvider,
  replaceHostedWordBank,
  saveBedrockSetup,
  setHostedVertexModels,
  setHostedPinnedQuestion,
  signOutChatGptSubscription,
  startChatGptDeviceLogin,
  updateHostedProviderScope,
  updateHostedSteering,
  type AwsCredentialOptionsDto,
  type AwsKeySlot,
  type HostedMutationResult,
  type HostedPatchInput,
  type HostedProviderScopeUpdate,
  type HostedSettingsDto,
  type SecretSlotRequest,
} from "../lib/api";
import { shouldInstallHostedSettings } from "../lib/liveHosted";
import {
  bedrockCredentialGuidance,
  bedrockInvalidMessage,
  bedrockRefreshFailureGuidance,
  credentialSummary,
  providerPresentation,
  type NormalizedBedrockSetup,
} from "../lib/hosted";
import type {
  AwsProfileDiscoveryState,
  BedrockActionOutcome,
} from "./HostedBedrock";
import { HostedDialog, HostedSwitch } from "./HostedCommon";
import { HostedLanguagePreferences } from "./HostedLanguage";
import { HostedLanes } from "./HostedLanes";
import { HostedProviders } from "./HostedProviders";

export type HostedPreferencesSection = "overview" | "provider" | "routing" | "language" | "advanced";

export default function HostedPreferences({
  section,
  onNavigate,
}: {
  section: HostedPreferencesSection;
  onNavigate: (section: HostedPreferencesSection) => void;
}) {
  const [settings, setSettings] = useState<HostedSettingsDto | null>(null);
  const settingsRef = useRef<HostedSettingsDto | null>(null);
  const busyRef = useRef(false);
  const [busy, setBusy] = useState("");
  const [status, setStatus] = useState("");
  const [statusAction, setStatusAction] = useState<{
    section: HostedPreferencesSection;
    label: string;
  } | null>(null);
  const [loadError, setLoadError] = useState("");
  const [masterDisclosure, setMasterDisclosure] = useState(false);
  const [awsOptions, setAwsOptions] = useState<AwsCredentialOptionsDto | null>(null);
  const [awsProfileDiscoveryState, setAwsProfileDiscoveryState] =
    useState<AwsProfileDiscoveryState>("loading");
  const awsOptionsRequestRef = useRef(0);
  const [bedrockResetToken, setBedrockResetToken] = useState(0);

  // Profile names and secret presence are read separately from the settings document: they describe the
  // machine, not the saved preferences, and an older backend simply leaves them null.
  const refreshAwsOptions = useCallback(async (): Promise<boolean> => {
    const request = ++awsOptionsRequestRef.current;
    setAwsProfileDiscoveryState("loading");
    try {
      const options = await listAwsCredentialOptions();
      if (request === awsOptionsRequestRef.current) {
        setAwsOptions(options);
        setAwsProfileDiscoveryState("loaded");
      }
      return true;
    } catch {
      if (request === awsOptionsRequestRef.current) {
        setAwsOptions(null);
        setAwsProfileDiscoveryState("error");
      }
      return false;
    }
  }, []);

  const installSettings = useCallback((next: HostedSettingsDto) => {
    if (!shouldInstallHostedSettings(settingsRef.current, next)) return;
    settingsRef.current = next;
    setSettings(next);
    setLoadError("");
  }, []);

  const reload = useCallback(async () => {
    try {
      installSettings(await getHostedSettings());
    } catch (error) {
      setLoadError(`Failed to load hosted preferences: ${String(error)}`);
    }
  }, [installSettings]);

  useEffect(() => {
    void refreshAwsOptions();
  }, [refreshAwsOptions]);

  useEffect(() => {
    let active = true;
    let stop: (() => void) | undefined;
    const refreshIfActive = async () => {
      if (!active) return;
      try {
        const next = await getHostedSettings();
        if (active) installSettings(next);
      } catch (error) {
        if (active) setLoadError(`Failed to load hosted preferences: ${String(error)}`);
      }
    };
    onHostedStateChanged(() => void refreshIfActive())
      .then((unlisten) => {
        if (active) stop = unlisten;
        else unlisten();
      })
      .catch(() => {
        // Commands still return coherent snapshots if event subscription is unavailable.
      });
    void refreshIfActive();
    return () => {
      active = false;
      stop?.();
    };
  }, [installSettings]);

  function acceptMutation(result: HostedMutationResult, success: string): boolean {
    installSettings(result.settings);
    switch (result.status) {
      case "applied":
        setStatus(success);
        return true;
      case "unchanged":
        setStatus("No canonical change; the backend kept the current settings.");
        return true;
      case "conflict":
        setStatus("Hosted settings changed elsewhere. The latest state is loaded; review and try again.");
        return false;
      case "invalid":
        setStatus(`Nothing was saved. ${bedrockInvalidMessage(result.field, result.reason)}`);
        return false;
      case "disabled_for_session":
        setStatus(
          `Disabled for this session, but persistence failed (${result.code.replace(/_/gu, " ")}).`,
        );
        return true;
    }
  }

  async function runMutation(
    label: string,
    success: string,
    operation: (observedRevision: number) => Promise<HostedMutationResult>,
  ): Promise<boolean> {
    const current = settingsRef.current;
    if (!current || busyRef.current) return false;
    busyRef.current = true;
    setBusy(label);
    setStatus("");
    setStatusAction(null);
    try {
      return acceptMutation(await operation(current.state_revision), success);
    } catch (error) {
      setStatus(`${label} failed: ${String(error)}`);
      return false;
    } finally {
      busyRef.current = false;
      setBusy("");
    }
  }

  const onPatch = (patch: HostedPatchInput, success: string) =>
    runMutation("Update", success, (revision) => patchHostedSettings(revision, patch));

  const onSteering = (text: string) =>
    runMutation("Steering update", "Default steering saved for the next request.", (revision) =>
      updateHostedSteering(revision, text, true),
    );

  const onWordBank = (entries: string[]) =>
    runMutation("Word-bank update", "Unique-word bank saved and affected requests fenced.", (revision) =>
      replaceHostedWordBank(revision, entries),
    );

  const onScope = (update: HostedProviderScopeUpdate) =>
    runMutation("Provider setup update", "Provider setup saved. Refresh this provider to check access and load its models.", (revision) =>
      updateHostedProviderScope(revision, update),
    );

  async function runBedrockMutation(
    label: string,
    success: string,
    operation: (observedRevision: number) => Promise<HostedMutationResult>,
  ): Promise<BedrockActionOutcome> {
    const current = settingsRef.current;
    if (!current || busyRef.current) return { status: "failed" };
    busyRef.current = true;
    setBusy(label);
    setStatus("");
    setStatusAction(null);
    try {
      const result = await operation(current.state_revision);
      const accepted = acceptMutation(result, success);
      if (
        result.status === "applied" ||
        result.status === "unchanged" ||
        result.status === "conflict"
      ) {
        setBedrockResetToken((token) => token + 1);
      }
      if (result.status === "invalid") {
        return { status: "invalid", field: result.field, reason: result.reason };
      }
      if (result.status === "conflict") return { status: "conflict" };
      return accepted ? { status: "accepted" } : { status: "failed" };
    } catch (error) {
      setStatus(`${label} failed: ${String(error)}`);
      return { status: "failed" };
    } finally {
      busyRef.current = false;
      setBusy("");
    }
  }

  const onBedrockSetup = (setup: NormalizedBedrockSetup) =>
    runBedrockMutation(
      "Bedrock setup",
      "Bedrock setup saved. Refresh models to check AWS access and load the regional catalog.",
      (revision) =>
        saveBedrockSetup({
          observed_state_revision: revision,
          mode: setup.mode,
          profile: setup.profile,
          role_arn: setup.roleArn,
          region: setup.region,
          setup_name: setup.setupName,
        }),
    );

  const onClearBedrockSetup = () =>
    runBedrockMutation(
      "Clear Bedrock setup",
      "Bedrock setup cleared. Stored AWS key values were left unchanged.",
      (revision) => clearBedrockSetup({ observed_state_revision: revision }),
    );

  const onVertexModels = (models: string[]) =>
    runMutation("Vertex model update", "Vertex model list saved; refresh the catalog to use it.", (revision) =>
      setHostedVertexModels(revision, models),
    );

  /// The sheet is native; only its outcome comes back, never the value the user typed.
  async function onPromptSecret(request: SecretSlotRequest): Promise<boolean> {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy("Secure entry");
    setStatus("");
    setStatusAction(null);
    try {
      const outcome = await promptForProviderSecret(request);
      setStatus(
        outcome === "stored"
          ? "Credential saved in Corti's private secret store. Refresh this provider below to check access and load its models."
          : outcome === "rejected"
            ? "That value cannot be a credential; nothing was stored."
            : "Cancelled; nothing was stored.",
      );
      return outcome === "stored";
    } catch (error) {
      setStatus(`Secure entry failed: ${String(error)}`);
      return false;
    } finally {
      await refreshAwsOptions();
      await reload();
      busyRef.current = false;
      setBusy("");
    }
  }

  async function onClearSecret(request: SecretSlotRequest): Promise<boolean> {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy("Remove credential");
    setStatus("");
    setStatusAction(null);
    try {
      await clearProviderSecret(request);
      setStatus("Removed from Corti's private secret store.");
      return true;
    } catch (error) {
      setStatus(`Removal failed: ${String(error)}`);
      return false;
    } finally {
      await refreshAwsOptions();
      await reload();
      busyRef.current = false;
      setBusy("");
    }
  }

  async function onChatGptAction(
    label: string,
    success: string,
    operation: () => Promise<unknown>,
  ): Promise<boolean> {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy(label);
    setStatus("");
    setStatusAction(null);
    try {
      await operation();
      await reload();
      setStatus(success);
      return true;
    } catch (error) {
      setStatus(`${label} failed: ${String(error)}`);
      await reload();
      return false;
    } finally {
      busyRef.current = false;
      setBusy("");
    }
  }

  const onStartChatGpt = () =>
    onChatGptAction(
      "ChatGPT sign-in",
      "Open the authorization page and enter the displayed code. Corti will finish in the background.",
      startChatGptDeviceLogin,
    );

  const onCancelChatGpt = () =>
    onChatGptAction(
      "Cancel ChatGPT sign-in",
      "ChatGPT sign-in cancelled; no new credential was stored.",
      cancelChatGptDeviceLogin,
    );

  const onSignOutChatGpt = () =>
    onChatGptAction(
      "ChatGPT sign-out",
      "Corti's ChatGPT credential was removed from its private secret store.",
      signOutChatGptSubscription,
    );

  async function onOpenChatGptLogin(): Promise<boolean> {
    try {
      await openChatGptDeviceLogin();
      return true;
    } catch (error) {
      setStatus(`Could not open ChatGPT authorization: ${String(error)}`);
      return false;
    }
  }

  async function onRefreshProvider(provider: string, transport: string): Promise<boolean> {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy("Provider refresh");
    setStatus("");
    setStatusAction(null);
    try {
      const refreshed = await refreshHostedProvider(provider, transport);
      await reload();
      if (refreshed.credential.state === "ready" && refreshed.models.length > 0) {
        setStatus(
          `Provider ready — ${refreshed.models.length} exact ${refreshed.models.length === 1 ? "model" : "models"} available. Choose one for each rewrite mode you want.`,
        );
        setStatusAction({ section: "routing", label: "Choose rewrite models" });
      } else if (refreshed.credential.state !== "ready") {
        if (transport === "bedrock_runtime") {
          const current = settingsRef.current;
          setStatus(
            bedrockCredentialGuidance(
              current?.bedrock.mode ?? "default_chain",
              refreshed.credential,
              current?.bedrock.profile ?? null,
            ) ?? "The saved Bedrock setup still needs attention.",
          );
        } else {
          setStatus("The provider still needs attention. Review its credential and provider setup below.");
        }
      } else {
        setStatus("The provider connected, but returned no usable models. Review its account, project/region, and model access, then refresh again.");
      }
      return true;
    } catch (error) {
      if (transport === "bedrock_runtime") {
        const current = settingsRef.current;
        const recovery = bedrockRefreshFailureGuidance(
          current?.bedrock.mode ?? "default_chain",
          error,
          current?.bedrock.profile ?? null,
        );
        setStatus(`Bedrock model refresh failed. ${recovery}`);
      } else {
        setStatus(`Provider refresh failed: ${String(error)}. Check the credential, provider setup, network, quota, and billing, then try again.`);
      }
      await reload();
      return false;
    } finally {
      busyRef.current = false;
      setBusy("");
    }
  }

  async function onPinned(template: string): Promise<boolean> {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy("Pinned question update");
    setStatus("");
    setStatusAction(null);
    try {
      const current = settingsRef.current;
      if (!current) return false;
      const result = await setHostedPinnedQuestion(current.state_revision, template);
      const accepted = acceptMutation(
        result,
        template.trim() ? "Pinned question template saved." : "Pinned question template cleared.",
      );
      return accepted;
    } catch (error) {
      setStatus(`Pinned question update failed: ${String(error)}`);
      return false;
    } finally {
      busyRef.current = false;
      setBusy("");
    }
  }

  async function acknowledgeAndEnableMaster() {
    const current = settingsRef.current;
    if (!current || busyRef.current) return;
    busyRef.current = true;
    setBusy("Master enable");
    setStatus("");
    setStatusAction(null);
    try {
      const acknowledged = await patchHostedSettings(current.state_revision, {
        kind: "set_egress_acknowledged",
        acknowledged: true,
      });
      if (!acceptMutation(acknowledged, "Hosted egress disclosure acknowledged.")) return;
      const afterAcknowledgement = settingsRef.current;
      if (!afterAcknowledgement) return;
      const enabled = await patchHostedSettings(afterAcknowledgement.state_revision, {
        kind: "set_master",
        enabled: true,
      });
      if (acceptMutation(enabled, "Hosted rewrite Master enabled. Lanes remain independently controlled.")) {
        setMasterDisclosure(false);
      }
    } catch (error) {
      setStatus(`Master enable failed: ${String(error)}`);
    } finally {
      busyRef.current = false;
      setBusy("");
    }
  }

  if (!settings) {
    return (
      <section className="card hosted-loading" aria-live="polite">
        <h2>Hosted rewrite</h2>
        <p className="muted">{loadError || "Loading secret-free provider settings…"}</p>
        {loadError && (
          <button className="btn-secondary" type="button" onClick={() => void reload()}>
            Try again
          </button>
        )}
      </section>
    );
  }

  const isBusy = Boolean(busy);
  const languageActions = {
    busy: isBusy,
    onSteering,
    onWordBank,
    onPinned,
    onPatch,
  };
  const providerActions = {
    busy: isBusy,
    onRefresh: onRefreshProvider,
    onScope,
    onPromptSecret,
    onClearSecret,
    onStartChatGpt,
    onCancelChatGpt,
    onSignOutChatGpt,
    onOpenChatGptLogin,
    onVertexModels,
    bedrock: {
      busy: isBusy,
      onSave: onBedrockSetup,
      onClear: onClearBedrockSetup,
      onPromptKey: (slot: AwsKeySlot) => onPromptSecret({ provider: "aws", slot }),
      onClearKey: (slot: AwsKeySlot) => onClearSecret({ provider: "aws", slot }),
      onReloadProfiles: refreshAwsOptions,
      onRefresh: () => onRefreshProvider("amazon", "bedrock_runtime"),
    },
  };

  return (
    <div className={`hosted-stack hosted-preferences-${section}`}>
      {(status || busy || loadError) && (
        <div
          className={`hosted-status-banner${loadError ? " hosted-status-error" : ""}`}
          role={loadError ? "alert" : "status"}
          aria-live="polite"
        >
          <span>{loadError || (busy ? `${busy}…` : status)}</span>
          {!loadError && !busy && statusAction && (
            <button
              className="btn-secondary"
              type="button"
              onClick={() => onNavigate(statusAction.section)}
            >
              {statusAction.label}
            </button>
          )}
        </div>
      )}

      <div className="hosted-preference-pane" hidden={section !== "overview"}>
          <HostedSetupGuide settings={settings} onNavigate={onNavigate} />

          <section className="card hosted-egress-card" aria-labelledby="hosted-master-heading">
            <div className="hosted-master-layout">
              <div>
                <p className="hosted-eyebrow">Privacy boundary</p>
                <h2 id="hosted-master-heading">Text egress</h2>
                <p className="lead">
                  One master control holds every hosted request. Local transcription and raw transcript filing
                  continue whether this is on or off.
                </p>
              </div>
              <HostedSwitch
                compact
                label="Master"
                description={settings.control.master_enabled ? "Hosted text may leave" : "All hosted text held"}
                checked={settings.control.master_enabled}
                disabled={isBusy}
                onChange={(enabled) => {
                  if (!enabled) {
                    void onPatch(
                      { kind: "set_master", enabled: false },
                      "Master disabled. In-flight work is canceled best effort; late text will not apply.",
                    );
                  } else if (settings.control.egress_acknowledged) {
                    void onPatch(
                      { kind: "set_master", enabled: true },
                      "Master enabled. Each rewrite mode remains independently controlled.",
                    );
                  } else {
                    setMasterDisclosure(true);
                  }
                }}
              />
            </div>

            <div className="hosted-privacy-highlight">
              <strong>Audio always stays on this Mac.</strong>
              <span>Hosted providers receive text only, and only from a mode you explicitly enable.</span>
            </div>

            <details className="hosted-privacy-details">
              <summary>What can leave this Mac?</summary>
              <ul className="hosted-egress-facts">
                <li>Selected transcript text, saved spellings, steering, and questions may be sent.</li>
                <li>Connecting or refreshing a provider never enables Master or a rewrite mode.</li>
                <li>Cancellation is best effort after dispatch; provider billing may still occur.</li>
                <li>Provider retention, residency, and account terms continue to apply.</li>
              </ul>
            </details>

            <div className="hosted-master-state">
              <span className={settings.control.master_enabled ? "hosted-state-on" : "hosted-state-off"}>
                {settings.control.master_enabled ? "Master on" : "Master off"}
              </span>
              <span>
                Disclosure {settings.control.egress_acknowledged ? "acknowledged" : "not acknowledged"}
              </span>
              {!settings.control.master_enabled && settings.control.egress_acknowledged && (
                <button
                  className="btn-quiet"
                  type="button"
                  disabled={isBusy}
                  onClick={() =>
                    void onPatch(
                      { kind: "set_egress_acknowledged", acknowledged: false },
                      "Hosted egress disclosure acknowledgement reset.",
                    )
                  }
                >
                  Reset acknowledgement
                </button>
              )}
            </div>
          </section>
      </div>

      <div className="hosted-preference-pane" hidden={section !== "provider"}>
        <HostedProviders
          providers={settings.providers}
          scopes={settings.scopes}
          bedrock={settings.bedrock}
          vertexModels={settings.vertex_models}
          awsOptions={awsOptions}
          awsProfileDiscoveryState={awsProfileDiscoveryState}
          bedrockResetToken={bedrockResetToken}
          preferredSelection={settings.control.final_lane.selection}
          actions={providerActions}
        />
      </div>
      <div className="hosted-preference-pane" hidden={section !== "routing"}>
        <HostedLanes settings={settings} busy={isBusy} onPatch={onPatch} />
      </div>
      <div className="hosted-preference-pane" hidden={section !== "language"}>
        <HostedLanguagePreferences settings={settings} actions={languageActions} />
      </div>
      <div className="hosted-preference-pane" hidden={section !== "advanced"}>
        <HostedDiagnostics settings={settings} busy={isBusy} onPatch={onPatch} />
        <HostedTruthDisclosure finalDeadline={settings.final_deadline_seconds} />
      </div>

      <HostedDialog
        open={masterDisclosure}
        title="Enable hosted transcript egress?"
        confirmLabel="Acknowledge and enable"
        busy={isBusy}
        onCancel={() => setMasterDisclosure(false)}
        onConfirm={() => void acknowledgeAndEnableMaster()}
      >
        <p className="hosted-disclosure-statement">
          Selected transcript text, unique words, steering, and questions will leave this Mac. Audio is not
          sent by hosted rewrite.
        </p>
        <p>
          Paid calls use the exact provider/model selected in each enabled mode. Provider retention and account
          terms apply, and cancellation may not prevent billing after dispatch.
        </p>
      </HostedDialog>
    </div>
  );
}

function HostedSetupGuide({
  settings,
  onNavigate,
}: {
  settings: HostedSettingsDto;
  onNavigate: (section: HostedPreferencesSection) => void;
}) {
  const finalSelection = settings.control.final_lane.selection;
  const selectedProvider = settings.providers.find(
    (candidate) =>
      candidate.descriptor.provider === finalSelection.provider &&
      candidate.descriptor.transport === finalSelection.transport,
  );
  const provider =
    selectedProvider ??
    settings.providers.find(
      (candidate) => candidate.credential.state === "ready" && candidate.models.length > 0,
    ) ??
    settings.providers.find((candidate) => candidate.credential.state === "ready");
  const providerName = provider
    ? providerPresentation(provider.descriptor.provider, provider.descriptor.transport).shortName
    : null;
  const providerState = provider
    ? credentialSummary(provider.credential, provider.descriptor.transport).label
    : "Not chosen";
  const providerReady = provider?.credential.state === "ready" && provider.models.length > 0;
  const modelName = finalSelection.model;
  const finalReady = Boolean(modelName && settings.control.final_lane.enabled);

  return (
    <section className="card hosted-guide-card" aria-labelledby="hosted-guide-heading">
      <div className="hosted-card-head">
        <div>
          <p className="hosted-eyebrow">Recommended path</p>
          <h2 id="hosted-guide-heading">One provider. Final rewrite first.</h2>
          <p>
            Most people need only a final cleanup pass. Live cleanup and automatic questions are optional and
            can use the same provider later.
          </p>
        </div>
      </div>
      <ol className="hosted-setup-list">
        <li className={providerReady ? "is-complete" : undefined}>
          <span className="hosted-step-number">1</span>
          <div>
            <strong>Connect a provider</strong>
            <p>{providerName ? `${providerName} · ${providerState}` : "Choose the API account you already trust and bill."}</p>
          </div>
          <button className="btn-secondary" type="button" onClick={() => onNavigate("provider")}>
            {providerReady
              ? "Review"
              : provider?.credential.state === "ready"
                ? "Load models"
                : provider
                  ? "Connect"
                  : "Choose"}
          </button>
        </li>
        <li className={finalReady ? "is-complete" : undefined}>
          <span className="hosted-step-number">2</span>
          <div>
            <strong>Configure Final rewrite</strong>
            <p>
              {finalReady
                ? `${modelName} · enabled`
                : modelName
                  ? `${modelName} selected · enable Final rewrite next`
                  : "Pick one exact model, then enable the final cleanup pass."}
            </p>
          </div>
          <button className="btn-secondary" type="button" onClick={() => onNavigate("routing")}>
            {finalReady ? "Review" : "Configure"}
          </button>
        </li>
        <li className={settings.control.master_enabled ? "is-complete" : undefined}>
          <span className="hosted-step-number">3</span>
          <div>
            <strong>Allow text egress</strong>
            <p>{settings.control.master_enabled ? "Master is on." : "Review the boundary below, then enable Master."}</p>
          </div>
          <span className={settings.control.master_enabled ? "hosted-state-on" : "hosted-state-off"}>
            {settings.control.master_enabled ? "On" : "Off"}
          </span>
        </li>
      </ol>
    </section>
  );
}

function HostedDiagnostics({
  settings,
  busy,
  onPatch,
}: {
  settings: HostedSettingsDto;
  busy: boolean;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
}) {
  const update = (history: boolean, live: boolean) =>
    onPatch(
      {
        kind: "set_display_preferences",
        show_history_diagnostics: history,
        show_live_metrics_by_default: live,
      },
      "Diagnostics defaults saved. Request generations and cache keys were not changed.",
    );
  return (
    <section className="card hosted-diagnostics-card" aria-labelledby="hosted-diagnostics-heading">
      <div className="hosted-card-head">
        <div>
          <p className="hosted-eyebrow">Display only</p>
          <h2 id="hosted-diagnostics-heading">Diagnostics defaults</h2>
          <p>Both defaults are off until explicitly enabled and do not alter requests or cache identity.</p>
        </div>
        <span className="hosted-readonly-value">Final deadline · {settings.final_deadline_seconds}s</span>
      </div>
      <div className="hosted-diagnostics-grid">
        <HostedSwitch
          label="Show history diagnostics"
          description="Reveal content-free provider/model, usage, cache, cost, and timing fields in history."
          checked={settings.show_history_diagnostics}
          disabled={busy}
          onChange={(enabled) => void update(enabled, settings.show_live_metrics_by_default)}
        />
        <HostedSwitch
          label="Show live metrics by default"
          description="Open live Details with queue/auth/TTFB/TTFT/stream/total metrics visible."
          checked={settings.show_live_metrics_by_default}
          disabled={busy}
          onChange={(enabled) => void update(settings.show_history_diagnostics, enabled)}
        />
      </div>
      <p className="muted small">
        Diagnostics contain typed, content-free metadata. They do not include transcript, prompts, steering,
        word-bank text, questions, answers, credentials, provider bodies, or account/project identifiers.
      </p>
    </section>
  );
}

function HostedTruthDisclosure({ finalDeadline }: { finalDeadline: number }) {
  return (
    <section className="card hosted-truth-card" aria-labelledby="hosted-truth-heading">
      <div className="hosted-card-head">
        <div>
          <p className="hosted-eyebrow">Before enabling</p>
          <h2 id="hosted-truth-heading">What the catalog does—and does not—promise</h2>
        </div>
      </div>
      <dl className="hosted-truth-grid">
        <div>
          <dt>Latency</dt>
          <dd>
            Live has hard fallback deadlines, not a speed promise. Final can wait up to {finalDeadline} seconds;
            raw text remains recoverable.
          </dd>
        </div>
        <div>
          <dt>Quality</dt>
          <dd>
            Account availability and structured output do not prove rewrite quality or speed. Benchmarks are
            guidance, not a lock: you may try any eligible model, and Live keeps raw text on delay or failure.
          </dd>
        </div>
        <div>
          <dt>Cost</dt>
          <dd>
            Direct APIs are metered. Estimates appear only with matching reviewed tariffs. Unknown,
            subscription, and local-hit costs are never shown as $0.00.
          </dd>
        </div>
        <div>
          <dt>Cache</dt>
          <dd>
            Corti's encrypted exact cache and provider caching are separate. A local hit means no provider
            request—not a zero-priced request.
          </dd>
        </div>
        <div>
          <dt>Retention</dt>
          <dd>
            Provider terms, training controls, residency, and regulated-data eligibility remain account/model
            responsibilities. Corti cannot purge remote storage.
          </dd>
        </div>
      </dl>
    </section>
  );
}
