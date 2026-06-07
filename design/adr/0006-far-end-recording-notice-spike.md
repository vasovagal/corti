# ADR 0006 — Far-end "recording in progress" notice: feasibility spike

- **Status:** Proposed (spike / study only — 2026-06-07)
- **Tracks:** #31. Related: #22 (recording-consent / biometric notes), guardrails #3, #4, #5, #10.

## Context

#31 asks whether corti could make the **far end** of a call hear a spoken **"recording in
progress"** notice — a consent courtesy, since corti records the far end's audio via the
system-audio tap. The issue is explicit that this is a *study, not a build-yet*, and that the
spike's **first** job is to answer "is there even a delivery path?" before any TTS-voice bikeshedding.

### The hard caveat: corti has no outbound audio path to the far end (lead with this)

corti's owner runs the **tap-only / "webinar" capture path** and joins calls **without a hot mic**
("this relies on joining with a hot mic, which I never do"). In code this is
`CaptureSession::start_tap_only_recording` (`crates/corti-coreaudio/src/capture.rs:214`), which calls
`start_inner(target, capture_mic = false, …)` (`capture.rs:219`). In that mode:

- The aggregate device is clocked off an **input-less output** device
  (`listener::tap_only_clock_device`, `capture.rs:254`; the device is chosen specifically to expose
  **no input streams** — `crates/corti-coreaudio/src/listener.rs:50-78`), so **the microphone is
  never opened** — no orange "mic in use" dot, no mic TCC prompt (`capture.rs:208-220`).
- The IO proc (`io_proc`, `capture.rs:415`) reads **only the `input` buffer list** and pushes those
  samples into the capture ring. The CoreAudio render/playback buffer `_out: *mut AudioBufferList`
  (`capture.rs:420`) is **received and never written** — corti renders zero output samples anywhere.

So the capture engine is **strictly an input sink**. There is no microphone in the call, and the
aggregate is a passive tap, not a device any conferencing app presents as its mic. **corti therefore
has no outbound audio path to the far end at all in the owner's workflow.** Any "make the far end
hear X" feature must *first manufacture* such a path — that is the real cost, not the TTS.

This is the framing constraint for everything below: a notice can only reach the far end if some
audio device that corti can write into is **selected as the conferencing app's microphone**, which
the tap-only owner never does.

## The three delivery mechanisms (per #31), cheapest → heaviest

### (a) Play the notice out the local speaker (local playout, mic re-captures it)

- **How it would reach the far end:** corti plays the synthesized WAV out the default output; the
  *user's own hot mic* picks it up acoustically and the conferencing app sends that to the far end.
- **macOS feasibility:** trivial — a few lines (`AudioDeviceCreateIOProcID` rendering into `_out`, or
  just `afplay`/`AVAudioPlayer`). No new TCC identity to *play* (capture identity is unrelated;
  guardrail #10 is about capture only).
- **Intrusiveness:** the **owner** hears the notice too (annoying/looping risk), and quality is poor
  (room acoustics, AEC on the conferencing side may suppress it as "echo").
- **Guardrail fit:** clean — no new binding, no new device, pure local playout.
- **Verdict for the owner: useless.** It only reaches the far end *if the mic is hot*, which the
  owner never does. Fine as a throwaway baseline / the only thing that works for the hot-mic case.

### (b) Inject into the capture aggregate's input

- **The idea (from #31):** corti already builds a CoreAudio **aggregate device** for capture
  (`create_aggregate`, `capture.rs:261`); feed notice samples *into* an input sub-device so a
  conferencing app reading that device hears them.
- **Reality:** **rejected — this conflates two unrelated audio graphs.** The aggregate exists only so
  *corti's own IO proc* can pull mic+tap on one clock; it is corti's **capture source**, not a device
  the conferencing app has selected as its mic. Writing samples "into" it would at best route them
  back into *corti's own recording*, not outward to any other app. CoreAudio aggregates do not
  present a synthesized input to *other* processes — only a virtual driver (mechanism c) does. In the
  tap-only path the aggregate's only input-capable member is the process tap (a read-only capture
  object); there is deliberately **no input device** in it (`listener.rs:50`). There is nothing here
  to inject into that the far end could hear.
- **Guardrail fit:** N/A — the mechanism doesn't exist as described. Confirmed *no*.

### (c) Virtual audio input device (AudioServerPlugIn, à la BlackHole/Loopback)

- **How it would reach the far end:** ship a CoreAudio **AudioServerPlugIn** the user selects **as
  the conferencing app's microphone**; the driver mixes the real mic + corti's synthesized notice.
  This is the **only** mechanism that reliably reaches the far end.
- **macOS feasibility:** possible but **heavy.** An AudioServerPlugIn is a separate signed bundle
  installed into `/Library/Audio/Plug-Ins/HAL`, requiring its own code-signing, an installer with
  admin rights, and a `coreaudiod` restart. It is a large, long-lived new platform surface.
- **Intrusiveness:** maximal — a system-wide driver the user must install and then **manually select
  as their mic** in every call.
- **Guardrail fit:** the worst fit of the three. **Guardrail #3** ("own the platform bindings — no
  far-reaching macOS-binding dependency without an ADR; a virtual audio device is a big new surface")
  and **#4** (capture is CoreAudio process tap + aggregate) both flag this as a major addition needing
  its own dedicated ADR, not a spike side-effect. It is roughly the scope of building BlackHole.
- **The fatal catch for *this* owner:** even fully built, it **only works if the user selects a mic
  device in the call** — i.e. it requires exactly the hot-mic workflow the owner avoids. It does
  **nothing** for the pure tap-only flow. So the heaviest, riskiest mechanism still does not solve
  the stated problem.

### Summary table

| Mechanism | Reaches far end? | Cost | Guardrail fit | Helps tap-only owner? |
|---|---|---|---|---|
| (a) Play-to-speaker | Only if mic hot (acoustic) | Trivial | Clean | **No** |
| (b) Aggregate-input injection | No (wrong audio graph) | N/A — not a real path | N/A | **No** |
| (c) Virtual AudioServerPlugIn | Yes, reliably | Heavy (signed driver + installer, own ADR) | Poor (#3/#4) | **No** (still needs a mic device in-call) |

**Every mechanism that could reach the far end requires a hot mic / selected mic device in the
call.** The owner never provides one. There is no delivery path for the owner's workflow.

## TTS synthesis — sherpa-onnx `OfflineTts`, no new dependency: **confirmed YES**

Independent of delivery, #31 asks whether the notice waveform can be synthesized with **no new
dependency**, reusing the ONNX Runtime binding corti already links for ASR.

- corti pins **`sherpa-onnx = "1.13"`** in `crates/corti-transcribe-local/Cargo.toml`, resolved to
  **`sherpa-onnx 1.13.2`** in `Cargo.lock` (line 4444). The ASR engine already imports a dozen of its
  types (`crates/corti-transcribe-local/src/engine.rs:15-21`).
- The **same 1.13.2 crate** exposes a full offline TTS API (verified against the published docs.rs
  API for 1.13.2): `OfflineTts`, `OfflineTts::create(&OfflineTtsConfig)`, `tts.generate(text, sid,
  speed) -> GeneratedAudio`, `tts.sample_rate()`, plus model configs `OfflineTtsModelConfig`,
  `OfflineTtsVitsModelConfig`, `OfflineTtsKokoroModelConfig`, `OfflineTtsMatchaModelConfig`,
  `OfflineTtsKittenModelConfig`, `OfflineTtsSupertonicModelConfig`, `OfflineTtsPocketModelConfig`,
  `OfflineTtsZipvoiceModelConfig`.
- **Conclusion: a configurable "recording in progress" string → waveform is fully achievable with
  zero new dependencies** — the exact binding corti already links. This squarely satisfies the
  guardrail-#3 spirit ("we already have one ONNX/GPU binding, don't add more").

**Asset it would need:** one TTS model set, fetched once and cached under
`~/Library/Caches/corti/models/` (guardrail #5; same mechanism as ASR models —
`crates/corti-transcribe-local/src/models.rs:40-47`, `dirs::cache_dir()/corti/models`). Smallest
sensible English voice: a **VITS Piper** model (e.g. `vits-piper-en_US-*`, ~60–90 MB) or **Kitten**
(smaller). Larger/higher-quality (Kokoro ~300 MB) is overkill for a 3-second fixed phrase.

**Cheaper still — and the real recommendation for synthesis:** the notice text is short and rarely
changes, so a **tiny prebaked WAV** shipped in-repo (or synthesized once and cached) is the zero-cost
fallback and avoids pulling a TTS model at all. TTS only earns its keep if the notice text must be
*runtime-configurable by the user*; even then, synthesize-once-and-cache beats synthesize-per-call.

## Decision (recommendation): **DEFER far-end delivery; ship a local owner-side notice instead**

**Build-vs-defer: DEFER the far-end notice for the owner's workflow.** Rationale, grounded strictly
in the no-outbound-path reality and the consent guardrails:

1. **No delivery path exists for the tap-only owner.** (a) needs a hot mic; (b) is not a real path;
   (c) is a heavy signed driver that *still* needs a selected mic device in-call. The owner provides
   none of these. Building any of them does not make the far end hear anything in the actual
   workflow — we would be building a feature the owner's setup structurally cannot use.
2. **The only mechanism that genuinely reaches the far end (c)** is a major new platform surface that
   guardrails #3/#4 say warrants its **own** ADR and is comparable to building BlackHole — disproportionate
   for a courtesy notice, and even then gated on the hot-mic case.
3. **Consent is better served locally and honestly.** #31 and #22 frame this as an all-party-consent
   / BIPA courtesy. Since corti is the *recorder*, the cheapest, most truthful consent win is a
   **local owner-facing notice** ("corti is recording") — a visible/audible indicator to the *owner*
   — which is buildable today against the existing Tauri webview (ADR 0004) with no audio-path work,
   no driver, and no new dependency. Pretending to notify the far end while having no path to them
   would be *worse* for consent than being clear that corti records passively and the owner is
   responsible for disclosing per their jurisdiction.
4. **sherpa-onnx TTS is real and free**, but it is the *easy* half of the problem; it does not change
   the delivery verdict. Synthesis readiness is not the blocker — delivery is.

**What would change the verdict:** the owner adopting a **hot-mic workflow**. If the owner ever joins
with a mic, mechanism (a) (play-to-speaker, acoustic) becomes a viable trivial prototype, and (c)
(virtual device mixing real mic + notice) becomes a justifiable — though heavy — clean far-end path.
Until then, far-end delivery is deferred as structurally infeasible for this workflow.

## Consequences

- **No far-end-delivery implementation issue is filed** (no viable path for the owner's workflow).
  The proposed follow-up below is the *local owner-side* consent notice, which is buildable and the
  cheaper consent/UX win #31 itself calls out.
- **sherpa-onnx `OfflineTts` is documented as available** for any future audible-notice work
  (owner-side or, in a hot-mic future, far-end) — no new dependency, model cached under guardrail #5.
- **Mechanism (c) (virtual AudioServerPlugIn) is explicitly out of scope** here; if it is ever
  pursued (hot-mic future) it requires its own ADR per guardrails #3/#4.
- No code changed; capture remains input-only (`capture.rs` unchanged).

## Proposed follow-up issue (file with owner approval — orchestrator handles, not this spike)

> **Title:** Local "corti is recording" consent notice (owner-facing), optional sherpa-onnx TTS
>
> **Label:** Feature
>
> **Body:**
>
> Spike #31 / ADR 0006 found there is **no outbound audio path to the far end** in the owner's
> tap-only / no-hot-mic workflow (capture is input-only; the only reliable far-end mechanism is a
> heavy signed virtual audio driver that *still* needs a selected mic in-call). Far-end delivery is
> therefore deferred. The cheaper, honest consent win is a **local, owner-facing** "corti is
> recording" notice.
>
> **Scope:**
> - A clear in-app indicator while a recording is active — visible in the Tauri webview (ADR 0004)
>   and/or a one-shot audible chime/spoken cue **to the owner's own speakers** at recording start
>   (local playout only; no far-end claim, no mic, no virtual device).
> - Optional **configurable spoken text** synthesized via the already-linked sherpa-onnx `OfflineTts`
>   (`sherpa-onnx 1.13.2`, no new dependency). Model (e.g. a small VITS Piper voice, ~60–90 MB)
>   fetched once and cached under `~/Library/Caches/corti/models/` (guardrail #5), mirroring the ASR
>   model fetch (`corti-transcribe-local/src/models.rs`). **Default to a tiny prebaked WAV / chime**
>   so no TTS model download is required unless the user customizes the text; if synthesizing,
>   synthesize-once-and-cache (never per call).
> - Settings toggle: visual-only (default) / visual + audible.
>
> **Explicitly out of scope:** any attempt to deliver the notice to the far end (no play-to-speaker
> bleed-through reliance, no aggregate-input injection, no AudioServerPlugIn). A virtual audio device
> would be a separate ADR per guardrails #3/#4 and only makes sense if the owner adopts a hot-mic
> workflow.
>
> **Acceptance:** recording start shows an unmistakable local indicator; audible cue plays only to
> the owner when enabled; default build pulls no TTS model and adds no dependency. Relates to #22
> (consent/biometric).
