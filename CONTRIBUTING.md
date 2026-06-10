# Contributing to corti

Thanks for hacking on corti. This file covers the workspace layout, building, testing, and
the dev-only setup (signing, AWS, models). Design rationale and the binding invariants live
in [`design/`](./design/) — read the ADRs and [`design/guardrails.md`](./design/guardrails.md)
before any architectural change, and update the matching ADR when you change a decision.

## Prerequisites

- **Apple Silicon** (`aarch64-apple-darwin`) on **macOS 15+** — no Intel, no universal
  binaries, no older OS (`minimumSystemVersion = 15.0`).
- **Rust 1.96+** (pinned via `rust-version` in `Cargo.toml`).
- **Node.js 24+** and a current **Xcode / macOS SDK** for the Tauri frontend.

## Workspace layout

| Crate | Role |
|-------|------|
| `corti-core` | Shared domain types (no platform deps) |
| `corti-coreaudio` | Owned CoreAudio bindings: mic-in-use listener, process attribution, process taps |
| `corti-detect` | Start/stop trigger + attribution state machine |
| `corti-capture` | Aggregate-device capture graph → multitrack WAV |
| `corti-transcribe` | `Transcriber` trait + diarized-transcript Markdown renderer |
| `corti-transcribe-aws` | AWS Transcribe batch backend (`aws` feature) |
| `corti-transcribe-local` | Local offline backend — Parakeet-TDT via ONNX/sherpa-onnx (`local` feature) |
| `corti-aec` | Offline NLMS/FDAF echo canceller — removes speaker bleed from the mic track |
| `corti-tap` | Standalone CLI: force-tap system audio to a WAV on demand (`--inbox` to file a note) |
| `corti-queue` | Durable job store + crash recovery |
| `corti-vagus` | The `vagus` CLI shell-out (the only vagus touchpoint) |
| `app/` | Tauri 2 tray binary |

## Build / test

```sh
cargo build                              # build all library crates
cargo clippy --all-targets -- -D warnings
cargo fmt --check                        # formatting (run cargo fmt before pushing)
cargo test

# both transcription backends compiled together (runtime-selectable)
cargo build --features aws,local

# the Tauri tray app
cargo tauri dev                          # dev server + hot reload
cargo tauri build                        # produces .app + .dmg under target/.../bundle/
```

The app's transcription features are `aws` (default) and `local`; ship builds use
`--features aws,local` and select the active backend at runtime via
`CORTI_TRANSCRIBE_BACKEND`.

### Local backend models

The offline backend needs its ONNX models (Parakeet-TDT, Silero VAD, and — for opt-in
far-end diarization — pyannote-segmentation + an English speaker-embedding model, default
NeMo TitaNet-Large; ~0.7 GB total, cached under
`~/Library/Caches/corti/models/`). Fetch them up front:

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
  Recording grants. Because ad-hoc signing's cdhash changes per build, macOS re-prompts on
  each run; for repeated dev runs create a stable self-signed cert and pass
  `CORTI_SIGN_ID=<cert-name>`. CLI examples and tests don't need the grant.
- **AWS.** The `aws` backend uses the standard credential chain (env vars, `~/.aws/config`,
  or an IAM role). The minimal IAM policy:

  ```json
  {
    "Version": "2012-10-17",
    "Statement": [
      { "Sid": "CortiTranscribeJobs", "Effect": "Allow",
        "Action": ["transcribe:StartTranscriptionJob", "transcribe:GetTranscriptionJob"],
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

- **CI** (`ci.yml`) runs on `macos-26` only — `corti-coreaudio`'s bindings link CoreAudio
  and don't build elsewhere.
- **Release** (`release.yml`) is tag-triggered (`v*`): it builds + bundles the `.app`,
  uploads `.dmg` + `.app.zip` to the GitHub release, and bumps the private Homebrew cask.
  `warm-cache.yml` pre-seeds the build cache.

See [`design/adr/`](./design/adr) for the rationale (distribution, signing, backend
selection, AEC) and [`design/STATUS.md`](./design/STATUS.md) for current state.

This is a personal repo under the **`vasovagal`** GitHub org.
