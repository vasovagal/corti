//! Scoring functions for AEC quality: measure how well a cleaned signal suppresses echo while preserving
//! the near-end (user's voice). Used by the tuning sweep to rank parameter configurations.

use serde::Serialize;

/// Machine-readable AEC scores for one run, emitted as a single JSON line by the `aec_file` example's
/// `--json` flag (and consumed by the corti-bench harness). Bundles the bounded `[0,1]` correlation scores
/// with the true dB-ERLE figure of merit.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AecScores {
    /// `AecScore::echo_suppression` (1 − mean far-end correlation; higher = less residual echo).
    pub echo_suppression: f32,
    /// `AecScore::near_end_preservation` (near-end energy retention during user-talking windows).
    pub near_end_preservation: f32,
    /// `AecScore::combined` (`0.7·echo_suppression + 0.3·near_end_preservation`).
    pub combined: f32,
    /// True Echo Return Loss Enhancement in dB over echo-only regions (see [`erle_db`]).
    pub erle_db: f32,
}

/// Quality scores for a single AEC run.
#[derive(Debug, Clone, Copy)]
pub struct AecScore {
    /// How much echo was removed: 1.0 = perfect suppression, 0.0 = no improvement. Derived from the
    /// mean sliding-window cross-correlation between the cleaned mic and the far-end.
    pub echo_suppression: f32,
    /// How well the user's voice is preserved: 1.0 = untouched, 0.0 = completely eaten. Measured during
    /// windows where the user is genuinely talking (high mic energy, low mic↔far-end correlation).
    pub near_end_preservation: f32,
    /// Weighted composite: `0.7 * echo_suppression + 0.3 * near_end_preservation`.
    pub combined: f32,
}

/// Score an AEC result by comparing the cleaned mic against the raw mic and far-end.
///
/// `mic` and `far_end` are the raw (pre-AEC) signals; `clean` is the AEC output. All three must be the
/// same length. `sample_rate` sizes the sliding windows.
pub fn score(mic: &[f32], far_end: &[f32], clean: &[f32], sample_rate: u32) -> AecScore {
    let suppression = echo_suppression(clean, far_end, sample_rate);
    let preservation = near_end_preservation(mic, far_end, clean, sample_rate);
    AecScore {
        echo_suppression: suppression,
        near_end_preservation: preservation,
        combined: 0.7 * suppression + 0.3 * preservation,
    }
}

/// Sliding-window normalized cross-correlation between cleaned mic and far-end. Returns 1 − mean |corr|:
/// higher = less residual echo. Windows of 0.5s, stepped by 0.25s.
fn echo_suppression(clean: &[f32], far_end: &[f32], sample_rate: u32) -> f32 {
    let win = (sample_rate as usize) / 2; // 0.5s
    let step = win / 2; // 0.25s
    let n = clean.len().min(far_end.len());
    if n < win {
        return 0.5;
    }

    let mut sum_corr = 0.0f64;
    let mut count = 0u32;
    let mut pos = 0;
    while pos + win <= n {
        let c = &clean[pos..pos + win];
        let f = &far_end[pos..pos + win];
        let f_energy: f64 = f.iter().map(|&v| v as f64 * v as f64).sum();
        if f_energy > 1e-10 {
            let corr = normalized_correlation(c, f);
            sum_corr += corr.abs() as f64;
            count += 1;
        }
        pos += step;
    }

    if count == 0 {
        return 0.5;
    }
    let mean_corr = (sum_corr / count as f64) as f32;
    (1.0 - mean_corr).clamp(0.0, 1.0)
}

/// Near-end preservation: during windows where the user is genuinely talking (raw mic has high energy
/// relative to far-end, AND low correlation with far-end), measure energy retention in the cleaned signal.
fn near_end_preservation(mic: &[f32], far_end: &[f32], clean: &[f32], sample_rate: u32) -> f32 {
    let win = (sample_rate as usize) / 2;
    let step = win / 2;
    let n = mic.len().min(far_end.len()).min(clean.len());
    if n < win {
        return 1.0;
    }

    let mut sum_ratio = 0.0f64;
    let mut count = 0u32;
    let mut pos = 0;
    while pos + win <= n {
        let m = &mic[pos..pos + win];
        let f = &far_end[pos..pos + win];
        let c = &clean[pos..pos + win];

        let m_energy: f64 = m.iter().map(|&v| v as f64 * v as f64).sum();
        let f_energy: f64 = f.iter().map(|&v| v as f64 * v as f64).sum();

        // User is talking: mic energy > 2× far-end energy AND low correlation with far-end.
        if m_energy > 2.0 * f_energy && m_energy > 1e-10 {
            let corr = normalized_correlation(m, f).abs();
            if corr < 0.5 {
                let c_energy: f64 = c.iter().map(|&v| v as f64 * v as f64).sum();
                let ratio = (c_energy / m_energy).min(1.0);
                sum_ratio += ratio;
                count += 1;
            }
        }
        pos += step;
    }

    if count == 0 {
        return 1.0; // no user-talking windows found; can't measure, assume fine
    }
    (sum_ratio / count as f64) as f32
}

/// True Echo Return Loss Enhancement in **dB**: `10·log10(echo_energy / residual_energy)`, measured over
/// **echo-only** regions (the near-end speaker is silent, so every sample is room echo of the far-end).
///
/// This is the physically meaningful AEC figure of merit — how many dB of far-end echo the canceller
/// removed — as opposed to the bounded `[0,1]` correlation scores above. It generalizes the test helper
/// `tail_erle_db` (which hardcodes the synthetic fixture's `[2s, 3s)` tail) into a real public API: the
/// caller supplies the region mask via the optional `near_ref`.
///
/// `raw_mic` is the pre-AEC mic (echo + any near-end), `cleaned` is the AEC output, both same length.
/// `far_end` and `sample_rate` are accepted so the API can window by far-end activity (and so callers and
/// a future bench need not special-case the signature); the energy ratio itself is `raw_mic` vs `cleaned`.
///
/// Echo-only-region selection:
/// - If `near_ref` is `Some(reference)` (a clean near-end track, e.g. a headset capture), only samples
///   where `|near_ref|` is below a small fraction of its own RMS are counted — those are the genuinely
///   echo-only samples, exactly the regions where ERLE is well-defined. Samples where the user is talking
///   are excluded (their energy isn't echo, so they would corrupt the ratio).
/// - If `near_ref` is `None`, the whole span is treated as echo-only. For the offline 2-track pair
///   (mic = ch0 raw, cleaned = AEC output) this matches the assumption that the scored region is a
///   far-end-driven echo tail with no near-end speech — the same assumption `tail_erle_db` bakes in.
///
/// Returns `0.0` when no echo-only samples are found or when the echo energy is ~0 (nothing to enhance).
/// A large positive value (e.g. > 12 dB) means strong cancellation.
pub fn erle_db(
    raw_mic: &[f32],
    cleaned: &[f32],
    far_end: &[f32],
    sample_rate: u32,
    near_ref: Option<&[f32]>,
) -> f32 {
    let _ = (far_end, sample_rate); // reserved for far-end-activity windowing; ratio is raw vs cleaned.
    let n = raw_mic.len().min(cleaned.len());
    if n == 0 {
        return 0.0;
    }

    // Build the echo-only mask. With a near-end reference, gate on its instantaneous magnitude vs an RMS
    // threshold; without one, every sample in the span counts (caller asserts the region is echo-only).
    let near_gate = near_ref.map(|r| {
        let m = r.len().min(n);
        let rms = (r[..m].iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / m.max(1) as f64)
            .sqrt() as f32;
        // 5% of RMS: below this the near-end is effectively silent (echo-only).
        0.05 * rms
    });

    let mut echo_energy = 0.0f64;
    let mut residual_energy = 0.0f64;
    for i in 0..n {
        if let (Some(thresh), Some(r)) = (near_gate, near_ref) {
            // Out-of-range reference samples are treated as silent (counted).
            if r.get(i).map(|v| v.abs() > thresh).unwrap_or(false) {
                continue; // near-end active here → not an echo-only sample
            }
        }
        echo_energy += (raw_mic[i] as f64) * (raw_mic[i] as f64);
        residual_energy += (cleaned[i] as f64) * (cleaned[i] as f64);
    }

    if echo_energy <= 1e-12 {
        return 0.0; // no echo energy to enhance
    }
    let residual_energy = residual_energy.max(1e-12);
    (10.0 * (echo_energy / residual_energy).log10()) as f32
}

/// Pearson correlation coefficient between two equal-length slices.
fn normalized_correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f64;
    let ma: f64 = a.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mb: f64 = b.iter().map(|&v| v as f64).sum::<f64>() / n;
    let (mut num, mut da, mut db) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..a.len() {
        let (xa, xb) = (a[i] as f64 - ma, b[i] as f64 - mb);
        num += xa * xb;
        da += xa * xa;
        db += xb * xb;
    }
    let denom = (da.sqrt() * db.sqrt()).max(1e-12);
    (num / denom) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white-ish noise in `[-amp, amp]` (LCG; reproducible, no `rand` dep).
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let u = (s >> 32) as f32 / u32::MAX as f32;
                (u * 2.0 - 1.0) * amp
            })
            .collect()
    }

    #[test]
    fn erle_db_high_when_canceller_works() {
        // Synthetic echo-only fixture: the raw mic is far-end echo at amplitude 0.5; a working canceller
        // knocks it down to a small residual. erle_db must report a large positive dB value.
        let sr = 48_000u32;
        let n = sr as usize; // 1 s, all echo-only (no near-end)
        let far = noise(n, 0.5, 1);
        let raw_mic = far.clone(); // mic == far echo (pure echo)
        // 30 dB cancellation: residual is far attenuated by 10^(-30/20) ≈ 0.0316.
        let cleaned: Vec<f32> = far.iter().map(|&v| v * 0.0316).collect();

        let erle = erle_db(&raw_mic, &cleaned, &far, sr, None);
        assert!(
            erle > 12.0,
            "erle_db {erle:.1} dB <= 12 dB for a working canceller"
        );
        // ~30 dB expected from the 0.0316 scale.
        assert!((erle - 30.0).abs() < 1.5, "erle_db {erle:.1} dB not ~30 dB");
    }

    #[test]
    fn erle_db_near_zero_for_passthrough() {
        // No cancellation (cleaned == raw_mic) ⇒ ratio 1 ⇒ 0 dB.
        let sr = 48_000u32;
        let far = noise(sr as usize, 0.5, 2);
        let raw_mic = far.clone();
        let erle = erle_db(&raw_mic, &raw_mic, &far, sr, None);
        assert!(erle.abs() < 0.01, "passthrough erle_db {erle:.3} dB not ~0");
    }

    #[test]
    fn erle_db_excludes_near_end_active_samples() {
        // raw_mic = echo (cancelled well) over [0, n/2), plus a loud near-end burst over [n/2, n) that the
        // canceller correctly PRESERVES (so cleaned ≈ raw there). With a near_ref marking the burst, those
        // samples are excluded and ERLE reflects only the echo-only first half (high). Without the mask the
        // preserved near-end energy would crush the ratio toward 0 dB.
        let sr = 48_000u32;
        let n = sr as usize;
        let half = n / 2;
        let far = noise(n, 0.5, 3);

        let mut raw_mic = vec![0.0f32; n];
        let mut cleaned = vec![0.0f32; n];
        let mut near_ref = vec![0.0f32; n];
        for i in 0..n {
            if i < half {
                raw_mic[i] = far[i]; // echo-only
                cleaned[i] = far[i] * 0.0316; // ~30 dB cancellation
            } else {
                let v = 0.8 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
                raw_mic[i] = v; // near-end speech, no echo
                cleaned[i] = v; // preserved untouched
                near_ref[i] = v; // reference marks the user talking
            }
        }

        let with_mask = erle_db(&raw_mic, &cleaned, &far, sr, Some(&near_ref));
        assert!(
            with_mask > 12.0,
            "masked erle_db {with_mask:.1} dB <= 12 dB (near-end samples not excluded?)"
        );

        // Without the mask the preserved near-end dominates and ERLE collapses.
        let no_mask = erle_db(&raw_mic, &cleaned, &far, sr, None);
        assert!(
            no_mask < with_mask - 6.0,
            "mask did not exclude near-end: masked {with_mask:.1} dB vs unmasked {no_mask:.1} dB"
        );
    }

    #[test]
    fn perfect_cancellation_scores_high() {
        let sr = 48_000u32;
        let n = sr as usize * 2;
        // Far-end is noise; mic is the same noise (pure echo, no near-end).
        let far: Vec<f32> = (0..n).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        let mic = far.clone();
        // Perfect cancellation: cleaned = silence.
        let clean = vec![0.0f32; n];
        let s = score(&mic, &far, &clean, sr);
        assert!(
            s.echo_suppression > 0.95,
            "echo_suppression={}",
            s.echo_suppression
        );
    }

    #[test]
    fn no_cancellation_scores_low() {
        let sr = 48_000u32;
        let n = sr as usize * 2;
        let far: Vec<f32> = (0..n).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        let mic = far.clone();
        // No cancellation: cleaned = raw mic (still full echo).
        let s = score(&mic, &far, &mic, sr);
        assert!(
            s.echo_suppression < 0.15,
            "echo_suppression={}",
            s.echo_suppression
        );
    }

    #[test]
    fn near_end_preservation_detects_voice_loss() {
        let sr = 48_000u32;
        let n = sr as usize * 2;
        // Mic is a loud tone (user talking), far-end is quiet uncorrelated noise.
        let mic: Vec<f32> = (0..n)
            .map(|i| 0.8 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin())
            .collect();
        let far: Vec<f32> = (0..n).map(|i| 0.1 * ((i as f32 * 0.37).sin())).collect();
        // Good preservation: clean ≈ mic.
        let s_good = score(&mic, &far, &mic, sr);
        // Bad preservation: clean = silence (voice eaten).
        let clean_bad = vec![0.0f32; n];
        let s_bad = score(&mic, &far, &clean_bad, sr);
        assert!(
            s_good.near_end_preservation > s_bad.near_end_preservation + 0.5,
            "good={} bad={}",
            s_good.near_end_preservation,
            s_bad.near_end_preservation
        );
    }
}
