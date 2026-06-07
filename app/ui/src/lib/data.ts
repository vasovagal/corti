// Typed access to the auditable JSON datasets. The JSON files hold the data; this module casts each
// import to its interface (Vite imports JSON natively) and exposes a dev-time validate().

import jurisdictionsRaw from "../data/jurisdictions.json";
import casesRaw from "../data/cases.json";
import trendsRaw from "../data/trends.json";
import ethicsRaw from "../data/ethics.json";
import cultureRaw from "../data/culture.json";
import voiceprintRaw from "../data/voiceprint.json";
import sourcesRaw from "../data/sources.json";
import type {
  CaseLaw,
  CultureContent,
  EthicsContent,
  Jurisdiction,
  SourceRef,
  TechTrend,
  VoiceprintContent,
} from "../data/types";

export const JURISDICTIONS = jurisdictionsRaw as unknown as Jurisdiction[];
export const CASES = casesRaw as unknown as CaseLaw[];
export const TRENDS = trendsRaw as unknown as TechTrend[];
export const ETHICS = ethicsRaw as unknown as EthicsContent;
export const CULTURE = cultureRaw as unknown as CultureContent;
export const VOICEPRINT = voiceprintRaw as unknown as VoiceprintContent;
export const SOURCES = sourcesRaw as unknown as SourceRef[];

/** Sentinel for "I'm not sure where they are" — kept OUT of JURISDICTIONS so counts stay clean. */
export const UNKNOWN_JURISDICTION: Jurisdiction = {
  id: "unknown",
  name: "Somewhere else / not sure",
  region: "Unknown",
  scope: "country",
  consent: "unknown",
  statute: "—",
  notes: "Location unknown — the governing law cannot be determined, so treat the call as all-party.",
  confidence: "low",
  lastReviewed: "2026-06-07",
};

export function jurisdictionById(id: string): Jurisdiction | undefined {
  if (id === UNKNOWN_JURISDICTION.id) return UNKNOWN_JURISDICTION;
  return JURISDICTIONS.find((j) => j.id === id);
}

/** Group jurisdictions by region, preserving first-seen region order. */
export function groupByRegion(items: Jurisdiction[]): [string, Jurisdiction[]][] {
  const order: string[] = [];
  const map = new Map<string, Jurisdiction[]>();
  for (const j of items) {
    if (!map.has(j.region)) {
      map.set(j.region, []);
      order.push(j.region);
    }
    map.get(j.region)!.push(j);
  }
  return order.map((r) => [r, map.get(r)!] as [string, Jurisdiction[]]);
}

const CONSENT_VALUES = new Set(["one-party", "all-party", "mixed", "unknown"]);
const CONFIDENCE_VALUES = new Set(["high", "medium", "low"]);

/**
 * Data-integrity check, run in DEV from main.tsx and in the test suite. Throws (with every problem
 * listed) on malformed data so a bad hand-edit to the JSON fails loudly instead of shipping silently.
 */
export function validate(): void {
  const problems: string[] = [];
  const by = (s: string) => JURISDICTIONS.filter((j) => j.scope === s).length;

  if (by("state") !== 50) problems.push(`expected 50 US states, found ${by("state")}`);
  if (by("district") !== 1) problems.push(`expected 1 district (DC), found ${by("district")}`);
  if (by("federal") !== 1) problems.push(`expected 1 federal row, found ${by("federal")}`);
  if (by("country") < 10) problems.push(`expected >= 10 countries, found ${by("country")}`);

  const ids = new Set<string>();
  for (const j of JURISDICTIONS) {
    if (ids.has(j.id)) problems.push(`duplicate jurisdiction id: ${j.id}`);
    ids.add(j.id);
    if (!j.name || !j.statute || !j.notes) problems.push(`${j.id}: missing required field`);
    if (!CONSENT_VALUES.has(j.consent)) problems.push(`${j.id}: bad consent '${j.consent}'`);
    if (!CONFIDENCE_VALUES.has(j.confidence)) problems.push(`${j.id}: bad confidence '${j.confidence}'`);
    if (!/^\d{4}-\d{2}-\d{2}$/.test(j.lastReviewed)) problems.push(`${j.id}: bad lastReviewed date`);
    if (j.consent === "mixed" && !j.phoneRule && !j.inPersonRule && !j.sourceDisagreement) {
      problems.push(`${j.id}: mixed without phone/inPerson rules or sourceDisagreement flag`);
    }
  }

  if (CASES.length < 5) problems.push(`expected >= 5 cases, found ${CASES.length}`);
  if (TRENDS.length < 3) problems.push(`expected >= 3 trends, found ${TRENDS.length}`);
  if (ETHICS.scenarios.length < 4) problems.push(`expected >= 4 ethics scenarios`);
  if (CULTURE.profiles.length < 3) problems.push(`expected >= 3 culture profiles`);
  if (VOICEPRINT.laws.length < 3) problems.push(`expected >= 3 voiceprint laws`);
  if (SOURCES.length < 8) problems.push(`expected >= 8 sources, found ${SOURCES.length}`);

  if (problems.length) {
    throw new Error("Ethics guide data validation failed:\n - " + problems.join("\n - "));
  }
}
