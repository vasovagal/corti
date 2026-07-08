# 04a — AEC calibration: from echo ghosts to clean attribution

> **Superseded — calibration dropped per [ADR 0007](adr/0007-streaming-aec-first.md).** The
> `corti --calibrate-aec` parameter sweep described here read **persisted raw 2-track
> recordings**, which the streaming-AEC-first / no-durability stance removes. The CLI
> entrypoint and the sweep driver (`tune.rs`) have been deleted. This post-mortem is kept for
> the analysis and the still-valid env-var/`config.toml` AEC overrides; revisit calibration
> when durability + raw retention return.

Post-mortem and reference for the June 2026 AEC tuning work. Companion to
[`04-corti-aec.md`](04-corti-aec.md); that doc covers the algorithm, this one covers why the defaults
weren't enough and what we did about it.

## The problem

On speaker calls (Zoom, Slack), the transcript was full of **echo ghost segments** — the mic picks up
the far-end audio playing through the speakers, and the residual bleed survives AEC well enough for
Parakeet to transcribe it. These ghosts get attributed to "Me" because they appear on the mic channel.

Concrete example from the 2026-06-11 13:02 Zoom call: a colleague delivers a long explanation about
GCP DNS peering. The original transcript has **222 "Me" segments** in a call where I spoke maybe 40
times. Segments like `[00:26] Me: So we today have inside Google file.` and `[00:33] Me: DNS records
all set up, but we don't have that pushed out to the formation.` — garbled, time-shifted echoes of
what "Them" just said. The transcript was borderline unusable for review.

## How AEC was supposed to work

The FDAF (frequency-domain adaptive filter) models the room's echo path `h` so that
`echo_estimate = h * far_end`, then subtracts: `clean = mic - echo_estimate`. The adaptive filter
converges over the first seconds of the call, learning the speaker-to-mic transfer function. A
double-talk gate freezes adaptation when the user speaks (to avoid diverging the filter on near-end
speech). Two offline passes ensure even the call opening is cleaned — pass 1 converges the filter,
pass 2 re-applies from sample 0.

With default parameters (`mu=0.3`, `filter_len=4096`, `power_smoothing=0.9`, `double_talk_ratio=2.0`),
the FDAF achieves ~94.7% echo energy suppression on a 35-minute Zoom call. That sounds impressive.

## Why 95% suppression wasn't enough

The problem is that ASR models can decode speech at very low signal levels. Even with 95% of the echo
energy removed, the residual 5% is still above Parakeet's detection threshold — especially for clear,
well-articulated speech from a remote speaker on a good connection. The FDAF removes the *bulk* of the
echo, but the tail — nonlinearities in the speakers, late reverb, model mismatch — persists as faint
but intelligible speech. The transcriber dutifully transcribes it and stamps it "Me."

This is a well-known limitation of linear adaptive filters. Every real-world AEC system (phones,
conferencing hardware, WebRTC) pairs the adaptive filter with a **residual echo suppressor** — a
spectral post-filter that aggressively attenuates frequency bins where echo is estimated to dominate.
We shipped without one.

## The 864-run parameter sweep

Before adding the post-filter, we needed to understand how much headroom the FDAF itself had. We built
a scoring system (`corti_aec::score`) and a parallel sweep harness (`corti_aec::tune`), then ran
**144 parameter combinations x 6 recordings = 864 AEC runs**, each on 5 minutes of audio:

- `filter_len`: 4096, 8192, 12288, 16384
- `mu` (step size): 0.3, 0.5, 0.7, 1.0
- `power_smoothing`: 0.8, 0.9, 0.95
- `double_talk_ratio`: 1.0, 1.5, 2.0, 3.0

Scoring metrics per run:
- **Echo suppression**: 1 minus the mean windowed cross-correlation between the cleaned mic and
  far-end. Higher = more echo removed.
- **Near-end preservation**: ratio of clean-to-raw energy in windows where the user is talking
  (high mic energy, low mic-to-far correlation). Higher = less voice distortion.
- **Combined**: 0.7 * suppression + 0.3 * preservation.

### What we found

**Two bugs blocked the sweep initially.** At `mu >= 0.5` with `power_smoothing >= 0.9`, the filter
diverged to NaN. Root cause: the symmetric power smoother (`P = γP + (1-γ)|X|²`) lags behind sudden
power increases, so when far-end energy jumps, `P << |X|²`, the gradient explodes, and the weights
blow up. Fixed with a **one-sided power smoother** — power increases are tracked immediately (instant
response), decreases are smoothed (conservative). Additionally, a **weight leakage** term
(`W = (1-1e-5)W + mu*G`) prevents unbounded weight growth over long recordings.

With those stability fixes, all 144 configs produced valid scores. Key findings:

1. **Zoom vs Slack scores diverge dramatically.** Echo suppression on 5-min truncated Zoom files:
   0.08-0.28. On Slack files: 0.57-0.89. Different apps route audio differently — Zoom's audio
   pipeline does its own processing that changes the echo characteristics.

2. **Full-length recordings score much higher than 5-min truncates.** The same default config scored
   echo_suppression=0.257 on 5 minutes of the Zoom call but 0.947 on the full 35 minutes. The filter
   needs convergence time, and 5 minutes with lots of double-talk isn't always enough.

3. **The FDAF ceiling is ~0.95 echo suppression** regardless of parameters. More aggressive mu (0.7)
   matched the default (0.3) on full-length recordings — the filter converges either way given enough
   time. The remaining 5% is structural: nonlinearities, late reverb, model mismatch.

4. **The best sweep config** (fl=4096, mu=0.3, ps=0.9, dtr=1.0) scored 0.632 mean combined on the
   5-min truncated corpus. The defaults (same but dtr=2.0) scored 0.619. The difference is marginal
   and driven by slightly more aggressive double-talk gating.

## The fix: residual echo suppressor

Since the FDAF is at its ceiling, we added a **Wiener-style spectral post-filter** as a separate pass
after the adaptive filter converges. For each block:

1. Compute the echo estimate in the frequency domain: `Y = X * W`
2. Compute the FDAF error: `E = FFT(d - IFFT(Y))`
3. Per-bin suppression gain: `G(k) = max(1 - alpha * |Y(k)|^2 / |E(k)|^2, floor)`
4. Apply: `E_post(k) = G(k) * E(k)`
5. IFFT back to time domain

The gain attenuates frequency bins where the echo estimate is a large fraction of the error signal.
Bins dominated by near-end speech (where |E| >> |Y|) pass through with G near 1.0. The spectral
floor (0.01 = -40 dB) prevents complete nulling, which would produce artifacts.

During double-talk blocks (user speaking), the suppressor passes through unsuppressed — same gate as
the FDAF.

**Parameters:** `alpha = 2.5` (overestimation factor; accounts for the echo estimate being imperfect),
`floor = 0.01`. The alpha is deliberately aggressive — for offline transcription, we'd rather
slightly over-suppress echo (losing a few quiet syllables of bleed) than under-suppress and generate
ghost segments.

## Outcome

On the 2026-06-11 13:02 Zoom call (35 minutes, the worst offender):

| Metric | Before | After |
|--------|--------|-------|
| Echo suppression score | 0.947 | 0.963 |
| Near-end preservation | 0.918 | 0.916 |
| Total transcript segments | 294 | 118 |
| "Me" segments | 222 | 46 |
| "Them" segments | 72 | 72 |
| AEC wall-clock time | 107s | 107s |

The 176 eliminated "Me" segments were all echo ghosts. The surviving 46 are genuine interjections
("Yeah.", "Mm-hmm.", "Gotcha.") and real contributions. "Them" count is unchanged — no real speech
was lost.

## Maintaining accuracy

The defaults (`mu=0.3`, `filter_len=8192` (benchmark-tuned up from 4096; see
`design/06-benchmark-harness.md`), `power_smoothing=0.9`, `double_talk_ratio=2.0`,
`suppress_residual=2.5`) are now baked into `AecConfig::default()`. These were validated across 6
recordings spanning Zoom and Slack, speaker setups, and call durations from 12 to 54 minutes.

For users whose setup produces unusual echo characteristics (different speaker placement, room
acoustics, or conferencing apps that do aggressive preprocessing), individual parameters can be
overridden via `CORTI_AEC_*` environment variables. The `--calibrate-aec` sweep that once auto-saved
best parameters to `config.toml` was **removed** with durability (see the banner; ADR 0007) — it read
persisted raw recordings that no longer exist.

The sweep driver (`corti_aec::tune`, `--calibrate-aec`) is deleted; the `aec_file` example still
accepts all AEC parameters as CLI flags for A/B testing individual recordings.

## Will new users need to recalibrate?

**No.** The defaults should work well for the common case — laptop speakers with a built-in mic, on
Zoom/Slack/Meet/Teams. The three things that matter:

1. **The residual suppressor (`suppress_residual=2.5`) does the heavy lifting.** The FDAF is already
   good enough with defaults; it's the post-filter that eliminates the transcription-level ghosts.
   This doesn't depend on room acoustics — it's a spectral-domain cleanup that works regardless of
   the echo path shape.

2. **The one-sided power smoother and weight leakage are unconditional stability fixes.** They prevent
   divergence across the full parameter space. No user will hit NaN scores.

3. **The double-talk gate is conservative (`ratio=2.0`).** It errs toward freezing (preserving near-end
   voice) rather than adapting (risking filter divergence). This is the right tradeoff for
   transcription — a slightly under-cancelled echo that gets post-filtered is better than a
   diverged filter that distorts the user's voice.

The defaults are designed to be good-enough-without-tuning. The main scenario where hand-tuning helps
is external speakers at a distance (conference room setups with longer room impulse responses), where
bumping `filter_len` (via `CORTI_AEC_*`) models the longer echo path — the automated `--calibrate-aec`
sweep that once did this is gone (ADR 0007).
