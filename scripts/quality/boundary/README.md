# Authored boundary matrix

The existing macOS native job invokes this matrix through `../run_native.py`.
Only authored `say`/`afconvert` fixtures and the existing checksum-pinned models
are used. `fixtures.json` fixes all text, raw alternatives, source rates, phases,
forced cuts, chunk sizes and epoch limits before inference.

`generate.py` writes the exact WAV hashes, utterance windows and occurrence
inventory. Its structural validator reconstructs every pause and utterance from
the generated source files and checks all sample windows before models load.
`run.py` repeats that check, verifies installed model bytes against their pinned
archives, builds the test executable and prints an execution seal containing
the fixture, executable, toolchain, model and implementation hashes. It executes
those unchanged inputs and checks their identities again afterward.

The Rust runner calls actual batch, uncut live and bounded live paths. Its raw
TSV records admitted input, canonical ASR attempts, typed outcomes, source
ownership, selected boundaries and native epoch headroom. The validator checks
the actual irregular chunk sequence, nominal recurrence, finite settlement
reserve, reset origins and terminal ownership for both tracks. A model-free
positive receipt tests the validator; it is not an inference result.

Every uncut control and every cell with only natural cuts (including the
maintenance cell) must have at most 5% ordinary word-token edits against its
batch baseline, with no changed, deleted or duplicated raw critical occurrence.

A forced or deadline cut splits a speech region without overlap re-decoding: the
word straddling the cut is decoded in two parts, and each part has less context,
so number formatting or a homophone can also differ elsewhere in that region.
Cells with such cuts are held to a no-silent-loss contract instead
(`forced_gate` in `matrix.py`): at most 4 edits within one second of each forced
cut, at most 2% of batch tokens deleted away from every cut, and no critical
occurrence wholly deleted away from a cut. A critical occurrence damaged within
a cut's second is reported as `degraded_critical`; the app marks every forced cut
with a `ForcedBoundary` quality span. Decoding resumes exactly at the cut: after
a VAD reset, one second of already-decoded audio is replayed on the VAD window
grid to warm the model and is never decoded again.

Missing or ambiguous authored coverage, missing required critical crossings,
failed decode coverage or incomplete evidence fails the run. Locating a critical
occurrence ignores sentence punctuation and a leading capital; scoring compares
raw tokens. Fixtures use the standard Samantha voice when it is installed.
Boundary-local diagnostics reuse the whole-run alignment and do not replace it.
On failure the run prints a `BOUNDARY_DIAGNOSTIC` report to the job log.

Run the portable validator checks with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/quality/boundary -p 'test_*.py'
```

Native output retains the pre-inference seals, plan, raw TSV, parsed results,
scores and process time/RSS log. Production EOF repairs, the native test runner
and the wrapper require independent review before a native campaign is launched.
