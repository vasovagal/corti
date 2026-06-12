//! Scoring functions for AEC quality: measure how well a cleaned signal suppresses echo while preserving
//! the near-end (user's voice). Used by the tuning sweep to rank parameter configurations.

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
