import { useEffect, useState } from "react";
import { getPipelineActivity, type PipelineActivity } from "./lib/api";
import { PIPELINE_BOXES, activeBoxKeysForActivity } from "./lib/pipeline";

// The "How Corti Works" window: a live diagram of the detect → capture → echo-cancel → transcribe → file
// pipeline. Polls get_pipeline_activity at 1 Hz (like Console) and pulses whichever box is active now.
export default function How() {
  const [activity, setActivity] = useState<PipelineActivity | null>(null);

  useEffect(() => {
    document.title = "How Corti Works — Corti";
  }, []);

  // Poll the current stage at 1 Hz; keep the last snapshot on a failed poll.
  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const a = await getPipelineActivity();
        if (alive) setActivity(a);
      } catch {
        // Ignore poll failures.
      }
    };
    tick(); // prime immediately
    const id = setInterval(tick, 1000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const active = new Set(
    activeBoxKeysForActivity(activity?.stage ?? "idle", activity?.recording ?? false),
  );

  return (
    <div className="app">
      <header className="app-header">
        <h1>How Corti Works</h1>
        <p className="subtitle">
          Every call flows through these steps. The lit step is happening right now.
        </p>
      </header>

      <main className="tab-content">
        <div className="how-flow">
          {PIPELINE_BOXES.map((box, i) => (
            <div className="how-step" key={box.key}>
              <div className={`how-box${active.has(box.key) ? " how-box-active" : ""}`}>
                <h4>{box.title}</h4>
                <p>{box.blurb}</p>
              </div>
              {i < PIPELINE_BOXES.length - 1 && (
                <span className="how-arrow" aria-hidden="true">
                  →
                </span>
              )}
            </div>
          ))}
        </div>

        <p className="how-detail">{activity?.detail || "Idle — waiting for a call."}</p>

        <p className="muted small">
          It all runs on your Mac: audio is captured locally, and nothing is transcribed until the call
          ends.
        </p>
      </main>
    </div>
  );
}
