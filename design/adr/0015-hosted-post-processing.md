# ADR 0015 — Hosted transcript post-processing without weakening raw/Vagus boundaries

- **Status:** Accepted for implementation design (2026-08-21)
- **Amends:** ADR 0001/guardrail 1 (one current-note read/final rewrite), ADR 0002/guardrail 3
  (narrow AppKit secure-entry + Security.framework Keychain binding), ADR 0010 (final publication boundary),
  ADR 0014 (provenance schema 2 may describe applied hosted text)
- **References:** [design 07](../07-paid-model-post-processing.md), ADRs 0009/0012/0013,
  [#112](https://github.com/vasovagal/corti/issues/112),
  [#130](https://github.com/vasovagal/corti/issues/130)

## Context

Corti should optionally clean finalized live transcript rows, run a stronger paid final rewrite before the
note is final, and answer live questions. It needs direct API/Vertex credentials, a requested device-style
subscription path, exact caching, and truthful tokens/costs. Transcript text, a spelling bank, steering, and
questions are sensitive egress. Cancellation can race provider billing. Existing live notes are deliberately
created and OS-synced during a call so a crash does not lose every transcript window.

The requested provider transports are not equally supportable. Vertex ADC and direct OpenAI/Anthropic APIs
have documented application contracts. Corti also needs ChatGPT-plan access without making a local Codex
app-server a runtime dependency; Dekopon's proven direct device authorization and fixed Responses transport
provide that narrower implementation pattern. Anthropic explicitly says third-party developers may not offer
Claude.ai login or route Free/Pro/Max credentials. Treating all provider logins as interchangeable would
misstate product/legal support and cost.

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
   direct API and Amazon Bedrock are documented shipping transports. Native ChatGPT subscription access is
   an experimental shipping transport because its fixed endpoint is provider-controlled and not a public API
   contract. ChatGPT uses
   OpenAI's device authorization, authenticated model catalog, and fixed Codex Responses HTTP endpoint directly;
   Corti launches no Codex app-server and exposes no tools. Claude subscription routing is blocked with no
   adapter/import command absent written Anthropic permission. Direct API billing must not masquerade as
   subscription access.
5. **Use host-owned secret storage.** Direct API keys, the local-cache master key, and Corti's complete
   rotating ChatGPT credential document are separate non-synchronizing macOS generic-password Keychain items.
   React/config/SQLite/Vagus/logs/events/subprocess arguments never contain them. A narrow app-only AppKit
   secure-entry sheet and Security.framework wrapper are approved platform bindings under guardrail 3. Vertex
   access tokens are memory-only and Corti does not copy ADC refresh credentials. Corti never reads credentials
   owned by Codex, Pi, Dekopon, or Claude.
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

### Amendment — native ChatGPT subscription transport replaces the app-server proposal (#130)

Corti implements the device flow directly against fixed `auth.openai.com` endpoints, stores its own versioned
access/rotating-refresh credential in the Keychain, refreshes before expiry, and retries once after a 401. A
bounded, login-id-owned worker polls only while Preferences displays the fixed verification URL and user code.
Tokens, OAuth bodies, and the ChatGPT account id cannot cross IPC or appear in logs/debug output. The account
id instead derives an opaque live connection scope inside the credential owner; that scope is not persisted
separately, so a crash or account switch cannot pair a new credential with an old cache fence. A rotated
credential that cannot be saved remains usable only by the current in-flight call and projects a non-durable
Keychain error rather than `Ready`.

After login, Corti queries `https://chatgpt.com/backend-api/codex/models` and offers only account-returned
models marked API-supported. Rewrite/question calls go straight to
`https://chatgpt.com/backend-api/codex/responses`, with no tools, shell, file access, or local server. The
provider controls quota and model availability; Corti records reported usage and `included_subscription` with
a null dollar amount. OpenAI Platform API keys remain a separate metered transport.

The old `codex_app_server` descriptor is removed from the production provider catalog. No Codex process,
`CODEX_HOME`, app-server protocol, or imported Codex/Pi/Dekopon credential participates in this path.

## Consequences

- Live cleanup latency is phrase-closure latency; Corti does not invent unstable ASR partials.
- A user can always read/file raw text when hosted work is disabled, waiting, stale, canceled, malformed,
  failed, or crash-ambiguous.
- A literal requirement that no Vagus file exist before final processing is not met for live calls; changing
  that requires explicitly superseding ADRs 0010/0012 and accepting greater crash loss.
- Claude Free/Pro/Max support remains a product/legal blocker, not an engineering TODO. ChatGPT subscription
  access is a distinct fixed-endpoint transport with included-quota accounting and no dollar estimate; model
  availability and limits remain controlled by OpenAI.
- Keychain/AppKit integration and an encrypted cache store add macOS-specific code, but they prevent secret
  exposure to the webview/config and preserve Corti's Apple-only platform stance.
- SigV4 and the `vnd.amazon.eventstream` decoder are Corti's own ~350 lines rather than an AWS dependency.
  They are pure and table-tested against AWS's published vectors, but they must track any future change to
  the signing or framing contract.
- Provider retention, training, residency, and regulated-data suitability remain account/model release gates.
  Corti cannot erase provider-side cache or guarantee cancellation prevented billing.
- Queue schema additions are content-free and additive; existing recording status strings remain unchanged so
  downgrade readers do not fail merely because final post-processing was active.
