import { describe, expect, it } from "vitest";
import { computeConsent } from "./consent";
import { UNKNOWN_JURISDICTION, jurisdictionById, validate } from "./data";
import type { Jurisdiction } from "../data/types";

function jur(id: string): Jurisdiction {
  const j = jurisdictionById(id);
  if (!j) throw new Error(`missing jurisdiction ${id}`);
  return j;
}

describe("data integrity", () => {
  it("validate() passes on the shipped dataset", () => {
    expect(() => validate()).not.toThrow();
  });
});

describe("computeConsent", () => {
  it("two one-party states → one-party sufficient (but still recommend consent)", () => {
    const r = computeConsent(jur("us-tx"), [jur("us-ny")], "phone");
    expect(r.verdict).toBe("ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT");
    expect(r.safeAction).toBeTruthy();
  });

  it("one-party recorder + all-party other → all-party (Kearney)", () => {
    const r = computeConsent(jur("us-tx"), [jur("us-ca")], "phone");
    expect(r.verdict).toBe("ALL_PARTY_CONSENT_REQUIRED");
  });

  it("the user's (NH,[CA]) example → all-party (both all-party)", () => {
    const r = computeConsent(jur("us-nh"), [jur("us-ca")], "phone");
    expect(r.verdict).toBe("ALL_PARTY_CONSENT_REQUIRED");
  });

  it("(NH,[MA,CA]) → all-party", () => {
    const r = computeConsent(jur("us-nh"), [jur("us-ma"), jur("us-ca")], "inPerson");
    expect(r.verdict).toBe("ALL_PARTY_CONSENT_REQUIRED");
  });

  it("(NH,[France]) → all-party international, GDPR caveat surfaced", () => {
    const r = computeConsent(jur("us-nh"), [jur("fr")], "phone");
    expect(r.verdict).toBe("ALL_PARTY_CONSENT_REQUIRED");
    expect(r.caveats.some((c) => c.includes("GDPR"))).toBe(true);
  });

  it("cross-border to a one-party country (Canada) is still all-party", () => {
    const r = computeConsent(jur("us-tx"), [jur("ca")], "phone");
    expect(r.verdict).toBe("ALL_PARTY_CONSENT_REQUIRED");
  });

  it("a single non-US one-party country (Japan) → one-party sufficient", () => {
    const r = computeConsent(jur("jp"), [jur("jp")], "phone");
    expect(r.verdict).toBe("ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT");
  });

  it("Oregon is medium-sensitive: phone → one-party, in-person → all-party", () => {
    const phone = computeConsent(jur("us-or"), [jur("us-tx")], "phone");
    expect(phone.verdict).toBe("ONE_PARTY_SUFFICIENT_BUT_RECOMMEND_CONSENT");
    const inPerson = computeConsent(jur("us-or"), [jur("us-tx")], "inPerson");
    expect(inPerson.verdict).toBe("ALL_PARTY_CONSENT_REQUIRED");
  });

  it("a mixed state with no sub-rule for the medium (Michigan) → consult + verify locally", () => {
    const r = computeConsent(jur("us-tx"), [jur("us-mi")], "phone");
    expect(r.verdict).toBe("CONSULT_COUNSEL");
    expect(r.verifyLocally).toBe(true);
  });

  it("unknown location → consult counsel", () => {
    const r = computeConsent(jur("us-tx"), [UNKNOWN_JURISDICTION], "phone");
    expect(r.verdict).toBe("CONSULT_COUNSEL");
    expect(r.verifyLocally).toBe(true);
  });

  it("always returns a non-empty safeAction, even for the one-party verdict", () => {
    const r = computeConsent(jur("us-tx"), [jur("us-ny")], "phone");
    expect(r.safeAction.length).toBeGreaterThan(0);
  });
});
