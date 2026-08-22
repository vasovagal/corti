# Corti release-binary size analysis

- **Baseline:** `ba0884fabdd5f3e996b0755be83cf798229121a8`
- **Tracking:** [#120 — Shrink the release binary with full LTO](https://github.com/vasovagal/corti/issues/120)
- **Measured:** 2026-08-22 on Apple Silicon, macOS 26.6, rustc 1.97.0
- **Target:** `aarch64-apple-darwin`, `-C target-cpu=apple-m1`

## Result

Corti was not in Vagus's starting position from [vagus#31](https://github.com/vasovagal/vagus/pull/31):
Corti already stripped release symbols, used ThinLTO, and used one codegen unit. The remaining safe profile
win is **FullLTO**. On one source tree and toolchain, changing only `lto = "thin"` to `lto = "fat"` removes
2,460,112 bytes (5.44%) from the shipped executable and 643,741 bytes (3.55%) from its `gzip -9`
representation, with no AEC throughput regression and no linkage change.

The larger apparent win, `opt-level = "z"`, is rejected. It nearly halves measured AEC throughput, which is
the wrong trade for a live audio application. `panic = "abort"` is also rejected because the live session
thread deliberately catches panics and falls back to the crash-safe batch path.

## Reproduction conditions

The UI was built first with `npm ci && npm run build`. Cargo ran locked and offline with the hash-verified
sherpa-onnx 1.13.2 static archive supplied through `SHERPA_ONNX_LIB_DIR`. Provider/AWS credential variables
were removed and no application or paid provider call was run. The default feature set was unchanged:
`aws + local + local-ggml`.

The baseline and candidate were built sequentially in one worktree:

```sh
# Existing profile: ThinLTO, codegen-units=1, strip=true.
cargo build --release --locked -p corti-app

# Same tree/profile, changing only the LTO mode.
cargo build --release --locked -p corti-app \
  --config 'profile.release.lto="fat"'

# Size ceiling only; rejected on measured performance.
cargo build --release --locked -p corti-app \
  --config 'profile.release.lto="fat"' \
  --config 'profile.release.opt-level="z"'
```

Every build started with more than 50 GiB physically free. The shared bounded sccache remained configured;
no shared Cargo target, `cargo clean`, incremental override, or cache relocation was used.

## Same-toolchain A/B

Logical bytes are from `stat`; gzip is `gzip -9`; ZIP is `ditto -c -k --sequesterRsrc` over the executable
alone so compression can be compared without changing app-bundle metadata.

| Profile | Executable | Delta | `gzip -9` | Delta | ZIP | Delta |
|---|---:|---:|---:|---:|---:|---:|
| Existing ThinLTO / opt-3 | 45,263,200 B | — | 18,125,066 B | — | 18,162,862 B | — |
| **FullLTO / opt-3** | **42,803,088 B** | **−2,460,112 B (−5.44%)** | **17,481,325 B** | **−643,741 B (−3.55%)** | **17,513,040 B** | **−649,822 B (−3.58%)** |
| FullLTO / opt-z (rejected) | 33,428,784 B | −11,834,416 B (−26.15%) | 14,564,012 B | −3,561,054 B (−19.65%) | 14,589,180 B | −3,573,682 B (−19.68%) |

Observed wall time was 260.10 s for the first ThinLTO build and 342.95 s for the later FullLTO build. Those
numbers are directional rather than a clean cold-build benchmark because the target and shared sccache were
warm in different ways. The release-link cost is acceptable: `warm-cache.yml` already runs the exact release
profile on qualifying `main` pushes before any tag build.

### Signed app-bundle A/B

The canonical Tauri command was also run twice with the same UI output, deployment target, ad-hoc signing,
and bundle metadata. The ThinLTO arm used a Cargo config override; the FullLTO arm used this branch's profile.
Both passed `scripts/verify-release-bundle.sh` and strict deep `codesign` verification.

| Artifact | ThinLTO | **FullLTO** | Delta |
|---|---:|---:|---:|
| Signed bundle executable | 44,958,688 B | **42,500,816 B** | **−2,457,872 B (−5.47%)** |
| `.app` logical file bytes | 45,095,604 B | **42,637,732 B** | **−2,457,872 B (−5.45%)** |
| Release-style `.app.zip` | 18,035,852 B | **17,401,258 B** | **−634,594 B (−3.52%)** |

The bundle executables are slightly smaller than the standalone A/B because Tauri sets the release bundle's
macOS 15 deployment target. The comparison remains controlled: both bundle arms used that same target and
identical non-executable files.

## Mach-O sections

`xcrun llvm-size -m` attributes the FullLTO reduction mostly to Rust/C++ text deduplication and unwind/link
metadata. `__const` below combines `__TEXT` and `__DATA_CONST` sections.

| Section | ThinLTO | FullLTO | Delta |
|---|---:|---:|---:|
| `__text` | 33,500,068 B | 32,258,408 B | −1,241,660 B |
| `__const` | 4,929,640 B | 4,674,536 B | −255,104 B |
| `__eh_frame` | 1,669,368 B | 1,308,340 B | −361,028 B |
| `__gcc_except_tab` | 1,571,524 B | 1,492,380 B | −79,144 B |
| `__unwind_info` | 515,184 B | 453,632 B | −61,552 B |
| `__cstring` | 1,271,487 B | 1,255,366 B | −16,121 B |
| `__LINKEDIT` segment | 1,081,344 B | 622,592 B | −458,752 B |

Both binaries list the same 24 system libraries/frameworks under `otool -L`; FullLTO adds or removes no
dynamic dependency. Static sherpa/ONNX and transcribe.cpp linkage is unchanged.

## Where the text comes from

`cargo-bloat 0.12.1 --crates` was run against an unstripped, `link-dead-code` analysis build. Its 56.6 MiB
file size is intentionally **not** comparable to the stripped A/B above, but its 31.9 MiB text attribution
shows where optimization can and cannot help:

| Owner/group | Attributed text | Share of analysis `.text` |
|---|---:|---:|
| sherpa-onnx / ONNX Runtime | 11.2 MiB | 34.9% |
| transcribe.cpp / GGML | 1.3 MiB | 4.1% |
| unknown native symbols | 4.6 MiB | 14.3% |
| Rust standard library | 3.1 MiB | 9.7% |
| named AWS crates | 3.14 MiB | 9.8% |
| Tokio + HTTP + TLS clients | 2.41 MiB | 7.5% |
| Tauri/Wry/window/menu stack | 1.53 MiB | 4.8% |
| Corti workspace crates | 1.54 MiB | 4.8% |

The native ASR runtimes are earned by current product decisions. ADR 0003 still needs sherpa/ONNX for VAD
and optional diarization even when GGML performs ASR; ADR 0011 keeps both engines in the standard build until
the real-call live soak and build-topology gates are resolved. Dynamic linkage would also weaken Corti's
single-bundle release contract. FullLTO cannot optimize precompiled native archives internally, but it can
deduplicate and inline the broad Rust graph around them.

## Hot-path check

A deterministic 60-second, 48 kHz stereo fixture (delayed correlated echo plus intermittent near-end tone)
was processed five times by `corti-aec`'s release `aec_file` example. The measured interval is the AEC
operation itself, excluding WAV read/write.

| Profile | Runs (seconds) | Median | Throughput |
|---|---|---:|---:|
| ThinLTO / opt-3 | 0.373, 0.376, 0.373, 0.370, 0.367 | 0.373 s | 161× realtime |
| **FullLTO / opt-3** | 0.369, 0.368, 0.368, 0.371, 0.383 | **0.369 s** | **163× realtime** |
| FullLTO / opt-z | 0.737, 0.742, 0.730, 0.730, 0.720 | 0.730 s | 82× realtime |

FullLTO is within run-to-run noise. Size optimization remains compatible with the speed-oriented ADR 0003
profile. Opt-z is 1.96× slower on this Rust DSP path and is therefore rejected despite its 26.15% file win.

## Rejected or deferred changes

- **`opt-level = "z"`:** measurable 1.96× AEC slowdown. The app's capture/AEC/live path is not a download
  whose runtime can be traded casually for bytes.
- **`panic = "abort"`:** `app/src/live.rs::session_thread` uses `catch_unwind` so a live-path panic can retain
  its partial note and rebuild from the recording. Abort would turn that bounded fallback into process loss.
- **Remove an ASR runtime:** sherpa remains VAD/diarization infrastructure and the upgrade-safe recognizer;
  GGML has not cleared ADR 0011's live soak/build-topology gates. This needs a product/architecture PR, not a
  profile optimization.
- **Drop AWS SSO/credential-process support:** AWS's named code is visible, but removing standard credential
  paths would be a user-facing regression for a modest fraction of the whole binary.
- **Replace one-shot UI helper crates:** `rfd` and the permission plugin are below the material attribution
  threshold after LTO. Reimplementing native dialogs/TCC calls would add maintenance and safety risk for a
  much smaller gain.
- **Disable HTTP compression/TLS features:** PNG/Tauri and AWS still retain the underlying compression/TLS
  crates, while provider/model downloads would lose expected transport behavior.
- **Change release archive format:** outside the executable scope and coupled to Homebrew/release consumers.

## Verification

All provider/AWS credential variables were removed for Cargo gates. Tests used injected transports and did not
make paid or ambient-credential calls.

| Gate | Result | Wall time |
|---|---:|---:|
| `cargo fmt --all -- --check` | pass | 1.30 s |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | pass | 66.81 s |
| `cargo test --locked --workspace` | 417 passed, 6 ignored | 77.07 s |
| app AWS-only clippy / tests | pass / 146 passed, 1 ignored | 31.52 s / 39.86 s |
| app local-only clippy / tests | pass / 148 passed, 1 ignored | 30.05 s / 47.84 s |
| UI typecheck / tests / build | pass / 68 passed / pass | 7.98 s / 2.13 s / 8.16 s |
| locked, offline FullLTO app build | pass; 42,803,088 B | 261.54 s |
| canonical signed FullLTO `.app` bundle | pass | 253.87 s |
| `verify-release-bundle.sh`; strict deep `codesign` | pass | included above |

The final branch build was byte-identical to the earlier FullLTO override candidate
(`sha256:92fccd5bd37e24c919367694dc6084dd44b9ad9a164200c27481f9c357636ac9`). `otool -L`
reported the same 24 system libraries/frameworks as ThinLTO. The six ignored workspace tests require external
ASR/VAD/diarization model fixtures; no test failed.

After delivery, remove this worktree's `target/` and `node_modules` only after confirming no Cargo/rustc/Node
process uses them; preserve the shared sccache.
