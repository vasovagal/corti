import { useMemo, useState, type FormEvent, type RefObject } from "react";
import {
  cancelHostedQuestion,
  runHostedSubscriptionNow,
  setHostedSubscriptions,
  submitHostedQuestion,
  type HostedAssistantExchange,
  type HostedAssistantSnapshot,
  type HostedAssistantSubscription,
  type HostedPatchInput,
  type HostedSettingsDto,
  type HostedSubscriptionPreset,
  type PreferencesSection,
} from "../lib/api";
import {
  errorLabel,
  hostedErrorGuidance,
  laneConfigurationGuidance,
  unknownHostedErrorGuidance,
  type HostedActionGuidance,
} from "../lib/hosted";
import {
  boundAssistantExchanges,
  cacheObservationLabel,
  formatHostedCost,
  questionStatusLabel,
  tokenEntries,
  type LiveCallDetail,
} from "../lib/liveHosted";
import {
  MAX_SUBSCRIPTIONS,
  displayedAnswer,
  newSubscriptionId,
  orderAssistantCards,
  presetDefaults,
  presetLabel,
  splitAnswerLines,
} from "../lib/subscriptions";
import { HostedDialog, HostedSwitch } from "../settings/HostedCommon";

interface AssistantProps {
  snapshot: HostedAssistantSnapshot | null;
  settings: HostedSettingsDto | null;
  calls: LiveCallDetail[];
  sessionActive: boolean;
  detailsEnabled: boolean;
  loading: boolean;
  error: string;
  closeButtonRef?: RefObject<HTMLButtonElement | null>;
  onClose?: () => void;
  onRefresh: () => Promise<void>;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
  onOpenPreferences: (section: PreferencesSection) => Promise<void>;
}

const QUICK_ADD_PRESETS: HostedSubscriptionPreset[] = ["asked_of_me", "running_summary"];

export function Assistant({
  snapshot,
  settings,
  calls,
  sessionActive,
  detailsEnabled,
  loading,
  error,
  closeButtonRef,
  onClose,
  onRefresh,
  onPatch,
  onOpenPreferences,
}: AssistantProps) {
  const [question, setQuestion] = useState("");
  const [questionBusy, setQuestionBusy] = useState("");
  const [questionError, setQuestionError] = useState<HostedActionGuidance | null>(null);
  const [subscriptionBusy, setSubscriptionBusy] = useState("");
  const [subscriptionStatus, setSubscriptionStatus] = useState("");
  const [confirmAuto, setConfirmAuto] = useState(false);

  const exchanges = useMemo(
    () => boundAssistantExchanges(snapshot?.exchanges ?? []),
    [snapshot?.exchanges],
  );
  const omitted = Math.max(0, (snapshot?.exchanges.length ?? 0) - exchanges.length);
  const cards = useMemo(() => orderAssistantCards(snapshot?.subscriptions ?? []), [snapshot?.subscriptions]);
  const saved = settings?.subscriptions ?? [];
  const questionConfiguration = settings
    ? laneConfigurationGuidance(settings, "question")
    : null;
  const questionsConfigured = Boolean(settings && !questionConfiguration);
  const questionsReady = Boolean(
    sessionActive &&
      questionsConfigured &&
      settings?.control.master_enabled &&
      settings.control.questions.enabled,
  );
  const autoEnabled = Boolean(settings?.control.pinned_auto_enabled);

  async function submitQuestion(event: FormEvent) {
    event.preventDefault();
    const value = question.trim();
    if (!value || questionBusy) return;
    setQuestionBusy("submit");
    setQuestionError(null);
    try {
      await submitHostedQuestion(value);
      setQuestion("");
      await onRefresh();
    } catch (reason) {
      setQuestionError(unknownHostedErrorGuidance(reason));
    } finally {
      setQuestionBusy("");
    }
  }

  async function cancelQuestion(callId: string) {
    if (questionBusy) return;
    setQuestionBusy(callId);
    setQuestionError(null);
    try {
      await cancelHostedQuestion(callId);
      await onRefresh();
    } catch (reason) {
      setQuestionError(unknownHostedErrorGuidance(reason));
    } finally {
      setQuestionBusy("");
    }
  }

  async function addPreset(preset: HostedSubscriptionPreset) {
    if (!settings || subscriptionBusy) return;
    setSubscriptionBusy(`add-${preset}`);
    setSubscriptionStatus("");
    try {
      const id = newSubscriptionId(preset, saved.map((item) => item.id));
      const result = await setHostedSubscriptions(settings.state_revision, [
        ...saved,
        presetDefaults(preset, id),
      ]);
      if (result.status === "conflict") {
        setSubscriptionStatus("Settings changed elsewhere; refreshed. Try again.");
      } else if (result.status === "invalid" || result.status === "disabled_for_session") {
        setSubscriptionStatus("The question could not be saved.");
      } else {
        setSubscriptionStatus(`${presetLabel(preset)} added.`);
      }
      await onRefresh();
    } catch (reason) {
      setSubscriptionStatus(`Could not add the question: ${String(reason)}`);
    } finally {
      setSubscriptionBusy("");
    }
  }

  async function setEnabled(id: string, enabled: boolean) {
    if (!settings || subscriptionBusy) return;
    setSubscriptionBusy(`toggle-${id}`);
    setSubscriptionStatus("");
    try {
      const next = saved.map((item) => (item.id === id ? { ...item, enabled } : item));
      const result = await setHostedSubscriptions(settings.state_revision, next);
      if (result.status === "conflict") {
        setSubscriptionStatus("Settings changed elsewhere; refreshed. Try again.");
      }
      await onRefresh();
    } catch (reason) {
      setSubscriptionStatus(`Could not update the question: ${String(reason)}`);
    } finally {
      setSubscriptionBusy("");
    }
  }

  async function runNow(id: string) {
    if (subscriptionBusy) return;
    setSubscriptionBusy(`run-${id}`);
    setSubscriptionStatus("");
    try {
      await runHostedSubscriptionNow(id);
      await onRefresh();
    } catch (reason) {
      setSubscriptionStatus(`Could not run the question: ${String(reason)}`);
    } finally {
      setSubscriptionBusy("");
    }
  }

  const subscriptionGuidance = !sessionActive
    ? "Start the microphone test or join a live call to run subscribed questions."
    : questionConfiguration
      ? questionConfiguration.message
      : !settings?.control.questions.enabled
        ? "Enable Questions to run subscribed questions."
        : !settings.control.master_enabled
          ? "Turn on Master to allow subscribed questions to run."
          : !autoEnabled
            ? "Turn on Automatic questions, or use Catch up now on a card."
            : null;

  return (
    <div className="live-assistant-panel">
      <header className="live-assistant-head">
        <div>
          <p className="hosted-eyebrow">Session only</p>
          <h2>Assistant</h2>
        </div>
        {onClose && (
          <button
            ref={closeButtonRef}
            className="btn-icon live-assistant-close"
            type="button"
            aria-label="Close assistant"
            onClick={onClose}
          >
            ×
          </button>
        )}
      </header>
      <p className="live-assistant-privacy">
        Answers stay in bounded memory and are never added to the transcript note.
      </p>

      {loading && !snapshot && <p className="live-assistant-loading">Loading assistant…</p>}
      {error && (
        <div className="live-assistant-remedy" role="alert">
          <span>{error}</span>
          <button className="btn-secondary" type="button" onClick={() => void onRefresh()}>
            Try again
          </button>
        </div>
      )}

      <section className="live-pinned-card live-subscriptions" aria-labelledby="live-subscriptions-heading">
        <header>
          <div>
            <h3 id="live-subscriptions-heading">Subscribed questions</h3>
            <span>
              {cards.length} / {MAX_SUBSCRIPTIONS} saved · re-asked as the transcript grows
            </span>
          </div>
          <button
            className="btn-quiet"
            type="button"
            onClick={() => void onOpenPreferences("hosted-language")}
          >
            Manage
          </button>
        </header>

        {settings && (
          <HostedSwitch
            label="Automatic questions"
            description={
              autoEnabled
                ? "Each subscription runs again when its own thresholds are met."
                : "Off · enabling requires repeated-cost acknowledgement. Catch up now still works per card."
            }
            checked={autoEnabled}
            disabled={!settings.control.questions.enabled || !questionsConfigured}
            onChange={(enabled) => {
              if (enabled) setConfirmAuto(true);
              else {
                void onPatch(
                  { kind: "set_pinned_auto", enabled: false, acknowledged: false },
                  "Automatic questions are off.",
                );
              }
            }}
          />
        )}

        {subscriptionGuidance && cards.length > 0 && (
          <p className="live-pinned-guidance">{subscriptionGuidance}</p>
        )}

        {cards.length === 0 ? (
          <div className="live-subscription-empty">
            <p className="live-answer-placeholder">
              No subscribed questions yet. Add one to keep a running answer while you listen.
            </p>
            <div className="live-subscription-add">
              {QUICK_ADD_PRESETS.map((preset) => (
                <button
                  key={preset}
                  className="btn-secondary"
                  type="button"
                  disabled={!settings || Boolean(subscriptionBusy)}
                  onClick={() => void addPreset(preset)}
                >
                  {subscriptionBusy === `add-${preset}` ? "Adding…" : `Add “${presetLabel(preset)}”`}
                </button>
              ))}
            </div>
          </div>
        ) : (
          <ol className="live-subscription-cards">
            {cards.map((card) => (
              <li key={card.id}>
                <SubscriptionCard
                  card={card}
                  call={card.exchange ? calls.find((item) => item.call_id === card.exchange?.call_id) : undefined}
                  detailsEnabled={detailsEnabled}
                  sessionActive={sessionActive}
                  questionsReady={questionsReady}
                  autoEnabled={autoEnabled}
                  busy={subscriptionBusy}
                  onRunNow={() => void runNow(card.id)}
                  onSetEnabled={(enabled) => void setEnabled(card.id, enabled)}
                  onOpenPreferences={onOpenPreferences}
                />
              </li>
            ))}
          </ol>
        )}
        {cards.length > 0 && cards.length < MAX_SUBSCRIPTIONS && (
          <div className="live-subscription-add">
            {QUICK_ADD_PRESETS.filter((preset) => !cards.some((card) => card.preset === preset)).map(
              (preset) => (
                <button
                  key={preset}
                  className="btn-quiet"
                  type="button"
                  disabled={!settings || Boolean(subscriptionBusy)}
                  onClick={() => void addPreset(preset)}
                >
                  {subscriptionBusy === `add-${preset}` ? "Adding…" : `+ ${presetLabel(preset)}`}
                </button>
              ),
            )}
          </div>
        )}
        {subscriptionStatus && (
          <p className="live-debounce-state" role="status" aria-live="polite">
            {subscriptionStatus}
          </p>
        )}
      </section>

      <section className="live-thread" aria-labelledby="live-thread-heading">
        <div className="live-thread-head">
          <div>
            <h3 id="live-thread-heading">Ad-hoc thread</h3>
            <span>{exchanges.length} / 20 retained</span>
          </div>
          {settings && !questionsConfigured ? (
            <button
              className="btn-secondary"
              type="button"
              onClick={() => void onOpenPreferences(questionConfiguration?.section ?? "hosted-routing")}
            >
              Configure questions
            </button>
          ) : settings && !settings.control.questions.enabled ? (
            <button
              className="btn-secondary"
              type="button"
              onClick={() =>
                void onPatch(
                  { kind: "set_lane_enabled", lane: "question", enabled: true },
                  "Questions enabled for the next request.",
                )
              }
            >
              Enable questions
            </button>
          ) : settings && !settings.control.master_enabled ? (
            <button
              className="btn-secondary"
              type="button"
              onClick={() => {
                if (!settings.control.egress_acknowledged) {
                  void onOpenPreferences("hosted");
                } else {
                  void onPatch(
                    { kind: "set_master", enabled: true },
                    "Master enabled for the next question.",
                  );
                }
              }}
            >
              {settings.control.egress_acknowledged ? "Turn on Master" : "Review privacy"}
            </button>
          ) : null}
        </div>
        <form className="live-question-form" onSubmit={(event) => void submitQuestion(event)}>
          <label htmlFor="live-question">Ask about the transcript as it stands now</label>
          <textarea
            id="live-question"
            rows={2}
            maxLength={32 * 1024}
            value={question}
            disabled={!questionsReady || questionBusy === "submit"}
            placeholder={
              questionsReady
                ? "Ask one grounded question"
                : !sessionActive
                  ? "Start the microphone test or join a live call first"
                  : !questionsConfigured
                    ? "Configure a Questions model in Preferences first"
                    : !settings?.control.questions.enabled
                      ? "Enable Questions first"
                      : "Turn on Master first"
            }
            onChange={(event) => setQuestion(event.target.value)}
            onKeyDown={(event) => {
              if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
                event.preventDefault();
                event.currentTarget.form?.requestSubmit();
              }
            }}
          />
          <div>
            <span>⌘↵ to send · bounded FIFO</span>
            <button
              className="btn-primary"
              type="submit"
              disabled={!questionsReady || !question.trim() || Boolean(questionBusy)}
            >
              {questionBusy === "submit" ? "Queuing…" : "Ask"}
            </button>
          </div>
        </form>
        {questionError && (
          <div className="live-assistant-remedy" role="alert">
            <span>Question failed. {questionError.message}</span>
            {questionError.section && questionError.actionLabel && (
              <button
                className="btn-secondary"
                type="button"
                onClick={() => void onOpenPreferences(questionError.section!)}
              >
                {questionError.actionLabel}
              </button>
            )}
          </div>
        )}
        {omitted > 0 && <p className="live-thread-omitted">{omitted} older exchange(s) omitted.</p>}
        {exchanges.length === 0 ? (
          <p className="live-answer-placeholder">No ad-hoc questions in this session.</p>
        ) : (
          <ol className="live-exchanges">
            {exchanges.map((exchange) => (
              <li key={exchange.call_id}>
                <QuestionResult
                  exchange={exchange}
                  shownAnswer={exchange.answer ?? exchange.partial_answer ?? null}
                  answerIsEarlier={false}
                  answerIsPartial={!exchange.answer && Boolean(exchange.partial_answer)}
                  call={calls.find((item) => item.call_id === exchange.call_id)}
                  detailsEnabled={detailsEnabled}
                  onOpenPreferences={onOpenPreferences}
                  onCancel={
                    ["queued", "waiting_for_credential", "running"].includes(exchange.status)
                      ? () => void cancelQuestion(exchange.call_id)
                      : undefined
                  }
                  canceling={questionBusy === exchange.call_id}
                />
              </li>
            ))}
          </ol>
        )}
      </section>

      <HostedDialog
        open={confirmAuto}
        title="Allow repeated paid questions?"
        confirmLabel="Acknowledge repeated cost"
        onCancel={() => setConfirmAuto(false)}
        onConfirm={() => {
          void onPatch(
            { kind: "set_pinned_auto", enabled: true, acknowledged: true },
            "Automatic questions enabled.",
          ).then((saved) => {
            if (saved) setConfirmAuto(false);
          });
        }}
      >
        <p>
          Each subscribed question can make another paid request whenever its thresholds are met.
          Cancellation after dispatch may still be billed; an exact local cache hit is not guaranteed.
        </p>
      </HostedDialog>
    </div>
  );
}

function SubscriptionCard({
  card,
  call,
  detailsEnabled,
  sessionActive,
  questionsReady,
  autoEnabled,
  busy,
  onRunNow,
  onSetEnabled,
  onOpenPreferences,
}: {
  card: HostedAssistantSubscription;
  call: LiveCallDetail | undefined;
  detailsEnabled: boolean;
  sessionActive: boolean;
  questionsReady: boolean;
  autoEnabled: boolean;
  busy: string;
  onRunNow: () => void;
  onSetEnabled: (enabled: boolean) => void;
  onOpenPreferences: (section: PreferencesSection) => Promise<void>;
}) {
  const exchange = card.exchange;
  const shown = displayedAnswer(card);
  const askedOfMe = card.preset === "asked_of_me";
  const failed = exchange?.status === "failed" || exchange?.status === "canceled";
  const failureGuidance = exchange?.error ? hostedErrorGuidance(exchange.error) : null;
  const costLabel = exchange?.cost_label ?? (call ? formatHostedCost(call.cost) : null);
  const usage = exchange?.usage ?? call?.usage;
  const tokens = usage ? tokenEntries(usage) : [];
  const stateLabel = exchange
    ? questionStatusLabel(exchange.status)
    : card.pending
      ? "Waiting for quiet"
      : card.enabled
        ? "Watching"
        : "Paused";
  const stateClass = exchange
    ? `live-answer-state live-answer-${exchange.status}`
    : "live-answer-state";
  return (
    <article
      className={`live-answer live-subscription-card${askedOfMe ? " live-subscription-asked" : ""}${card.enabled ? "" : " live-subscription-paused"}`}
      data-subscription={card.id}
    >
      <header>
        <div className="live-subscription-title">
          <strong>{askedOfMe ? "Last question for you" : card.title}</strong>
          <span>
            {presetLabel(card.preset)} · {card.run_count} run(s)
          </span>
        </div>
        <span className={stateClass}>{card.in_flight && !exchange ? "Queued" : stateLabel}</span>
      </header>
      {exchange && <span className="live-subscription-revision">As of transcript r{exchange.as_of_revision.toLocaleString()}</span>}
      {shown.text ? (
        <div className="live-answer-copy">
          {shown.kind === "previous" && (
            <p className="live-answer-updating">Updating · previous accepted answer shown</p>
          )}
          {shown.kind === "partial" && <p className="live-answer-updating">Streaming…</p>}
          <AnswerBody text={shown.text} />
        </div>
      ) : failed ? (
        <p className="live-answer-fallback">
          No answer applied{exchange?.error ? ` · ${errorLabel(exchange.error)}` : ""}. The transcript remains
          available.
        </p>
      ) : exchange ? (
        <p className="live-answer-running">{questionStatusLabel(exchange.status)}…</p>
      ) : (
        <p className="live-answer-placeholder">
          {!card.enabled
            ? "Paused. Turn it on to run again."
            : !sessionActive
              ? "Runs once a live session is active."
              : askedOfMe
                ? "Waits for the other speakers to ask you something, then a short pause. Use Catch up now to check right away."
                : autoEnabled
                  ? "Waiting for enough new context: about 40 words or 30 seconds of speech, then a short pause."
                  : "Automatic questions are off. Use Catch up now to run it once."}
        </p>
      )}
      {failureGuidance && (
        <div className="live-answer-remedy">
          <span>{failureGuidance.message}</span>
          {failureGuidance.section && failureGuidance.actionLabel && (
            <button
              className="btn-quiet"
              type="button"
              onClick={() => void onOpenPreferences(failureGuidance.section!)}
            >
              {failureGuidance.actionLabel}
            </button>
          )}
        </div>
      )}
      {exchange?.context_truncated && <p className="live-context-note">Earlier transcript omitted.</p>}
      <div className="live-answer-accounting">
        <span>{costLabel ?? (exchange ? (failed ? "Cost unavailable" : "Cost pending") : "No run yet")}</span>
        {detailsEnabled && exchange && <span>{cacheObservationLabel(exchange.cache ?? call?.cache)}</span>}
      </div>
      {detailsEnabled && tokens.length > 0 && (
        <dl className="live-answer-tokens" aria-label="Answer token usage">
          {tokens.map(([label, value]) => (
            <div key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </div>
          ))}
        </dl>
      )}
      <div className="live-subscription-actions">
        <button
          className="btn-secondary"
          type="button"
          disabled={!questionsReady || card.in_flight || Boolean(busy)}
          onClick={onRunNow}
          title={card.in_flight ? "A run is already in progress" : "Ask now, whatever the thresholds say"}
        >
          {busy === `run-${card.id}` ? "Queuing…" : card.in_flight ? "Running…" : "Catch up now"}
        </button>
        <HostedSwitch
          compact
          label={card.enabled ? "On" : "Off"}
          checked={card.enabled}
          disabled={Boolean(busy)}
          onChange={onSetEnabled}
        />
      </div>
    </article>
  );
}

/** Bullet answers render as a list; everything else as paragraphs. Plain text only — never HTML. */
function AnswerBody({ text }: { text: string }) {
  const { bullets, lines } = splitAnswerLines(text);
  if (bullets) {
    return (
      <ul className="live-answer-bullets">
        {lines.map((line, index) => (
          <li key={`${index}-${line}`}>{line}</li>
        ))}
      </ul>
    );
  }
  return (
    <>
      {lines.map((line, index) => (
        <p key={`${index}-${line}`}>{line}</p>
      ))}
    </>
  );
}

function QuestionResult({
  exchange,
  shownAnswer,
  answerIsEarlier,
  answerIsPartial = false,
  call,
  detailsEnabled,
  onOpenPreferences,
  onCancel,
  canceling = false,
}: {
  exchange: HostedAssistantExchange;
  shownAnswer: string | null;
  answerIsEarlier: boolean;
  answerIsPartial?: boolean;
  call: LiveCallDetail | undefined;
  detailsEnabled: boolean;
  onOpenPreferences: (section: PreferencesSection) => Promise<void>;
  onCancel?: () => void;
  canceling?: boolean;
}) {
  const costLabel = exchange.cost_label ?? (call ? formatHostedCost(call.cost) : null);
  const usage = exchange.usage ?? call?.usage;
  const tokens = usage ? tokenEntries(usage) : [];
  const cache = exchange.cache ?? call?.cache;
  const failed = exchange.status === "failed" || exchange.status === "canceled";
  const failureGuidance = exchange.error ? hostedErrorGuidance(exchange.error) : null;
  return (
    <article className={`live-answer live-answer-${exchange.status}`}>
      <header>
        <span className="live-answer-state">{questionStatusLabel(exchange.status)}</span>
        <span>As of transcript r{exchange.as_of_revision.toLocaleString()}</span>
      </header>
      <p className="live-question-copy">{exchange.question}</p>
      {shownAnswer ? (
        <div className="live-answer-copy">
          {answerIsEarlier && <p className="live-answer-updating">Updating · previous accepted answer shown</p>}
          {answerIsPartial && <p className="live-answer-updating">Streaming…</p>}
          <AnswerBody text={shownAnswer} />
        </div>
      ) : failed ? (
        <p className="live-answer-fallback">
          No answer applied{exchange.error ? ` · ${errorLabel(exchange.error)}` : ""}. The transcript remains
          available.
        </p>
      ) : (
        <p className="live-answer-running">{questionStatusLabel(exchange.status)}…</p>
      )}
      {failureGuidance && (
        <div className="live-answer-remedy">
          <span>{failureGuidance.message}</span>
          {failureGuidance.section && failureGuidance.actionLabel && (
            <button
              className="btn-quiet"
              type="button"
              onClick={() => void onOpenPreferences(failureGuidance.section!)}
            >
              {failureGuidance.actionLabel}
            </button>
          )}
        </div>
      )}
      {exchange.context_truncated && <p className="live-context-note">Earlier transcript omitted.</p>}
      <div className="live-answer-accounting">
        <span>{costLabel ?? (failed ? "Cost unavailable" : "Cost pending")}</span>
        {detailsEnabled && <span>{cacheObservationLabel(cache)}</span>}
      </div>
      {detailsEnabled && tokens.length > 0 && (
        <dl className="live-answer-tokens" aria-label="Answer token usage">
          {tokens.map(([label, value]) => (
            <div key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </div>
          ))}
        </dl>
      )}
      {onCancel && (
        <button className="btn-quiet live-cancel-question" type="button" disabled={canceling} onClick={onCancel}>
          {canceling ? "Canceling…" : "Cancel"}
        </button>
      )}
    </article>
  );
}
