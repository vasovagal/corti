import { useMemo, useState } from "react";
import { CASES, JURISDICTIONS, TRENDS, groupByRegion, jurisdictionById } from "../lib/data";
import { ConfidenceBadge, ConsentPill, VerifyBadge } from "../components/Badge";
import type { Jurisdiction } from "../data/types";

function ruleDetail(j: Jurisdiction): string {
  if (j.consent === "mixed") {
    const parts: string[] = [];
    if (j.phoneRule) parts.push(`phone: ${j.phoneRule}`);
    if (j.inPersonRule) parts.push(`in-person: ${j.inPersonRule}`);
    return parts.join(" · ");
  }
  return "";
}

function JurisdictionRow({ j }: { j: Jurisdiction }) {
  const detail = ruleDetail(j);
  return (
    <tr>
      <td className="cell-name">
        {j.name}
        {j.scope === "federal" && <span className="tag">floor</span>}
      </td>
      <td>
        <ConsentPill rule={j.consent} />
        {detail && <div className="rule-detail">{detail}</div>}
      </td>
      <td className="cell-notes">
        <div>{j.notes}</div>
        <div className="cell-meta">
          <span className="statute">{j.statute}</span>
          {j.caseLaw && <span className="case">· {j.caseLaw}</span>}
        </div>
        <div className="cell-badges">
          <ConfidenceBadge value={j.confidence} />
          {(j.sourceDisagreement || j.consent === "mixed" || j.consent === "unknown") && <VerifyBadge />}
          <span className="reviewed">reviewed {j.lastReviewed}</span>
        </div>
      </td>
    </tr>
  );
}

export function Legality() {
  const [q, setQ] = useState("");
  const federal = jurisdictionById("us-federal");

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return JURISDICTIONS;
    return JURISDICTIONS.filter(
      (j) => j.name.toLowerCase().includes(needle) || j.region.toLowerCase().includes(needle),
    );
  }, [q]);

  const groups = groupByRegion(filtered);

  return (
    <section>
      <h2>Is it legal to record?</h2>
      {federal && (
        <div className="callout">
          <strong>The U.S. federal floor:</strong> {federal.notes}{" "}
          <span className="statute">({federal.statute})</span> States may go stricter (all-party), never weaker.
        </div>
      )}
      <p className="lead">
        "One-party" means a participant may record their own conversation. "All-party" (often called
        "two-party") means everyone must consent or be notified. Many states are <em>mixed</em> — the answer
        depends on phone vs. in-person, recent case law, or whether the conversation was private. The labels
        below are a starting point, not a verdict: read the notes.
      </p>

      <input
        className="search"
        type="search"
        placeholder="Filter by state, country, or region…"
        value={q}
        onChange={(e) => setQ(e.target.value)}
        aria-label="Filter jurisdictions"
      />

      {groups.map(([region, items]) => (
        <div key={region} className="region-group">
          <h3>{region}</h3>
          <table className="jtable">
            <thead>
              <tr>
                <th>Jurisdiction</th>
                <th>Consent</th>
                <th>What to know</th>
              </tr>
            </thead>
            <tbody>
              {items.map((j) => (
                <JurisdictionRow key={j.id} j={j} />
              ))}
            </tbody>
          </table>
        </div>
      ))}
      {groups.length === 0 && <p className="muted">No jurisdictions match "{q}".</p>}

      <h2 className="section-gap">Landmark cases</h2>
      <div className="card-grid">
        {CASES.map((c) => (
          <article key={c.id} className="card">
            <h4>
              {c.shortName} <span className="muted">{c.citation}</span>
            </h4>
            <p className="holding">{c.holding}</p>
            <p className="why">
              <strong>Why it matters:</strong> {c.whyItMatters}
            </p>
            <div className="tags">
              {c.doctrineTags.map((t) => (
                <span key={t} className="tag">
                  {t}
                </span>
              ))}
            </div>
          </article>
        ))}
      </div>

      <h2 className="section-gap">Technology trends</h2>
      <div className="card-grid">
        {TRENDS.map((t) => (
          <article key={t.id} className="card">
            <h4>{t.name}</h4>
            <p className="muted small">
              {t.category} · {t.status}
            </p>
            <p>{t.summary}</p>
            <p className="small">
              <strong>Examples:</strong> {t.examples.join(", ")}
            </p>
            <p className="small">
              <strong>Legal theories:</strong> {t.governingTheories.join(", ")}
            </p>
          </article>
        ))}
      </div>
    </section>
  );
}
