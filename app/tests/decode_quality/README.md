# App decode fallback contract

`run.py` builds an offline test crate from the current production functions in `pipeline.rs`,
`transcribe.rs`, `jobs.rs`, `live.rs` and their checkpoint/config/provenance helpers.
`boundaries.rs` replaces only the GUI/tray/stats, hosted-service, lexicon and native boundaries.
The test-only backend returns invented text and typed quality; no model, credentials, network or
user audio are involved.

It is the base layer of the evidence harness, which adds the evidence module that production now
depends on. Run the complete suite, including this directory's `full_path.rs` and `retention.rs`
tests, with:

```sh
python3 app/tests/evidence/run.py --clippy
```

The tests cover one backend invocation per fresh batch (none for a changed backend), retained
partial Batch losses, same-inode canonical rewrites, checkpoint replay without audio or another
ASR call, and preservation of `ForcedBoundary`/`WindowNotFiled` quality through completion and
recovery. Native decoder correctness is a separate native gate.

The generated crate starts from the workspace `Cargo.lock`; the runner fails if offline resolution
adds or changes any registry package, then runs Cargo with `--locked --offline`.
