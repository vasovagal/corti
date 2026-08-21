# Going LIVE with Corti — verification and metrics

> **Required release gate: RED.** Source, Rust, UI, deterministic-capture, and signed `.app` gates are green, but the canonical Tauri DMG command failed twice because Finder's styling AppleScript timed out with `-1712`. No current DMG size is inferred. Delivery should use a draft PR until that gate is rerun successfully.

## Scope and conditions

Verification ran from `/Users/xavier/code/vasovagal/.corti-worktrees/live-post-processing` on branch `feat/live-post-processing`, initially clean at `0033a5b0843949f004c8c520b8c648ac9142f079`. The feature source baseline is its merge base, `cc6f75ae42aa3e5640efced4366f97e54d0432dc`. Measurements ran from 2026-08-21 12:26:42Z through the 12:59:02Z cleanup on macOS 26.6 (arm64), rustc 1.97.0, cargo 1.97.0, Node 26.7.0, npm 11.19.0, tauri-cli 2.11.4, Playwright 1.62.1, Vite 8.0.16, and Vitest 4.1.8.

Cargo used the repository's normal `aarch64-apple-darwin` target and `-C target-cpu=apple-m1`. Homebrew sccache 0.17.0 remained the configured shared warm `rustc-wrapper`; no `RUSTC_WRAPPER`, `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `CARGO_TARGET_DIR`, or incremental setting was changed. No `cargo clean` was run. Every install or build started only after a physical `df -k` receipt exceeded 50 GiB.

All Cargo dependency resolution was `--locked --offline` with `CARGO_NET_OFFLINE=true`. The exact safety prefix (followed by `/usr/bin/time -p cargo ...`) was:

```sh
env -u CORTI_HOSTED_PRODUCTION_ARMED -u OPENAI_API_KEY -u ANTHROPIC_API_KEY \
  -u GOOGLE_APPLICATION_CREDENTIALS -u GOOGLE_CLOUD_PROJECT -u CLOUDSDK_CORE_PROJECT \
  -u AWS_ACCESS_KEY_ID -u AWS_SECRET_ACCESS_KEY -u AWS_SESSION_TOKEN -u AWS_PROFILE \
  -u AWS_WEB_IDENTITY_TOKEN_FILE -u AWS_CONTAINER_CREDENTIALS_RELATIVE_URI \
  -u AWS_CONTAINER_CREDENTIALS_FULL_URI -u AWS_CONTAINER_AUTHORIZATION_TOKEN \
  AWS_EC2_METADATA_DISABLED=true AWS_SHARED_CREDENTIALS_FILE=/dev/null \
  AWS_CONFIG_FILE=/dev/null CARGO_NET_OFFLINE=true
```

The production app was built but never launched. Hosted tests use injected credentials, clocks, stores, and transports; the only explicitly approved app execution test uses its injected transport. Playwright replaces Tauri IPC with synthetic fixtures and aborts every non-loopback browser request. No paid-model request or ambient provider credential lookup occurred, and no credential or personal transcript content was logged or serialized.

Policy posture remained explicit in the verified build:

- Claude Free/Pro/Max import/routing remains blocked without written Anthropic permission; direct Anthropic API support is a separate documented adapter.
- Codex app-server support is experimental, feature-gated, and off by default; `--all-features` compiled it but did not run a process or provider request.
- Vertex remains ADC-based, and Rust, React, and Playwright assert the exact visible unarmed warning: `gcloud token isn't armed`.

## Gate results and wall times

Commands below were timed with `/usr/bin/time -p`. Cargo commands used the offline/unarmed environment above; npm/Playwright commands removed the same provider variables.

| Gate command | Result | Wall time |
|---|---:|---:|
| `cargo fmt --all -- --check` (final post-fix run) | pass | 1.31 s |
| `cargo test --locked --offline -p corti-postprocess -p corti-postprocess-providers -p corti-queue -p corti-vagus` | 110 passed, 0 failed | 20.87 s |
| `cargo test --locked --offline -p corti-app` (targeted default-feature run) | 138 passed, 0 failed | 103.08 s |
| `cargo clippy --locked --offline --all-targets -- -D warnings` (final retry) | pass | 5.39 s |
| `cargo clippy --locked --offline --all-targets --all-features -- -D warnings` | pass | 6.71 s |
| `cargo test --locked --offline` (final post-fix workspace gate) | 376 passed, 0 failed, 5 ignored | 53.73 s |
| `cargo clippy --locked --offline -p corti-app --all-targets --no-default-features --features aws -- -D warnings` | pass | 27.00 s |
| `cargo test --locked --offline -p corti-app --no-default-features --features aws` | 135 passed, 0 failed | 16.47 s |
| `cargo clippy --locked --offline -p corti-app --all-targets --no-default-features --features local -- -D warnings` | pass | 31.41 s |
| `cargo test --locked --offline -p corti-app --no-default-features --features local` | 137 passed, 0 failed | 39.96 s |
| `cd app/ui && npm ci` | pass; 56 packages | 0.80 s |
| `cd app/ui && npm run typecheck` | pass | 1.91 s |
| `cd app/ui && npm test -- --reporter=verbose` | 61 passed in 8 files | 1.28 s |
| `cd app/ui && npm run build` | pass; 60 modules | 2.24 s |
| `cd screenshots && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci` | pass; 7 packages | 0.49 s |
| `cd screenshots && npm run typecheck` | pass | 0.87 s |
| `cd screenshots && CI=1 CORTI_TEST_URL=http://127.0.0.1:1425 npm run capture -- --retries=0` | 17 passed, run 1 | 10.12 s |
| same deterministic capture command, fresh server/output | 17 passed, run 2 | 9.84 s |
| `APPLE_SIGNING_IDENTITY=- CARGO_NET_OFFLINE=true SHERPA_ONNX_LIB_DIR=$PWD/target/sherpa-onnx-prebuilt/sherpa-onnx-v1.13.2-osx-arm64-static-lib/lib cargo tauri build --ci --bundles app,dmg -- --locked --offline` | `.app` pass; **DMG fail** | 408.97 s |
| same inputs, `cargo tauri build -vv --ci --bundles dmg -- --locked --offline` | **DMG fail reproduced** | 234.47 s |

The final workspace suite's five ignored tests require real ASR/VAD/diarization model files (and, for some, a speech WAV); those external fixtures were not supplied. One capture run plus the final Rust workspace and UI suites represents 454 passing tests (376 Rust + 61 Vitest + 17 Playwright), without double-counting the repeated capture or backend-configuration reruns.

The frontend build emitted 424-byte HTML, 40,450-byte CSS, and 366,019-byte JavaScript artifacts (406,893 logical bytes total). `npm ci` also reported two high-severity audit findings in the unchanged pre-feature UI lockfile; audit is not a configured gate. The all-feature clippy lane emitted the existing non-fatal warning that no CoreML-enabled sherpa library was selected for that lane. The workspace test emitted the existing Cargo warning that the AWS and local `transcribe_file` examples share an output filename.

## Reproduced defects and corrections

The first default clippy attempt failed after 18.01 s on `clippy::derivable_impls`. Replacing the manual `OutputTokenAccounting::default` with equivalent `#[derive(Default)]`/`#[default]` behavior exposed three additional Rust 1.97 lints on the 22.22 s retry: two `large_enum_variant` findings and one auditable eight-input cost helper. The accounting event and store cipher payloads are now boxed; the pricing helper has a scoped `too_many_arguments` allowance. Final fmt, clippy, workspace tests, and both app feature lanes passed after those changes.

Two verifier-harness errors were corrected without source impact: the first screenshot install invocation placed an `env` assignment before `-u` and exited 127, and the first process check matched its own `awk` command. The corrected install passed, and the corrected process/open-file check found no worktree cargo, rustc, Node, Vite, esbuild, or open-file user before cleanup.

The required DMG failure is not hidden. The first non-verbose run left the signed `.app` valid but reported only `bundle_dmg.sh` failure. The verbose retry identified the reproducible cause:

```text
Finder got an error: AppleEvent timed out. (-1712)
Failed running AppleScript
```

The intermediate image detached normally. Killing/restarting the user's Finder or weakening the release command with `--skip-jenkins` was outside this verification scope, so no current DMG is claimed. The release workflow's exact DMG gate remains red.

## Source and dependency deltas

Sizes are logical blob bytes, not filesystem allocation. The tracked implementation snapshot was measured before adding this report so that the report cannot recursively change its own metric:

```sh
git ls-tree -r -l cc6f75ae42aa3e5640efced4366f97e54d0432dc \
  | awk '{ bytes += $4; files++ } END { print files, bytes }'
git ls-files -z | xargs -0 stat -f '%z' \
  | awk '{ bytes += $1; files++ } END { print files, bytes }'
git diff --shortstat cc6f75ae42aa3e5640efced4366f97e54d0432dc
```

| Metric | Before (`cc6f75a`) | Verified implementation | Delta |
|---|---:|---:|---:|
| All tracked files | 251 | 297 | +46 (+18.3267%) |
| All tracked logical bytes | 2,327,659 | 3,556,077 | +1,228,418 (+52.7748%) |
| Code files (`.css`, `.html`, `.rs`, `.ts`, `.tsx`) | 105 | 147 | +42 (+40.0000%) |
| Code logical bytes (same extension set) | 1,173,891 | 2,316,291 | +1,142,400 (+97.3174%) |
| Diff | — | 72 files, +34,787/−283 | — |

Cargo manifest/lock measurements used `tomllib` over `git show <rev>:Cargo.toml` and counted exact `[[package]]` records in each lockfile.

| Dependency metric | Before | After | Delta |
|---|---:|---:|---:|
| Workspace members | 14 | 16 | +2 |
| `[workspace.dependencies]` entries | 23 | 32 | +9 |
| `Cargo.lock` package records | 693 | 697 | +4 |
| npm lockfiles | unchanged | unchanged | 0 |

The two new members are `corti-postprocess` and `corti-postprocess-providers`. The nine shared dependency entries comprise those two internal crates plus `base64`, `hmac`, `sha2`, `zeroize`, `unicode-normalization`, `unicode-casefold`, and `unicode-segmentation`. The exact lock additions are the two workspace crates and two registry packages: `unicode-casefold 0.2.0` and `unicode-normalization 0.1.25`; there are no lock removals. Other newly direct app dependencies were already transitively locked.

## Release artifact comparison

No baseline worktree was created. The baseline is the official published v0.13.0 release (`50cfb92`, tag `v0.13.0`), downloaded over its public URLs. Both published asset digests matched GitHub and the downloaded DMG passed `hdiutil verify`. The current build used the hash-pinned sherpa archive `e2d704b01c392970ee7fb90d7e74fd854528a172de0381228e987b79ac479f8e`, ran sequentially with 94.466 GiB free, and reported 4m24s for its optimized Cargo compile. `scripts/verify-release-bundle.sh` and strict deep `codesign` verification passed on the current ad-hoc-signed `.app`.

| Artifact | Official v0.13.0 | Current | Delta |
|---|---:|---:|---:|
| Signed bundle executable | 43,171,632 B | 44,743,264 B | +1,571,632 B (+3.6404%) |
| `.app` logical file bytes (4 files) | 43,308,548 B | 44,880,180 B | +1,571,632 B (+3.6289%) |
| `.app` allocated size (`du -sk`) | 42,300 KiB | 43,836 KiB | +1,536 KiB (+3.6312%) |
| Release `.app.zip` | 17,219,479 B | 17,933,511 B | +714,032 B (+4.1467%) |
| Release DMG | 17,335,842 B | **unavailable: red gate** | n/a |
| Current unbundled optimized binary | n/a | 44,985,600 B | n/a |

Relevant SHA-256 values:

- baseline `.app.zip`: `7211fcaeb22672dfd89dcd8778fee167d6e93207f932adcd687520ec616372af`
- baseline DMG: `9c0e28b0d2eb152baf819576f5b065a5cf93ccb465ec8ace46649704df4cfcdd`
- baseline bundled executable: `1121586401aa21f75d5a488090e6d328ad05f017768a24503fb50341647c9a99`
- current bundled executable: `63b651f279aef51fd9d2fa25bcf84aa002dfcbaf69cdaea5d1999274f7d1162f`
- current `.app.zip` made with the release workflow's `ditto -c -k --sequesterRsrc --keepParent`: `c5b05fc8b96e9ff22f827a0ea3cd2455576022d656d5322883dea273bd9e2dd4`

The first complete Tauri command took 408.97 s including the Finder timeout; its Cargo release phase was 264 s. The focused verbose retry took 234.47 s, including a 101 s optimized app rebuild and the second Finder timeout. The current zip is measured and integrity-tested but not a publishable release pair while the DMG is absent.

## Deterministic UI artifacts

Both Playwright runs used one worker, zero retries, a fresh Vite server on `127.0.0.1:1425`, fixed time, fixed dark viewport/media settings, synthetic IPC/event fixtures, and non-loopback request blocking. All 19 PNG SHA-256 lines were byte-identical between runs (`diff` exit 0). The final set was copied and hash-verified at:

`/Users/xavier/code/vasovagal/.vagus-artifacts/going-live-corti/ui-final`

It contains 19 PNGs and 6,139,199 logical bytes. The sorted run manifest's SHA-256 is `1087f3c958fc52457bd753eed25142564fad56adce2471d0d5014c897c5bb7a5`. A contact sheet was visually reviewed; all content is synthetic. Individual file sizes are:

```text
282660 assistant-pinned-desktop.png
196534 assistant-pinned-narrow.png
196534 live-assistant-drawer.png
325815 live-diff-cost-desktop.png
177594 live-diff-cost-narrow.png
325815 live-rewriting-assistant.png
319983 live-transcript.png
124754 pipeline.png
543766 preferences-desktop.png
307912 preferences-narrow.png
89107 recording-queue.png
280927 reduced-motion-desktop.png
154944 reduced-motion-narrow.png
1879415 settings-hosted.png
312892 settings-local.png
145107 vertex-recovery-desktop.png
154310 vertex-recovery-narrow.png
155034 vertex-warning-desktop.png
166096 vertex-warning-narrow.png
```

## Free-space and cache receipts

Every install/build threshold check passed. Available 1-KiB blocks immediately before each operation were:

```text
ui npm ci                 107817796   ui typecheck              107743284
ui test                   107741876   ui build                  107740804
screens npm ci retry      107739192   screens typecheck         107692144
Playwright run 1          107692512   Playwright run 2          107687700
relevant Rust tests       107686220   app default tests         107048156
clippy initial            103121664   clippy retry              102920108
clippy final retry        102564092   clippy all features       102544224
workspace tests           102415976   app AWS clippy            101265988
app AWS tests             100765088   app local clippy          100490740
app local tests           100131832   release build              99054500
DMG retry                  97146056   current app zip             97108852
```

The lowest start was 97,108,852 KiB (92.610 GiB), safely above 50 GiB; space never approached the 25 GiB stop threshold. Shared sccache's cumulative start/end snapshots were 955/4,158 compile requests, 812/1,417 hits, and 96/2,263 misses. Overall cumulative hit rate moved from 89.43% to 38.51% (Rust 73.22% to 13.12%) as branch-specific and release-profile objects were compiled. The final shared counter also recorded 15 compiler failures, including the reproduced lint failures and compiler probes; all authoritative final Cargo test/clippy outcomes are reported above.

Before cleanup, no process command or open file referenced this worktree. Exact removed paths and allocated sizes were:

- `target`: 10,886,644 KiB
- `app/ui/node_modules`: 76,416 KiB
- `screenshots/node_modules`: 45,744 KiB

Physical free space moved from 97,068,976 KiB (92.572 GiB) to 107,575,720 KiB (102.592 GiB), a receipt of +10,506,744 KiB (+10.020 GiB). Only those three requested paths were removed; `cargo clean` was not used.

## Workflow token ledger

| Runtime/phase | Tokens |
|---|---:|
| Prior workflow `going-live-with-corti-mt2h123x-8fc98r` (runtime-reported) | 21,805,203 |
| Core Recovery | 30,062,684 |
| App Backend | 88,295,232 |
| React Experience | 29,011,598 |
| Adversarial Review | 24,892,287 |
| Hardening | 66,120,369 |
| **Reported total through Hardening** | **260,187,373** |

A verification-phase token value was not exposed by the runtime, so none is invented.
