import { useMemo } from "react";
import { diffLiveText } from "../lib/liveDiff";

export function DiffText({ raw, clean }: { raw: string; clean: string }) {
  const spans = useMemo(() => diffLiveText(raw, clean), [raw, clean]);
  return (
    <span className="live-diff-text">
      {spans.map((span, index) => {
        if (span.kind === "delete") {
          return (
            <del
              className="live-diff-delete"
              data-diff-kind="removed"
              aria-label={`Removed: ${spoken(span.text)}`}
              key={`delete-${index}`}
            >
              {span.text}
            </del>
          );
        }
        if (span.kind === "insert") {
          return (
            <ins
              className="live-diff-insert"
              data-diff-kind="added"
              aria-label={`Added: ${spoken(span.text)}`}
              key={`insert-${index}`}
            >
              {span.text}
            </ins>
          );
        }
        return <span key={`equal-${index}`}>{span.text}</span>;
      })}
    </span>
  );
}

function spoken(value: string): string {
  return value.trim() || "space";
}
