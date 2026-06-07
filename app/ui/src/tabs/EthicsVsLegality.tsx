import { ETHICS } from "../lib/data";
import type { EthicsScenario } from "../data/types";

const LEGAL_LABEL: Record<EthicsScenario["legalStatus"], string> = {
  likely_legal: "Likely legal",
  likely_illegal: "Likely illegal",
  depends: "Depends",
};

const ETHICAL_LABEL: Record<EthicsScenario["ethicalStatus"], string> = {
  appropriate: "Appropriate",
  questionable: "Questionable",
  inappropriate: "Inappropriate",
};

function ScenarioCard({ s }: { s: EthicsScenario }) {
  return (
    <article className="card scenario">
      <h4>{s.title}</h4>
      <div className="axes">
        <span className={`axis axis-legal-${s.legalStatus}`}>Legal: {LEGAL_LABEL[s.legalStatus]}</span>
        <span className={`axis axis-eth-${s.ethicalStatus}`}>
          Ethical: {ETHICAL_LABEL[s.ethicalStatus]}
        </span>
        {s.sensitiveContext && <span className="axis axis-sensitive">sensitive context</span>}
      </div>
      <p className="small">
        <strong>Often legal because:</strong> {s.whyLegal}
      </p>
      <p className="small">
        <strong>But ethically:</strong> {s.whyEthicalProblem}
      </p>
    </article>
  );
}

export function EthicsVsLegality() {
  return (
    <section>
      <h2>Legality, ethics, and morality</h2>
      <div className="card-grid three">
        {ETHICS.systems.map((s) => (
          <article key={s.name} className="card">
            <h4>{s.name}</h4>
            <p>{s.definition}</p>
          </article>
        ))}
      </div>

      <div className="callout strong">{ETHICS.coreMessage}</div>

      <h3 className="section-gap">Four lenses for judging a recording</h3>
      <div className="card-grid">
        {ETHICS.lenses.map((l) => (
          <article key={l.id} className="card">
            <h4>{l.name}</h4>
            <p className="q">{l.question}</p>
            <p className="small">{l.appliedToRecording}</p>
          </article>
        ))}
      </div>

      <h3 className="section-gap">Contextual integrity</h3>
      <p className="lead">{ETHICS.contextualIntegrity}</p>

      <h3 className="section-gap">Legal-but-unethical: the quadrant in practice</h3>
      <div className="card-grid">
        {ETHICS.scenarios.map((s) => (
          <ScenarioCard key={s.id} s={s} />
        ))}
      </div>

      <h3 className="section-gap">Practical guidance</h3>
      <ul className="guidance">
        {ETHICS.guidance.map((g, i) => (
          <li key={i}>{g}</li>
        ))}
      </ul>
    </section>
  );
}
