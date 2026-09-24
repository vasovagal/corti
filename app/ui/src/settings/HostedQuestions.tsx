import { useEffect, useState } from "react";
import type {
  HostedPatchInput,
  HostedSettingsDto,
  HostedSubscription,
  HostedSubscriptionPreset,
} from "../lib/api";
import {
  MAX_SUBSCRIPTIONS,
  SUBSCRIPTION_PRESETS,
  newSubscriptionId,
  parseNameHints,
  presetDefaults,
  presetDescription,
  presetLabel,
  subscriptionSetProblem,
} from "../lib/subscriptions";
import { HostedDialog, HostedSwitch } from "./HostedCommon";

interface QuestionsActions {
  busy: boolean;
  onSubscriptions: (subscriptions: HostedSubscription[]) => Promise<boolean>;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
}

/** Editor over the saved question subscriptions (hosted.toml schema 2). The whole set is saved at once;
 * per-subscription state (run counts, answers) lives only in the live window. */
export function HostedQuestions({
  settings,
  actions,
}: {
  settings: HostedSettingsDto;
  actions: QuestionsActions;
}) {
  const saved = settings.subscriptions ?? [];
  const [draft, setDraft] = useState<HostedSubscription[]>(saved);
  const [dirty, setDirty] = useState(false);
  const [acknowledging, setAcknowledging] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);

  useEffect(() => {
    if (!dirty) setDraft(settings.subscriptions ?? []);
  }, [dirty, settings.subscriptions]);

  const problem = subscriptionSetProblem(draft);
  const laneReady = settings.control.questions.enabled && Boolean(settings.control.questions.selection.model);

  function update(id: string, patch: (item: HostedSubscription) => HostedSubscription) {
    setDraft((current) => current.map((item) => (item.id === id ? patch(item) : item)));
    setDirty(true);
  }

  function add(preset: HostedSubscriptionPreset) {
    const id = newSubscriptionId(preset, draft.map((item) => item.id));
    setDraft((current) => [...current, presetDefaults(preset, id)]);
    setDirty(true);
    setExpanded(id);
  }

  function remove(id: string) {
    setDraft((current) => current.filter((item) => item.id !== id));
    setDirty(true);
  }

  async function save() {
    if (problem) return;
    if (await actions.onSubscriptions(draft)) setDirty(false);
  }

  return (
    <details className="card hosted-language-card hosted-disclosure-card hosted-questions-card" open>
      <summary className="hosted-card-head hosted-disclosure-summary">
        <div>
          <h3>Subscribed questions</h3>
          <p>Saved questions re-asked as the transcript grows; each has its own trigger, context window and layout.</p>
        </div>
        <span className="hosted-disclosure-meta">
          <span className={saved.length > 0 ? "hosted-configured" : "muted"}>
            {saved.length} / {MAX_SUBSCRIPTIONS} saved
          </span>
          <span className="hosted-disclosure-chevron" aria-hidden="true">›</span>
        </span>
      </summary>
      <div className="hosted-disclosure-body">
        <HostedSwitch
          label="Run subscribed questions automatically"
          description={
            settings.control.pinned_auto_enabled
              ? "Acknowledged; each subscription runs again when its own thresholds are met."
              : "Off by default; enabling requires a repeated-cost acknowledgement. \"Catch up now\" in the live window works either way."
          }
          checked={settings.control.pinned_auto_enabled}
          disabled={actions.busy || (!settings.control.pinned_auto_enabled && !laneReady)}
          onChange={(enabled) => {
            if (enabled) setAcknowledging(true);
            else {
              void actions.onPatch(
                { kind: "set_pinned_auto", enabled: false, acknowledged: false },
                "Automatic questions are off.",
              );
            }
          }}
        />
        {!laneReady && (
          <p className="muted small">Enable the Questions lane with an exact catalog model before auto-run.</p>
        )}

        {draft.length === 0 && (
          <p className="muted small">No subscriptions yet. Add a preset below or a custom question.</p>
        )}
        <ol className="hosted-subscription-list">
          {draft.map((item) => {
            const open = expanded === item.id;
            return (
              <li key={item.id} className={`hosted-subscription${item.enabled ? "" : " hosted-subscription-paused"}`}>
                <div className="hosted-subscription-row">
                  <button
                    className="btn-quiet hosted-subscription-toggle"
                    type="button"
                    aria-expanded={open}
                    onClick={() => setExpanded(open ? null : item.id)}
                  >
                    <strong>{item.title || presetLabel(item.preset)}</strong>
                    <span className="muted small">
                      {presetLabel(item.preset)} · {item.output} · {item.trigger.on_speakers === "all" ? "all speakers" : `${item.trigger.on_speakers} only`}
                    </span>
                  </button>
                  <HostedSwitch
                    compact
                    label={item.enabled ? "On" : "Off"}
                    checked={item.enabled}
                    disabled={actions.busy}
                    onChange={(enabled) => update(item.id, (current) => ({ ...current, enabled }))}
                  />
                </div>
                {open && (
                  <div className="hosted-subscription-fields">
                    <label className="settings-field">
                      <span>Title</span>
                      <input
                        type="text"
                        value={item.title}
                        maxLength={128}
                        onChange={(event) => update(item.id, (current) => ({ ...current, title: event.target.value }))}
                      />
                    </label>
                    <label className="settings-field">
                      <span>Question {item.preset !== "none" && "(blank uses the preset's built-in question)"}</span>
                      <textarea
                        rows={3}
                        value={item.template}
                        maxLength={8 * 1024}
                        placeholder={
                          item.preset === "none"
                            ? "Ask about the current decision, risk, or next step"
                            : presetDescription(item.preset)
                        }
                        onChange={(event) => update(item.id, (current) => ({ ...current, template: event.target.value }))}
                      />
                    </label>
                    <div className="hosted-subscription-grid">
                      <label className="settings-field">
                        <span>Preset</span>
                        <select
                          value={item.preset}
                          onChange={(event) => {
                            const preset = event.target.value as HostedSubscriptionPreset;
                            update(item.id, (current) => ({
                              ...presetDefaults(preset, current.id),
                              title: current.title,
                              template: current.template,
                              enabled: current.enabled,
                              name_hints: current.name_hints,
                            }));
                          }}
                        >
                          {SUBSCRIPTION_PRESETS.map((preset) => (
                            <option key={preset} value={preset}>
                              {presetLabel(preset)}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label className="settings-field">
                        <span>Layout</span>
                        <select
                          value={item.output === "json_questions" ? "bullets" : item.output}
                          onChange={(event) =>
                            update(item.id, (current) => ({
                              ...current,
                              output: event.target.value as HostedSubscription["output"],
                            }))
                          }
                        >
                          <option value="paragraph">Paragraph</option>
                          <option value="bullets">Bullet list</option>
                        </select>
                      </label>
                      <label className="settings-field">
                        <span>Counts speech from</span>
                        <select
                          value={item.trigger.on_speakers}
                          onChange={(event) =>
                            update(item.id, (current) => ({
                              ...current,
                              trigger: {
                                ...current.trigger,
                                on_speakers: event.target.value as HostedSubscription["trigger"]["on_speakers"],
                              },
                            }))
                          }
                        >
                          <option value="all">Everyone</option>
                          <option value="them">Them only</option>
                          <option value="me">Me only</option>
                        </select>
                      </label>
                      <label className="settings-field">
                        <span>New words before a run</span>
                        <input
                          type="number"
                          min={0}
                          max={100_000}
                          value={item.trigger.min_new_words}
                          onChange={(event) =>
                            update(item.id, (current) => ({
                              ...current,
                              trigger: { ...current.trigger, min_new_words: Math.max(0, Number(event.target.value) || 0) },
                            }))
                          }
                        />
                      </label>
                      <label className="settings-field">
                        <span>Or new speech (seconds)</span>
                        <input
                          type="number"
                          min={0}
                          max={3_600}
                          value={Math.round(item.trigger.min_new_speech_ms / 1_000)}
                          onChange={(event) =>
                            update(item.id, (current) => ({
                              ...current,
                              trigger: {
                                ...current.trigger,
                                min_new_speech_ms: Math.max(0, Number(event.target.value) || 0) * 1_000,
                              },
                            }))
                          }
                        />
                      </label>
                      <label className="settings-field">
                        <span>Quiet before a run (ms)</span>
                        <input
                          type="number"
                          min={0}
                          max={60_000}
                          step={50}
                          value={item.trigger.quiet_ms}
                          onChange={(event) =>
                            update(item.id, (current) => ({
                              ...current,
                              trigger: { ...current.trigger, quiet_ms: Math.max(0, Number(event.target.value) || 0) },
                            }))
                          }
                        />
                      </label>
                      <label className="settings-field">
                        <span>Context</span>
                        <select
                          value={item.context.window}
                          onChange={(event) =>
                            update(item.id, (current) => ({
                              ...current,
                              context: {
                                ...current.context,
                                window: event.target.value as HostedSubscription["context"]["window"],
                                minutes: current.context.minutes || 5,
                                rows: current.context.rows || 20,
                              },
                            }))
                          }
                        >
                          <option value="whole">Whole session (bounded by the model)</option>
                          <option value="last_minutes">Last N minutes</option>
                          <option value="last_rows">Last N rows</option>
                        </select>
                      </label>
                      {item.context.window === "last_minutes" && (
                        <label className="settings-field">
                          <span>Minutes</span>
                          <input
                            type="number"
                            min={1}
                            max={600}
                            value={item.context.minutes}
                            onChange={(event) =>
                              update(item.id, (current) => ({
                                ...current,
                                context: { ...current.context, minutes: Math.max(1, Number(event.target.value) || 1) },
                              }))
                            }
                          />
                        </label>
                      )}
                      {item.context.window === "last_rows" && (
                        <label className="settings-field">
                          <span>Rows</span>
                          <input
                            type="number"
                            min={1}
                            max={10_000}
                            value={item.context.rows}
                            onChange={(event) =>
                              update(item.id, (current) => ({
                                ...current,
                                context: { ...current.context, rows: Math.max(1, Number(event.target.value) || 1) },
                              }))
                            }
                          />
                        </label>
                      )}
                      {item.preset === "asked_of_me" && (
                        <label className="settings-field hosted-subscription-wide">
                          <span>Names you answer to (comma-separated)</span>
                          <input
                            type="text"
                            value={item.name_hints.join(", ")}
                            placeholder="Xavier, Xav"
                            onChange={(event) =>
                              update(item.id, (current) => ({ ...current, name_hints: parseNameHints(event.target.value) }))
                            }
                          />
                        </label>
                      )}
                    </div>
                    <div className="other-row">
                      <span className="muted small">id {item.id}</span>
                      <button className="btn-danger-subtle" type="button" disabled={actions.busy} onClick={() => remove(item.id)}>
                        Remove
                      </button>
                    </div>
                  </div>
                )}
              </li>
            );
          })}
        </ol>

        <div className="hosted-subscription-add">
          {SUBSCRIPTION_PRESETS.map((preset) => (
            <button
              key={preset}
              className="btn-quiet"
              type="button"
              disabled={actions.busy || draft.length >= MAX_SUBSCRIPTIONS}
              title={presetDescription(preset)}
              onClick={() => add(preset)}
            >
              + {presetLabel(preset)}
            </button>
          ))}
        </div>

        {problem && <p className="hosted-inline-error small">{problem}</p>}
        <div className="other-row">
          <button className="btn-primary" type="button" disabled={!dirty || Boolean(problem) || actions.busy} onClick={() => void save()}>
            Save questions
          </button>
          <button
            className="btn-quiet"
            type="button"
            disabled={!dirty || actions.busy}
            onClick={() => {
              setDraft(saved);
              setDirty(false);
            }}
          >
            Discard changes
          </button>
        </div>
        <p className="muted small">
          A subscription becomes eligible after its new-word or new-speech threshold, then its quiet period.
          "Asked of me" additionally waits for a question directed at you from the other speakers. Saving
          changes cancels in-flight runs of the questions you changed and answers the transcript already on
          screen at the next opportunity.
        </p>
      </div>

      <HostedDialog
        open={acknowledging}
        title="Allow repeated paid questions?"
        confirmLabel="Acknowledge repeated cost"
        busy={actions.busy}
        onCancel={() => setAcknowledging(false)}
        onConfirm={() => {
          void actions.onPatch(
            { kind: "set_pinned_auto", enabled: true, acknowledged: true },
            "Automatic questions enabled.",
          ).then((saved) => {
            if (saved) setAcknowledging(false);
          });
        }}
      >
        <p>
          Each subscribed question can create another paid provider request whenever its thresholds are met.
          An exact local cache hit may avoid a provider request, but is not guaranteed. Cancellation after
          dispatch may still be billed.
        </p>
        <p>
          Automatic answers use the Questions lane's separately selected provider and model. Turning on this
          switch does not change Master or that lane.
        </p>
      </HostedDialog>
    </details>
  );
}
