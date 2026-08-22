# ADR 0016 — Privacy-conscious local offline tracing

- **Status:** Proposed (2026-08-22; #122)
- **References:** [`vasovagal-tracing` architecture v1](https://github.com/vasovagal/vasovagal-tracing/blob/eebe5bbbba597b64dabd2d1981d18ba71bab9869/docs/architecture-v1.md), guardrails 3/5/9, ADRs 0007/0009/0012/0013

## Context

Corti's diagnostics console and rolling text log are useful for individual failures, but they do not provide
schema-stable span timing for comparing recording, transcription, live-window, filing, and durable-job phases
offline. Vagus needs the same facility. Neither tool should need a network collector, and tracing must not
weaken Corti's transcript/audio/credential privacy boundaries or expose another Settings control surface.

The shared public `vasovagal-tracing` crate owns activation, strict config parsing, secure local JSONL storage,
projection onto an immutable operation catalogue, rotation/retention, validation, and graceful flushing. It
deliberately does not install a subscriber or offer OTLP, sockets, uploads, arbitrary paths, arbitrary
attributes, or raw log export.

## Decision

1. **Compile-time boundary.** The app feature is `offline-tracing` and is included in official defaults. It
   enables the optional shared crate and fans out to empty `offline-tracing` instrumentation features in the
   selected AWS/local backend crates. `--no-default-features` omits the shared crate and every new callsite;
   Corti's existing diagnostics `tracing` dependencies remain independent. The integration branch pins the
   exact reviewed pushed revision `eebe5bbbba597b64dabd2d1981d18ba71bab9869`; no Git/path dependency may
   merge. Replace it with reviewed registry `vasovagal-tracing = "0.1.1"` and lockfile changes after the
   protected tokenless release exists.
2. **Runtime off by default and outside Settings.** Corti adds no Settings field, DTO member, command, or UI.
   The shared crate resolves only:

   ```text
   present VASOVAGAL_TRACE → corti.yaml → disabled
   ```

   An environment value is exactly lowercase `true` or `false`; any other/non-Unicode present value is
   invalid and disables without YAML fallback. The config path on Linux and macOS is
   `${XDG_CONFIG_HOME:-$HOME/.config}/vasovagal/corti.yaml`, with this exact ≤4 KiB one-document shape:

   ```yaml
   version: 1
   tracing:
     enabled: true
   ```

   Required native values, duplicate/unknown-field rejection, no aliases/tags/merges, and relative/empty XDG
   rejection are owned by the shared crate. Missing or invalid input fails closed and remains silent.
3. **Local secure output only.** Activated sessions write schema-v1 JSONL beneath
   `${XDG_STATE_HOME:-$HOME/.local/state}/vasovagal/traces/corti/`. The shared crate creates/verifies owned
   non-symlink `0700` directories and create-new/no-follow `0600` files, then applies bounded buffering,
   16 MiB/day rotation, 14-day/128-file/256 MiB retention, and partial-tail recovery. There is no path
   override or network mechanism. Storage failure is a no-op for Corti.
4. **Subscriber composition preserves diagnostics.** Tray and headless startup call `prepare`, compose its
   optional exact-target layer directly on `Registry`, and use `try_init`. The console, daily diagnostics file,
   and stderr each receive their own historical `RUST_LOG` → `CORTI_LOG` → `info` `EnvFilter` plus an exact
   `vasovagal::trace` exclusion. Those filters cannot enable/disable offline output or format schema events as
   logs. Subscriber conflict disables the offline session without panic or trace-file creation.
5. **Explicit lifetimes.** One guard owner keeps both the diagnostics `WorkerGuard` and shared `TraceGuard`
   alive across `app.run`. Headless dispatch closes `corti.cli`, drains the trace for up to two seconds, drops
   diagnostics, and only then calls `process::exit`. A crash/SIGKILL may omit the summary; complete JSONL lines
   remain independently valid. The crate installs no signal handler.
6. **Bounded, reviewed instrumentation.** Static catalogue operations cover CLI dispatch; recording queue;
   transcription AEC/audio decode/model load/channel/backend/diarization; checkpoint/cloud cleanup/Vagus
   filing/completion; live consume/window flush/note sync/finish; and durable retry/cleanup/retention. Worker
   dispatchers and explicit parent spans cross pipeline/live thread boundaries. Live aggregate spans are
   repeatedly entered only while processing chunks and are exited before blocking receives. There are no
   per-sample/frame/VAD-window/token/poll/wakeup/UI-refresh spans.
7. **Privacy boundary.** Attributes are restricted to reviewed enums, booleans, and bounded counts. Corti
   never supplies transcript/word/audio content, title, owning-app/bundle identity, recording/job IDs, paths,
   AWS bucket/key/profile/credentials, CLI arguments/results, raw errors, or unbounded labels. Errors map to
   fixed low-cardinality codes, with unknowns as `other`. Random session/trace/span/operation identifiers are
   generated by the shared crate; durable retries receive fresh IDs and only bounded attempt kind/count.

## Consequences

- Offline `jq`, DuckDB, Python, Polars, or SQLite analysis can compare useful phase timings without operating a
  collector or changing normal stdout/stderr/UI behavior.
- Runtime activation adds a bounded lossy writer thread and local files; disabled runtime retains only the
  compiled callsites. A compiled-out build carries neither the exporter nor those callsites.
- Schema v1 cannot accept new operation names or attributes. Future instrumentation requires schema v2 rather
  than forwarding arbitrary fields.
- Long-run CPU/RSS/trace-volume and real audio-deadline qualification still require a signed-bundle soak on
  representative calls; CI covers schema-valid headless output, diagnostics isolation, parentage, Settings
  shape, and default/tracing-only/compiled-out feature lanes on Rust 1.96 and latest stable.
