import { useEffect, useMemo, useState } from "react";
import type {
  HostedLane,
  HostedLaneControl,
  HostedPatchInput,
  HostedProviderCacheMode,
  HostedSettingsDto,
} from "../lib/api";
import {
  billingDisclosure,
  emptySelection,
  findExactModel,
  modelUnavailableReason,
  modelsForProvider,
  parseProviderKey,
  providerKey,
  providerPresentation,
  selectionForModel,
  supportTierLabel,
} from "../lib/hosted";
import { HostedSwitch } from "./HostedCommon";

const LANE_COPY: Record<HostedLane, { title: string; eyebrow: string; description: string }> = {
  live: {
    title: "Live cleanup",
    eyebrow: "Fast lane",
    description:
      "Cleans closed phrases only. First-text target is 2 seconds and terminal deadline is 5 seconds; raw text wins on delay or failure.",
  },
  final: {
    title: "Final rewrite",
    eyebrow: "Quality lane",
    description:
      "Runs a stronger all-or-nothing pass before final filing. Failure, timeout, or stale output falls back to the latest validated clean or raw text.",
  },
  question: {
    title: "Questions",
    eyebrow: "Assistant lane",
    description:
      "Uses an independent model for explicit questions and the one pinned template. Answers are text-only and scoped to the current transcript.",
  },
};

export function HostedLanes({
  settings,
  busy,
  onPatch,
}: {
  settings: HostedSettingsDto;
  busy: boolean;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
}) {
  return (
    <section aria-labelledby="hosted-lanes-heading">
      <div className="hosted-section-heading">
        <div>
          <p className="hosted-eyebrow">Routing</p>
          <h2 id="hosted-lanes-heading">Choose only the modes you need</h2>
        </div>
        <p>Each mode can keep its own exact model, but reusing one provider is the simplest setup.</p>
      </div>
      <div className="hosted-routing-guide">
        <strong>Start with Final rewrite.</strong>
        <span>
          It improves the transcript once before filing. Add Live cleanup only for faster on-screen polish,
          and Questions only if you use the transcript assistant.
        </span>
      </div>
      <div className="hosted-lane-grid">
        <HostedLaneCard
          lane="final"
          control={settings.control.final_lane}
          settings={settings}
          busy={busy}
          onPatch={onPatch}
        />
        <HostedLaneCard
          lane="live"
          control={settings.control.live}
          settings={settings}
          busy={busy}
          onPatch={onPatch}
        />
        <HostedLaneCard
          lane="question"
          control={settings.control.questions}
          settings={settings}
          busy={busy}
          onPatch={onPatch}
        />
      </div>
    </section>
  );
}

function HostedLaneCard({
  lane,
  control,
  settings,
  busy,
  onPatch,
}: {
  lane: HostedLane;
  control: HostedLaneControl;
  settings: HostedSettingsDto;
  busy: boolean;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
}) {
  const selectedProviderKey =
    control.selection.provider && control.selection.transport
      ? providerKey(control.selection.provider, control.selection.transport)
      : "";
  const [draftProviderKey, setDraftProviderKey] = useState(selectedProviderKey);
  const [expanded, setExpanded] = useState(lane === "final");

  useEffect(() => {
    setDraftProviderKey(selectedProviderKey);
  }, [selectedProviderKey]);

  const providerOptions = useMemo(
    () =>
      settings.providers.filter((state) => {
        const current =
          state.descriptor.provider === control.selection.provider &&
          state.descriptor.transport === control.selection.transport;
        return state.descriptor.support_tier !== "blocked" && (state.models.length > 0 || current);
      }),
    [control.selection.provider, control.selection.transport, settings.providers],
  );
  const parsedProvider = parseProviderKey(draftProviderKey);
  const catalog = parsedProvider
    ? modelsForProvider(settings.providers, parsedProvider.provider, parsedProvider.transport)
    : [];
  const selectedModel = findExactModel(
    settings.providers,
    control.selection.provider,
    control.selection.transport,
    control.selection.model,
  );
  const selectionIsCurrent =
    selectedModel !== null && modelUnavailableReason(selectedModel, lane) === null;
  const complete = Boolean(
    control.selection.provider && control.selection.transport && control.selection.model,
  );
  const effective = settings.control.master_enabled && control.enabled && selectionIsCurrent;
  const laneState = effective
    ? "Active"
    : control.enabled && !settings.control.master_enabled
      ? "Held by Master"
      : control.enabled
        ? "Catalog unavailable"
        : "Off";
  const copy = LANE_COPY[lane];

  const setSelection = (selection: Parameters<typeof selectionPatch>[1], message: string) =>
    onPatch(selectionPatch(lane, selection), message);

  return (
    <details
      className={`card hosted-lane-card${effective ? " hosted-lane-effective" : ""}`}
      open={expanded}
      onToggle={(event) => setExpanded(event.currentTarget.open)}
    >
      <summary className="hosted-card-head hosted-lane-head">
        <div>
          <p className="hosted-eyebrow">{copy.eyebrow}</p>
          <h3>{copy.title}</h3>
        </div>
        <span className="hosted-lane-summary-status">
          <span className={`hosted-lane-state${effective ? " hosted-state-on" : ""}`}>
            {laneState}
          </span>
          <span className="hosted-disclosure-chevron" aria-hidden="true">›</span>
        </span>
      </summary>
      <div className="hosted-lane-body">
      <p className="muted small hosted-lane-description">
        {copy.description}
        {lane === "final" && ` Configured deadline: ${settings.final_deadline_seconds} seconds.`}
      </p>

      <HostedSwitch
        compact
        label={`Enable ${copy.title}`}
        description={
          effective
            ? "Eligible requests may dispatch now."
            : control.enabled && !settings.control.master_enabled
              ? "Configured, but Master prevents egress."
              : control.enabled
                ? "Enabled, but the exact model is not in the current catalog."
                : "Selection is retained while this lane is off."
        }
        checked={control.enabled}
        disabled={busy || (!control.enabled && (!complete || !selectionIsCurrent))}
        onChange={(enabled) =>
          void onPatch(
            { kind: "set_lane_enabled", lane, enabled },
            `${copy.title} ${enabled ? "enabled" : "disabled"}.`,
          )
        }
      />

      <div className="hosted-lane-selection-grid">
      <div className="settings-field hosted-lane-field">
        <label htmlFor={`hosted-provider-${lane}`}>Provider catalog</label>
        <select
          id={`hosted-provider-${lane}`}
          className="jselect"
          value={draftProviderKey}
          disabled={busy}
          onChange={(event) => setDraftProviderKey(event.target.value)}
        >
          <option value="">Choose a refreshed provider…</option>
          {providerOptions.map((state) => {
            const presentation = providerPresentation(
              state.descriptor.provider,
              state.descriptor.transport,
            );
            return (
              <option
                key={providerKey(state.descriptor.provider, state.descriptor.transport)}
                value={providerKey(state.descriptor.provider, state.descriptor.transport)}
              >
                {presentation.shortName} · {supportTierLabel(state.descriptor.support_tier)} · {state.models.length}{" "}
                {state.models.length === 1 ? "model" : "models"}
              </option>
            );
          })}
        </select>
        {providerOptions.length === 0 && (
          <p className="muted small">Refresh a documented provider after its backend credential is ready.</p>
        )}
      </div>

      <div className="settings-field hosted-lane-field">
        <label htmlFor={`hosted-model-${lane}`}>Exact paid model</label>
        <select
          id={`hosted-model-${lane}`}
          className="jselect"
          value={
            parsedProvider?.provider === control.selection.provider &&
            parsedProvider.transport === control.selection.transport
              ? (control.selection.model ?? "")
              : ""
          }
          disabled={!parsedProvider || busy}
          onChange={(event) => {
            const model = catalog.find((candidate) => candidate.exact_model_id === event.target.value);
            if (!model || modelUnavailableReason(model, lane)) return;
            void setSelection(
              selectionForModel(model, control.selection.cache_policy.local),
              `${copy.title} model updated.`,
            );
          }}
        >
          <option value="">Choose an exact catalog model…</option>
          {control.selection.model &&
            parsedProvider?.provider === control.selection.provider &&
            parsedProvider.transport === control.selection.transport &&
            !catalog.some((candidate) => candidate.exact_model_id === control.selection.model) && (
              <option value={control.selection.model} disabled>
                {control.selection.model} · unavailable in the current catalog
              </option>
            )}
          {catalog.map((model) => {
            const reason = modelUnavailableReason(model, lane);
            return (
              <option
                key={`${model.exact_model_id}:${model.region ?? ""}`}
                value={model.exact_model_id}
                disabled={reason !== null}
              >
                {model.exact_model_id}
                {model.region ? ` · ${model.region}` : ""}
                {reason ? ` · ${reason}` : ""}
              </option>
            );
          })}
        </select>
        {lane === "live" && catalog.length > 0 && catalog.every((model) => modelUnavailableReason(model, lane)) && (
          <p className="hosted-field-error">This catalog has no model that passed the live benchmark gate.</p>
        )}
      </div>
      </div>

      <details className="hosted-lane-advanced">
        <summary>Cache, cost &amp; model details</summary>
        <div className="hosted-lane-advanced-body">
      <div className="settings-field hosted-lane-field">
        <label htmlFor={`hosted-local-cache-${lane}`}>Local exact cache</label>
        <select
          id={`hosted-local-cache-${lane}`}
          className="jselect"
          value={control.selection.cache_policy.local}
          disabled={!complete || busy}
          onChange={(event) => {
            if (!selectedModel) return;
            void setSelection(
              selectionForModel(
                selectedModel,
                event.target.value as HostedLaneControl["selection"]["cache_policy"]["local"],
                control.selection.cache_policy.provider,
              ),
              `${copy.title} local cache policy updated.`,
            );
          }}
        >
          <option value="reusable">Reusable when encrypted cache is available</option>
          <option value="recovery_only">Mandatory final recovery only</option>
          <option value="memory_only">Memory only (final recovery still mandatory)</option>
        </select>
      </div>

      <div className="settings-field hosted-lane-field">
        <label htmlFor={`hosted-provider-cache-${lane}`}>Provider cache</label>
        <ProviderCacheControl
          id={`hosted-provider-cache-${lane}`}
          control={control}
          selectedModel={selectedModel}
          busy={busy}
          onChange={(providerCache) => {
            if (!selectedModel) return;
            void setSelection(
              selectionForModel(
                selectedModel,
                control.selection.cache_policy.local,
                providerCache,
              ),
              `${copy.title} provider cache policy updated.`,
            );
          }}
        />
      </div>

      {selectedModel ? (
        <ModelTruth model={selectedModel} lane={lane} providerCache={control.selection.cache_policy.provider} />
      ) : complete ? (
        <p className="hosted-selection-warning" role="status">
          Saved selection is not in the current account/region catalog. It will not be silently substituted.
        </p>
      ) : (
        <p className="muted small">No model selected. This mode cannot be enabled.</p>
      )}
        </div>
      </details>

      {!control.enabled && complete && (
        <button
          className="btn-quiet hosted-clear-selection"
          type="button"
          disabled={busy}
          onClick={() =>
            void setSelection(
              emptySelection(control.selection.cache_policy.local),
              `${copy.title} selection cleared.`,
            )
          }
        >
          Clear selection
        </button>
      )}
      </div>
    </details>
  );
}

function selectionPatch(
  lane: HostedLane,
  selection: {
    provider: string | null;
    transport: string | null;
    model: string | null;
    local_cache: HostedLaneControl["selection"]["cache_policy"]["local"];
    provider_cache: HostedProviderCacheMode;
  },
): HostedPatchInput {
  return { kind: "set_lane_selection", lane, selection };
}

function ProviderCacheControl({
  id,
  control,
  selectedModel,
  busy,
  onChange,
}: {
  id: string;
  control: HostedLaneControl;
  selectedModel: ReturnType<typeof findExactModel>;
  busy: boolean;
  onChange: (cache: HostedProviderCacheMode) => void;
}) {
  const mode = control.selection.cache_policy.provider;
  if (selectedModel?.capabilities.implicit_cache_may_apply) {
    return (
      <>
        <select id={id} className="jselect" value="unavoidable_implicit" disabled>
          <option value="unavoidable_implicit">Unavoidable implicit provider caching may apply</option>
        </select>
        <p className="muted small">
          This catalog says implicit caching may occur. Corti cannot truthfully offer an Off setting.
        </p>
      </>
    );
  }
  if (selectedModel?.transport === "codex_app_server" || mode === "unavailable") {
    return (
      <>
        <select id={id} className="jselect" value="unavailable" disabled>
          <option value="unavailable">Provider cache policy unavailable</option>
        </select>
        <p className="muted small">The broker/model owns this behavior; Corti does not claim control.</p>
      </>
    );
  }
  return (
    <>
      <select
        id={id}
        className="jselect"
        value={mode}
        disabled={!selectedModel || busy}
        onChange={(event) => onChange(event.target.value as HostedProviderCacheMode)}
      >
        <option value="off">Off · no explicit provider cache request</option>
        {selectedModel?.capabilities.explicit_prefix_cache && (
          <option value="explicit_stable_prefix" disabled={mode !== "explicit_stable_prefix"}>
            Explicit stable-prefix cache
          </option>
        )}
      </select>
      <p className="muted small">
        Off controls Corti's cache request only; it is not a promise about provider retention or training.
        Explicit caching requires a separate retention acknowledgement that is not exposed by this DTO.
      </p>
    </>
  );
}

function ModelTruth({
  model,
  lane,
  providerCache,
}: {
  model: NonNullable<ReturnType<typeof findExactModel>>;
  lane: HostedLane;
  providerCache: HostedProviderCacheMode;
}) {
  const cache =
    providerCache === "unavoidable_implicit"
      ? "Provider implicit caching may apply; local purge cannot remove it."
      : providerCache === "explicit_stable_prefix"
        ? "Explicit stable-prefix caching is requested and may retain transcript-adjacent words remotely."
        : providerCache === "unavailable"
          ? "Provider cache behavior is not controlled by Corti."
          : "No explicit provider cache requested; general provider retention still applies.";
  return (
    <dl className="hosted-model-truth">
      <div>
        <dt>Latency</dt>
        <dd>
          {lane === "live" && model.benchmarked_for_live
            ? "Live benchmark gate passed; no universal latency promise."
            : "No measured latency promise in this catalog."}
        </dd>
      </div>
      <div>
        <dt>Quality</dt>
        <dd>
          Structured output supported. Catalog availability is not a quality rating or silent fallback.
        </dd>
      </div>
      <div>
        <dt>Cost</dt>
        <dd>{billingDisclosure(model.billing_basis, model.tariff_version)}</dd>
      </div>
      <div>
        <dt>Cache</dt>
        <dd>{cache}</dd>
      </div>
      <div>
        <dt>Retention</dt>
        <dd>Provider/account terms apply. Corti cannot purge provider-held data.</dd>
      </div>
      <div>
        <dt>Limits</dt>
        <dd>
          {model.max_context_tokens.toLocaleString()} context · {model.max_output_tokens.toLocaleString()} output
          tokens
        </dd>
      </div>
    </dl>
  );
}
