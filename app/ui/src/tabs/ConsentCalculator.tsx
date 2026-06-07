import { useMemo, useState } from "react";
import {
  JURISDICTIONS,
  UNKNOWN_JURISDICTION,
  groupByRegion,
  jurisdictionById,
} from "../lib/data";
import { computeConsent } from "../lib/consent";
import { ConfidenceBadge, ConsentPill, VerifyBadge } from "../components/Badge";
import type { Medium, Verdict } from "../data/types";

const SELECT_OPTIONS = [...JURISDICTIONS, UNKNOWN_JURISDICTION];

const VERDICT_LABEL: Record<Verdict, string> = {
  ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT: "One-party likely sufficient — but get consent anyway",
  ALL_PARTY_CONSENT_REQUIRED: "All-party consent required",
  CONSULT_COUNSEL: "Unclear — treat as all-party / consult counsel",
};

const VERDICT_CLASS: Record<Verdict, string> = {
  ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT: "verdict-ok",
  ALL_PARTY_CONSENT_REQUIRED: "verdict-warn",
  CONSULT_COUNSEL: "verdict-caution",
};

function JurisdictionSelect({
  value,
  onChange,
  label,
}: {
  value: string;
  onChange: (id: string) => void;
  label: string;
}) {
  return (
    <select
      className="jselect"
      value={value}
      aria-label={label}
      onChange={(e) => onChange(e.target.value)}
    >
      {groupByRegion(SELECT_OPTIONS).map(([region, items]) => (
        <optgroup key={region} label={region}>
          {items.map((j) => (
            <option key={j.id} value={j.id}>
              {j.name}
            </option>
          ))}
        </optgroup>
      ))}
    </select>
  );
}

export function ConsentCalculator() {
  const [recorderId, setRecorderId] = useState("us-ca");
  const [otherIds, setOtherIds] = useState<string[]>(["us-nh"]);
  const [medium, setMedium] = useState<Medium>("phone");

  const recorder = jurisdictionById(recorderId) ?? UNKNOWN_JURISDICTION;
  const others = otherIds.map((id) => jurisdictionById(id) ?? UNKNOWN_JURISDICTION);

  const result = useMemo(
    () => computeConsent(recorder, others, medium),
    [recorder, others, medium],
  );

  const selected = [recorder, ...others];

  return (
    <section>
      <h2>Cross-jurisdiction consent calculator</h2>
      <p className="lead">
        Enter where each person is. The calculator applies the conservative rule courts tend to follow: if any
        party is in an all-party state — or anyone is in another country — treat the whole conversation as
        all-party. It can never tell you a recording is safe; it tells you when you must get everyone's consent.
      </p>

      <div className="calc">
        <div className="calc-field">
          <label>You are recording from</label>
          <JurisdictionSelect value={recorderId} onChange={setRecorderId} label="Recorder location" />
        </div>

        <div className="calc-field">
          <label>The other parties are in</label>
          {otherIds.map((id, i) => (
            <div key={i} className="other-row">
              <JurisdictionSelect
                value={id}
                label={`Other party ${i + 1} location`}
                onChange={(v) => setOtherIds(otherIds.map((o, j) => (j === i ? v : o)))}
              />
              <button
                className="btn-icon"
                aria-label="Remove party"
                disabled={otherIds.length === 1}
                onClick={() => setOtherIds(otherIds.filter((_, j) => j !== i))}
              >
                ✕
              </button>
            </div>
          ))}
          <button className="btn-add" onClick={() => setOtherIds([...otherIds, "us-ca"])}>
            + Add another party
          </button>
        </div>

        <div className="calc-field">
          <label>Conversation type</label>
          <div className="radio-row">
            <label className="radio">
              <input
                type="radio"
                name="medium"
                checked={medium === "phone"}
                onChange={() => setMedium("phone")}
              />
              Phone / video / online
            </label>
            <label className="radio">
              <input
                type="radio"
                name="medium"
                checked={medium === "inPerson"}
                onChange={() => setMedium("inPerson")}
              />
              In person
            </label>
          </div>
        </div>
      </div>

      <div className={`verdict ${VERDICT_CLASS[result.verdict]}`}>
        <div className="verdict-head">{VERDICT_LABEL[result.verdict]}</div>
        <p>{result.rationale}</p>
        {result.verifyLocally && (
          <p className="verify-note">
            <VerifyBadge /> One or more locations are mixed, uncertain, or disputed between sources — confirm
            the specific rule before relying on this.
          </p>
        )}
      </div>

      <div className="safe-action">
        <strong>Safest action:</strong> {result.safeAction}
      </div>

      <h3>Why</h3>
      <ul className="why-list">
        <li>
          <strong>Governing law could be:</strong>
          <ul>
            {result.governingLawCandidates.map((g, i) => (
              <li key={i}>{g}</li>
            ))}
          </ul>
        </li>
        {result.caveats.map((c, i) => (
          <li key={i}>{c}</li>
        ))}
      </ul>

      <h3>The jurisdictions you selected</h3>
      <table className="jtable">
        <tbody>
          {selected.map((j, i) => (
            <tr key={`${j.id}-${i}`}>
              <td className="cell-name">
                {i === 0 ? "You" : `Party ${i}`}: {j.name}
              </td>
              <td>
                <ConsentPill rule={j.consent} />
              </td>
              <td className="cell-notes">
                <div>{j.notes}</div>
                <div className="cell-badges">
                  <ConfidenceBadge value={j.confidence} />
                  {(j.sourceDisagreement || j.consent === "mixed" || j.consent === "unknown") && (
                    <VerifyBadge />
                  )}
                </div>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
