export type DiffKind = "equal" | "delete" | "insert";

export interface DiffSpan {
  kind: DiffKind;
  text: string;
}

const MAX_LCS_CELLS = 120_000;

type SegmenterPart = { segment: string };
type SegmenterLike = { segment(value: string): Iterable<SegmenterPart> };
type SegmenterConstructor = new (
  locales?: string | string[],
  options?: { granularity: "word" },
) => SegmenterLike;

/** Unicode-aware word, punctuation, and whitespace tokens. No token or span leaves this process. */
export function tokenizeLiveText(value: string): string[] {
  const Segmenter = (Intl as unknown as { Segmenter?: SegmenterConstructor }).Segmenter;
  if (Segmenter) {
    return Array.from(new Segmenter(undefined, { granularity: "word" }).segment(value), (part) =>
      part.segment,
    );
  }
  return value.match(/\s+|[\p{L}\p{N}\p{M}_]+(?:['’][\p{L}\p{N}\p{M}_]+)*|[^\s]/gu) ?? [];
}

/**
 * Deterministic token-level LCS diff. Very large pathological rows use a bounded delete/insert fallback
 * rather than allocating quadratically; raw and clean strings are never modified or serialized.
 */
export function diffLiveText(raw: string, clean: string): DiffSpan[] {
  if (raw === clean) return raw ? [{ kind: "equal", text: raw }] : [];

  const before = tokenizeLiveText(raw);
  const after = tokenizeLiveText(clean);
  let prefix = 0;
  while (prefix < before.length && prefix < after.length && before[prefix] === after[prefix]) {
    prefix += 1;
  }
  let suffix = 0;
  while (
    suffix < before.length - prefix &&
    suffix < after.length - prefix &&
    before[before.length - suffix - 1] === after[after.length - suffix - 1]
  ) {
    suffix += 1;
  }

  const spans: DiffSpan[] = [];
  appendSpan(spans, "equal", before.slice(0, prefix).join(""));
  const oldMiddle = before.slice(prefix, before.length - suffix);
  const newMiddle = after.slice(prefix, after.length - suffix);

  if (oldMiddle.length === 0) {
    appendSpan(spans, "insert", newMiddle.join(""));
  } else if (newMiddle.length === 0) {
    appendSpan(spans, "delete", oldMiddle.join(""));
  } else if ((oldMiddle.length + 1) * (newMiddle.length + 1) > MAX_LCS_CELLS) {
    appendSpan(spans, "delete", oldMiddle.join(""));
    appendSpan(spans, "insert", newMiddle.join(""));
  } else {
    appendMiddleDiff(spans, oldMiddle, newMiddle);
  }

  appendSpan(spans, "equal", suffix ? before.slice(before.length - suffix).join("") : "");
  return spans;
}

function appendMiddleDiff(spans: DiffSpan[], before: string[], after: string[]) {
  const width = after.length + 1;
  const table = new Uint32Array((before.length + 1) * width);
  for (let oldIndex = before.length - 1; oldIndex >= 0; oldIndex -= 1) {
    for (let newIndex = after.length - 1; newIndex >= 0; newIndex -= 1) {
      const index = oldIndex * width + newIndex;
      table[index] =
        before[oldIndex] === after[newIndex]
          ? table[(oldIndex + 1) * width + newIndex + 1] + 1
          : Math.max(table[(oldIndex + 1) * width + newIndex], table[index + 1]);
    }
  }

  let oldIndex = 0;
  let newIndex = 0;
  while (oldIndex < before.length || newIndex < after.length) {
    if (
      oldIndex < before.length &&
      newIndex < after.length &&
      before[oldIndex] === after[newIndex]
    ) {
      appendSpan(spans, "equal", before[oldIndex]);
      oldIndex += 1;
      newIndex += 1;
    } else if (
      oldIndex < before.length &&
      (newIndex >= after.length ||
        table[(oldIndex + 1) * width + newIndex] >= table[oldIndex * width + newIndex + 1])
    ) {
      appendSpan(spans, "delete", before[oldIndex]);
      oldIndex += 1;
    } else {
      appendSpan(spans, "insert", after[newIndex]);
      newIndex += 1;
    }
  }
}

function appendSpan(spans: DiffSpan[], kind: DiffKind, text: string) {
  if (!text) return;
  const last = spans[spans.length - 1];
  if (last?.kind === kind) last.text += text;
  else spans.push({ kind, text });
}
