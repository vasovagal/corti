# 07 — Paid hosted-model post-processing

- **Status:** Implemented in v0.14, amended by native ChatGPT subscription transport (#130)
- **Decision ADR:** [ADR 0015](adr/0015-hosted-post-processing.md)
- **Tracking issues:** [#112 — hosted post-processing](https://github.com/vasovagal/corti/issues/112), [#130 — native ChatGPT subscription device auth](https://github.com/vasovagal/corti/issues/130)
- **Code baseline reviewed:** `cc6f75ae42aa3e5640efced4366f97e54d0432dc` (`v0.13.0`)
- **Architecture recon fan-out before this writer:** 2,404,707 tokens
- **Last provider-truth check:** 2026-08-21; links are in [Provider sources](#provider-sources)

This is the implementation contract for optional paid hosted-model processing **after** Corti's ASR. It
adds low-latency live cleanup, a stronger final rewrite, live questions, caching, and truthful per-call
telemetry without adding another transcription backend or a downloadable/local rewrite model.

Vertex ADC and direct OpenAI/Anthropic APIs are viable. ChatGPT subscription access is implemented without
an app-server: Corti follows the direct device authorization and fixed HTTP Responses pattern proven in
Dekopon, owns its rotating credential in Corti's private secret store, and exposes no tool surface. Anthropic explicitly
prohibits a third-party product from routing requests through a user's Claude Free/Pro/Max credentials. Corti
must keep direct API billing, included ChatGPT quota, and blocked Claude subscription access distinct.

> **#130 amendment:** references below to a conditional Codex app-server/process design are historical and
> superseded. The production catalog contains `chatgpt_subscription`, not `codex_app_server`; no Codex process,
> private home, stdio protocol, or imported credential participates.

## 1. Decisions at a glance

1. Hosted processing is an optional downstream layer. Raw ASR is immediate, immutable, and always the
   fallback. Audio never goes to a rewrite/question provider.
2. Preserve ADRs 0010/0012. A live Vagus note may still be created and receive crash-safe raw windows during
   the call. “Final before a Vagus note” means **before that note is finally published as
   `State: transcribed `**; batch notes still receive the final pass before `vagus add-note`. A literal
   “no note exists until final” interpretation would remove existing crash safety and requires a different
   product decision.
3. Add a runtime-free `corti-postprocess` domain crate and an async-edge
   `corti-postprocess-providers` adapter crate. Tauri coordination remains in `app`; only `corti-vagus`
   reads or rewrites the current recording's returned note path.
4. Four independent lanes share provider/catalog/auth/cache infrastructure: `live`, `final`,
   `ad_hoc_question`, and `pinned_question`. Auto lanes are single-flight with one newest pending snapshot;
   explicit ad-hoc questions are a bounded visible FIFO and are never silently coalesced.
5. Hosted settings are default-off and separately revisioned from the existing reload-between-jobs
   `AppConfig`. Disable takes effect in memory before persistence; enable requires a persisted egress
   acknowledgement and never follows merely from connecting a provider.
6. Every request and event carries a full generation fence. Cancellation is best effort; application is
   strict. Late text is discarded, while late terminal usage/cost is still recorded.
7. Exact local results and final recovery state are encrypted outside Vagus. A keyed canonical request
   digest includes every semantic input. A validated provider result is committed before it can alter the
   UI or note.
8. Vertex uses Application Default Credentials (ADC), not ordinary `gcloud auth login`. While unarmed, a
   dedicated manager resolves credentials every **exactly five seconds**. The first cache miss intending to
   dispatch in an unarmed episode emits exactly `gcloud token isn't armed`, then newest-state catch-up runs
   when armed.
9. Persist content-free call records in additive queue schema v2. Usage and costs remain nullable when the
   provider does not prove them. Subscription access is `included_subscription` with no dollar amount;
   unknown is never rendered as `$0.00`.
10. `gpt-5.6-luna` is an exact, verified OpenAI API model optimized for cost-sensitive/high-volume work. It
    is not a generic “Luna,” not local, and not presumed low-latency or available through Codex. It is not a
    default until Corti benchmarks it and the authenticated direct-OpenAI catalog reports it.

## 2. Reconciliation with the current code

The extension points below are based on code, not the older design snapshots.

| Current fact | Consequence for this design |
|---|---|
| `app/src/live.rs::consume_chunks` publishes only completed VAD regions and then accumulates them into a bounded durability window. `TranscriptPublisher::words` currently returns no ids, while `LiveTranscriptStore` assigns `seq` internally. | “Live” latency begins at phrase closure, not unstable ASR token arrival. Refactor publication to create one stable finalized-row envelope before the UI/store/coordinator fan-out; never add a second recognizer. |
| `app/src/live.rs::finish_session` flushes both ASR tails, writes the final raw window, and directly calls `corti_vagus::note::flip_state`. | The final hosted boundary is inserted after tail flush and before that flip. The ASR/model state can be dropped while the hosted coordinator works. |
| Live notes are lazily created on the first non-empty durability window and synced before their path is published. | Hosted work cannot delay or own note creation. During-call appends remain raw and crash-safe; accepted hosted text is published in one same-note rewrite before the final flip. |
| `LiveTranscriptStore` in `app/src/live_view.rs` retains at most 2,000 rows/about 1 MiB and events contain one append-only line. | It cannot assemble a full final request or Vertex catch-up. Add an encrypted session ledger and protocol v2; keep this UI store bounded. |
| `AppConfig` is cloned into live sessions and the serial pipeline; `PipelineMsg::ReloadConfig` applies between jobs. | It cannot satisfy changes during a call. Add managed `PostprocessControl` with monotonic revisions and patch commands; do not route hosted controls through the stale whole-form Settings save. |
| `transcribe_and_file` currently runs ASR, then `FilingCheckpoint::store`, then filing. | Batch/fallback final processing goes between successful ASR and the canonical v2 checkpoint. The checkpoint stores the text actually filed, so filing retries never repeat paid work. |
| `FilingCheckpoint` v1 is atomic JSON beside audio and holds transcript, note ownership, provenance, and AWS staging. | Loader accepts v1 and maps it to “no postprocess”; writer emits v2 with applied postprocess provenance. Pre-dispatch/final recovery lives in the encrypted store so an ambiguous paid call is not blindly repeated. |
| `corti-queue` schema version is 1; `recordings` stores only aggregate `transcribe_secs`. The pipeline thread owns its one WAL connection; Queue UI opens read-only. | Migrate transactionally to v2 with content-free `postprocess_calls` and a small recording projection. Terminal records arrive through a crash-safe outbox and are upserted only by the pipeline owner. |
| `corti-vagus::note` currently permits append, state flip, same-inode body/provenance rewrite, and call-site delete. | Add only an opaque current-note read/rewrite helper sufficient to replace this recording's transcript. Provider adapters never see a Vagus path. ADR 0015 and guardrail 1 bind the expansion. |
| The frontend subscribes before snapshot but currently accepts a revision jump instead of detecting a gap. | Protocol v2 includes `from_revision`; React keeps the last raw view and refetches immediately on a gap/process-epoch change. |
| The live reader exists only for eligible local detector calls. AWS and manual webinar paths are batch-only. | Hosted live cleanup/questions ride the existing local live ASR path only. All completed ASR paths can use the final lane; hosted processing does not make AWS/webinar transcription live. |
| Current utility windows are on-demand Tauri webviews; Live defaults to 760×620. | Reuse the window and activation lifecycle, enlarge Live for the split pane, and use a drawer below the responsive breakpoint. |

No production code in the reviewed baseline already performs hosted rewriting, stores provider credentials,
records LLM tokens/cost, or caches transcript rewrites.

## 3. Scope and non-goals

### Goals

- Low-latency cleanup of newly finalized live rows, with raw/clean/change views.
- A stronger all-or-nothing final pass before batch note creation or live-note final publication.
- Immediate master/lane toggles, model changes, word-bank changes, and steering changes during a call.
- Vertex ADC discovery/catch-up, direct API credentials, and native ChatGPT device authorization.
- Exact encrypted local caching plus privacy-gated provider prefix caching.
- Fine-grained call latency, normalized usage, cache source, and truthful cost history.
- A trainable unique-word bank in deterministic cache-stable prompts.
- One pinned auto-question and explicit ad-hoc questions over the live transcript.
- Accessible, restrained rewrite motion with a complete reduced-motion mode.
- Hermetic tests that cannot spend money or consume ambient credentials.

### Non-goals

- No audio egress to these providers.
- No change to ASR/VAD/AEC/diarization models and no new local LLM/GGUF/ONNX rewrite model.
- No speculative partial-word ASR and no whole-call in-memory transcript authority.
- No provider tools, web search, shell, file access, or agent actions. This is text transformation/Q&A only.
- No promise of HIPAA/BAA, residency, zero retention, or training opt-out until the selected provider/account
  configuration proves it.
- No exact reconciliation to a provider invoice. Direct-API dollars are versioned estimates from reported
  usage; subscription dollars are unavailable.
- No automatic repeat of a crash-ambiguous or already-streaming paid request.
- No Claude.ai Free/Pro/Max credential import or routing absent a written Anthropic agreement.

## 4. Provider truth and support tiers

Support tier is data returned by Rust and shown beside every provider/model. It is not inferred in React.

| Provider/transport | Tier and release posture | Authentication | Cache/cost behavior | Binding limitations |
|---|---|---|---|---|
| **Google Vertex direct API** | `documented`; ship-capable after project/region tests | ADC discovered by Google's auth library. Setup text is `gcloud auth application-default login`. Access tokens are memory-only. | Encrypted local exact cache. Provider context/implicit caching is separately disclosed. Use versioned provider/model/region tariffs and terminal usage. | A token proves neither project, quota project, IAM, API enablement, billing, quota, region, nor model availability. Ordinary `gcloud auth login` is not ADC. |
| **OpenAI direct API** | `documented`; ship-capable | Platform API key in Corti's private secret store or an approved workload identity. | Responses streaming/structured output; explicit stable-prefix caching when enabled; terminal cached/read/write/reasoning usage retained. | This path is usage-billed and separate from ChatGPT subscription access. |
| **ChatGPT subscription (direct fixed endpoints)** | `experimental`; ship-capable with the private-endpoint limitation visible | Corti-owned OpenAI device authorization; the versioned access/rotating-refresh document lives in Corti's private secret store. | Authenticated `/backend-api/codex/models` catalog; streaming `/backend-api/codex/responses`; reported tokens retained; dollar cost is null with `included_subscription`; provider-cache controls are unavailable. | No Codex server or tools. Endpoints are fixed and provider-controlled; quota/model availability can change. Corti never imports Codex, Pi, or Dekopon credentials. |
| **Anthropic direct API** | `documented`; ship-capable if API billing is acceptable | Anthropic API key in Corti's private secret store or supported workload identity. | Encrypted local exact cache and explicit provider cache blocks only when enabled; terminal usage + audited tariff estimate. | This is API billing, not a Claude.ai subscription. |
| **Amazon Bedrock (`ConverseStream`)** | `documented`; ship-capable | Resolved AWS credentials in any of five flavors: default chain, named profile, static key pair in Corti's private secret store, assumed role, or IAM Identity Center (SSO). Requests are SigV4-signed. | Encrypted local exact cache. Explicit provider caching is not offered: `ListFoundationModels` does not disclose which models honour Converse cache points. Terminal usage from the trailing `metadata` frame. | Regional — the catalog and model availability differ per region. `ListFoundationModels` publishes no token limits, so the catalog declares conservative floors. Structured output rides forced tool use, not a JSON-schema response format. |
| **Claude subscription (Free/Pro/Max)** | `blocked`; descriptor only, no adapter or setup command | None in Corti. | None. | Anthropic states that third-party developers must use API keys/cloud providers and may not offer Claude.ai login or route Free/Pro/Max credentials. Written commercial permission is required before this row changes. |

A provider connection **never** turns on transcript egress. Connecting, selecting, and enabling are three
separate actions.

### 4.1 Model catalog and selection policy

`ModelCatalog` is the intersection of:

1. the authenticated account/project/region catalog (ChatGPT uses the fixed authenticated `/backend-api/codex/models` endpoint; direct adapters use a
   provider listing endpoint where authoritative, otherwise a versioned Corti allowlist plus a cheap
   availability probe);
2. adapter capabilities: text input/output, streaming, structured row output, context size, cache modes;
3. the selected billing/retention policy; and
4. a versioned Corti benchmark manifest for lane suitability.

A `ModelDescriptor` contains provider, transport, support tier, exact model/snapshot id, account-scoped
availability, region, context/output limits, streaming/structured-output/cache capabilities, billing basis,
tariff provenance, deprecation, and optional measured Corti profiles. Rust revalidates every selection at
request time. There is no silent provider/model substitution.

Auto-live eligibility requires a representative Corti benchmark with p95 time-to-first-text ≤1.5 s, p95
complete cleanup ≤4.0 s, valid structured output, and no cleanup-quality regression on the frozen corpus.
Question recommendations use the same interactive profile. Final recommendations select the best measured
quality that fits the final deadline and context/chunk policy. Unbenchmarked models can be shown with
`Unbenchmarked for Corti`; they are disabled for automatic live use and are never defaulted.

OpenAI's current model page verifies exact id `gpt-5.6-luna`, text output, streaming, structured outputs,
prompt caching, a large context, and cost-sensitive/high-volume positioning. It does **not** verify Corti
TTFT, rewrite quality, cancellation waste, or Codex-catalog availability. Show Luna only in a direct-OpenAI
catalog that reports it, with the unbenchmarked label until the release gate passes.

## 5. Crates, ownership, and threads

### 5.1 Dependency diagram

```text
corti-core ─────────────────────────────────────────────────────────────┐
                                                                       │
corti-postprocess (new, runtime-free)                                  │
  domain ids/fences/requests/results/events/errors                     │
  prompt + word-bank canonicalization                                 │
  pure schedulers, validators, cache keys, pricing                    │
       ▲                                                               │
       │                                                               │
corti-postprocess-providers (new, async edge)                          │
  OpenAI / ChatGPT / Anthropic / Vertex / Bedrock adapters            │
  credential/catalog normalization; injected HTTP/clock               │
       ▲                                                               │
       │                                                               │
app (Tauri integration) ───────────► corti-queue (schema v2/history)   │
  PostprocessControl                  ▲                                │
  PostprocessCoordinator              │ sole-writer PipelineMsg         │
  auth/keychain/store/word-bank       │                                │
  live + pipeline integration         └──────── app telemetry outbox    │
       │
       └───────────────────────────► corti-vagus
                                     only current-note read/rewrite/file
```

`corti-postprocess` may depend on small serialization/crypto-hash crates and `corti-core`, but not Tokio,
Tauri, HTTP, the secret store, SQLite, or Vagus. Provider adapters are blocking from the coordinator's perspective;
each adapter hides its private current-thread Tokio runtime and injected transport.

### 5.2 Core API shape

Names are normative; exact module placement may vary without changing behavior.

```rust
pub enum Lane { Live, Final, AdHocQuestion, PinnedQuestion }
pub enum SupportTier { Documented, Experimental, Blocked }
pub enum BillingBasis {
    MeteredEstimate,
    IncludedSubscription,
    NoProviderRequest,
    Unknown,
}

pub struct RequestFence {
    pub process_epoch: ProcessEpoch,
    pub session_generation: u64,
    pub transcript_revision: u64,
    pub control_revision: u64,
    pub lane_revision: u64,
    pub steering_revision: u64,
    pub bank_revision: u64,
    pub question_revision: Option<u64>,
}

pub struct HostedRequest {
    pub call_id: CallId,
    pub group_id: RequestGroupId,
    pub lane: Lane,
    pub fence: RequestFence,
    pub provider: ProviderId,
    pub transport: TransportId,
    pub model: ModelId,
    pub targets: Vec<RewriteTarget>,
    pub context: Vec<ContextRow>,
    pub prompt: CanonicalPrompt,
    pub deadline: MonotonicDeadline,
    pub cache_policy: CachePolicy,
}

pub trait ProviderAdapter: Send {
    fn descriptor(&self) -> ProviderDescriptor;
    fn catalog(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError>;
    fn execute(
        &mut self,
        request: &HostedRequest,
        cancel: &CancellationToken,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, PostprocessError>;
}

pub trait CredentialSource: Send { fn resolve(&mut self) -> CredentialState; }
pub trait ExactCache: Send { fn lookup(...); fn commit_validated(...); }
pub trait PricingCatalog: Send + Sync { fn estimate(...) -> Option<CostEstimate>; }
pub trait Clock: Send + Sync { /* wall + monotonic */ }
pub trait IdSource: Send + Sync { /* process/session/call ids */ }
```

`ProviderEvent` variants are `Queued`, `AuthWaiting`, `DispatchStarted`, `Headers`, `FirstText`,
`TextDelta`, `UsageProvisional`, `CacheObserved`, `Completed`, `Canceled`, and `Failed`. Every event carries
call id, lane, target/group id, and the complete fence. Provider bodies and secrets are never event fields.

### 5.3 Process topology

```text
CoreAudio/HAL -> capture writer -> bounded tee -> corti-live (unchanged hot path)
                                              | raw row publishes immediately
                                              | try_send finalized row envelope
                                              v
                                 corti-hosted-control (one std thread)
                                   | pure lane scheduling/fences
                 ┌─────────────────┼───────────────────┐
                 v                 v                   v
       provider workers      auth managers      corti-hosted-store
       max 4 globally        one/provider       one SQLite owner
       max 2/provider        Vertex 5 s poll    encrypted cache/ledger
       blocking HTTP         ChatGPT device     final journal/outbox
                 |                 |                   |
                 └──── normalized events/results ─────┘
                                   |
                    protocol-v2 live event projection
                                   |
                              React webview

corti-pipeline (sole queue.db writer)
  ASR -> request final over coordinator -> wait on response channel only
      -> FilingCheckpoint v2 -> Vagus -> Done
  periodically imports content-free telemetry outbox and acknowledges it
```

- Coordinator command queue: bounded 256. `corti-live` uses `try_send`; saturation marks hosted rows
  skipped and preserves raw. The serial pipeline may block while sending/waiting for final, but it performs
  no network, auth polling, cache SQLite, or provider runtime work.
- Provider event queue: bounded 256. Text deltas are coalesced to at most 30 UI updates/s and 8 KiB/event.
  Provider workers may block off the hot path rather than allocate without bound.
- Worker policy: maximum four provider calls globally, two per provider. Keep one interactive slot for
  live/questions; final chunking uses at most two calls and cannot consume every slot. Live > ad hoc > pinned
  > final priority, with a final request promoted after two seconds so continuous speech cannot starve it.
- Capture/HAL/writer and ASR publication never await hosted work. There is no Tokio dependency in their
  crates.

## 6. Configuration and at-any-time control

### 6.1 Files and secrets

- Existing `~/.local/share/corti/config.toml` remains the transcription/AEC/storage document.
- New `~/.local/share/corti/hosted.toml` contains only non-secret hosted defaults and disclosure versions.
  It is atomically replaced, synced, and mode 0600.
- New `~/.local/share/corti/word-bank.json` contains `WordBankDocument`; it is atomically replaced, synced,
  and mode 0600.
- API keys, cache keys, and Corti's ChatGPT rotating credential document are separate owner-only files
  (mode 0600 in a mode-0700 directory) under `~/.local/share/corti/hosted-secrets/`; the macOS Keychain is
  unusable for an ad-hoc-signed app (ADR 0015 amendment, 2026-08-26). They never enter TOML, JSON DTOs,
  React, SQLite, logs, Vagus, screenshots, or subprocess arguments.
- Vertex tokens stay in memory. Corti uses ADC but never copies its refresh credential into Corti storage.
- ChatGPT access/refresh tokens are Corti-owned; other applications' credential files are never read.

`HostedPreferences` contains: master/live/final/question defaults; provider/model selection per lane;
provider scopes (non-secret alias/project/region/quota-project); default steering; the one saved pinned-question
template and auto-run acknowledgement; local/provider cache policy; final deadline; egress acknowledgement
version; `show_history_diagnostics`; and `show_live_metrics_by_default`.

The saved pinned text is a template reused between calls. Answers and ad-hoc history are session-only.

### 6.2 Managed runtime state

`PostprocessControl` is seeded from `HostedPreferences` but is not a mutable `AppConfig` clone. It owns one
immutable `ControlSnapshot` behind an atomic/lock and these monotonic values:

```text
control_revision       master/provider-wide behavior
live_lane_revision     live model/toggle/policy
final_lane_revision    final model/toggle/policy
question_lane_revision question model/toggle/policy
steering_revision      effective default/session steering
bank_revision          canonical word-bank content
question_revision      pinned/ad-hoc identity
session_generation     recording identity/restart
```

Patch commands include the caller's observed control revision. Rust validates, persists, returns the
canonical new snapshot, and emits one `hosted-state-changed` event so Settings and Live windows converge.
A conflict returns current state; React rebases only the user's changed field.

Behavior:

- **Disable master/lane:** publish the disabled runtime snapshot first, increment revisions, cancel affected
  calls, reject late application, then persist. If persistence fails, runtime remains safely disabled and UI
  says `Off for this session · could not save`.
- **Enable:** validate provider/model and disclosure, persist first, then publish enabled state. Failure means
  no egress.
- **Model/provider/cache/word-bank/default-steering change:** persist, increment only affected revisions,
  best-effort cancel, and apply to the next request.
- **Live steering override:** session-memory only, button text `Apply to next request`; persists only if the
  user separately chooses `Use as default`.
- **Display preference/Live Details override:** never changes a request fence and never cancels paid work.
- A change after a final dispatch discards old output. Starting a replacement final call requires the
  explicit action `Apply and rerun final (may bill twice)`; otherwise Corti publishes the safe fallback.

## 7. Scheduling and state machines

### 7.1 Common request state

```text
Idle
  -> Debouncing (auto lanes only)
  -> Queued
  -> LocalCacheLookup
       -> CacheHit -> Validate -> Apply
       -> Miss
  -> AwaitingCredential / Arming
  -> Dispatching
  -> Streaming
  -> Validating
  -> RecoveryCommit (mandatory encrypted journal; reusable cache if enabled)
  -> Apply
  -> Completed

Any nonterminal state -> Superseded | Canceled | Deadline | Failed
Late provider terminal -> Telemetry only; never Apply
```

Local lookup happens before credential resolution. A valid local hit sends no request and therefore does not
raise an unarmed Vertex warning. Cache corruption is evicted and treated as a miss. If the encryption key is
unavailable, hosted dispatch fails closed rather than silently writing plaintext or losing final recovery.

Automatic retries are deliberately narrow: at most one retry when the injected transport proves zero request
body bytes were sent, or after a refreshable credential fails before model execution. Once request bytes or
response bytes may have reached a provider, the same attempt is not automatically repeated. A later live
revision is a new call. Canceling cannot promise remote compute or billing stopped.

### 7.2 Live cleanup lane

- Trigger: one or more VAD-closed rows from `consume_chunks`; never open/unstable words.
- Quiet debounce: 150 ms. A request targets at most 8 new rows and 4 KiB UTF-8; excess remains newest pending.
- Context: up to 8 preceding rows, latest validated clean text where available, otherwise raw.
- Single-flight per session. While in flight, replace one pending snapshot with the newest fenced target set.
- Deadlines: first text within 2 s and terminal result within 5 s of dispatch. Missing either cancels/discards.
- Stable row identity is assigned before fan-out: `publish_words` returns finalized view-row envelopes to the
  bounded UI store, coordinator, and ledger. The ledger separately records the canonical durability rows
  produced by `flush_window` after optional far-end diarization. This matters because current immediate
  `Them` rows can later split into `Them N` segments.
- Accepted output updates only transient clean view rows and the encrypted ledger. During-call Vagus appends
  remain canonical raw/diarized rows, synced, and nonblocking.
- At finish, a live clean replacement is eligible for the safe note assembly only when its stable word-span
  target still maps exactly to one canonical durability row; a row split/merged by diarization falls back to
  raw. If no strong final applies, exact clean mappings plus raw for every uncovered row are rewritten once
  before the state flip. If no eligible clean row exists, the existing raw body is simply flipped.
- A saturated coordinator, disabled lane, auth wait, timeout, malformed output, stale fence, or provider error
  leaves that row visibly raw. No raw text is deleted.

### 7.3 Final lane and crash state

Final uses a stronger independently selected paid model. Input is the complete typed transcript: batch ASR
segments directly, or the encrypted ledger's canonical post-diarization durability rows after both tails
flush—not the differently grouped immediate view rows. A ledger ingress drop marks it incomplete and disables
the live final pass; Corti does not pretend the bounded UI store is complete.

Oversized transcripts are deterministically split at row boundaries. Each chunk has read-only two-row
boundary context, explicit target ids, the prompt/chunker versions in its key, and no overlapping targets.
At most two chunks run concurrently. All chunks must validate and be recovery-committed before any text is
published; one failure cancels peers best effort and applies none.

The v1 final budget is 90 s total including auth wait, queueing, and all chunks; each dispatched chunk has a
45 s cap. Benchmarking can revise these versioned policy constants, not silently change a running request.
On disable, deadline, unavailable auth, error, malformed output, or app exit, file the safe live-clean/raw
assembly. The UI says why and the raw transcript remains available.

The encrypted `final_attempts` journal is the billing/recovery authority:

```text
Prepared (no dispatch) -> Dispatched -> ResultCached -> Applied -> Checkpointed
       |                     |              |
 crash: safe to retry        |              +-> recover from exact cache; no provider call
                             +-> no terminal cache = Ambiguous; never auto-repeat

Prepared/Dispatched -> Abandoned (disable/deadline) -> fallback checkpoint
```

For batch/fallback, successful ASR is followed by final processing, then `FilingCheckpoint` v2 stores the
actual transcript/provenance, then status advances to `PendingNote`. Filing retries consume that checkpoint
and never call a hosted provider. On crash recovery:

- `Prepared` with no dispatch may safely resume if still enabled and inside policy.
- `Dispatched` plus a valid exact result resumes from cache.
- `Dispatched` without a terminal result is `Ambiguous`; build/file a fallback checkpoint and expose an
  explicit confirmed `Retry hosted final…` action. Ordinary Queue Retry never creates a second paid call.

For live notes, the note remains `State: transcribing` during the bounded final wait. `corti-vagus` writes the
accepted final or safe mixed body with the in-progress state, syncs it, then performs the existing same-width
state flip and sync. A crash is handled by the existing `Recording` startup reaper plus the final journal.

### 7.4 Credential and Vertex state

Every provider projects a secret-free credential state to UI/events:

```text
Absent -> Resolving -> Ready(expires/source)
                    -> AwaitingUser / DeviceAuthorization (broker-owned flows)
Ready -> Refreshing -> Ready
Any -> Rejected | Unsupported | SanitizedError
```

API-key adapters do not invent refresh; a 401 moves to `Rejected`. Device authorization exposes only the
intended verification URL/code and login id. A blocked provider is always `Unsupported` with a policy reason
and has no connect command.

Vertex specializes the state machine and episode behavior:

```text
Absent/Unarmed(episode N)
  -> Resolving
       -> Ready(lease, expiry)
       -> Unarmed(episode N, next_poll = last_attempt + 5.000 s)
Ready -> Refreshing -> Ready
Ready/Refreshing -> Unarmed(episode N+1) on loss/revocation
Any -> Error/Rejected (sanitized, actionable)
```

The manager is off capture, ASR, and pipeline threads. While unarmed it has one resolver attempt in flight and
schedules attempts on an injected monotonic cadence at exactly five-second boundaries (no overlapping polls).
At 4.999 s no second attempt exists; advancing 1 ms starts it.

On the first local-cache miss that intends to dispatch during an unarmed episode:

1. emit one `role=alert` notice whose entire visible message is exactly **`gcloud token isn't armed`**;
2. set affected lane state to `Arming`;
3. retain only the newest fenced pending snapshot for each auto lane; and
4. continue five-second resolution.

On successful resolution, emit `Catching up`, run one still-valid newest snapshot per auto lane, then `Ready`.
Do not replay every missed transcript revision. Ad-hoc questions remain their explicit bounded FIFO and show
`Waiting for gcloud`; they are not silently dropped. A final pending snapshot expires with its final deadline.
Token readiness remains separate from `Service error` for project/IAM/API/billing/quota/region/model failures.

### 7.5 Questions

- **Ad hoc:** explicit submit snapshots the newest clean-or-raw ledger revision. One runs at a time; FIFO cap
  8. Every item remains visible with queued/as-of/cancel/error/usage state. Deadline 30 s. No coalescing.
- **Pinned:** exactly one saved question template. React debounces edits for 500 ms, but Rust owns revision,
  cancellation, progress, and execution. Empty text clears it.
- Meaningful progress is at least **40 newly finalized Unicode word tokens or 30 newly covered speech
  seconds**, with at least one new row, since the last accepted pinned answer/request watermark. Once reached,
  Rust waits a 750 ms quiet period. Progress during an in-flight call sets one dirty bit; at completion at most
  one newest-watermark rerun is scheduled.
- Turning on automatic pinned evaluation requires acknowledgement that it can make repeated paid calls. The
  card shows run count and known/unknown session estimate.
- Question context is capped at the newest 16,000 input tokens (or the smaller catalog limit after prompt and
  output reserve). The answer says `Earlier transcript omitted` and shows its as-of revision when truncated.
- Answers/thread bodies remain in memory: pinned answer plus at most 20 ad-hoc exchanges/256 KiB. They never
  enter Vagus, queue telemetry, logs, screenshots, or provenance. Durable Q&A caching is off by default.

## 8. Prompt, output, and word-bank contracts

### 8.1 Stable prompt layout

Canonical prompt bytes are versioned and serialized in this order:

```text
[developer: immutable Corti rewrite/question policy]
[developer: output JSON schema + examples]
[developer: canonical unique-word bank]       <-- stable prefix/cache breakpoint
[user: effective steering]
[user: immutable context rows]
[user: target rows or question]
```

No timestamp, call id, random id, current date, config revision, or account name appears before the stable
prefix boundary. Row ids and transcript/question content are the dynamic suffix. Every adapter—including
ChatGPT subscription—sends no tools, shell, file access, web search, or agent actions.

Steering is quoted as untrusted user policy, not concatenated into system syntax. Transcript and word-bank
content are untrusted data. The output validator, not the prompt, is the security boundary.

### 8.2 Rewrite schema

```json
{
  "schema": 1,
  "replacements": [
    { "row_id": "r-000042", "text": "Corrected text only." }
  ]
}
```

A replacement may omit unchanged rows. It may not add an unknown/duplicate id, reorder rows, change
speaker/timestamps, emit markup, or return invalid UTF-8/control characters. Limits per target: non-empty
unless raw was empty, at most `max(raw_bytes * 3 / 2, raw_bytes + 256)`, and aggregate output within catalog
and request bounds. Final chunk target sets must be exact/disjoint. Validation failure applies nothing.

Questions use a separate schema `{schema, answer, cited_row_ids, context_truncated}`. Citations must reference
provided rows; no citation is treated as grounded proof.

### 8.3 `WordBankDocument`

```json
{
  "schema": 1,
  "revision": 17,
  "entries": ["Argo CD", "Dekopon", "Vagus"],
  "content_digest": "..."
}
```

Rust is authoritative:

- Unicode NFC, trim, collapse internal Unicode whitespace to one ASCII space.
- Reject newline/control/bidi-control injection, entries over 128 Unicode scalar values, more than 5,000
  entries, or a canonical document over 256 KiB.
- Deduplicate by Unicode case-folded key. An existing display spelling wins until explicitly edited.
- Sort prompt/document entries by folded key then UTF-8 display bytes; UI insertion order never affects cache.
- Increment revision only when canonical entries change. Digest is over canonical JSON; externally recorded
  fingerprints are HMAC-derived so a note/telemetry database does not expose a dictionary-testable hash.
- UI supports add, bulk paste, search, edit/remove, and clear confirmation. `Remember spelling` is offered on
  an accepted clean insertion. Corti never auto-learns provider output.

Any word-bank content change invalidates affected request keys and fences in-flight results.

## 9. Exact local/provider caching

### 9.1 Storage and cryptography

Authoritative storage is `~/Library/Caches/corti/postprocess/cache.sqlite3` in WAL mode, owned by one
`corti-hosted-store` thread. Content tables contain only ciphertext plus low-sensitivity metadata.

- A 256-bit random master key is generated locally and stored as one private file (§6.1).
- HKDF-SHA-256 derives distinct `digest-v1`, `cache-aead-v1`, `ledger-aead-v1`, and
  `provenance-fingerprint-v1` keys.
- XChaCha20-Poly1305 uses a random 192-bit nonce per row and authenticates schema/table/key metadata as AAD.
- Request identities are HMAC-SHA-256, not plaintext hashes. File names and SQLite indexes contain no
  transcript, prompt, question, word, steering, account id, or token.
- A mandatory short-lived encrypted recovery entry is written for every validated final result even when
  reusable caching is disabled. Reusable caching controls later hits, not crash correctness.
- Default reusable rewrite cache: enabled, 30-day TTL, 512 MiB LRU cap. Q&A: memory-only; optional durable
  cache requires a separate acknowledgement, 24-hour TTL, and shares the cap.
- `Purge hosted text` closes the store, removes DB/WAL/SHM, and recreates it. `Rotate encryption key` purges,
  deletes the stored key, and generates a new one. APFS/SSD secure erasure is not promised; destroying the
  key makes surviving ciphertext unusable.

The session ledger is append-on-disk and call-memory-bounded. It stores encrypted immediate view rows,
canonical post-diarization durability rows, stable word-span mappings, and accepted clean versions independent
of `LiveTranscriptStore`. It is deleted after `Done` plus outbox acknowledgement;
failed/recoverable sessions expire with recording retention. A per-session 16 MiB plaintext-equivalent cap
marks hosted final unavailable rather than growing without bound.

### 9.2 Canonical exact-cache key

```text
HMAC-SHA256(K_digest,
  "corti-postprocess-key-v1\0" || canonical_cbor({
    provider_id, transport_id, support_tier,
    connection_scope_uuid, region,
    exact_model_id, adapter_version,
    prompt_template_version, output_schema_version, chunker_version,
    lane, billing/cache policy,
    word_bank_canonical_digest, steering_canonical_digest,
    targets[{row_id, speaker, start_ms, end_ms, raw_or_clean_text}],
    context[{row_id, speaker, start_ms, end_ms, raw_or_clean_text}],
    question_if_any
  }))
)
```

`connection_scope_uuid` is a Corti-local opaque id for one provider account/project configuration; raw
account/project ids are not keys or telemetry. Milliseconds are deterministically rounded. Canonical CBOR
uses fixed field order, normalized strings, and integer times.

| Change | Local result key | Provider stable-prefix key | Fence/cancel |
|---|---:|---:|---:|
| Transcript/context/target/question text | invalidates | no (dynamic suffix) | yes |
| Provider, transport, account scope, region, exact model | invalidates | invalidates | yes |
| Prompt/output/chunker/adapter version | invalidates | invalidates | yes |
| Word-bank canonical content | invalidates | invalidates | yes |
| Effective steering | invalidates | no (dynamic suffix) | yes |
| Provider cache mode/retention policy | invalidates | invalidates | yes |
| Session/call id, wall time, diagnostics visibility | excluded | excluded | no |
| Toggle off/on with otherwise identical semantics | result remains reusable | remains reusable | current work canceled |
| Pricing catalog update | result remains reusable | remains reusable | no |

A provider cache key is a base64url HMAC over stable provider/model/account/prompt/bank/cache semantics; it
never embeds a readable word bank or session id.

### 9.3 Provider caching policy

Local exact caching is checked first and is always the source of truth. Provider prefix caching has a
separate per-provider control because it may retain transcript-adjacent words remotely.

- OpenAI: when off, request explicit-only mode with no breakpoint where supported, avoiding an implicit
  changing-suffix write. When on, one explicit breakpoint follows the stable prefix and reported read/write
  tokens are retained.
- Anthropic: omit explicit cache controls when off; mark only the stable prefix when on.
- Vertex: expose explicit/implicit context-cache behavior and account for reported hits. If an API/model has
  unavoidable implicit caching, say so; do not label it disabled.
- ChatGPT subscription: the private endpoint/model owns cache behavior; show
  `Provider cache policy unavailable` and retain only provider-reported observed cached tokens.

Provider-specific minimum prefix sizes, write charges, TTLs, and account eligibility come from the audited
model descriptor. Corti does not pad a sensitive prompt merely to cross a cache threshold and never promises
a hit.

Provider caching defaults off until a separate provider-retention acknowledgement. It may be enabled only
for an account/workload the user declares eligible. Corti purge cannot purge provider-side cache; UI says so.

## 10. Persistence and migrations

### 10.1 Queue schema v1 → v2

Migration is a rerunnable transaction before the normal schema create/stamp. It probes
`pragma_table_info('recordings')` before each `ALTER TABLE`, then creates tables/indexes and stamps version 2
in one transaction. Enable foreign keys on every writer/read connection. Existing v1 rows require no backfill.

```sql
ALTER TABLE recordings ADD COLUMN postprocess_state TEXT;
ALTER TABLE recordings ADD COLUMN postprocess_updated_at TEXT;

CREATE TABLE IF NOT EXISTS postprocess_calls (
  call_id                  TEXT PRIMARY KEY,
  recording_id             TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE,
  request_group_id         TEXT NOT NULL,
  target_id                TEXT,
  lane                     TEXT NOT NULL,
  attempt_no               INTEGER NOT NULL,
  provider_id              TEXT NOT NULL,
  transport_id             TEXT NOT NULL,
  support_tier             TEXT NOT NULL,
  model_id                 TEXT NOT NULL,
  adapter_version          INTEGER NOT NULL,
  prompt_version           INTEGER NOT NULL,
  output_schema_version    INTEGER NOT NULL,
  session_generation       INTEGER NOT NULL,
  transcript_revision      INTEGER NOT NULL,
  control_revision         INTEGER NOT NULL,
  steering_revision        INTEGER NOT NULL,
  bank_revision            INTEGER NOT NULL,
  question_revision        INTEGER,
  outcome                  TEXT NOT NULL,
  error_code               TEXT,
  cache_source             TEXT NOT NULL,
  provider_request_sent    INTEGER NOT NULL,
  usage_complete           INTEGER NOT NULL,
  input_tokens             INTEGER,
  output_tokens            INTEGER,
  cached_read_tokens       INTEGER,
  cached_write_tokens      INTEGER,
  reasoning_tokens         INTEGER,
  cost_micros              INTEGER,
  currency                 TEXT,
  billing_basis            TEXT NOT NULL,
  pricing_catalog_version  TEXT,
  tariff_id                TEXT,
  tariff_effective_at      TEXT,
  queued_at                TEXT NOT NULL,
  dispatched_at            TEXT,
  completed_at             TEXT,
  queue_us                 INTEGER,
  auth_us                  INTEGER,
  cache_lookup_us          INTEGER,
  connect_us               INTEGER,
  ttfb_us                  INTEGER,
  ttft_us                  INTEGER,
  stream_us                INTEGER,
  parse_us                 INTEGER,
  cache_commit_us          INTEGER,
  total_us                 INTEGER,
  created_at               TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_postprocess_recording_lane_time
  ON postprocess_calls(recording_id, lane, queued_at);
CREATE INDEX IF NOT EXISTS idx_postprocess_time
  ON postprocess_calls(queued_at);
PRAGMA user_version = 2;
```

No prompt, transcript, replacement, diff, steering text, word-bank entry, question, answer, credential,
account/project id, provider response body, or provider error body has a column. `error_code` uses only the
sanitized taxonomy in §13.

`recordings.status` keeps the existing `JobStatus` grammar for downgrade safety. The transient/durable
projection `postprocess_state` supplies `awaiting_auth`, `dispatching`, `finalizing`, `fallback`, or
`complete`; Queue UI renders `Post-processing` while appropriate. Startup reconciliation clears stale
projection state from the final journal. This avoids making an older binary unable to parse a new status.

Terminal call records first enter `telemetry_outbox` in the cache store. The coordinator sends/imports them
through `PipelineMsg`; the sole pipeline writer `INSERT ... ON CONFLICT(call_id) DO UPDATE`, then acknowledges
the outbox row. If a recording row does not exist yet, import remains pending. Read-only Queue commands return
separate live/final/question aggregates and optional details.

Call rows are retained with their recording history (currently at least 90 days) and cascade on row GC.

### 10.2 Encrypted store schema

```sql
CREATE TABLE cache_entries (
  key_hmac BLOB PRIMARY KEY,
  kind TEXT NOT NULL,
  provider_id TEXT NOT NULL,
  model_id TEXT NOT NULL,
  lane TEXT NOT NULL,
  nonce BLOB NOT NULL,
  ciphertext BLOB NOT NULL,
  created_at TEXT NOT NULL,
  last_accessed_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  stored_bytes INTEGER NOT NULL
);
CREATE TABLE session_rows (
  session_id TEXT NOT NULL,
  row_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  nonce BLOB NOT NULL,
  ciphertext BLOB NOT NULL,
  PRIMARY KEY(session_id, row_id)
);
CREATE TABLE session_state (
  session_id TEXT PRIMARY KEY,
  generation INTEGER NOT NULL,
  complete INTEGER NOT NULL,
  ledger_incomplete INTEGER NOT NULL,
  plaintext_bytes INTEGER NOT NULL,
  expires_at TEXT NOT NULL
);
CREATE TABLE final_attempts (
  recording_id TEXT PRIMARY KEY,
  call_id TEXT NOT NULL,
  request_key_hmac BLOB NOT NULL,
  fence_nonce BLOB NOT NULL,
  fence_ciphertext BLOB NOT NULL,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE telemetry_outbox (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  call_id TEXT NOT NULL UNIQUE,
  payload_json BLOB NOT NULL,
  created_at TEXT NOT NULL
);
```

The outbox payload is content-free call metadata; encrypting it is optional but it must contain no free-form
provider body/error. One store owner performs TTL/LRU/purge/rotation and batches `last_accessed_at` updates.

### 10.3 Filing checkpoint and provenance

`FilingCheckpoint` writer moves to version 2; loader accepts v1. Add:

```text
applied_postprocess: AppliedPostprocessProvenance (serde default = none)
source_transcript_fingerprint: optional keyed fingerprint
final_attempt_call_ids: bounded list/digest, content-free
```

`transcript` remains the exact text to file. A v2 checkpoint is created only after the final outcome is
settled, so every filing retry is free of hosted inference.

Corti note provenance moves to schema 2 with an optional `postprocess` object describing the **applied** text:

```json
{
  "state": "none|live|live_mixed|final",
  "provider": "openai",
  "transport": "openai_api",
  "support_tier": "documented",
  "model": "exact-model-id",
  "adapter_version": 1,
  "prompt_version": 1,
  "output_schema_version": 1,
  "word_bank": {"revision": 17, "fingerprint": "...", "count": 3},
  "steering_fingerprint": "...",
  "cache_source": "local|provider|network|mixed",
  "live_revision_summary": {"count": 2, "fingerprint": "..."},
  "final_outcome": "applied|disabled|timeout|failed|ambiguous"
}
```

Omit provider/model fields when no hosted text was applied. Questions, prompts, token/cost, account/project,
sensitive region/tenancy, steering text, word-bank entries, and errors stay out. Mixed live output records a
bounded keyed summary, not unbounded per-row metadata. Batch checkpoint retries preserve this exact object;
never rebuild it from current Settings.

## 11. Usage, latency, and cost accounting

### 11.1 Normalized usage

Terminal provider usage maps to nullable nonnegative fields:

- input tokens;
- output tokens;
- cached-read input tokens;
- cached-write input tokens;
- reasoning tokens; and
- `usage_complete`.

Do not derive terminal “actual” usage from UTF-8 length. A client tokenizer may drive a clearly provisional
live estimate, but provisional values are event-only. Canceled/disconnected calls without terminal usage are
unknown. Late terminal usage is persisted even when its fenced content was discarded.

A local exact hit creates a call row with `provider_request_sent=false`, `billing_basis=no_provider_request`,
and nullable incremental tokens/cost. UI text is `Local cache · no provider request`, not `$0.00`.

### 11.2 Timing definitions

All phase durations are monotonic microseconds; wall timestamps are canonical UTC for ordering.

```text
queue       schedule -> provider worker selected
cache       cache lookup start -> hit/miss/error
auth        credential wait/refresh only
connect     dispatch start -> response headers
TTFB        dispatch start -> first response byte/headers
TTFT        dispatch start -> first accepted text delta
stream      first text -> terminal frame
parse       terminal text -> validated typed result
cacheCommit validation -> mandatory recovery/cache commit
total       schedule -> terminal completion/failure
```

A phase not observed is null, not zero. Live Details shows per-call and rolling p50/p95 only from like lanes;
history aggregates do not combine live and final latency.

### 11.3 Tariffs and truthful rendering

`PricingCatalog` is a reviewed, versioned data file with source URL, retrieval date, provider/model, region,
tier/context rules, token classes, currency, and effective interval. Arithmetic uses integer micros and
checked integer operations. The model/region/tier/effective-date row must match the dispatch timestamp; absent
or stale data yields unknown.

- Direct API: `metered_estimate`, nullable integer micros, shown as `Estimated $…` with tariff version.
- ChatGPT subscription: `included_subscription`, `cost_micros=NULL`, shown exactly
  `Included subscription · cost unavailable`.
- Missing/incomplete tariff or usage: `unknown`, null, shown exactly `Cost unavailable`.
- Local exact hit: `no_provider_request`, null, shown exactly `Local cache · no provider request`.
- Never coerce null to zero and never render unknown/subscription as `$0.00`.

Recording aggregates return `known_estimate_micros`, `known_call_count`, `included_call_count`,
`no_provider_call_count`, and `unknown_call_count`. UI renders, for example,
`Estimated $0.0184 across 5 calls · 2 included · 1 unavailable`; it does not label the known subtotal a total
when unknown calls exist.

## 12. Privacy and threat model

### 12.1 Data classification and trust boundaries

| Data | Local persistence | May egress | Durable history/Vagus |
|---|---|---|---|
| Audio | Existing recordings cache | **Never** to hosted postprocess | Existing policy only |
| Raw/clean transcript | Raw Vagus/checkpoint as today; encrypted ledger/cache | Selected rows/context after enable | Applied note text; no telemetry content |
| Word bank | mode-0600 document + encrypted cache | Yes, in stable prompt prefix | keyed fingerprint/count only |
| Steering | hosted prefs/session memory + encrypted cache | Yes | keyed fingerprint only |
| Questions/answers | bounded session memory; optional encrypted 24 h cache | Yes | no body/content |
| API keys/cache key/ChatGPT rotating credential | private secret store only | Selected provider authorization only | never |
| Vertex token | memory only | Vertex authorization header | never |
| Usage/timing/cost | queue DB | provider already knows its call | content-free history |

Trust boundaries are React ↔ Tauri commands, Rust ↔ secret store/ADC, Rust ↔ cache filesystem, adapter ↔ remote
provider, app ↔ native ChatGPT device-auth worker, and `corti-vagus` ↔ the current returned note.

### 12.2 Threats and controls

- **Accidental egress:** master is default-off; first enable requires versioned disclosure; connection does
  not enable; raw stays available; per-lane kill switches are immediate.
- **Secret exfiltration:** React receives presence/source metadata only. Native `NSSecureTextField` writes
  directly through a narrow Security.framework wrapper. Secret material is non-cloneable/zeroized; provider
  errors/logs are sanitized. No secret in command args or screenshots.
- **Transcript prompt injection:** requests expose no tools or process surface. Output is id-addressed and
  strictly validated; malformed or non-schema text falls back to immutable raw ASR.
- **Stale/racing output:** full fences on every event; immutable raw; mismatched content is discarded.
- **Cache disclosure/dictionary attack:** AEAD content, HMAC keys, opaque account scope, no plaintext file
  names. Master-key loss fails closed. WAL/SHM contain ciphertext, not prompts/results.
- **Provider retention/training/cache:** separate acknowledgement, provider-specific disclosure, no false
  “off” when implicit caching exists, and no claim that local purge reaches provider storage.
- **Credential confusion:** the ChatGPT transport uses only Corti's fixed secret slot and fixed auth/model
  hosts. It never imports `~/.codex/auth.json`, Pi/Dekopon credentials, or Claude credential files, and no
  model subprocess exists to compromise.
- **Vagus overreach:** providers return typed text only. `corti-vagus` owns an opaque current-note handle/path
  from this recording and performs the one final bounded rewrite/flip; no index/other note access.
- **Billing after cancel/crash:** UI warns that dispatch may still bill; terminal usage persists late;
  ambiguous dispatch is not auto-repeated.
- **Telemetry leakage:** fixed error taxonomy, no provider body/free-form content, no account/project id,
  no Q&A/diff/prompt.
- **Tests spending money:** injected transports/credentials/clocks/processes; non-loopback network and real
  process launch denied; ambient credentials ignored.

Out of scope threats include a malicious local administrator, compromise of the selected provider, and
forensic recovery of old APFS blocks while the old encryption key is still available. Regulated-data use is
blocked until provider/account retention, residency, training, and BAA requirements are explicitly approved.

## 13. Failure, cancellation, and errors

Sanitized error codes are:

```text
AuthUnarmed, AuthRejected, Permission, Quota, RateLimited, ModelUnavailable,
Network, Timeout, Canceled, Superseded, PolicyBlocked, Cache, MalformedOutput,
Provider, BrokerExited, AmbiguousDispatch, Internal
```

Free-form provider response bodies are diagnostic-memory only after redaction and never durable. Persistent
lane state includes a recovery action. Toasts deduplicate by unarmed episode or call id.

Master/lane disable, steering/bank/model/provider changes, superseding revisions, session end, explicit
cancel, deadline, and shutdown set an atomic cancellation token and ask the adapter to abort. Provider workers
may still receive terminal usage. A canceled call that dispatched says `Canceled · provider billing may
still occur`.

Raw text is preserved through all states: disabled, queued, cache failure, auth wait, stale, canceled,
finalizing, provider failure, malformed output, and ambiguous crash.

## 14. Live protocol v2 and exact UX

### 14.1 Wire protocol

```text
LiveSnapshotV2 {
  protocol_version: 2,
  process_epoch,
  revision,
  session_id, session_generation,
  retained_from_seq,
  rows[raw + optional clean + rewrite_state + commit_epoch],
  lane_states, provider_states, metrics,
  assistant_state, notices
}

LiveDeltaV2 {
  protocol_version: 2,
  process_epoch,
  session_id, session_generation,
  from_revision, revision,
  ops[]
}
```

Typed ops are `reset`, `row_upsert`, `trim_before`, `lane_state`, `provider_state`, `metrics_replace`,
`assistant_upsert`, `assistant_remove`, `pinned_replace`, and `notice`. React subscribes and buffers before
snapshot, replays only contiguous deltas, ignores duplicates, and immediately refetches on a gap/process epoch
change while retaining the last raw view. The 30-second reconciliation snapshot remains a repair path.

A row contains immutable id/sequence, speaker, start/end, and `raw_text`; clean text is versioned separately.
React computes Unicode-aware word/punctuation diff spans in memoized memory only. Diff markup is never sent to
Rust, SQLite, cache, Vagus, logs, or telemetry.

### 14.2 Settings information architecture

Settings becomes three keyboard-operable tabs:

1. **Transcription** — current backend/AEC/live-ASR controls.
2. **Hosted rewrite** — privacy/master, provider cards, Live + Final cards, steering, word bank,
   cache/privacy, pinned assistant, diagnostics/cost.
3. **Storage & local models** — current paths/retention/downloaded ASR/VAD/diarization models. It explicitly
   says hosted rewrite models are not downloaded.

Hosted controls use immediate patch commands, not the existing bottom-of-form Save. Order and behavior:

- Egress card first. First enable modal states: `Selected transcript text, unique words, steering, and
  questions will leave this Mac. Audio is not sent by hosted rewrite.` Buttons are `Not now` and
  `Acknowledge and enable`.
- Provider connection cards show `Documented`, `Experimental`, or `Blocked`. Connection never toggles master.
- Vertex card shows project/region/quota-project and ADC status, with setup command
  `gcloud auth application-default login`. `Armed` means token only; service readiness is separate.
- OpenAI/Anthropic direct cards use `Add key…`, `Replace key…`, and `Remove key`; native secure entry never
  reflects the key into React.
- ChatGPT subscription is visibly `Experimental`; its device UX shows only the fixed verification URL and
  user code. It explains included quota, private endpoint limits, and that no Codex server/API key is used.
- Claude subscription card says `Blocked — use Anthropic API billing` and has no credential-import button.
- Live and Final cards have independent provider/model selectors, tariff basis, enable switch, and cache
  disclosure. Unavailable/benchmark reasons remain visible.
- Word-bank editor supports chips, bulk paste, search, edit/remove, `Clear…`, count/revision, and
  `Remember spelling`; never auto-learns.
- Separate persistent switches: `Show history diagnostics` and `Show live metrics by default`.

### 14.3 Live window

Default size becomes 1,100×700; minimum 640×420. At ≥820 px it is a resizable transcript/assistant split
(initially 65/35). Below 820 px, Assistant is a focus-trapped drawer with unread badge, Escape/backdrop close,
and focus restoration. Drawer/rewrite updates preserve transcript scroll; auto-follow only when within 80 px
of the bottom.

Header controls are immediate `Master`, `Live`, and `Final` switches plus a steering popover whose primary
button is `Apply to next request`. The transcript has an accessible `Raw | Clean | Changes` segmented
control:

- `Raw`: immutable ASR rows.
- `Clean`: accepted clean text, visibly falling back to raw per row.
- `Changes`: `<del>`/`<ins>` word/punctuation changes with screen-reader labels.

Lane text states are exactly understandable without color: `Disabled`, `Waiting for phrase`, `Queued`,
`Arming`, `Catching up`, `Rewriting`, `Finalizing`, `Clean`, `Using raw`, and `Failed`. Raw renders before any
hosted state.

The Vertex notice is once per unarmed episode, `role=alert`, and its visible text is exactly:

> gcloud token isn't armed

Diagnostics show lane/model, token categories, queue/auth/TTFB/TTFT/stream/total, cache source, and truthful
cost label. `Details` is a per-window override only.

Assistant sidebar puts the pinned card first and bounded ad-hoc thread below. Every answer shows as-of
revision, context truncation, state/cancel, token/cache/cost truth, and pinned session run count. Enabling
pinned auto-run requires the repeated-call acknowledgement.

### 14.4 Motion and accessibility

Queued/rewriting uses a soft static magenta activity edge plus subtle pulse. One accepted clean commit gets a
single non-looping magenta → violet → cyan edge wash keyed by `commit_epoch`; changed tokens retain a static
magenta accent. Raw/Clean view switches and layout do not animate.

Under `prefers-reduced-motion: reduce`, remove all hosted keyframes, transitions, rainbow movement, smooth
scrolling, and auto-animated focus. Keep static borders/badges/text. Forced-colors mode uses system colors and
visible outlines. Tabs, switches, segmented control, dialogs, toast, drawer, and live regions follow WAI-ARIA
patterns; concise state changes are announced without rereading transcript rows.

## 15. Rollout and external gates

1. **Foundation, no egress:** land ADR/domain/control/fences/prompt goldens/schema/cache with all hosted
   defaults off. Migration and UI blocked descriptors can ship safely.
2. **Documented providers:** direct OpenAI API, direct Anthropic API, and Vertex ADC behind a developer flag;
   complete privacy/retention review, tariff provenance, provider fixtures, and model benchmarks.
3. **Final lane canary:** opt-in final only, cache mandatory, small user cohort. Verify crash ambiguity,
   Vagus same-note publication, costs, and purge.
4. **Live cleanup + protocol/sidebar:** enable per user after p95/quality gates; then pinned questions behind a
   separate repeated-cost acknowledgement.
5. **General availability:** only documented transports/models that meet measured gates. Remote kill switch
   may disable a provider/model descriptor but must never silently substitute another.
6. **ChatGPT subscription:** ship the direct fixed-endpoint transport visibly experimental, with a provider
   kill switch and no inference fallback/substitution if OpenAI changes its private contract.
7. **Claude subscription:** remains blocked until a written Anthropic commercial agreement explicitly allows
   Corti's third-party routing. Direct Anthropic API does not satisfy that requirement by renaming it.

Rollback disables master/provider catalogs without deleting raw text. Queue v2 is additive and existing
`JobStatus` strings remain readable by v0.13; older binaries ignore new tables/columns. Before first GA,
measure provider TTFT/quality/cancel waste on the frozen Corti corpus and record benchmark manifest/tariff
retrieval dates. Provider retention/training/residency/BAA eligibility is a release checklist, not an inferred
property.

## 16. Acceptance criteria

### Architecture and boundaries

- [ ] Hosted processing is downstream of ASR; no audio is sent and no local rewrite model is added.
- [ ] `corti-postprocess` is runtime-free; provider/Tokio/HTTP/process code stays at async edges.
- [ ] Capture, writer, and live ASR use bounded nonblocking handoff and raw transcript publication never waits.
- [ ] Only `corti-vagus` can create/read/rewrite/flip/delete this recording's returned note; providers receive
      no path and return typed text/metadata only.
- [ ] Live raw windows keep ADR 0012 crash durability; final processing occurs before final state flip, while
      batch final occurs before checkpoint/add-note.
- [ ] Every applied content/status event matches the complete generation fence; stale terminal usage is kept
      but stale text is not.

### Providers and credentials

- [ ] Provider cards/types distinguish documented, experimental, and blocked support.
- [ ] Vertex uses ADC from `gcloud auth application-default login`, polls every exactly five seconds while
      unarmed, emits exactly `gcloud token isn't armed` once per episode on an intended dispatch, and catches
      up only newest valid auto-lane state.
- [ ] Direct OpenAI/Anthropic use the private secret store/workload identity and never claim subscription/device auth.
- [ ] ChatGPT device auth is Corti-owned, secret-store-backed, bounded, fixed-host, refresh-token rotating, and
      never exposes tokens/account ids over IPC, logs, debug, or preferences; no Codex server/tools exist.
- [ ] Claude subscription has no adapter/import/setup command absent written permission.
- [ ] Model choices come from provider/account/region catalogs plus capability/benchmark gates; no silent
      substitution and no local model catalog contamination.
- [ ] `gpt-5.6-luna` appears only as the exact direct-OpenAI catalog id, with unbenchmarked latency until
      measured and no inferred ChatGPT-subscription availability/default.

### Controls, lanes, and filing

- [ ] Master/live/final/question/model/steering/word-bank changes take effect on the next request in an active
      session, cancel affected work best effort, and fence late output.
- [ ] Connection never enables egress; first master enable requires persisted disclosure acknowledgement.
- [ ] Live is phrase-closure cleanup, single-flight/newest-pending, with 150 ms debounce and raw fallback.
- [ ] Final is stronger, deterministic-chunked, all-or-nothing, and bounded by the documented deadlines.
- [ ] Final disable/auth timeout/error/exit files latest validated clean-or-raw text rather than blocking or
      erasing raw.
- [ ] Cache-committed final recovery never repeats a provider call; crash-ambiguous dispatch never auto-retries
      and requires explicit confirmed retry.
- [ ] Filing checkpoint v2 stores exact applied text/provenance, so filing retries never call a paid provider.
- [ ] Exactly one pinned template exists; 500 ms edit debounce, meaningful-progress policy, one coalesced rerun,
      and repeated-cost acknowledgement work in Rust.
- [ ] Ad-hoc questions are bounded, visible, cancelable, as-of-revision, and never silently coalesced.

### Cache, privacy, telemetry, and cost

- [ ] Word bank normalizes/validates/deduplicates/sorts deterministically, increments revision only on canonical
      change, and occupies the stable prompt prefix.
- [ ] Exact keys include every semantic dimension and exclude session/time/display-only dimensions as specified.
- [ ] Cache/ledger/recovery content is AEAD-encrypted with a locally stored master key; HMAC identities,
      TTL/LRU cap, purge, and key rotation work with no plaintext in DB/WAL/file names.
- [ ] Provider caching is separate, privacy-gated, accurately discloses implicit behavior, and is never claimed
      purgeable by Corti.
- [ ] Queue schema v2 migration is transactional/reopen-safe; pipeline remains sole writer and UI read-only.
- [ ] Durable calls contain normalized usage, nullable cost, tariff provenance, cache outcome, revisions, and
      fine latency—but no transcript/prompt/diff/word/question/answer/secret/provider body.
- [ ] Unknown/incomplete/subscription/local-hit costs use the exact truthful labels and never `$0.00`.
- [ ] Late usage for canceled/stale calls persists; provisional usage is visibly provisional and not stored as
      terminal truth.
- [ ] Applied provenance schema 2 is checkpoint-stable and contains no credentials, account ids, prompt/steering
      text, question content, or cost.

### UX and accessibility

- [ ] Settings has Transcription / Hosted rewrite / Storage & local models tabs and immediate hosted patches.
- [ ] Live shows raw immediately, `Raw | Clean | Changes`, split/sidebar or accessible narrow drawer, one pinned
      card, bounded ad-hoc thread, and per-window Details override.
- [ ] Protocol v2 detects gaps/process changes, retains last raw view, and repairs from snapshot.
- [ ] Accepted rewrites animate once with tasteful magenta/rainbow accents; reduced motion removes animation,
      transitions, and smooth scrolling while retaining visible state.
- [ ] Keyboard, focus restoration/trap, live regions, forced colors, non-color status, and semantic diffs pass
      accessibility tests.
- [ ] History exposes separate live/final/question aggregates/details according to diagnostic preference.

### Test safety

- [ ] Automated tests ignore ambient credentials and deny provider domains/non-loopback networking, installed
      `gcloud`/Claude processes, and the real secret store unless an injected fake is used.
- [ ] Every adapter/auth/cache/pricing/UI path has deterministic fixtures; no test makes a real paid API call.

## 17. Test plan — no paid inference

### Pure Rust/domain

- Golden canonical prompt bytes, stable-prefix boundary, structured schemas, output validation, expansion
  limits, Unicode/NFC/casefold word-bank behavior, injection rejection, revisions/fingerprints.
- Key identity table: canonical-equivalent requests hit; every semantic field invalidates; session/time/details
  do not. Provider cache key contains no readable words.
- Fake-clock scheduler tests for live debounce/coalescing/deadlines/fairness, final chunk all-or-nothing,
  control changes, cancellation, stale fences, and bounded ad-hoc FIFO.
- Pinned policy at 39/40 words and 29.999/30 s, 500 ms edit debounce, 750 ms quiet period, edit/clear cancel,
  and one dirty rerun.
- Pricing effective-date/region/tier/cache-token formulas, integer overflow, stale/unknown tariffs,
  subscription/null/local-hit rendering, and mixed aggregates.

### Credentials/providers

- Vertex fake clock advances 4.999 s then 1 ms; assert exact poll count, one exact warning per episode,
  `Arming -> Catching up -> Ready`, newest-only auto catch-up, final expiry, and token-ready/service-failed
  distinction.
- Secret-store tests cover absent/store/replace/read/delete/loosened-mode/symlink/rotation; supplied secret never appears in any
  serialized DTO/event/log/screenshot.
- Scripted native ChatGPT HTTP fixtures for device code/pending/denial/expiry, transient-poll backoff,
  rotating refresh persistence, 401 refresh/retry, authenticated model list, malformed OAuth/SSE, and logout.
  No app-server/process or ambient credential is permitted.
- Canned OpenAI/Anthropic/Vertex SSE/JSON fixtures for deltas, terminal usage/cache fields, malformed frames,
  401/403/429/5xx taxonomy, cancellation after dispatch, late usage, and missing usage.
- Test adapter factory receives explicit deny-network/deny-process implementations even when ambient env vars,
  ADC files, or binaries exist. Only explicit loopback fixture servers are allowed.

### Storage/recovery/queue/Vagus

- Injected key/RNG/clock encrypted-store tests: no prompt/result plaintext in DB/WAL/SHM/file names; wrong key,
  corruption, TTL/LRU, mandatory recovery commit, reusable-cache off, purge, and rotate.
- Long synthetic call proves UI store, coordinator queues, Q&A, and memory stay bounded while encrypted ledger
  can assemble final; ledger ingress/cap failure disables final safely.
- Crash points at Prepared, Dispatched, ResultCached, Applied, and Checkpointed prove safe retry/cache reuse/
  ambiguity fallback and never a second automatic paid attempt.
- Queue v1 fixture migrates to v2 and reopens; outbox import/upsert/ack is idempotent; nullable usage/cost and
  known/included/unknown aggregates are correct; no content columns; cascade/retention works.
- Fake Vagus binary/temp notes prove batch final precedes `add-note`; live rewrite stays same inode and precedes
  flip; raw fallback works; schema-2 provenance is exact and secret-free; retry uses checkpoint snapshot.

### App/frontend

- Mock phrase integration: raw event precedes hosted scheduling, handoff cannot block, clean commit applies once,
  stale clean drops, During-call Vagus append remains raw, final/mixed rewrite is exact.
- Protocol reducer: subscribe-before-snapshot buffer, contiguous replay, duplicate ignore, gap refetch, process
  reset, retention trim, row replacement, notice dedupe, and raw retention during repair.
- Settings: connection leaves master off, disclosure gates enable, immediate patches reconcile conflicts,
  invalid/stale models reject, blocked/experimental labels, canonical word-bank controls, truthful cost strings.
- Diff components: Unicode/grapheme/punctuation/whitespace insert/delete/replace, `<ins>/<del>` labels,
  memoization, fallback, and no durable serialization.
- Assistant: pinned/ad-hoc timing/queue/cancel/as-of/truncation/cost/focus and bounded content.
- Playwright deterministic captures: hosted-off/ready Settings; live rewriting/changes+assistant; Vertex
  unarmed/catching-up; narrow drawer; reduced motion; queue hosted history. Assert exact warning and cost text.
- Mandatory CI harness fails if any adapter tries a non-loopback URL or real process. There is no opt-in paid
  test target in the normal workspace suite.

## 18. Implementation sequence and file map

1. Ratify ADR 0015/guardrails and land domain types, prompt/word-bank goldens, pure schedulers, pricing types.
2. Add queue v2 migration/call APIs and encrypted store/secret-store seams with fake-only tests.
3. Add managed control/patch commands/provider descriptors and Hosted rewrite Settings with every provider
   still blocked/off.
4. Add injected documented adapters/auth/catalog fixtures; complete benchmarks/privacy/tariff gates before
   enabling production factories.
5. Integrate closed-row live scheduling, encrypted ledger, protocol v2, raw/clean/diff UI, then final boundary
   in `finish_session` and `transcribe_and_file`.
6. Add Vagus current-note rewrite/provenance schema 2 and crash tests.
7. Add telemetry outbox/history diagnostics and cost formatting.
8. Add assistant lanes/sidebar/accessibility/motion and deterministic screenshots.
9. Ship native fixed-endpoint ChatGPT subscription access without an app-server; do not implement Claude subscription routing.

Expected production files include new crates under `crates/corti-postprocess*`; app modules
`postprocess.rs`, `postprocess_auth.rs`, `postprocess_store.rs`, `secret_store.rs`, and `word_bank.rs`; extensions
to `live.rs`, `live_view.rs`, `pipeline.rs`, `checkpoint.rs`, `config/settings/main/queue_ui/provenance`; queue and
Vagus migrations/helpers; and focused React components under `app/ui/src/settings/` and `app/ui/src/live/`.
Current-state `docs/` and `design/STATUS.md` update only when implementation ships.

## 19. Provider sources

Primary sources checked 2026-08-21:

- OpenAI GPT-5.6 Luna model: <https://developers.openai.com/api/docs/models/gpt-5.6-luna.md>
- OpenAI prompt caching: <https://developers.openai.com/api/docs/guides/prompt-caching.md>
- OpenAI Codex device authentication: <https://learn.chatgpt.com/docs/auth.md>
- Direct device-flow/Responses reference implementation: <https://github.com/dekopon-agents/dekopon/blob/main/crates/dekopon-model/src/chatgpt.rs>
- OpenAI Codex model endpoint schema: <https://github.com/openai/codex/blob/main/codex-rs/protocol/src/openai_models.rs>
- Anthropic Claude Code legal/authentication restriction:
  <https://code.claude.com/docs/en/legal-and-compliance.md>
- Google local ADC setup:
  <https://docs.cloud.google.com/docs/authentication/set-up-adc-local-dev-environment>
- Google ADC overview: <https://docs.cloud.google.com/docs/authentication/provide-credentials-adc>

Provider pages, prices, model catalogs, legal terms, and retention behavior can change. The adapter/catalog,
pricing manifest, and release checklist must record their own checked/effective versions; this design's
snapshot is not a runtime capability claim.
