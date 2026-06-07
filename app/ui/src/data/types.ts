// Type definitions for the Ethics & Legality guide datasets.
//
// IMPORTANT: this file holds *interfaces only* — no values. All actual data lives in the sibling
// `*.json` files so the (legal) content stays auditable and editable without reading code. Each JSON
// import is cast to the matching interface at its use site (see `lib/data.ts`), and a dev-time
// `validate()` checks shape/counts. This split is the project convention for reference data.

export type ConsentRule = "one-party" | "all-party" | "mixed" | "unknown";
export type Confidence = "high" | "medium" | "low";
export type Medium = "phone" | "inPerson";
export type Scope = "federal" | "state" | "district" | "country";

/** One recordable jurisdiction (a US state/DC/federal row, or a country). */
export interface Jurisdiction {
  id: string; // "us-ca", "us-dc", "us-federal", "fr"
  name: string; // "California"
  region: string; // "United States" | "Europe" | "Asia-Pacific" | ...
  scope: Scope;
  consent: ConsentRule;
  /** For `mixed` jurisdictions: the rule for phone/electronic communications. */
  phoneRule?: "one-party" | "all-party";
  /** For `mixed` jurisdictions: the rule for in-person oral communications. */
  inPersonRule?: "one-party" | "all-party";
  statute: string;
  caseLaw?: string;
  notes: string;
  confidence: Confidence;
  lastReviewed: string; // ISO date
  /** Sources disagree on this classification — surface a "verify locally" warning. */
  sourceDisagreement?: boolean;
  /** Org-level data-protection duty that binds the app (distinct from participant consent). */
  dataProtection?: string;
  /** Recording private speech is itself criminal and applies to participants (e.g. FR, DE). */
  criminalSpeechLaw?: boolean;
}

/** Deterministic output of the cross-jurisdiction consent calculator. */
export type Verdict =
  | "ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT"
  | "ALL_PARTY_CONSENT_REQUIRED"
  | "CONSULT_COUNSEL";

export interface ConsentResult {
  verdict: Verdict;
  rationale: string;
  safeAction: string;
  verifyLocally: boolean;
  governingLawCandidates: string[];
  caveats: string[];
}

export interface CaseLaw {
  id: string;
  shortName: string;
  citation: string;
  year: number;
  court: string;
  holding: string;
  whyItMatters: string;
  doctrineTags: string[];
}

export interface TechTrend {
  id: string;
  name: string;
  category: string;
  summary: string;
  examples: string[];
  governingTheories: string[];
  status: "active" | "split" | "emerging";
}

export interface EthicsLens {
  id: string;
  name: string;
  question: string;
  appliedToRecording: string;
}

export interface EthicsScenario {
  id: string;
  title: string;
  legalStatus: "likely_legal" | "likely_illegal" | "depends";
  ethicalStatus: "appropriate" | "questionable" | "inappropriate";
  whyLegal: string;
  whyEthicalProblem: string;
  sensitiveContext: boolean;
}

export interface EthicsContent {
  systems: { name: string; definition: string }[];
  coreMessage: string;
  lenses: EthicsLens[];
  contextualIntegrity: string;
  scenarios: EthicsScenario[];
  guidance: string[];
}

export interface CultureProfile {
  id: string;
  region: string;
  orientation: string; // "Liberty-based" | "Dignity-based" | "Harmony / contextual"
  comfortDelta: Confidence; // how far comfort lags legal permission (low/medium/high)
  summary: string;
  keyConcepts: string[];
  drivers: string;
}

export interface CultureContent {
  corePrinciple: string;
  chillingEffect: string;
  profiles: CultureProfile[];
  etiquette: string[];
}

export interface VoiceprintLaw {
  id: string;
  jurisdiction: string;
  law: string;
  requirements: string;
  enforcement: string;
  notes: string;
}

export interface VoiceprintContent {
  whatItIs: string;
  distinction: string;
  laws: VoiceprintLaw[];
  ethics: string[];
  cortiFlag: string;
}

export interface SourceRef {
  id: string;
  title: string;
  url: string;
  category: string;
}
