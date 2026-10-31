# Transcript quality accounting

Use this page to interpret missing audio, failed decoding and degraded transcript spans.
Recordings carry a bounded `QualityRecord`
([`corti-core/src/quality.rs`](../crates/corti-core/src/quality.rs)) through capture, live filing,
batch recovery, retries and redo. This page describes what it records and the limits it accepts.
Decisions are in [ADR 0010](../design/adr/0010-live-inbox-filing.md) and
[ADR 0014](../design/adr/0014-transcript-generation-provenance.md).

## The record

- **Bounds.** At most 64 epochs and 256 spans. Omitted spans keep class/resolution-aware counts;
  omitted epochs or saturated counters prevent any completeness claim. The private sidecar is at
  most 256 KiB and is authoritative; note frontmatter uses a labelled 48 KiB projection, and body
  markers use the full record.
- **Classes.** Source loss (audio never captured), processing loss (audio captured but not
  transcribed) and degraded-only facts (transcribed, but less trustworthy). Source loss is never
  "recovered" by a successful batch run; an authorized whole-file decode resolves processing loss
  only.
- **Durability.** Successful live outcomes publish quality through the existing checkpoint and
  sidecar before Done can remove transient authority. Legacy recordings gain processing history in
  an explicit `legacy_unknown` sidecar state without inventing capture authority.

## Capture (#162, #138)

Capture loss is kept as source-frame intervals and epoch offsets. Small gaps are exact-zero
unavailable frames; a run longer than ten seconds becomes an unfilled epoch jump. A changed device
rate is normalized off the callback to the fixed initial rate; unchanged-rate PCM is bit-identical.

When an epoch expects both tracks, the shorter track is padded with silence and the missing interval
is recorded as `MicTapMismatches`. Each tee chunk carries its retained start and epoch, so
positions survive dropped chunks. Unavailable samples are masked to zero after filtering.

Retries are armed before the first batch ASR so the original backend survives a crash. A fresh batch
or first live-to-batch fallback gets six attempts; changed or unknown backend origins refuse before
any staging, decoding or hosted work.

## Decoding (#163)

A region decodes as complete, recognized-empty, failed or partial. Failed and partial regions are
processing loss with their source interval; they are never filed as silence. There are no automatic
region retries; whole-file recovery remains the only recovery path. A failed resampler flushes the
valid tail before the missing interval, and skipped time shifts later word times immediately.
Dropped *successful* reports or mappings are counted as degraded-only metadata and never turn a
complete decode into a failure.

## Live boundaries (#155)

Live windows are cut by a pure policy ([`corti-transcribe/src/boundary.rs`](../crates/corti-transcribe/src/boundary.rs))
that waits, after the nominal window, for a gap where both channels are drained and no VAD region is
open. If none arrives within 22 seconds, or buffered text approaches its bound, a **forced** cut is
taken at the received frame and recorded as a `ForcedBoundary` degraded-only span. After a forced
cut the VAD replays one second of already-decoded audio on its window grid, so decoding resumes
exactly at the cut and nothing is decoded twice.

A forced cut is not overlap-decoded. The word straddling it is decoded in two parts, and because the
model attends over the whole region, a shorter pre-cut region can also flip a borderline word far
from the cut. The boundary matrix
([`scripts/quality/boundary/README.md`](../scripts/quality/boundary/README.md)) holds forced cells
to an explicit contract and lists the observed exceptions exactly; seam re-decoding is #168.

## Far end and echo (#156, #161)

A bounded tracker ([`corti-core/src/activity.rs`](../crates/corti-core/src/activity.rs)) watches
pre-AEC mic and tap levels. Sustained mic activity (-60 dBFS, five one-second bins) against a silent
tap is recorded as a *suspected silent far reference*. This flags suspected missing far-end audio;
it cannot prove the cause or recover that audio. Echo removal requires acoustic evidence and
preserves critical-word differences identified by the shared classifier (including names, numbers
and negations). Without acoustic evidence, including AWS, suspected duplicates stay in the note.

## Evidence (#164)

The optional audit file is described in [local transcript evidence](local-transcript-evidence.md).

## Tests

- [CI](../.github/workflows/ci.yml) runs frontend build/tests, Rust formatting, strict Clippy and
  Rust tests on macOS, including independent AWS-only and local-only app configurations.
- [mac-validation](../.github/workflows/mac-validation.yml) is path-filtered for code, harness,
  benchmark, lockfile and workflow changes, with manual dispatch also available. It runs Python
  validator/scoring checks, pinned native models, authored speech, the boundary matrix and the
  app-orchestration harness. Its commit matrix lints the selected PR commits.
- [`scripts/quality/run_native.py`](../scripts/quality/run_native.py) defines required native tests.
  The [evidence harness](../app/tests/evidence/README.md) can also run model-free on Linux; that
  does not certify native decoding, device behavior or the full app. Passing the boundary matrix
  includes its declared exceptions; it does not prove seam errors are fixed.
