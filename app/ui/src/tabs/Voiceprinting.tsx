import { VOICEPRINT } from "../lib/data";

export function Voiceprinting() {
  return (
    <section>
      <h2>Voiceprinting &amp; voice biometrics</h2>
      <p className="lead">{VOICEPRINT.whatItIs}</p>
      <div className="callout strong">{VOICEPRINT.distinction}</div>

      <h3 className="section-gap">The laws that apply to voiceprints</h3>
      <div className="card-grid">
        {VOICEPRINT.laws.map((l) => (
          <article key={l.id} className="card">
            <h4>{l.jurisdiction}</h4>
            <p className="muted small">{l.law}</p>
            <p className="small">
              <strong>Requires:</strong> {l.requirements}
            </p>
            <p className="small">
              <strong>Enforcement:</strong> {l.enforcement}
            </p>
            <p className="small">{l.notes}</p>
          </article>
        ))}
      </div>

      <h3 className="section-gap">The ethics</h3>
      <ul className="guidance">
        {VOICEPRINT.ethics.map((e, i) => (
          <li key={i}>{e}</li>
        ))}
      </ul>

      <h3 className="section-gap">What this means for Corti</h3>
      <div className="callout corti-flag">{VOICEPRINT.cortiFlag}</div>
    </section>
  );
}
