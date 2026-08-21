# ADR 0015 — Hosted transcript post-processing without weakening raw/Vagus boundaries

- **Status:** Accepted for implementation design (2026-08-21)
- **Amends:** ADR 0001/guardrail 1 (one current-note read/final rewrite), ADR 0002/guardrail 3
  (narrow AppKit secure-entry + Security.framework Keychain binding), ADR 0010 (final publication boundary),
  ADR 0014 (provenance schema 2 may describe applied hosted text)
- **References:** [design 07](../07-paid-model-post-processing.md), ADRs 0009/0012/0013,
  [#112](https://github.com/vasovagal/corti/issues/112)

## Context

Corti should optionally clean finalized live transcript rows, run a stronger paid final rewrite before the
note is final, and answer live questions. It needs direct API/Vertex credentials, a requested device-style
subscription path, exact caching, and truthful tokens/costs. Transcript text, a spelling bank, steering, and
questions are sensitive egress. Cancellation can race provider billing. Existing live notes are deliberately
created and OS-synced during a call so a crash does not lose every transcript window.

The requested provider transports are not equally supportable. Vertex ADC and direct OpenAI/Anthropic APIs
have documented application contracts. OpenAI documents a Codex app-server device-code flow but also says
app-server is experimental and unsupported for production workloads. Anthropic explicitly says third-party
developers may not offer Claude.ai login or route Free/Pro/Max credentials. Treating all of these as ordinary
shipping providers would misstate product/legal support and cost.

## Decision

1. **Hosted processing is downstream, optional, and default-off.** Raw ASR is immutable and always available;
   no rewrite/question provider receives audio. Connecting a credential never enables egress. First enable
   requires a versioned disclosure that transcript text, word-bank entries, steering, and questions may leave
   the Mac.
2. **Preserve the crash-safe live-note contract.** During-call Vagus windows remain raw and durable. “Final
   before Vagus” means before a live note becomes `State: transcribed ` and before `vagus add-note` on batch
   paths. Final accepted/mixed text is published through one same-note rewrite followed by the existing state
   flip. Delaying all live note creation is rejected because it contradicts ADRs 0010/0012.
3. **Keep Vagus authority narrow.** Only `corti-vagus` may read/rewrite the path Vagus returned for the current
   recording. Provider adapters receive typed rows and return typed replacements/metadata; they never see a
   note path or touch Vagus/index state. No other vault read/write is permitted.
4. **Make provider support status part of the contract.** Vertex direct API, OpenAI direct API, Anthropic
   direct API, and Amazon Bedrock are documented transports. Codex app-server/device code is compile- and approval-gated
   experimental support; it owns token persistence/refresh and must use local stdio, a dedicated private home,
   OS keyring, an empty private cwd, and denied tools/approvals. Claude subscription routing is blocked with no
   adapter/import command absent written Anthropic permission. Direct API billing must not masquerade as
   subscription access.
5. **Use host-owned secret storage.** Direct API keys and local-cache master key are non-synchronizing macOS
   generic-password Keychain items. React/config/SQLite/Vagus/logs/events/subprocess arguments never contain
   them. A narrow app-only AppKit secure-entry sheet and Security.framework wrapper are approved platform
   bindings under guardrail 3. Vertex access tokens are memory-only and Corti does not copy ADC refresh
   credentials. Corti never reads ordinary Codex/Claude credential files.
6. **Fence every result and journal paid boundaries.** A managed monotonic control state applies switches,
   models, steering, and word-bank changes to the next request during a call. Every event carries complete
   session/transcript/control/lane/steering/bank/question generations. Cancellation is best effort; late
   content is discarded and late terminal usage is retained. Valid final results are encrypted and committed
   before application. A dispatched call with no terminal cache after a crash is ambiguous and is never
   automatically repeated.
7. **Cache privately and account truthfully.** Exact local prompt/result and session-ledger content is AEAD
   encrypted outside Vagus under a Keychain key; request identities are keyed canonical digests. Provider
   prefix caching is separately disclosed and never falsely described as locally purgeable. Durable queue
   telemetry contains content-free call metadata, normalized nullable usage, fine-grained latency, cache
   outcome, and versioned nullable cost. Unknown/incomplete/subscription cost remains null and never renders
   as zero.
8. **Preserve sync-core/async-edge topology.** Runtime-free domain/scheduler code is separate from provider
   adapters. Hosted network, credentials, cache SQLite, and polling run on dedicated threads/private runtimes,
   never HAL/capture/writer/live-ASR threads. The pipeline remains the sole `queue.db` writer and may wait for
   a final response without performing network/auth work itself.

### Amendment — Amazon Bedrock joins the documented transports (2026-08-21)

Bedrock is a fifth documented transport. Decision 4's list now includes it, and decision 8's split is
preserved rather than excepted: `ConverseStream` is SigV4-signed over the existing sync `HttpTransport`
rather than through `aws-sdk-bedrockruntime`, which would pull the Smithy async runtime into the
runtime-free provider crate. AWS credential *resolution* stays in the app layer, where `aws-config`
already lives for the Transcribe backend, and reaches the adapter through an injected trait — the
provider crate still performs no ambient discovery.

Bedrock's credential surface is wider than one pasted key: default chain, named profile, static key pair,
assumed role, and IAM Identity Center. That made it the work that finally builds the secure-entry sheet and
Keychain wrapper decision 5 approved, which also retires the disabled OpenAI/Anthropic key buttons.
`hosted.toml` gains the credential mode, profile name, region, and role ARN; key material stays in the
Keychain, unchanged in kind from the other direct providers.

## Consequences

- Live cleanup latency is phrase-closure latency; Corti does not invent unstable ASR partials.
- A user can always read/file raw text when hosted work is disabled, waiting, stale, canceled, malformed,
  failed, or crash-ambiguous.
- A literal requirement that no Vagus file exist before final processing is not met for live calls; changing
  that requires explicitly superseding ADRs 0010/0012 and accepting greater crash loss.
- Claude Free/Pro/Max support remains a product/legal blocker, not an engineering TODO. Codex remains visibly
  experimental until OpenAI approves and supports the use.
- Keychain/AppKit integration and an encrypted cache store add macOS-specific code, but they prevent secret
  exposure to the webview/config and preserve Corti's Apple-only platform stance.
- SigV4 and the `vnd.amazon.eventstream` decoder are Corti's own ~350 lines rather than an AWS dependency.
  They are pure and table-tested against AWS's published vectors, but they must track any future change to
  the signing or framing contract.
- Provider retention, training, residency, and regulated-data suitability remain account/model release gates.
  Corti cannot erase provider-side cache or guarantee cancellation prevented billing.
- Queue schema additions are content-free and additive; existing recording status strings remain unchanged so
  downgrade readers do not fail merely because final post-processing was active.
