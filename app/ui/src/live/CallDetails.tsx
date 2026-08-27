import type { PreferencesSection } from "../lib/api";
import { errorLabel, hostedErrorGuidance } from "../lib/hosted";
import {
  cacheObservationLabel,
  callLaneLabel,
  formatHostedCost,
  latencyEntries,
  tokenEntries,
  type LiveCallDetail,
} from "../lib/liveHosted";

export function CallDetails({
  calls,
  onOpenPreferences,
}: {
  calls: LiveCallDetail[];
  onOpenPreferences: (section: PreferencesSection) => Promise<void>;
}) {
  return (
    <section className="live-call-details" aria-labelledby="live-call-details-heading">
      <div className="live-section-head">
        <div>
          <p className="hosted-eyebrow">Content-free diagnostics</p>
          <h2 id="live-call-details-heading">Hosted call details</h2>
        </div>
        <span className="live-details-count">Latest {calls.length} / 12</span>
      </div>
      {calls.length === 0 ? (
        <p className="live-details-empty">No hosted call has been observed in this session.</p>
      ) : (
        <ol className="live-call-list">
          {calls.map((call) => {
            const tokens = tokenEntries(call.usage);
            const phases = call.latency ? latencyEntries(call.latency) : [];
            const repair = call.error ? hostedErrorGuidance(call.error) : null;
            return (
              <li className="live-call-card" key={call.call_id}>
                <header>
                  <div>
                    <strong>{callLaneLabel(call.lane)}</strong>
                    {call.model && <span className="live-call-model">{call.model}</span>}
                  </div>
                  <span className={`live-finality live-finality-${call.finality}`}>
                    {call.finality === "provisional" ? "Provisional" : "Final"}
                  </span>
                </header>
                <p className="live-call-cost">
                  {call.finality === "provisional" && <span className="sr-only">Provisional cost: </span>}
                  {formatHostedCost(call.cost)}
                </p>
                <div className="live-call-facts">
                  <span>{cacheObservationLabel(call.cache)}</span>
                  {call.outcome && <span>{call.outcome.replace(/_/gu, " ")}</span>}
                  {call.error && <span className="live-call-error">{errorLabel(call.error)}</span>}
                </div>
                {repair && (
                  <div className="live-call-remedy">
                    <span>{repair.message}</span>
                    {repair.section && repair.actionLabel && (
                      <button
                        className="btn-quiet"
                        type="button"
                        onClick={() => void onOpenPreferences(repair.section!)}
                      >
                        {repair.actionLabel}
                      </button>
                    )}
                  </div>
                )}
                <dl className="live-metric-grid" aria-label="Token usage">
                  {tokens.length === 0 ? (
                    <div>
                      <dt>Tokens</dt>
                      <dd>Unavailable</dd>
                    </div>
                  ) : (
                    tokens.map(([label, value]) => (
                      <div key={label}>
                        <dt>{label}</dt>
                        <dd>{value}</dd>
                      </div>
                    ))
                  )}
                </dl>
                {phases.length > 0 && (
                  <dl className="live-metric-grid live-latency-grid" aria-label="Latency">
                    {phases.map(([label, value]) => (
                      <div key={label}>
                        <dt>{label}</dt>
                        <dd>{value}</dd>
                      </div>
                    ))}
                  </dl>
                )}
                {(call.late || call.late_content_discarded) && (
                  <p className="live-late-note">Late usage kept; stale text was discarded.</p>
                )}
                {call.outcome === "canceled" && call.provider_request_sent && (
                  <p className="live-late-note">Provider billing may still occur after cancellation.</p>
                )}
              </li>
            );
          })}
        </ol>
      )}
    </section>
  );
}
