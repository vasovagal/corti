# Contributing to corti

Build and test Corti on an Apple Silicon Mac. Check prerequisites, build the frontend, then run
the Rust checks below. Read [the guardrails](design/guardrails.md) and relevant [ADRs](design/adr/)
before an architectural change; update the matching ADR when a decision changes.

## Prerequisites

- **Apple Silicon** (`aarch64-apple-darwin`) on **macOS 15+** — no Intel, no universal
  binaries, no older OS (`minimumSystemVersion = 15.0`).
- **Rust 1.96+** (pinned via `rust-version` in `Cargo.toml`).
- **Node.js 24+** and a current **Xcode / macOS SDK** for the Tauri frontend.
- **CMake** (`brew install cmake`) — builds the pinned transcribe.cpp C++/Metal backend.
- **sccache** (`brew install sccache`) — the Rust compiler wrapper selected in `.cargo/config.toml`.
- **Tauri 2 CLI** — required for the `cargo tauri` commands.

## Workspace layout

| Crate | Role |
|-------|------|
| `corti-core` | Platform-free domain types and transcript Markdown renderer |
| `corti-coreaudio` | Owned CoreAudio bindings: mic-in-use listener, process attribution, process taps |
| `corti-detect` | Start/stop trigger + attribution state machine |
| `corti-capture` | Aggregate-device capture graph, streamed two-track WAV, and in-flight AEC |
| `corti-transcribe` | Transcription contract, cleanup, boundary policy, and local evidence |
| `corti-transcribe-aws` | AWS Transcribe batch backend (`aws` feature) |
| `corti-transcribe-local` | Parakeet through sherpa/CPU or transcribe.cpp/Metal (`local` feature) |
| `corti-aec` | Streaming and offline echo cancellation |
| `corti-postprocess` / `corti-postprocess-providers` | Hosted text contracts and provider adapters |
| `corti-chat` | Hosted conversation and subscription coordinator |
| `corti-lexicon` | Explicitly approved transcript correction rules |
| `corti-tap` | Standalone CLI: force-tap system audio to a WAV on demand (`--inbox` to file a note) |
| `corti-queue` | Durable job store + crash recovery |
| `corti-jobs` | Small background-job layer in queue.db: retries with backoff + periodic schedules |
| `corti-vagus` | The `vagus` CLI shell-out (the only vagus touchpoint) |
| `corti-bench` | Audio-quality benchmark harness (WER/cpWER/RSS/ERLE sweep over `bench/`) |
| `app/` | Tauri 2 tray binary |

## Build / test

Install frontend dependencies and build `app/ui/dist` before invoking Cargo: Tauri reads this
directory through `frontendDist` in `app/tauri.conf.json`.

```sh
npm --prefix app/ui ci
npm --prefix app/ui run build
npm --prefix app/ui test
```

```sh
cargo build                              # build the workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check                        # formatting (run cargo fmt before pushing)
cargo test

# standard app posture: both backends + sherpa/CPU and transcribe.cpp/Metal local engines
cargo build -p corti-app

# the Tauri tray app
cargo tauri dev                          # dev server + hot reload
cargo tauri build                        # produces .app + .dmg under target/.../bundle/

# deterministic marketing screenshots (real frontend + mocked Tauri IPC)
cd screenshots && npm install && npm run capture
```

The screenshot harness needs Playwright Chromium once (`npx playwright install chromium`) and writes
ignored Retina PNGs to `screenshots/output/`. See [`screenshots/README.md`](./screenshots/README.md).

The app defaults are `aws`, `local`, and `local-ggml`. Select the active backend with
`CORTI_TRANSCRIBE_BACKEND`; local ASR uses `CORTI_LOCAL_ASR_ENGINE=sherpa|ggml`. A minimal
`sherpa`-only build is `cargo build -p corti-app --no-default-features --features local`.
transcribe.cpp compiles pinned native C++/Metal source and requires CMake on the build machine.

## Local backend models

The offline backend needs one selected Parakeet artifact (sherpa's ONNX set or transcribe.cpp's Q8_0
GGUF), shared Silero VAD, and — for opt-in far-end diarization — pyannote segmentation + an English
embedding. The app's Settings → Models screen downloads the engine-aware set with checksum verification.
For sherpa CLI development, fetch the ONNX set up front:

```sh
crates/corti-transcribe-local/fetch-models.sh
```

## Verification examples

```sh
cargo run -p corti-coreaudio --bin probe                       # watch mic on/off + attribution live
cargo run -p corti-coreaudio --example splitwav -- rec.wav     # decode + split channels
cargo run -p corti-transcribe-aws  --example transcribe_file -- file.wav   # needs AWS creds
cargo run -p corti-transcribe-local --example transcribe_file -- file.wav  # needs local models
cargo run -p corti-queue --example inspect                     # inspect the durable job queue
```

## Dev setup

- **TCC / signing.** Running the bundled `.app` needs the macOS Microphone + System Audio
  Recording grants. Because ad-hoc signing's cdhash changes per build, macOS may ask for grants again; use a stable signing identity for repeated local capture tests. Any CLI that captures
  audio also needs a signed TCC identity and the appropriate grants. Pure decoding and queue
  examples do not capture audio. See [guardrail 10](design/guardrails.md).
- **AWS.** The `aws` backend uses the standard credential chain (env vars, `~/.aws/config`,
  or an IAM role). The minimal IAM policy:

  ```json
  {
    "Version": "2012-10-17",
    "Statement": [
      { "Sid": "CortiTranscribeJobs", "Effect": "Allow",
        "Action": ["transcribe:StartTranscriptionJob", "transcribe:GetTranscriptionJob",
                   "transcribe:DeleteTranscriptionJob"],
        "Resource": "*" },
      { "Sid": "CortiStagedObjects", "Effect": "Allow",
        "Action": ["s3:PutObject", "s3:GetObject", "s3:DeleteObject"],
        "Resource": "arn:aws:s3:::YOUR_BUCKET/corti/*" },
      { "Sid": "CortiListBucket", "Effect": "Allow",
        "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::YOUR_BUCKET",
        "Condition": { "StringLike": { "s3:prefix": "corti/*" } } }
    ]
  }
  ```

## CI / release

- **CI** ([`ci.yml`](.github/workflows/ci.yml)) builds/tests the frontend, checks formatting, and
  runs strict Clippy and Rust tests on `macos-26`, including AWS-only and local-only app builds.
- **Native validation** ([`mac-validation.yml`](.github/workflows/mac-validation.yml)) checks each PR
  commit, verifies model downloads, generates synthetic speech, and runs native ASR and boundary
  checks on macOS. This does not establish real microphone, device-change, TCC, or call behavior;
  those need an actual-Mac acceptance run.
- **Release** (`release.yml`) is tag-triggered (`v*`): it creates a draft, verifies the pinned native
  input and signed app bundle, uploads only `.dmg` + `.app.zip`, then publishes. The shared Homebrew cask
  is regenerated and pushed manually per `CLAUDE.md`; CI never writes the tap. `warm-cache.yml` pre-seeds
  the build cache.

See [`design/adr/`](./design/adr) for the rationale (distribution, signing, backend
selection, AEC) and [`design/STATUS.md`](./design/STATUS.md) for current state.

The repository belongs to the **`vasovagal`** GitHub organization.
