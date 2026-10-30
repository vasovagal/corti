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

Every candidate and uncut control must have at most 5% ordinary word-token
edits against its batch baseline, with no changed, deleted or duplicated raw
critical occurrence. Forced and maintenance cells use the same gates. Missing
or ambiguous authored coverage, missing required critical crossings, failed
decode coverage or incomplete evidence fails the run. Boundary-local diagnostics
reuse the whole-run alignment and do not replace it.

Run the portable validator checks with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/quality/boundary -p 'test_*.py'
```

Native output retains the pre-inference seals, plan, raw TSV, parsed results,
scores and process time/RSS log. Production EOF repairs, the native test runner
and the wrapper require independent review before a native campaign is launched.
