//! Parallel parameter sweep for AEC tuning. Runs every combination in the parameter grid against every
//! input recording via rayon, scores each, and ranks the results.

use rayon::prelude::*;

use crate::score::{AecScore, score};
use crate::{AecConfig, cancel};

/// A single recording pre-loaded into memory: name + deinterleaved mic/tap + sample rate.
pub struct Recording {
    pub name: String,
    pub mic: Vec<f32>,
    pub tap: Vec<f32>,
    pub sample_rate: u32,
}

/// Per-config aggregate result across all input recordings.
#[derive(Debug, Clone)]
pub struct TuneResult {
    pub config: AecConfig,
    pub per_file: Vec<(String, AecScore)>,
    pub mean: AecScore,
}

/// Build the default parameter grid (240 combos).
pub fn default_grid() -> Vec<AecConfig> {
    let filter_lens = [4096, 8192, 12288, 16384];
    let mus = [0.1, 0.2, 0.3, 0.5, 0.7];
    let dtr = [1.0, 1.5, 2.0, 3.0];
    let ps = [0.8, 0.9, 0.95];

    let mut grid = Vec::with_capacity(filter_lens.len() * mus.len() * dtr.len() * ps.len());
    for &filter_len in &filter_lens {
        for &mu in &mus {
            for &double_talk_ratio in &dtr {
                for &power_smoothing in &ps {
                    grid.push(AecConfig {
                        filter_len,
                        mu,
                        eps: 1e-6,
                        power_smoothing,
                        double_talk_ratio,
                        passes: 2,
                        suppress_residual: 2.5,
                    });
                }
            }
        }
    }
    grid
}

/// Run a parameter sweep: for every (recording, config) pair, run AEC and score the result.
/// Returns results sorted by mean combined score (best first).
///
/// Each (recording, config) pair is independent and runs in parallel via rayon.
pub fn sweep(recordings: &[Recording], grid: &[AecConfig]) -> Vec<TuneResult> {
    // Build all (config_idx, recording_idx) pairs for flat parallelism.
    let pairs: Vec<(usize, usize)> = (0..grid.len())
        .flat_map(|ci| (0..recordings.len()).map(move |ri| (ci, ri)))
        .collect();

    // Run all pairs in parallel. Each produces (config_idx, recording_name, score).
    let results: Vec<(usize, String, AecScore)> = pairs
        .par_iter()
        .map(|&(ci, ri)| {
            let cfg = &grid[ci];
            let rec = &recordings[ri];
            let clean = cancel(&rec.mic, &rec.tap, rec.sample_rate, cfg);
            let s = score(&rec.mic, &rec.tap, &clean, rec.sample_rate);
            (ci, rec.name.clone(), s)
        })
        .collect();

    // Group by config and compute mean scores.
    let mut by_config: Vec<Vec<(String, AecScore)>> = vec![Vec::new(); grid.len()];
    for (ci, name, s) in results {
        by_config[ci].push((name, s));
    }

    let mut tune_results: Vec<TuneResult> = by_config
        .into_iter()
        .enumerate()
        .map(|(ci, per_file)| {
            let mean = mean_score(&per_file);
            TuneResult {
                config: grid[ci].clone(),
                per_file,
                mean,
            }
        })
        .collect();

    tune_results.sort_by(|a, b| b.mean.combined.total_cmp(&a.mean.combined));
    tune_results
}

fn mean_score(scores: &[(String, AecScore)]) -> AecScore {
    if scores.is_empty() {
        return AecScore {
            echo_suppression: 0.0,
            near_end_preservation: 0.0,
            combined: 0.0,
        };
    }
    let n = scores.len() as f32;
    let sup: f32 = scores.iter().map(|(_, s)| s.echo_suppression).sum();
    let pres: f32 = scores.iter().map(|(_, s)| s.near_end_preservation).sum();
    let comb: f32 = scores.iter().map(|(_, s)| s.combined).sum();
    AecScore {
        echo_suppression: sup / n,
        near_end_preservation: pres / n,
        combined: comb / n,
    }
}
