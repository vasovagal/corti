import { useCallback, useEffect, useRef, useState } from "react";
import {
  getHostedSettings,
  onHostedStateChanged,
  patchHostedSettings,
  refreshHostedProvider,
  replaceHostedWordBank,
  setHostedPinnedQuestion,
  updateHostedProviderScope,
  updateHostedSteering,
  type HostedMutationResult,
  type HostedPatchInput,
  type HostedProviderScopeUpdate,
  type HostedSettingsDto,
} from "../lib/api";
import { HostedDialog, HostedSwitch } from "./HostedCommon";
import { HostedLanguagePreferences } from "./HostedLanguage";
import { HostedLanes } from "./HostedLanes";
import { HostedProviders } from "./HostedProviders";

export default function HostedPreferences() {
  const [settings, setSettings] = useState<HostedSettingsDto | null>(null);
  const settingsRef = useRef<HostedSettingsDto | null>(null);
  const busyRef = useRef(false);
  const [busy, setBusy] = useState("");
  const [status, setStatus] = useState("");
  const [loadError, setLoadError] = useState("");
  const [masterDisclosure, setMasterDisclosure] = useState(false);

  const installSettings = useCallback((next: HostedSettingsDto) => {
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
    runMutation("Connection update", "Connection scope saved; Master and lanes were not changed.", (revision) =>
      updateHostedProviderScope(revision, update),
    );

  async function onRefreshProvider(provider: string, transport: string): Promise<boolean> {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy("Provider refresh");
    setStatus("");
    try {
      await refreshHostedProvider(provider, transport);
      await reload();
      setStatus("Credential state and authenticated catalog refreshed. No lane was enabled.");
      return true;
    } catch (error) {
      setStatus(`Provider refresh failed: ${String(error)}`);
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
    try {
      await setHostedPinnedQuestion(template);
      await reload();
      setStatus(template.trim() ? "Pinned question template saved." : "Pinned question template cleared.");
      return true;
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
    codexApproved: settings.control.codex_experimental_approved,
    onRefresh: onRefreshProvider,
    onScope,
    onPatch,
  };

  return (
    <div className="hosted-stack">
      <section className="card hosted-egress-card" aria-labelledby="hosted-master-heading">
        <div className="hosted-master-layout">
          <div>
            <p className="hosted-eyebrow">Privacy boundary</p>
            <h2 id="hosted-master-heading">Hosted rewrite</h2>
            <p className="lead">
              Optional paid text cleanup and questions after ASR. Raw transcript publication never waits for
              this feature.
            </p>
          </div>
          <HostedSwitch
            compact
            label="Master"
            description={settings.control.master_enabled ? "Hosted egress allowed" : "All hosted egress held"}
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
                  "Master enabled. Each lane remains independently controlled.",
                );
              } else {
                setMasterDisclosure(true);
              }
            }}
          />
        </div>
        <ul className="hosted-egress-facts">
          <li><strong>Audio never leaves through hosted rewrite.</strong> Providers receive selected text only.</li>
          <li>Transcript text, word-bank entries, steering, and questions may leave this Mac.</li>
          <li>Connecting a provider never enables Master or any lane.</li>
          <li>Cancellation is best effort after dispatch; provider billing may still occur.</li>
        </ul>
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

      {(status || busy || loadError) && (
        <div
          className={`hosted-status-banner${loadError ? " hosted-status-error" : ""}`}
          role={loadError ? "alert" : "status"}
          aria-live="polite"
        >
          {loadError || (busy ? `${busy}…` : status)}
        </div>
      )}

      <HostedProviders providers={settings.providers} scopes={settings.scopes} actions={providerActions} />
      <HostedLanes settings={settings} busy={isBusy} onPatch={onPatch} />
      <HostedLanguagePreferences settings={settings} actions={languageActions} />
      <HostedDiagnostics settings={settings} busy={isBusy} onPatch={onPatch} />
      <HostedTruthDisclosure finalDeadline={settings.final_deadline_seconds} />

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
          Paid calls use the exact provider/model selected in each enabled lane. Provider retention and account
          terms apply, and cancellation may not prevent billing after dispatch.
        </p>
      </HostedDialog>
    </div>
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
            Account availability and structured output do not prove rewrite quality. Only backend-marked live
            benchmarks unlock a model for Live.
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
