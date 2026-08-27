import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import {
  getHostedAssistant,
  getHostedSettings,
  getLiveTestWindowGeneration,
  getLiveTranscript,
  onHostedStateChanged,
  onLiveTranscriptChanged,
  openPreferencesSection,
  patchHostedSettings,
  startLiveTest,
  stopLiveTest,
  updateHostedSteering,
  type HostedAccountingEvent,
  type HostedAssistantSnapshot,
  type HostedCallLane,
  type HostedCoordinatorEvent,
  type HostedLaneState,
  type HostedMutationResult,
  type HostedPatchInput,
  type HostedSettingsDto,
  type HostedTerminalEvent,
  type LiveTranscriptEvent,
  type LiveTranscriptLine,
  type LiveTranscriptSnapshot,
  type PreferencesSection,
} from "./lib/api";
import {
  VERTEX_UNARMED_WARNING,
  hostedErrorGuidance,
  hostedOnboardingGuidance,
  laneConfigurationGuidance,
  unknownHostedErrorGuidance,
  type HostedActionGuidance,
} from "./lib/hosted";
import {
  applyLiveSnapshot,
  formatLiveRange,
  reduceLiveEvent,
} from "./lib/liveTranscript";
import {
  applyHostedCallEvent,
  hostedUiFenceMatches,
  shouldInstallHostedSettings,
  type LiveCallDetail,
} from "./lib/liveHosted";
import { Assistant } from "./live/Assistant";
import { CallDetails } from "./live/CallDetails";
import { DiffText } from "./live/DiffText";
import {
  LiveHostedControls,
  TranscriptViewControl,
  type TranscriptView,
} from "./live/LiveControls";

interface WarningToast {
  key: string;
  message: string;
}

export default function LiveTranscript() {
  const [snapshot, setSnapshot] = useState<LiveTranscriptSnapshot | null>(null);
  const snapshotRef = useRef<LiveTranscriptSnapshot | null>(null);
  const [liveError, setLiveError] = useState("");
  const [repairing, setRepairing] = useState(false);
  const [testBusy, setTestBusy] = useState(false);
  const [windowGeneration, setWindowGeneration] = useState<number | null>(null);
  const [view, setView] = useState<TranscriptView>("clean");
  const [settings, setSettings] = useState<HostedSettingsDto | null>(null);
  const settingsRef = useRef<HostedSettingsDto | null>(null);
  const [hostedError, setHostedError] = useState("");
  const [controlBusy, setControlBusy] = useState("");
  const mutationBusy = useRef(false);
  const [controlStatus, setControlStatus] = useState("");
  const [controlRepair, setControlRepair] = useState<{
    label: string;
    section: PreferencesSection;
  } | null>(null);
  const [detailsEnabled, setDetailsEnabled] = useState(false);
  const detailsInitialized = useRef(false);
  const [laneStates, setLaneStates] = useState<Record<"live" | "final", HostedLaneState>>({
    live: "disabled",
    final: "disabled",
  });
  const [calls, setCalls] = useState<LiveCallDetail[]>([]);
  const [assistant, setAssistant] = useState<HostedAssistantSnapshot | null>(null);
  const [assistantLoading, setAssistantLoading] = useState(true);
  const [assistantError, setAssistantError] = useState("");
  const assistantRequest = useRef(0);
  const [toast, setToast] = useState<WarningToast | null>(null);
  const seenNotices = useRef(new Set<string>());
  const [announcement, setAnnouncement] = useState("");
  const scroller = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const narrow = useNarrowAssistant();
  const [assistantOpen, setAssistantOpen] = useState(false);
  const [assistantUnread, setAssistantUnread] = useState(0);
  const seenAnswers = useRef(new Set<string>());
  const assistantTrigger = useRef<HTMLButtonElement>(null);
  const assistantClose = useRef<HTMLButtonElement>(null);
  const drawer = useRef<HTMLElement>(null);
  const mainSurface = useRef<HTMLDivElement>(null);

  const showHostedGuidance = useCallback((guidance: HostedActionGuidance) => {
    setControlStatus(guidance.message);
    setControlRepair(
      guidance.section && guidance.actionLabel
        ? { section: guidance.section, label: guidance.actionLabel }
        : null,
    );
  }, []);

  const openPreferences = useCallback(async (section: PreferencesSection) => {
    try {
      await openPreferencesSection(section);
    } catch {
      setControlStatus("Could not open Preferences. Open it from the Corti menu and choose Hosted rewrite.");
      setControlRepair(null);
    }
  }, []);

  const installSettings = useCallback((next: HostedSettingsDto) => {
    const current = settingsRef.current;
    if (!shouldInstallHostedSettings(current, next)) return;
    const invalidatesGuidance = Boolean(
      current &&
        (current.control.process_epoch !== next.control.process_epoch ||
          next.state_revision > current.state_revision),
    );
    settingsRef.current = next;
    setSettings(next);
    setHostedError("");
    if (invalidatesGuidance) {
      setControlStatus("");
      setControlRepair(null);
    }
    if (!detailsInitialized.current) {
      detailsInitialized.current = true;
      setDetailsEnabled(next.show_live_metrics_by_default);
    }
    setLaneStates((current) => ({
      live:
        !next.control.master_enabled || !next.control.live.enabled
          ? "disabled"
          : current.live === "disabled"
            ? "waiting_for_phrase"
            : current.live,
      final:
        !next.control.master_enabled || !next.control.final_lane.enabled
          ? "disabled"
          : current.final === "disabled"
            ? "waiting_for_phrase"
            : current.final,
    }));
  }, []);

  useEffect(() => {
    let active = true;
    getLiveTestWindowGeneration()
      .then((generation) => {
        if (active) setWindowGeneration(generation);
      })
      .catch(() => {
        if (active) setLiveError("The Live Transcript window lifecycle is unavailable.");
      });
    return () => {
      active = false;
    };
  }, []);

  const refreshSettings = useCallback(async () => {
    try {
      installSettings(await getHostedSettings());
    } catch {
      setHostedError("Hosted controls are unavailable. Raw transcript viewing still works.");
    }
  }, [installSettings]);

  const refreshAssistant = useCallback(async () => {
    const request = ++assistantRequest.current;
    setAssistantLoading(true);
    try {
      const next = await getHostedAssistant();
      if (request !== assistantRequest.current) return;
      if (!next || !Array.isArray(next.exchanges)) throw new Error("invalid assistant snapshot");
      setAssistant(next);
      setAssistantError("");
    } catch {
      if (request === assistantRequest.current) {
        setAssistantError("Assistant state is unavailable. The transcript is unaffected.");
      }
    } finally {
      if (request === assistantRequest.current) setAssistantLoading(false);
    }
  }, []);

  useEffect(() => {
    document.title = "Live Transcript — Corti";
    let active = true;
    let remove: (() => void) | undefined;
    let current: LiveTranscriptSnapshot | null = null;
    let buffered: LiveTranscriptEvent[] = [];
    let fetching = false;
    let refreshAgain = false;
    let repairTimer: number | undefined;

    function publish(next: LiveTranscriptSnapshot) {
      current = next;
      snapshotRef.current = next;
      if (active) setSnapshot(next);
    }

    function scheduleRepair(delay = 0) {
      if (!active || repairTimer !== undefined) return;
      repairTimer = window.setTimeout(() => {
        repairTimer = undefined;
        void refresh();
      }, delay);
    }

    function replayBuffered(base: LiveTranscriptSnapshot): LiveTranscriptSnapshot {
      let next = base;
      const stillWaiting: LiveTranscriptEvent[] = [];
      const ordered = [...buffered].sort((left, right) => left.revision - right.revision);
      buffered = [];
      for (const event of ordered) {
        const reduced = reduceLiveEvent(next, event);
        if (reduced.outcome === "applied" && reduced.snapshot) next = reduced.snapshot;
        else if (reduced.outcome === "gap" || reduced.outcome === "process_change") {
          stillWaiting.push(event);
        }
      }
      buffered = stillWaiting;
      setRepairing(stillWaiting.length > 0);
      if (stillWaiting.length > 0) scheduleRepair(120);
      return next;
    }

    async function refresh() {
      if (!active) return;
      if (fetching) {
        refreshAgain = true;
        return;
      }
      fetching = true;
      try {
        const incoming = await getLiveTranscript();
        if (!active) return;
        const baseline = applyLiveSnapshot(current, incoming);
        publish(replayBuffered(baseline));
        setLiveError("");
      } catch {
        if (active) setLiveError("Could not refresh the live transcript. Existing raw rows are retained.");
      } finally {
        fetching = false;
        if (refreshAgain) {
          refreshAgain = false;
          scheduleRepair();
        }
      }
    }

    function receive(event: LiveTranscriptEvent) {
      if (!active) return;
      if (!current) {
        buffered.push(event);
        return;
      }
      const reduced = reduceLiveEvent(current, event);
      if (reduced.outcome === "applied" && reduced.snapshot) {
        const previousCommit = event.line?.commit_epoch ?? 0;
        publish(reduced.snapshot);
        if (previousCommit > 0 && event.line?.clean_text !== undefined) {
          setAnnouncement(`Clean rewrite accepted for row ${event.line.seq}.`);
        }
      } else if (reduced.outcome === "gap" || reduced.outcome === "process_change") {
        if (!buffered.some((item) => item.revision === event.revision)) buffered.push(event);
        setRepairing(true);
        scheduleRepair();
      }
    }

    void (async () => {
      try {
        remove = await onLiveTranscriptChanged(receive);
        if (!active) {
          remove();
          return;
        }
      } catch {
        // The reconciliation snapshot and interval remain a read-only fallback.
      }
      await refresh();
    })();
    const reconciliation = window.setInterval(() => void refresh(), 30_000);
    return () => {
      active = false;
      window.clearInterval(reconciliation);
      if (repairTimer !== undefined) window.clearTimeout(repairTimer);
      remove?.();
    };
  }, []);

  useEffect(() => {
    let active = true;
    let remove: (() => void) | undefined;
    let settingsTimer: number | undefined;
    let assistantTimer: number | undefined;

    function scheduleSettings() {
      if (settingsTimer !== undefined) return;
      settingsTimer = window.setTimeout(() => {
        settingsTimer = undefined;
        if (active) void refreshSettings();
      }, 80);
    }

    function scheduleAssistant() {
      if (assistantTimer !== undefined) window.clearTimeout(assistantTimer);
      assistantTimer = window.setTimeout(() => {
        assistantTimer = undefined;
        if (active) void refreshAssistant();
      }, 80);
    }

    function receive(event: HostedCoordinatorEvent) {
      if (!active) return;
      if (
        event.event === "lane_state" &&
        "lane" in event &&
        "state" in event &&
        "fence" in event
      ) {
        const laneEvent = event as Extract<HostedCoordinatorEvent, { event: "lane_state" }>;
        const lane = laneEvent.lane as HostedCallLane;
        if (!hostedUiFenceMatches(settingsRef.current, lane, laneEvent.fence)) return;
        if (lane === "live" || lane === "final") {
          setLaneStates((current) => ({
            ...current,
            [lane]: laneEvent.state as HostedLaneState,
          }));
        }
        if (lane === "ad_hoc_question" || lane === "pinned_question") scheduleAssistant();
        return;
      }
      if (event.event === "accounting" && "call_id" in event) {
        const accounting = event as HostedAccountingEvent;
        const currentSnapshot = snapshotRef.current;
        setCalls((current) =>
          applyHostedCallEvent(current, accounting, {
            processEpoch: settingsRef.current?.control.process_epoch ?? null,
            sessionId: currentSnapshot?.session_id ?? null,
            sessionGeneration: settingsRef.current?.control.session_generation ?? null,
          }),
        );
        if (accounting.lane.endsWith("question")) scheduleAssistant();
        return;
      }
      if (event.event === "terminal" && "recording_id" in event) {
        const terminal = event as HostedTerminalEvent;
        const currentSnapshot = snapshotRef.current;
        setCalls((current) =>
          applyHostedCallEvent(current, terminal, {
            processEpoch: settingsRef.current?.control.process_epoch ?? null,
            sessionId: currentSnapshot?.session_id ?? null,
            sessionGeneration: settingsRef.current?.control.session_generation ?? null,
          }),
        );
        if (terminal.lane === "live" && terminal.outcome !== "completed") {
          setLaneStates((current) => ({ ...current, live: terminal.outcome === "failed" ? "failed" : "using_raw" }));
        }
        if (
          terminal.error &&
          !["canceled", "superseded"].includes(terminal.error)
        ) {
          const guidance = hostedErrorGuidance(terminal.error);
          setControlStatus(guidance.message);
          setControlRepair(
            guidance.section && guidance.actionLabel
              ? { section: guidance.section, label: guidance.actionLabel }
              : null,
          );
        }
        if (terminal.lane.endsWith("question")) scheduleAssistant();
        return;
      }
      if (event.event === "notice" && "visible_message" in event && "episode" in event) {
        const message = String(event.visible_message);
        const key = `${settingsRef.current?.control.process_epoch ?? "process"}:${String(event.episode)}:${message}`;
        if (!seenNotices.current.has(key)) {
          seenNotices.current.add(key);
          setToast({ key, message });
        }
        return;
      }
      if (
        event.event === "control_changed" ||
        event.event === "provider_state" ||
        event.event === "persistence_warning"
      ) {
        scheduleSettings();
      }
    }

    void (async () => {
      try {
        remove = await onHostedStateChanged(receive);
        if (!active) {
          remove();
          return;
        }
      } catch {
        // Revision-checked commands and explicit refreshes remain authoritative.
      }
      await Promise.all([refreshSettings(), refreshAssistant()]);
    })();
    return () => {
      active = false;
      if (settingsTimer !== undefined) window.clearTimeout(settingsTimer);
      if (assistantTimer !== undefined) window.clearTimeout(assistantTimer);
      remove?.();
    };
  }, [refreshAssistant, refreshSettings]);

  const assistantRunning = Boolean(
    assistant &&
      [assistant.pinned, ...assistant.exchanges].some(
        (exchange) =>
          exchange && ["queued", "waiting_for_credential", "running"].includes(exchange.status),
      ),
  );
  useEffect(() => {
    if (!assistantRunning) return;
    const timer = window.setInterval(() => void refreshAssistant(), 1_000);
    return () => window.clearInterval(timer);
  }, [assistantRunning, refreshAssistant]);

  useEffect(() => {
    if (!snapshot) return;
    setCalls([]);
    setAssistant(null);
    setAssistantLoading(true);
    setAssistantUnread(0);
    seenAnswers.current.clear();
    const currentSettings = settingsRef.current;
    setLaneStates({
      live:
        currentSettings?.control.master_enabled && currentSettings.control.live.enabled
          ? "waiting_for_phrase"
          : "disabled",
      final:
        currentSettings?.control.master_enabled && currentSettings.control.final_lane.enabled
          ? "waiting_for_phrase"
          : "disabled",
    });
    setAnnouncement(`Live session changed. ${snapshot.lines.length} raw row(s) available.`);
    void refreshAssistant();
  }, [snapshot?.session_id, snapshot?.session_generation, refreshAssistant]);

  useEffect(() => {
    if (!assistant) return;
    const completed = [assistant.pinned, ...assistant.exchanges].filter(
      (exchange): exchange is NonNullable<typeof exchange> =>
        Boolean(exchange?.status === "completed" && exchange.answer),
    );
    let added = 0;
    for (const exchange of completed) {
      if (!seenAnswers.current.has(exchange.call_id)) {
        seenAnswers.current.add(exchange.call_id);
        added += 1;
      }
    }
    if (narrow && !assistantOpen && added > 0) setAssistantUnread((count) => count + added);
    if (!narrow || assistantOpen) setAssistantUnread(0);
  }, [assistant, assistantOpen, narrow]);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast((current) => (current?.key === toast.key ? null : current)), 8_000);
    return () => window.clearTimeout(timer);
  }, [toast]);

  useEffect(() => {
    if (!narrow || !assistantOpen) return;
    const priorFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    mainSurface.current?.setAttribute("inert", "");
    window.requestAnimationFrame(() => assistantClose.current?.focus());
    return () => {
      mainSurface.current?.removeAttribute("inert");
      (priorFocus ?? assistantTrigger.current)?.focus();
    };
  }, [assistantOpen, narrow]);

  useLayoutEffect(() => {
    const node = scroller.current;
    if (node && follow.current) node.scrollTop = node.scrollHeight;
  }, [snapshot?.revision, view]);

  const acceptMutation = useCallback(
    (result: HostedMutationResult, success: string): boolean => {
      installSettings(result.settings);
      setControlRepair(null);
      switch (result.status) {
        case "applied":
          setControlStatus(success);
          return true;
        case "unchanged":
          setControlStatus("No canonical change; current controls were kept.");
          return true;
        case "conflict":
          setControlStatus("Controls changed elsewhere. Latest state loaded; review and try again.");
          return false;
        case "invalid":
          setControlStatus("That settings update was invalid. Nothing was saved; review it in Preferences.");
          return false;
        case "disabled_for_session":
          setControlStatus("Off for this session because the setting could not be saved. Raw transcript behavior is unchanged.");
          setControlRepair({ label: "Open diagnostics", section: "hosted-advanced" });
          return true;
      }
    },
    [installSettings],
  );

  const mutateControl = useCallback(
    async (patch: HostedPatchInput, success: string): Promise<boolean> => {
      const current = settingsRef.current;
      if (!current || mutationBusy.current) return false;
      const label =
        patch.kind === "set_master"
          ? "master"
          : patch.kind === "set_lane_enabled"
            ? patch.lane
            : "control";
      mutationBusy.current = true;
      setControlBusy(label);
      setControlStatus("");
      setControlRepair(null);
      try {
        return acceptMutation(await patchHostedSettings(current.state_revision, patch), success);
      } catch (error) {
        showHostedGuidance(unknownHostedErrorGuidance(error));
        return false;
      } finally {
        mutationBusy.current = false;
        setControlBusy("");
      }
    },
    [acceptMutation, showHostedGuidance],
  );

  const mutateSteering = useCallback(
    async (text: string, persist: boolean): Promise<boolean> => {
      const current = settingsRef.current;
      if (!current || mutationBusy.current) return false;
      mutationBusy.current = true;
      setControlBusy("steering");
      setControlStatus("");
      setControlRepair(null);
      try {
        return acceptMutation(
          await updateHostedSteering(current.state_revision, text, persist),
          persist
            ? "Steering saved as default and applied to the next request."
            : "Session steering applied to the next request.",
        );
      } catch (error) {
        showHostedGuidance(unknownHostedErrorGuidance(error));
        return false;
      } finally {
        mutationBusy.current = false;
        setControlBusy("");
      }
    },
    [acceptMutation, showHostedGuidance],
  );

  async function startTest() {
    if (windowGeneration == null) {
      setLiveError("The microphone test cannot start from a stale window.");
      return;
    }
    setTestBusy(true);
    setLiveError("");
    try {
      await startLiveTest(windowGeneration);
    } catch {
      setLiveError("The microphone test could not start.");
    } finally {
      setTestBusy(false);
    }
  }

  async function stopTest() {
    setTestBusy(true);
    setLiveError("");
    try {
      await stopLiveTest();
    } catch {
      setLiveError("The microphone test could not stop cleanly.");
    } finally {
      setTestBusy(false);
    }
  }

  function closeAssistant() {
    setAssistantOpen(false);
  }

  function trapDrawerKeys(event: ReactKeyboardEvent<HTMLElement>) {
    if (event.key === "Escape") {
      event.preventDefault();
      closeAssistant();
      return;
    }
    if (event.key !== "Tab" || !drawer.current) return;
    const focusable = Array.from(
      drawer.current.querySelectorAll<HTMLElement>(
        "button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex='-1'])",
      ),
    ).filter((element) => element.getClientRects().length > 0);
    if (focusable.length === 0) {
      event.preventDefault();
      return;
    }
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  const mode = snapshot?.mode ?? "idle";
  const status = snapshot?.status ?? "loading";
  const canStart =
    windowGeneration != null && mode !== "call" && !snapshot?.active && status !== "stopping";
  const canStop = mode === "test" && snapshot?.active;
  const sessionActive = Boolean(snapshot?.active && snapshot.session_id);
  const rows = snapshot?.lines ?? [];
  const onboarding = settings ? hostedOnboardingGuidance(settings) : null;
  const assistantPanel = (
    <Assistant
      key={`${snapshot?.session_id ?? "no-session"}:${snapshot?.session_generation ?? "unknown"}`}
      snapshot={assistant}
      settings={settings}
      calls={calls}
      sessionActive={sessionActive}
      detailsEnabled={detailsEnabled}
      loading={assistantLoading}
      error={assistantError}
      closeButtonRef={narrow ? assistantClose : undefined}
      onClose={narrow ? closeAssistant : undefined}
      onRefresh={refreshAssistant}
      onPatch={mutateControl}
      onOpenPreferences={openPreferences}
    />
  );

  return (
    <div className="app live-app live-experience">
      <div className="live-main-surface" ref={mainSurface}>
        <header className="app-header live-header">
          <div className="live-title-block">
            <h1>{snapshot?.title ?? "Live transcript"}</h1>
            <p className="subtitle">
              <span className={`live-status live-status-${status}`} aria-hidden="true" />
              <span>{snapshot?.detail ?? emptyMessage(status)}</span>
            </p>
          </div>
          <div className="live-actions">
            <button
              ref={assistantTrigger}
              className="btn-secondary live-assistant-trigger"
              type="button"
              aria-haspopup="dialog"
              aria-expanded={narrow ? assistantOpen : undefined}
              onClick={() => {
                setAssistantUnread(0);
                setAssistantOpen(true);
              }}
            >
              Assistant
              {assistantUnread > 0 && <span className="live-unread-badge" aria-label={`${assistantUnread} unread`}>{assistantUnread}</span>}
            </button>
            {canStart && (
              <button className="btn-add" disabled={testBusy} onClick={() => void startTest()}>
                {testBusy ? "Starting…" : "Test microphone"}
              </button>
            )}
            {canStop && (
              <button
                className="btn-add live-stop"
                disabled={testBusy || status === "stopping"}
                onClick={() => void stopTest()}
              >
                {status === "stopping" ? "Stopping…" : "Stop test"}
              </button>
            )}
          </div>
        </header>

        <LiveHostedControls
          settings={settings}
          liveState={laneStates.live}
          finalState={laneStates.final}
          busy={controlBusy}
          detailsEnabled={detailsEnabled}
          onDetailsChange={setDetailsEnabled}
          onPatch={mutateControl}
          onSteering={mutateSteering}
          onBlockedEnable={() =>
            showHostedGuidance({
              message: "Review and acknowledge the text-only privacy boundary before enabling Master.",
              actionLabel: "Review privacy & enable",
              section: "hosted",
            })
          }
          onConfigurationNeeded={(lane) => {
            const guidance = settings ? laneConfigurationGuidance(settings, lane) : null;
            if (guidance) showHostedGuidance(guidance);
          }}
        />

        {onboarding && (
          <section className="live-setup-callout" aria-label="Hosted rewrite setup">
            <div>
              <strong>Finish hosted rewrite setup</strong>
              <span>{onboarding.message}</span>
            </div>
            {onboarding.section && onboarding.actionLabel && (
              <button
                className="btn-primary"
                type="button"
                onClick={() => void openPreferences(onboarding.section!)}
              >
                {onboarding.actionLabel}
              </button>
            )}
          </section>
        )}

        {mode === "test" && (
          <p className="callout small live-test-callout">
            Test mode listens only to your default microphone. It saves no recording or note and adds nothing
            to the queue. Enabled hosted modes may send transcript text and use their configured encrypted
            cache; automatic call detection resumes when you stop the test.
          </p>
        )}
        {(liveError || hostedError || controlStatus || repairing) && (
          <div
            className={`live-status-banner${liveError || hostedError ? " live-status-banner-error" : ""}`}
            role={liveError || hostedError ? "alert" : "status"}
            aria-live="polite"
          >
            <span>
              {liveError || hostedError || (repairing ? "Repairing a missed live update; existing raw rows remain visible." : controlStatus)}
            </span>
            {!liveError && !repairing && (controlRepair || hostedError) && (
              <button
                className="btn-secondary"
                type="button"
                onClick={() =>
                  void openPreferences(controlRepair?.section ?? "hosted-advanced")
                }
              >
                {controlRepair?.label ?? "Open hosted diagnostics"}
              </button>
            )}
          </div>
        )}

        <div className="live-shell">
          <main className="live-transcript-pane" aria-label="Live transcript viewer">
            <div className="live-transcript-toolbar">
              <TranscriptViewControl value={view} onChange={setView} />
              <span className="live-revision">Transcript r{snapshot?.revision ?? 0}</span>
            </div>
            {(snapshot?.evicted_lines ?? 0) > 0 && (
              <p className="muted small live-trimmed">
                {snapshot?.evicted_lines.toLocaleString()} earlier row(s) omitted from this bounded live view.
                The durable call note is unaffected.
              </p>
            )}
            <div
              className="live-scroll"
              ref={scroller}
              onScroll={(event) => {
                const node = event.currentTarget;
                follow.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
              }}
            >
              {rows.length === 0 ? (
                <div className="live-empty" role="status">
                  <p>{emptyMessage(status)}</p>
                  {status === "listening" && <p className="muted small">Raw rows appear as soon as a phrase closes.</p>}
                </div>
              ) : (
                <ol className="live-lines">
                  {rows.map((line, index) => (
                    <TranscriptRow
                      line={line}
                      view={view}
                      activity={
                        index === rows.length - 1 &&
                        ["debouncing", "queued", "arming", "catching_up", "rewriting"].includes(laneStates.live)
                      }
                      key={line.seq}
                    />
                  ))}
                </ol>
              )}
            </div>
            {detailsEnabled && (
              <CallDetails calls={calls} onOpenPreferences={openPreferences} />
            )}
          </main>

          {!narrow && <aside className="live-assistant-sidebar" aria-label="Transcript assistant">{assistantPanel}</aside>}
        </div>
      </div>

      {narrow && assistantOpen && (
        <div
          className="live-drawer-backdrop"
          onMouseDown={(event) => {
            if (event.currentTarget === event.target) closeAssistant();
          }}
        >
          <aside
            ref={drawer}
            className="live-assistant-drawer"
            role="dialog"
            aria-modal="true"
            aria-label="Transcript assistant"
            onKeyDown={trapDrawerKeys}
          >
            {assistantPanel}
          </aside>
        </div>
      )}

      {toast && (
        <div className="live-warning-toast">
          <span role="alert" aria-live="assertive" aria-atomic="true">{toast.message}</span>
          {toast.message === VERTEX_UNARMED_WARNING && (
            <button
              className="btn-secondary"
              type="button"
              onClick={() => void openPreferences("hosted-provider")}
            >
              Fix Vertex setup
            </button>
          )}
        </div>
      )}
      <div className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {announcement}
      </div>
    </div>
  );
}

function TranscriptRow({
  line,
  view,
  activity,
}: {
  line: LiveTranscriptLine;
  view: TranscriptView;
  activity: boolean;
}) {
  const commit = line.commit_epoch ?? 0;
  const priorCommit = useRef(commit);
  const animateAccepted = commit > 0 && commit !== priorCommit.current;
  useEffect(() => {
    priorCommit.current = commit;
  }, [commit]);
  const clean = typeof line.clean_text === "string" ? line.clean_text : null;
  const accepted = clean !== null && (line.rewrite_state === "clean" || commit > 0);
  const changed = accepted && clean !== line.text;
  const display = view === "raw" ? line.text : (clean ?? line.text);
  const stateLabel =
    view === "raw"
      ? "Raw"
      : !accepted
        ? "Raw fallback"
        : view === "changes" && changed
          ? "Changed"
          : "Clean";
  return (
    <li
      className={`live-line live-line-${line.speaker === "Me" ? "me" : "them"}${activity ? " live-line-active" : ""}${accepted ? " live-line-accepted" : ""}`}
    >
      <div
        className={`live-rewrite-surface${animateAccepted ? " live-accepted-wash" : ""}`}
        key={`${line.seq}:${commit}`}
      >
        <div className="live-line-meta">
          <time>{formatLiveRange(line)}</time>
          <strong>{line.speaker}</strong>
          <span className={`live-row-state live-row-state-${stateLabel.toLowerCase().replace(/\s/gu, "-")}`}>
            {stateLabel}
          </span>
        </div>
        <p>
          {view === "changes" && accepted && changed ? (
            <DiffText raw={line.text} clean={clean} />
          ) : (
            display
          )}
        </p>
      </div>
    </li>
  );
}

function emptyMessage(status: LiveTranscriptSnapshot["status"]): string {
  switch (status) {
    case "loading":
      return "Preparing live transcription…";
    case "listening":
      return "Listening for speech…";
    case "stopping":
      return "Flushing the final speech region…";
    case "unavailable":
    case "error":
      return "No live transcript is available. Raw text remains the fallback when present.";
    case "complete":
      return "The session completed without recognized speech.";
    default:
      return "Join a call, or run a microphone transcription test.";
  }
}

function useNarrowAssistant(): boolean {
  const query = "(max-width: 819px)";
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const media = window.matchMedia(query);
    const update = () => setMatches(media.matches);
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return matches;
}

// Keep this referenced in the bundle as the exact product warning; tests assert no decorated substitute.
export const LIVE_VERTEX_WARNING = VERTEX_UNARMED_WARNING;
