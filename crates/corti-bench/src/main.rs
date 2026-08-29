//! corti-bench — the corti audio-quality benchmark + tuning harness CLI.
//!
//! One Mac, one experiment at a time (see [`lock`]). Three primitives the optimizer composes.
//! `process`: optional offline AEC then local Parakeet/ONNX transcription → `DiarizedTranscript` JSON +
//! timing (wrap in `/usr/bin/time -l` for peak RSS; a no-flags run reproduces today's pipeline byte-for-byte).
//! `clean`: the same deterministic segment cleanup applied to an existing transcript JSON, so a metric
//! sweep over the rules costs no ASR (#149).
//! `aec`: offline `corti_aec::cancel` with all FDAF knobs as flags → cleaned WAV + optional per-signal dumps
//! for the python ERLE scorer. `capture`: serial, tray-guarded 2-track capture (needs the audio-capture grant).
//!
//! See `design/06-benchmark-harness.md`. The AWS backend is intentionally not linked here.

mod audio;
mod lock;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use corti_core::{OwningApp, RecordingMeta};
use corti_transcribe::Transcriber;
use corti_transcribe::segment::{
    CleanupConfig, CleanupStats, SpanEvidence, cleanup, cleanup_with_evidence,
};
use corti_transcribe_local::{LocalConfig, LocalTranscriber};

#[derive(Parser)]
#[command(
    name = "corti-bench",
    version,
    about = "corti audio-quality benchmark + tuning harness"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Skip the global single-experiment lock (debug only; defeats clean memory measurement).
    #[arg(long, global = true)]
    no_lock: bool,
}

#[derive(Subcommand)]
// Clap parses this once into a local and immediately destructures it; boxing a variant is not an
// option anyway (clap's `Subcommand` derive needs the plain `Args` type).
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// (Optional AEC →) local transcription of a WAV → DiarizedTranscript JSON + timing.
    Process(ProcessArgs),
    /// Apply the shipping segment cleanup to an existing DiarizedTranscript JSON (no audio, no ASR).
    Clean(CleanArgs),
    /// Offline AEC of a 2-track WAV with tunable FDAF knobs → cleaned WAV + timing.
    Aec(AecArgs),
    /// Record a 2-track (mic+tap) WAV for N seconds (macOS, needs the audio-capture TCC grant).
    Capture(CaptureArgs),
}

// ---- local transcription knobs (Option = "leave at the shipping default") --------------------------------

#[derive(Args)]
struct ProcessArgs {
    /// Input WAV: 1-channel (pristine far-end) or 2-track (ch0 mic, ch1 tap).
    #[arg(long)]
    input: PathBuf,
    /// Model directory (default: ~/Library/Caches/corti/models).
    #[arg(long)]
    model_dir: Option<PathBuf>,
    /// Run offline AEC before transcription (only affects 2-track input; mono is passed through).
    #[arg(long)]
    aec: bool,
    /// Keep the AEC `-clean.wav` sibling instead of deleting it after transcription.
    #[arg(long)]
    keep_clean: bool,
    /// Suppress the `<stem>-aec-stats.json` per-block echo record that `--aec` writes by default
    /// (#107 diagnostics). The sidecar always survives the run, `--keep-clean` or not.
    #[arg(long)]
    no_aec_stats: bool,

    // --- LocalConfig knobs ---
    #[arg(long, default_value_t = 4)]
    threads: i32,
    /// Diarize the far-end channel into Them 1/2/… (loads pyannote + embedding models).
    #[arg(long)]
    diarize: bool,
    #[arg(long)]
    embedding: Option<String>,
    #[arg(long)]
    diarize_threshold: Option<f32>,
    #[arg(long)]
    diarize_num_clusters: Option<i32>,
    #[arg(long)]
    diarize_min_on: Option<f32>,
    #[arg(long)]
    diarize_min_off: Option<f32>,
    #[arg(long)]
    vad_threshold: Option<f32>,
    #[arg(long)]
    vad_min_silence: Option<f32>,
    /// ASR decoding: "greedy_search" (default) or "modified_beam_search".
    #[arg(long)]
    asr_decoding: Option<String>,
    /// Beam width for modified_beam_search (sherpa max_active_paths).
    #[arg(long)]
    asr_beam: Option<i32>,
    /// Transducer blank penalty.
    #[arg(long)]
    asr_blank_penalty: Option<f32>,
    /// ASR engine for speech regions: "sherpa" (default) or "ggml" (transcribe.cpp/Metal, ADR 0011;
    /// needs a build with `--features ggml`).
    #[arg(long, default_value = "sherpa")]
    asr_engine: String,
    /// GGUF path for --asr-engine ggml (default: <model_dir>/parakeet-tdt-0.6b-v3-Q8_0.gguf).
    #[arg(long)]
    ggml_model: Option<PathBuf>,

    // --- AEC knobs (used only with --aec) ---
    #[command(flatten)]
    aec_knobs: AecKnobs,

    // --- segment cleanup knobs (#149) ---
    #[command(flatten)]
    cleanup_knobs: CleanupKnobs,

    /// Also write the transcript JSON to this file (always printed to stdout regardless).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
struct CleanArgs {
    /// Input DiarizedTranscript JSON (bare, as written by `process --out`).
    #[arg(long)]
    input: PathBuf,
    /// Write the cleaned DiarizedTranscript here (always printed to stdout regardless).
    #[arg(long)]
    out: Option<PathBuf>,
    /// The `<stem>-aec-stats.json` sidecar for the recording this transcript came from, so the echo pass
    /// can use audio evidence as well as text (#149 phase 3b). Without it the run is text-only.
    #[arg(long)]
    aec_stats: Option<PathBuf>,
    #[command(flatten)]
    knobs: CleanupKnobs,
}

/// `--cleanup` / `--no-cleanup` plus the four thresholds, so a sweep can attribute a metric change to one
/// pass. The default is the app default: cleanup ON, shipping thresholds.
#[derive(Args, Clone)]
struct CleanupKnobs {
    /// Run the deterministic segment cleanup (the default; the flag exists to be explicit in a sweep).
    #[arg(long, overrides_with = "no_cleanup")]
    cleanup: bool,
    /// Emit the raw `merge_by_time` timeline with no cleanup at all.
    #[arg(long, overrides_with = "cleanup")]
    no_cleanup: bool,
    /// Skip the cross-channel echo pass.
    #[arg(long)]
    no_echo_drop: bool,
    /// Skip the backchannel pass.
    #[arg(long)]
    no_drop_backchannels: bool,
    #[arg(long)]
    echo_window_seconds: Option<f64>,
    #[arg(long)]
    echo_containment: Option<f64>,
    /// Non-positive disables the fragment-merge pass.
    #[arg(long, allow_negative_numbers = true)]
    merge_gap_seconds: Option<f64>,
    /// dB a mic span may exceed the AEC's own echo estimate and still be dropped as echo. Only bites when
    /// per-block AEC statistics are available; a large negative value (e.g. -1000) switches the audio rule
    /// off while leaving the text rules alone.
    #[arg(long, allow_negative_numbers = true)]
    echo_audio_margin_db: Option<f32>,
}

impl CleanupKnobs {
    /// `None` ⇒ the pass is off entirely (`--no-cleanup`).
    fn config(&self) -> Option<CleanupConfig> {
        if self.no_cleanup {
            return None;
        }
        let d = CleanupConfig::default();
        Some(CleanupConfig {
            echo_drop: !self.no_echo_drop,
            drop_backchannels: !self.no_drop_backchannels,
            echo_window_seconds: self.echo_window_seconds.unwrap_or(d.echo_window_seconds),
            echo_containment: self.echo_containment.unwrap_or(d.echo_containment),
            merge_gap_seconds: self.merge_gap_seconds.unwrap_or(d.merge_gap_seconds),
            echo_audio_margin_db: self.echo_audio_margin_db.unwrap_or(d.echo_audio_margin_db),
        })
    }
}

/// The effective cleanup config + what it removed, for the stdout envelope. `null` when `--no-cleanup`.
/// `audio_evidence` says whether the echo pass actually had the canceller's per-block record to work from,
/// so a sweep can tell "the audio rule found nothing" from "there was nothing to look at".
fn cleanup_json(
    cfg: &CleanupConfig,
    stats: &CleanupStats,
    audio_evidence: bool,
) -> serde_json::Value {
    serde_json::json!({
        "echo_drop": cfg.echo_drop,
        "echo_window_seconds": cfg.echo_window_seconds,
        "echo_containment": cfg.echo_containment,
        "merge_gap_seconds": cfg.merge_gap_seconds,
        "drop_backchannels": cfg.drop_backchannels,
        "echo_audio_margin_db": cfg.echo_audio_margin_db,
        "audio_evidence": audio_evidence,
        "echo_dropped_me": stats.echo_dropped_me,
        "echo_dropped_them": stats.echo_dropped_them,
        "echo_dropped_audio": stats.echo_dropped_audio,
        "merged": stats.merged,
        "backchannels_dropped": stats.backchannels_dropped,
    })
}

/// Read a `-aec-stats.json` sidecar into the block record the echo pass consumes. A schema mismatch is an
/// error here, not a shrug: the harness was told explicitly where to look.
fn load_aec_blocks(path: &std::path::Path) -> Result<Vec<corti_aec::BlockStats>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let record: corti_capture::AecStatsFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing AEC statistics from {}", path.display()))?;
    anyhow::ensure!(
        record.schema_version == corti_capture::AEC_STATS_SCHEMA_VERSION,
        "{} was written by AEC statistics schema {} (this build reads {})",
        path.display(),
        record.schema_version,
        corti_capture::AEC_STATS_SCHEMA_VERSION
    );
    Ok(record.blocks)
}

/// `corti_aec::SpanStats` in the four-field shape `corti-transcribe` asks for (it does not depend on the
/// DSP crate). Mirrors `app/src/transcribe.rs::span_evidence`.
fn span_evidence(blocks: &[corti_aec::BlockStats], start: f64, end: f64) -> Option<SpanEvidence> {
    let s = corti_aec::span_stats(blocks, start, end)?;
    Some(SpanEvidence {
        mic_db: s.mean_mic_db,
        echo_estimate_db: s.mean_echo_estimate_db,
        double_talk_fraction: s.double_talk_fraction,
        blocks: s.blocks,
    })
}

#[derive(Args, Clone)]
struct AecKnobs {
    #[arg(long)]
    filter_len: Option<usize>,
    #[arg(long)]
    mu: Option<f32>,
    #[arg(long)]
    eps: Option<f32>,
    #[arg(long)]
    power_smoothing: Option<f32>,
    #[arg(long)]
    double_talk_ratio: Option<f32>,
    #[arg(long)]
    suppress_residual: Option<f32>,
    /// Disable residual-echo suppression (sets suppress_residual = 0).
    #[arg(long)]
    no_suppress: bool,
    #[arg(long)]
    max_lag_ms: Option<f32>,
}

#[derive(Args)]
struct AecArgs {
    /// Input 2-track WAV: ch0 = mic ("me"), ch1 = far-end tap ("them").
    #[arg(long)]
    input: PathBuf,
    /// Output cleaned 2-track WAV (ch0 = cleaned mic, ch1 = far-end passthrough).
    #[arg(long)]
    out: PathBuf,
    /// Also dump mic.wav / far.wav / clean.wav (mono f32) into this dir for the python ERLE scorer.
    #[arg(long)]
    dump_channels: Option<PathBuf>,
    #[command(flatten)]
    knobs: AecKnobs,
}

#[derive(Args)]
struct CaptureArgs {
    /// Seconds to record.
    #[arg(long)]
    seconds: f64,
    /// Move the finished recording to this path (default: leave it in the recordings cache).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Tap a specific process by PID instead of the global system mix.
    #[arg(long)]
    pid: Option<i32>,
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let _guard = lock::acquire(!cli.no_lock)?;
    match cli.cmd {
        Cmd::Process(a) => run_process(a),
        Cmd::Clean(a) => run_clean(a),
        Cmd::Aec(a) => run_aec(a),
        Cmd::Capture(a) => run_capture(a),
    }
}

/// stderr-only tracing so stdout stays a clean JSON channel for the scorers. `RUST_LOG`/`CORTI_LOG` filter.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var("CORTI_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}

// ---- process -------------------------------------------------------------------------------------------

fn run_process(a: ProcessArgs) -> Result<()> {
    let wall = Instant::now();

    // 1. Optional AEC (mirrors the shipping pipeline: corti_capture::write_clean_wav → StreamingAec).
    //    The harness is the diagnostic tool, so it asks for the per-block echo record by default.
    let aec_cfg = aec_config(&a.aec_knobs);
    let sidecar = if a.no_aec_stats {
        corti_capture::AecStatsSidecar::Off
    } else {
        corti_capture::AecStatsSidecar::Write
    };
    let (asr_input, clean_to_delete, aec_stats) = if a.aec {
        match corti_capture::write_clean_wav_with_options(
            &a.input,
            &aec_cfg,
            corti_aec::configured_lookahead_seconds(),
            sidecar,
        )
        .with_context(|| format!("running AEC on {}", a.input.display()))?
        {
            Some(clean) => {
                let del = if a.keep_clean {
                    None
                } else {
                    Some(clean.clone())
                };
                let stats = (!a.no_aec_stats).then(|| corti_capture::aec_stats_path(&a.input));
                (clean, del, stats)
            }
            // 1-channel input → tap-only, nothing to cancel; transcribe the raw input.
            None => (a.input.clone(), None, None),
        }
    } else {
        (a.input.clone(), None, None)
    };
    let aec_applied = asr_input != a.input;
    if let Some(p) = &aec_stats {
        tracing::info!(target: "corti::bench", path = %p.display(), "AEC block statistics");
    }

    // 2. Local transcription with the requested knobs.
    let cfg = local_config(&a);
    let now = chrono::Local::now();
    let meta = RecordingMeta {
        started_at: now,
        ended_at: Some(now),
        owning_app: OwningApp::unknown(),
        audio_path: asr_input.clone(),
    };
    let asr_t = Instant::now();
    let mut transcript = LocalTranscriber::new(cfg.clone())
        .transcribe(&asr_input, &meta)
        .with_context(|| format!("transcribing {}", asr_input.display()))?;
    let asr_ms = asr_t.elapsed().as_millis();

    // Same call, same order as `app/src/transcribe.rs`: cleanup runs on the merged timeline the backend
    // returned, before anything reads the transcript.
    // The `--aec` run just wrote the sidecar; feed it straight back in as the echo pass's audio evidence
    // so `process --cleanup --aec` matches what the app does with a recording it cleaned (#149 phase 3b).
    let evidence_blocks = match aec_stats.as_deref() {
        Some(path) => Some(load_aec_blocks(path)?),
        None => None,
    };
    let cleanup_report = a.cleanup_knobs.config().map(|cleanup_cfg| {
        let taken = std::mem::take(&mut transcript.segments);
        let (segments, stats) = match evidence_blocks.as_deref() {
            Some(blocks) => {
                let evidence = |start: f64, end: f64| span_evidence(blocks, start, end);
                cleanup_with_evidence(taken, &cleanup_cfg, &[], Some(&evidence))
            }
            None => cleanup(taken, &cleanup_cfg, &[]),
        };
        transcript.segments = segments;
        cleanup_json(&cleanup_cfg, &stats, evidence_blocks.is_some())
    });

    if let Some(p) = clean_to_delete {
        let _ = std::fs::remove_file(p);
    }

    // 3. `--out` gets the BARE DiarizedTranscript (the canonical hypothesis the scorers consume);
    //    stdout gets the full envelope (transcript + exact config + timing) for the results log.
    if let Some(path) = &a.out {
        let bare = serde_json::to_string(&transcript)?;
        std::fs::write(path, &bare).with_context(|| format!("writing {}", path.display()))?;
    }
    let out = serde_json::json!({
        "input": a.input.display().to_string(),
        "backend": "local",
        "aec_applied": aec_applied,
        "aec": if a.aec { Some(aec_json(&aec_cfg)) } else { None },
        "cleanup": cleanup_report,
        "aec_stats": aec_stats.as_ref().map(|p| p.display().to_string()),
        "config": local_config_json(&cfg),
        "n_segments": transcript.segments.len(),
        "wall_ms": wall.elapsed().as_millis(),
        "asr_ms": asr_ms,
        "transcript": transcript,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

// ---- clean ---------------------------------------------------------------------------------------------

/// Re-run the deterministic cleanup over a transcript that already exists. ASR is the expensive part of
/// `process`, and the cleanup rules are pure functions of text and timings, so a threshold sweep — or a
/// before/after score of a note replayed by `bench/scoring/cleanup.py` — never has to decode audio twice.
fn run_clean(a: CleanArgs) -> Result<()> {
    let raw = std::fs::read_to_string(&a.input)
        .with_context(|| format!("reading {}", a.input.display()))?;
    let transcript: corti_core::DiarizedTranscript = serde_json::from_str(&raw)
        .with_context(|| format!("parsing a DiarizedTranscript from {}", a.input.display()))?;
    let before = transcript.segments.len();

    let cfg = a
        .knobs
        .config()
        .context("`clean --no-cleanup` would be a no-op; drop the flag")?;
    let blocks = match a.aec_stats.as_deref() {
        Some(path) => Some(load_aec_blocks(path)?),
        None => None,
    };
    let (segments, stats) = match blocks.as_deref() {
        Some(blocks) => {
            let evidence = |start: f64, end: f64| span_evidence(blocks, start, end);
            cleanup_with_evidence(transcript.segments, &cfg, &[], Some(&evidence))
        }
        None => cleanup(transcript.segments, &cfg, &[]),
    };
    let cleaned = corti_core::DiarizedTranscript::new(segments);

    if let Some(path) = &a.out {
        std::fs::write(path, serde_json::to_string(&cleaned)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "input": a.input.display().to_string(),
            "aec_stats": a.aec_stats.as_ref().map(|p| p.display().to_string()),
            "cleanup": cleanup_json(&cfg, &stats, blocks.is_some()),
            "n_segments_before": before,
            "n_segments": cleaned.segments.len(),
            "transcript": cleaned,
        }))?
    );
    Ok(())
}

fn local_config(a: &ProcessArgs) -> LocalConfig {
    let mut c = LocalConfig {
        num_threads: a.threads,
        diarize_far_end: a.diarize,
        asr_engine: a.asr_engine.clone(),
        ggml_model: a.ggml_model.clone(),
        ..LocalConfig::default()
    };
    if let Some(d) = &a.model_dir {
        c.model_dir = Some(d.clone());
    }
    if let Some(v) = &a.embedding {
        c.embedding_model = v.clone();
    }
    if let Some(v) = a.diarize_threshold {
        c.diarize_threshold = v;
    }
    if let Some(v) = a.diarize_num_clusters {
        c.diarize_num_clusters = v;
    }
    if let Some(v) = a.diarize_min_on {
        c.diarize_min_duration_on = v;
    }
    if let Some(v) = a.diarize_min_off {
        c.diarize_min_duration_off = v;
    }
    if let Some(v) = a.vad_threshold {
        c.vad_threshold = v;
    }
    if let Some(v) = a.vad_min_silence {
        c.vad_min_silence = v;
    }
    if let Some(v) = &a.asr_decoding {
        c.asr_decoding = Some(v.clone());
    }
    if let Some(v) = a.asr_beam {
        c.asr_max_active_paths = Some(v);
    }
    if let Some(v) = a.asr_blank_penalty {
        c.asr_blank_penalty = Some(v);
    }
    c
}

fn local_config_json(c: &LocalConfig) -> serde_json::Value {
    serde_json::json!({
        "num_threads": c.num_threads,
        "diarize_far_end": c.diarize_far_end,
        "embedding_model": c.embedding_model,
        "diarize_threshold": c.diarize_threshold,
        "diarize_num_clusters": c.diarize_num_clusters,
        "diarize_min_duration_on": c.diarize_min_duration_on,
        "diarize_min_duration_off": c.diarize_min_duration_off,
        "vad_threshold": c.vad_threshold,
        "vad_min_silence": c.vad_min_silence,
        "asr_decoding": c.asr_decoding,
        "asr_max_active_paths": c.asr_max_active_paths,
        "asr_blank_penalty": c.asr_blank_penalty,
        "asr_engine": c.asr_engine,
        "ggml_model": c.ggml_model.as_ref().map(|p| p.display().to_string()),
    })
}

// ---- aec -----------------------------------------------------------------------------------------------

fn run_aec(a: AecArgs) -> Result<()> {
    let tt = audio::read_2track(&a.input)?;
    let cfg = aec_config(&a.knobs);

    let t = Instant::now();
    let cleaned = corti_aec::cancel(&tt.mic, &tt.far, tt.sample_rate, &cfg);
    let ms = t.elapsed().as_millis();

    audio::write_2track(&a.out, &cleaned, &tt.far, tt.sample_rate)?;
    if let Some(dir) = &a.dump_channels {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        audio::write_mono(&dir.join("mic.wav"), &tt.mic, tt.sample_rate)?;
        audio::write_mono(&dir.join("far.wav"), &tt.far, tt.sample_rate)?;
        audio::write_mono(&dir.join("clean.wav"), &cleaned, tt.sample_rate)?;
    }

    let secs = tt.mic.len() as f64 / tt.sample_rate as f64;
    let realtime_x = if ms > 0 {
        secs / (ms as f64 / 1000.0)
    } else {
        0.0
    };
    let out = serde_json::json!({
        "input": a.input.display().to_string(),
        "out": a.out.display().to_string(),
        "aec": aec_json(&cfg),
        "frames": tt.mic.len(),
        "sample_rate": tt.sample_rate,
        "audio_secs": secs,
        "wall_ms": ms,
        "realtime_x": realtime_x,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn aec_config(k: &AecKnobs) -> corti_aec::AecConfig {
    let mut c = corti_aec::AecConfig::default();
    if let Some(v) = k.filter_len {
        c.filter_len = v;
    }
    if let Some(v) = k.mu {
        c.mu = v;
    }
    if let Some(v) = k.eps {
        c.eps = v;
    }
    if let Some(v) = k.power_smoothing {
        c.power_smoothing = v;
    }
    if let Some(v) = k.double_talk_ratio {
        c.double_talk_ratio = v;
    }
    if k.no_suppress {
        c.suppress_residual = 0.0;
    } else if let Some(v) = k.suppress_residual {
        c.suppress_residual = v;
    }
    if let Some(v) = k.max_lag_ms {
        c.max_lag_ms = v;
    }
    c
}

fn aec_json(c: &corti_aec::AecConfig) -> serde_json::Value {
    serde_json::json!({
        "filter_len": c.filter_len,
        "mu": c.mu,
        "eps": c.eps,
        "power_smoothing": c.power_smoothing,
        "double_talk_ratio": c.double_talk_ratio,
        "suppress_residual": c.suppress_residual,
        "max_lag_ms": c.max_lag_ms,
    })
}

// ---- capture -------------------------------------------------------------------------------------------

fn run_capture(a: CaptureArgs) -> Result<()> {
    lock::assert_tray_not_running()?;
    let app = OwningApp::unknown();
    let rec = corti_capture::Recorder::start(&app, a.pid)
        .context("starting 2-track capture (is the audio-capture permission granted?)")?;
    tracing::info!(target: "corti::bench", seconds = a.seconds, "capturing 2-track audio");
    std::thread::sleep(std::time::Duration::from_secs_f64(a.seconds));
    let mut path = rec.finish().context("finalizing capture")?;

    if let Some(dest) = &a.out {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::rename(&path, dest)
            .with_context(|| format!("moving capture to {}", dest.display()))?;
        path = dest.clone();
    }

    // Non-silence check (Phase-0 go/no-go): per-channel RMS.
    let tt = audio::read_2track(&path).ok();
    let (mic_rms, tap_rms, frames, sr) = match tt {
        Some(t) => (
            audio::rms(&t.mic),
            audio::rms(&t.far),
            t.mic.len(),
            t.sample_rate,
        ),
        None => (0.0, 0.0, 0, 0),
    };
    let out = serde_json::json!({
        "path": path.display().to_string(),
        "seconds": a.seconds,
        "sample_rate": sr,
        "frames": frames,
        "mic_rms": mic_rms,
        "tap_rms": tap_rms,
        "mic_silent": mic_rms < 1e-5,
        "tap_silent": tap_rms < 1e-5,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
