// The cross-jurisdiction consent calculator: a pure, deterministic, conservative function.
// See design/05-app-tauri.md (Appendix A) and ADR 0004. The guiding rule is that the safe default is
// ALWAYS to get everyone's consent; a one-party verdict is informational, never a green light.

import type { ConsentResult, Jurisdiction, Medium } from "../data/types";

const SAFE_ACTION =
  "Disclose the recording to everyone at the start and get their consent — it is the safe choice in every jurisdiction.";

const CRIMINAL_PURPOSE_CAVEAT =
  "Even one-party consent is void if the recording is made to commit a crime or a tort (18 U.S.C. § 2511(2)(d)).";

/** Group US state/DC/federal rows under one country "US"; each country is its own group. */
function countryOf(j: Jurisdiction): string {
  return j.scope === "country" ? j.id : "US";
}

function isEurope(j: Jurisdiction): boolean {
  return j.region === "Europe";
}

/** Resolve a jurisdiction's effective rule for the chosen medium (mixed states split by medium). */
function effectiveRule(j: Jurisdiction, medium: Medium): Jurisdiction["consent"] {
  if (j.consent === "mixed") {
    const sub = medium === "phone" ? j.phoneRule : j.inPersonRule;
    return sub ?? "mixed";
  }
  return j.consent;
}

export function computeConsent(
  recorder: Jurisdiction,
  others: Jurisdiction[],
  medium: Medium,
): ConsentResult {
  const parties = [recorder, ...others];
  const governingLawCandidates = [
    `${recorder.name} — where the recording is made`,
    ...others.map((o) => `${o.name} — a recorded party's location`),
  ];
  const verifyLocally = parties.some(
    (j) => j.consent === "mixed" || j.consent === "unknown" || j.sourceDisagreement === true,
  );
  const caveats = [CRIMINAL_PURPOSE_CAVEAT];

  // 1. Unknown location → governing law can't be determined (mobile/VoIP risk).
  if (parties.some((j) => j.consent === "unknown")) {
    return {
      verdict: "CONSULT_COUNSEL",
      rationale:
        "At least one party's location is unknown. Mobile and VoIP calls make the governing law impossible to pin down, so treat the call as all-party and confirm before recording.",
      safeAction: SAFE_ACTION,
      verifyLocally: true,
      governingLawCandidates,
      caveats,
    };
  }

  // 2. Cross-border, or any European party → multiple national laws and/or GDPR can apply at once.
  const distinctCountries = new Set(parties.map(countryOf));
  const anyEurope = parties.some(isEurope);
  if (distinctCountries.size > 1 || anyEurope) {
    if (anyEurope) {
      caveats.push(
        "A European party brings GDPR into play: the recording is processing of personal data and needs a lawful basis, transparency, and a retention limit — independent of consent.",
      );
    }
    return {
      verdict: "ALL_PARTY_CONSENT_REQUIRED",
      rationale:
        distinctCountries.size > 1
          ? "Parties are in different countries, so more than one nation's law can apply at the same time. Default to explicit all-party consent and document a lawful basis."
          : "A party is covered by European law (GDPR, and in some countries criminal speech-secrecy rules), so explicit all-party consent is the safe baseline.",
      safeAction: SAFE_ACTION,
      verifyLocally,
      governingLawCandidates,
      caveats,
    };
  }

  // 3. One country (US states, or a single non-US country): resolve by the strictest applicable rule.
  const rules = parties.map((j) => effectiveRule(j, medium));
  if (rules.includes("all-party")) {
    return {
      verdict: "ALL_PARTY_CONSENT_REQUIRED",
      rationale:
        "At least one party is in an all-party-consent jurisdiction. A strict state's law can apply to protect its own resident even when you are in a one-party state (Kearney v. Salomon Smith Barney), and federal one-party consent does not preempt it.",
      safeAction: SAFE_ACTION,
      verifyLocally,
      governingLawCandidates,
      caveats,
    };
  }
  if (rules.includes("mixed")) {
    return {
      verdict: "CONSULT_COUNSEL",
      rationale:
        "A party is in a 'mixed' jurisdiction whose rule turns on medium, case law, or an unsettled interpretation. Treat it as all-party and verify the specific situation locally.",
      safeAction: SAFE_ACTION,
      verifyLocally: true,
      governingLawCandidates,
      caveats,
    };
  }
  return {
    verdict: "ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT",
    rationale:
      "Every party is in a one-party-consent jurisdiction, so recording a conversation you take part in is likely lawful. Disclosure is still recommended and has no downside.",
    safeAction: SAFE_ACTION,
    verifyLocally,
    governingLawCandidates,
    caveats,
  };
}
