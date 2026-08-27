import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import type {
  HostedLane,
  HostedLaneState,
  HostedPatchInput,
  HostedSettingsDto,
} from "../lib/api";
import { laneConfigurationGuidance } from "../lib/hosted";
import { laneStateLabel } from "../lib/liveHosted";

export type TranscriptView = "raw" | "clean" | "changes";

export function TranscriptViewControl({
  value,
  onChange,
}: {
  value: TranscriptView;
  onChange: (next: TranscriptView) => void;
}) {
  const views: TranscriptView[] = ["raw", "clean", "changes"];
  function move(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const current = views.indexOf(value);
    const next =
      event.key === "Home"
        ? views[0]
        : event.key === "End"
          ? views[views.length - 1]
          : views[(current + (event.key === "ArrowRight" ? 1 : -1) + views.length) % views.length];
    onChange(next);
    document.getElementById(`live-view-${next}`)?.focus();
  }
  return (
    <div
      className="live-view-control"
      role="radiogroup"
      aria-label="Transcript text view"
      onKeyDown={move}
    >
      {views.map((view) => (
        <button
          id={`live-view-${view}`}
          className={value === view ? "live-view-selected" : ""}
          type="button"
          role="radio"
          aria-checked={value === view}
          tabIndex={value === view ? 0 : -1}
          onClick={() => onChange(view)}
          key={view}
        >
          {view === "raw" ? "Raw" : view === "clean" ? "Clean" : "Changes"}
        </button>
      ))}
    </div>
  );
}

export function LiveHostedControls({
  settings,
  liveState,
  finalState,
  busy,
  detailsEnabled,
  onDetailsChange,
  onPatch,
  onSteering,
  onBlockedEnable,
  onConfigurationNeeded,
}: {
  settings: HostedSettingsDto | null;
  liveState: HostedLaneState;
  finalState: HostedLaneState;
  busy: string;
  detailsEnabled: boolean;
  onDetailsChange: (enabled: boolean) => void;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
  onSteering: (text: string, persist: boolean) => Promise<boolean>;
  onBlockedEnable: () => void;
  onConfigurationNeeded: (lane: HostedLane) => void;
}) {
  const [steeringOpen, setSteeringOpen] = useState(false);
  const [steering, setSteering] = useState("");
  const [persist, setPersist] = useState(false);
  const popover = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const textarea = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!steeringOpen && settings) setSteering(settings.default_steering);
  }, [settings, steeringOpen]);

  useEffect(() => {
    if (!steeringOpen) return;
    textarea.current?.focus();
    function closeOnPointer(event: MouseEvent) {
      if (!popover.current?.contains(event.target as Node) && event.target !== trigger.current) {
        setSteeringOpen(false);
      }
    }
    document.addEventListener("mousedown", closeOnPointer);
    return () => document.removeEventListener("mousedown", closeOnPointer);
  }, [steeringOpen]);

  function closeSteering(restoreFocus: boolean) {
    setSteeringOpen(false);
    if (restoreFocus) window.requestAnimationFrame(() => trigger.current?.focus());
  }

  const disabled = !settings || Boolean(busy);
  const master = settings?.control.master_enabled ?? false;
  const live = settings?.control.live.enabled ?? false;
  const final = settings?.control.final_lane.enabled ?? false;
  const liveNeedsSetup = settings ? laneConfigurationGuidance(settings, "live") !== null : true;
  const finalNeedsSetup = settings ? laneConfigurationGuidance(settings, "final") !== null : true;

  return (
    <section className="live-hosted-controls" aria-label="Hosted session controls">
      <div className="live-switches">
        <LiveSwitch
          label="Master"
          checked={master}
          disabled={disabled}
          pending={busy === "master"}
          onChange={(enabled) => {
            if (enabled && !settings?.control.egress_acknowledged) {
              onBlockedEnable();
              return;
            }
            void onPatch(
              { kind: "set_master", enabled },
              enabled
                ? "Master enabled for the next request."
                : "Master disabled. In-flight work is canceled best effort.",
            );
          }}
        />
        <LiveSwitch
          label="Live"
          checked={live}
          disabled={disabled}
          pending={busy === "live"}
          onChange={(enabled) => {
            if (enabled && liveNeedsSetup) {
              onConfigurationNeeded("live");
              return;
            }
            void onPatch(
              { kind: "set_lane_enabled", lane: "live", enabled },
              enabled ? "Live cleanup enabled." : "Live cleanup disabled; raw stays visible.",
            );
          }}
        />
        <LiveSwitch
          label="Final"
          checked={final}
          disabled={disabled}
          pending={busy === "final"}
          onChange={(enabled) => {
            if (enabled && finalNeedsSetup) {
              onConfigurationNeeded("final");
              return;
            }
            void onPatch(
              { kind: "set_lane_enabled", lane: "final", enabled },
              enabled ? "Final rewrite enabled." : "Final rewrite disabled; safe fallback remains.",
            );
          }}
        />
      </div>

      <div className="live-lane-summary" aria-live="polite" aria-atomic="true">
        <span className={`live-lane-chip live-lane-${liveState}`}>Live · {laneStateLabel(liveState)}</span>
        <span className={`live-lane-chip live-lane-${finalState}`}>Final · {laneStateLabel(finalState)}</span>
      </div>

      <div className="live-control-actions">
        <div className="live-steering-wrap">
          <button
            ref={trigger}
            className="btn-secondary live-steering-trigger"
            type="button"
            aria-expanded={steeringOpen}
            aria-haspopup="dialog"
            disabled={!settings}
            onClick={() => setSteeringOpen((open) => !open)}
          >
            Steering
          </button>
          {steeringOpen && (
            <div
              ref={popover}
              className="live-steering-popover"
              role="dialog"
              aria-label="Session steering"
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  event.preventDefault();
                  closeSteering(true);
                }
              }}
            >
              <label>
                <span>Instructions for the next hosted request</span>
                <textarea
                  ref={textarea}
                  rows={4}
                  maxLength={32 * 1024}
                  value={steering}
                  onChange={(event) => setSteering(event.target.value)}
                />
              </label>
              <label className="live-remember-steering">
                <input type="checkbox" checked={persist} onChange={(event) => setPersist(event.target.checked)} />
                Use as default
              </label>
              <div>
                <button className="btn-quiet" type="button" onClick={() => closeSteering(true)}>
                  Cancel
                </button>
                <button
                  className="btn-primary"
                  type="button"
                  disabled={Boolean(busy)}
                  onClick={() => {
                    void onSteering(steering, persist).then((saved) => {
                      if (saved) closeSteering(true);
                    });
                  }}
                >
                  Apply to next request
                </button>
              </div>
            </div>
          )}
        </div>
        <label className="live-details-toggle">
          <input
            type="checkbox"
            checked={detailsEnabled}
            onChange={(event) => onDetailsChange(event.target.checked)}
          />
          Details
        </label>
      </div>
    </section>
  );
}

function LiveSwitch({
  label,
  checked,
  disabled,
  pending,
  onChange,
}: {
  label: string;
  checked: boolean;
  disabled: boolean;
  pending: boolean;
  onChange: (enabled: boolean) => void;
}) {
  return (
    <label className="live-control-switch">
      <span>{label}</span>
      <span className="hosted-switch-control">
        <input
          type="checkbox"
          role="switch"
          aria-label={label}
          checked={checked}
          disabled={disabled}
          onChange={(event) => onChange(event.target.checked)}
        />
        <span className="hosted-switch-track" aria-hidden="true">
          <span />
        </span>
      </span>
      {pending && <span className="sr-only">Updating</span>}
    </label>
  );
}
