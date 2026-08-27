import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type RefObject,
} from "react";
import {
  cancelHostedQuestion,
  setHostedPinnedQuestion,
  submitHostedQuestion,
  type HostedAssistantExchange,
  type HostedAssistantSnapshot,
  type HostedPatchInput,
  type HostedSettingsDto,
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
  const [pinnedDraft, setPinnedDraft] = useState("");
  const [pinnedDirty, setPinnedDirty] = useState(false);
  const [pinnedSaveState, setPinnedSaveState] = useState("");
  const [confirmAuto, setConfirmAuto] = useState(false);
  const pinnedInitialized = useRef(false);
  const pinnedSaveSequence = useRef(0);
  const lastAcceptedPinned = useRef<HostedAssistantExchange | null>(null);

  useEffect(() => {
    if (snapshot?.pinned?.status === "completed" && snapshot.pinned.answer) {
      lastAcceptedPinned.current = snapshot.pinned;
    }
    if (!pinnedInitialized.current && snapshot) {
      if (snapshot.pinned?.question) setPinnedDraft(snapshot.pinned.question);
      pinnedInitialized.current = true;
    }
  }, [snapshot]);

  useEffect(() => {
    if (!pinnedDirty) return;
    const sequence = ++pinnedSaveSequence.current;
    setPinnedSaveState("Waiting to save…");
    const timer = window.setTimeout(() => {
      const observedRevision = settings?.state_revision;
      if (observedRevision === undefined) {
        setPinnedSaveState("Save unavailable");
        return;
      }
      setPinnedSaveState("Saving…");
      void setHostedPinnedQuestion(observedRevision, pinnedDraft)
        .then(async (result) => {
          if (pinnedSaveSequence.current !== sequence) return;
          if (result.status === "conflict") {
            setPinnedSaveState("Settings changed; review and try again");
            await onRefresh();
            return;
          }
          setPinnedDirty(false);
          setPinnedSaveState(pinnedDraft.trim() ? "Saved" : "Cleared");
          await onRefresh();
        })
        .catch((reason) => {
          if (pinnedSaveSequence.current === sequence) {
            setPinnedSaveState(`Save failed: ${String(reason)}`);
          }
        });
    }, 500);
    return () => window.clearTimeout(timer);
  }, [onRefresh, pinnedDirty, pinnedDraft, settings?.state_revision]);

  const exchanges = useMemo(
    () => boundAssistantExchanges(snapshot?.exchanges ?? []),
    [snapshot?.exchanges],
  );
  const omitted = Math.max(0, (snapshot?.exchanges.length ?? 0) - exchanges.length);
  const pinned = snapshot?.pinned ?? null;
  const shownPinnedAnswer = pinned?.answer ?? lastAcceptedPinned.current?.answer ?? null;
  const pinnedAnswerIsEarlier = Boolean(
    shownPinnedAnswer && pinned && pinned.status !== "completed" && !pinned.answer,
  );
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
  const templatePresent = Boolean(
    settings && (settings.control.pinned_question_revision > 0 || pinnedDraft.trim()),
  );

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

      <section className="live-pinned-card" aria-labelledby="live-pinned-heading">
        <header>
          <div>
            <h3 id="live-pinned-heading">Pinned question</h3>
            <span>{snapshot?.pinned_run_count ?? 0} session run(s)</span>
          </div>
          <span className={templatePresent ? "live-answer-state live-answer-completed" : "live-answer-state"}>
            {templatePresent ? "Saved" : "Not set"}
          </span>
        </header>
        <label className="live-assistant-field">
          <span>{templatePresent ? "Replace or edit the one pinned question" : "Pin one question"}</span>
          <textarea
            rows={2}
            maxLength={32 * 1024}
            value={pinnedDraft}
            placeholder={
              templatePresent && !pinnedDraft
                ? "Enter a complete replacement for the saved question"
                : "Ask for the current decision, risk, or next step"
            }
            onChange={(event) => {
              setPinnedDraft(event.target.value);
              setPinnedDirty(true);
            }}
          />
        </label>
        <p
          className={`live-debounce-state${pinnedSaveState.startsWith("Save failed") ? " live-assistant-error" : ""}`}
          role={pinnedSaveState.startsWith("Save failed") ? "alert" : "status"}
          aria-live="polite"
        >
          {pinnedSaveState || "Edits save after 500 ms of quiet."}
        </p>
        {pinnedSaveState.startsWith("Save failed") && (
          <button
            className="btn-quiet live-inline-repair"
            type="button"
            onClick={() => void onOpenPreferences("hosted-language")}
          >
            Edit pinned question in Preferences
          </button>
        )}

        {settings && (
          <HostedSwitch
            label="Automatic updates"
            description={
              settings.control.pinned_auto_enabled
                ? "Meaningful transcript progress can run this question again."
                : "Off · enabling requires repeated-cost acknowledgement."
            }
            checked={settings.control.pinned_auto_enabled}
            disabled={!templatePresent || !settings.control.questions.enabled || !questionsConfigured}
            onChange={(enabled) => {
              if (enabled) setConfirmAuto(true);
              else {
                void onPatch(
                  { kind: "set_pinned_auto", enabled: false, acknowledged: false },
                  "Automatic pinned questions are off.",
                );
              }
            }}
          />
        )}

        {templatePresent && (
          <p className="live-pinned-guidance">
            {!sessionActive
              ? "Start the microphone test or join a live call to run this question."
              : questionConfiguration
                ? questionConfiguration.message
                : !settings?.control.questions.enabled
                  ? "Enable Questions to run the pinned question."
                  : !settings.control.master_enabled
                    ? "Turn on Master to allow the pinned question to run."
                    : !settings.control.pinned_auto_enabled
                      ? "Turn on Automatic updates to run this question as the transcript grows."
                      : (snapshot?.pinned_run_count ?? 0) === 0
                        ? "Waiting for enough context: about 40 words or 30 seconds of speech, followed by a short pause. Existing context counts after setup."
                        : "The next update runs after about 40 new words or 30 seconds of new speech and a short pause."}
          </p>
        )}

        {pinned ? (
          <QuestionResult
            exchange={pinned}
            shownAnswer={shownPinnedAnswer}
            answerIsEarlier={pinnedAnswerIsEarlier}
            call={calls.find((item) => item.call_id === pinned.call_id)}
            detailsEnabled={detailsEnabled}
            onOpenPreferences={onOpenPreferences}
          />
        ) : (
          <p className="live-answer-placeholder">
            The running answer and its transcript revision will appear here.
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
                  shownAnswer={exchange.answer}
                  answerIsEarlier={false}
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
            "Automatic pinned questions enabled.",
          ).then((saved) => {
            if (saved) setConfirmAuto(false);
          });
        }}
      >
        <p>
          Each meaningful transcript update can make another paid request. Cancellation after dispatch may
          still be billed; an exact local cache hit is not guaranteed.
        </p>
      </HostedDialog>
    </div>
  );
}

function QuestionResult({
  exchange,
  shownAnswer,
  answerIsEarlier,
  call,
  detailsEnabled,
  onOpenPreferences,
  onCancel,
  canceling = false,
}: {
  exchange: HostedAssistantExchange;
  shownAnswer: string | null;
  answerIsEarlier: boolean;
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
          <p>{shownAnswer}</p>
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
