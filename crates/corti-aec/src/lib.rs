//! Offline acoustic echo cancellation for corti — **shell** (currently a pass-through).
//!
//! Removes speaker bleed from the mic track when the user is on speakers: the mic hears the far-end after a
//! room delay + coloring, so this is NOT `mic − far_end` but an **adaptive NLMS filter** that learns the
//! echo path. Runs offline (post-call), so it can use a long filter and double-talk handling. Full
//! algorithm + invariants in `design/04-corti-aec.md`.
//!
//! Invariant: the raw mic + far-end tracks are always preserved upstream; this produces an *additional*
//! cleaned track. Headphones remain the cleanest path; AEC is the speaker-user safety net.

/// NLMS configuration.
#[derive(Debug, Clone)]
pub struct AecConfig {
    /// Adaptive-filter length in taps (e.g. 4096 ≈ 85 ms of echo at 48 kHz).
    pub filter_len: usize,
    /// Step size (≈ 0.1–0.5).
    pub mu: f32,
    /// Regularization to avoid divide-by-zero on quiet far-end (≈ 1e-6).
    pub eps: f32,
}

impl Default for AecConfig {
    fn default() -> Self {
        Self {
            filter_len: 4096,
            mu: 0.3,
            eps: 1e-6,
        }
    }
}

/// Remove far-end echo from `mic`, returning the cleaned near-end signal (same length as `mic`).
///
/// **Shell:** currently returns the mic signal unchanged (pass-through) so the pipeline can wire it in
/// safely before the DSP lands. Replace with the NLMS implementation from `design/04-corti-aec.md`.
pub fn cancel(mic: &[f32], _far_end: &[f32], _sample_rate: u32, _cfg: &AecConfig) -> Vec<f32> {
    // TODO(design/04-corti-aec.md): delay-align, NLMS adaptive filter with double-talk gating.
    mic.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_preserves_mic() {
        let mic = vec![0.1, -0.2, 0.3];
        let far = vec![1.0, 1.0, 1.0];
        assert_eq!(cancel(&mic, &far, 48_000, &AecConfig::default()), mic);
    }
}
