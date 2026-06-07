import { CULTURE } from "../lib/data";

export function CulturalNorms() {
  return (
    <section>
      <h2>Cultural norms: consent is not comfort</h2>
      <div className="callout strong">{CULTURE.corePrinciple}</div>

      <h3 className="section-gap">The chilling effect</h3>
      <p className="lead">{CULTURE.chillingEffect}</p>

      <h3 className="section-gap">How attitudes differ</h3>
      <div className="card-grid">
        {CULTURE.profiles.map((p) => (
          <article key={p.id} className="card">
            <h4>{p.region}</h4>
            <p className="muted small">
              {p.orientation} · comfort gap: {p.comfortDelta}
            </p>
            <p>{p.summary}</p>
            <div className="tags">
              {p.keyConcepts.map((c) => (
                <span key={c} className="tag">
                  {c}
                </span>
              ))}
            </div>
            <p className="small">
              <strong>Why:</strong> {p.drivers}
            </p>
          </article>
        ))}
      </div>

      <h3 className="section-gap">Etiquette the responsible recorder follows</h3>
      <ul className="guidance">
        {CULTURE.etiquette.map((e, i) => (
          <li key={i}>{e}</li>
        ))}
      </ul>
    </section>
  );
}
