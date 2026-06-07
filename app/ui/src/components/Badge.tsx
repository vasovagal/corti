import type { Confidence, ConsentRule } from "../data/types";

const CONSENT_LABEL: Record<ConsentRule, string> = {
  "one-party": "One-party",
  "all-party": "All-party",
  mixed: "Mixed",
  unknown: "Unknown",
};

export function ConsentPill({ rule }: { rule: ConsentRule }) {
  return <span className={`pill pill-${rule}`}>{CONSENT_LABEL[rule]}</span>;
}

export function ConfidenceBadge({ value }: { value: Confidence }) {
  return <span className={`badge badge-${value}`}>{value} confidence</span>;
}

export function VerifyBadge() {
  return <span className="badge badge-verify">verify locally</span>;
}
