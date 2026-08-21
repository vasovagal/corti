// Region lists shared by the Transcription and Hosted panes. Each selector injects its current value if
// it isn't in the list, so a region we haven't enumerated is still editable rather than silently reset.

/** Common AWS commercial regions, used by the Transcribe backend selector. */
export const AWS_REGIONS = [
  "us-east-1",
  "us-east-2",
  "us-west-1",
  "us-west-2",
  "ca-central-1",
  "eu-west-1",
  "eu-west-2",
  "eu-central-1",
  "eu-north-1",
  "ap-south-1",
  "ap-southeast-1",
  "ap-southeast-2",
  "ap-northeast-1",
  "ap-northeast-2",
  "sa-east-1",
];

/** Regions where Bedrock runtime is offered. Narrower than the Transcribe list — us-west-1, ca-central-1,
 * eu-north-1, and ap-northeast-2 have no Bedrock runtime endpoint. */
export const BEDROCK_REGIONS = [
  "us-east-1",
  "us-east-2",
  "us-west-2",
  "eu-west-1",
  "eu-west-2",
  "eu-west-3",
  "eu-central-1",
  "ap-south-1",
  "ap-southeast-1",
  "ap-southeast-2",
  "ap-northeast-1",
  "sa-east-1",
];

/** The list to render, with `current` injected when it isn't already known. */
export function regionOptions(known: string[], current: string | null | undefined): string[] {
  const value = current?.trim();
  return value && !known.includes(value) ? [value, ...known] : known;
}
