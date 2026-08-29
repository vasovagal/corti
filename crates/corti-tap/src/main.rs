//! Force-tap CLI: capture system audio (global tap) to a 2-track WAV on demand.
//!
//! ```sh
//! corti-tap                              # record speakers, Ctrl-C to stop, prints WAV path
//! corti-tap --label "K8s webinar"        # custom label (used in the note title if --inbox)
//! corti-tap --inbox                      # also transcribe + file as a vagus note
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("corti-tap is macOS-only (Apple Silicon, latest macOS).");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn run() -> anyhow::Result<()> {
    use anyhow::Context;
    use corti_capture::Recorder;
    use corti_core::OwningApp;

    let args = Args::parse();

    #[cfg(feature = "inbox")]
    if args.live && args.inbox {
        anyhow::bail!("--live and --inbox are mutually exclusive");
    }

    if args.live {
        #[cfg(feature = "live")]
        return run_live(&args);
        #[cfg(not(feature = "live"))]
        anyhow::bail!(
            "--live requires a build with `--features live` (it links the local ASR stack)"
        );
    }

    #[cfg(feature = "inbox")]
    if args.inbox {
        preflight_inbox()?;
    }

    let app = OwningApp {
        bundle_id: None,
        name: args.label.clone(),
    };

    // `--no-mic` takes the tap-only path so the mic is never opened (no orange "mic in use" dot).
    let recorder = if args.no_mic {
        Recorder::start_tap_only(&app, None)
    } else {
        Recorder::start(&app, None)
    }
    .context("starting capture (is the audio-capture TCC permission granted?)")?;
    eprintln!("recording system audio — Ctrl-C to stop");
    eprintln!("  WAV: {}", recorder.output_path().display());

    install_sigint_handler();
    while !STOP.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    eprintln!("\nstopping capture…");
    let wav = if args.no_mic {
        recorder.finish_tap_only().context("finishing capture")?
    } else {
        recorder.finish().context("finishing capture")?
    };
    eprintln!("wrote {}", wav.display());

    #[cfg(feature = "inbox")]
    if args.inbox {
        file_to_inbox(&args.label, &wav)?;
    }

    Ok(())
}

struct Args {
    label: String,
    no_mic: bool,
    /// Print a live transcript as the call proceeds (requires the `live` build feature).
    live: bool,
    #[cfg(feature = "inbox")]
    inbox: bool,
}

impl Args {
    fn parse() -> Self {
        let mut label = "System audio".to_string();
        let mut no_mic = false;
        let mut live = false;
        #[cfg(feature = "inbox")]
        let mut inbox = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--label" => {
                    label = args.next().unwrap_or_else(|| {
                        eprintln!("--label requires a value");
                        std::process::exit(1);
                    });
                }
                "--no-mic" => no_mic = true,
                "--live" => live = true,
                #[cfg(feature = "inbox")]
                "--inbox" => inbox = true,
                "--help" | "-h" => {
                    eprintln!("usage: corti-tap [--label <name>] [--no-mic] [--live] [--inbox]");
                    eprintln!("  --label <name>  recording label (default: \"System audio\")");
                    eprintln!(
                        "  --no-mic        tap-only 1-channel WAV; mic never opened (no orange dot)"
                    );
                    eprintln!(
                        "  --live          print a live transcript as the call proceeds (needs a build"
                    );
                    eprintln!(
                        "                  with `--features live`). The mic is echo-cancelled first, so"
                    );
                    eprintln!(
                        "                  AEC lookahead delays the first mic words — default 5 s, tune"
                    );
                    eprintln!(
                        "                  with CORTI_AEC_LOOKAHEAD_SECS. Skipped under --no-mic."
                    );
                    #[cfg(feature = "inbox")]
                    eprintln!("  --inbox         transcribe + file as a vagus note");
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    eprintln!("usage: corti-tap [--label <name>] [--no-mic] [--live] [--inbox]");
                    std::process::exit(1);
                }
            }
        }
        Self {
            label,
            no_mic,
            live,
            #[cfg(feature = "inbox")]
            inbox,
        }
    }
}

fn install_sigint_handler() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
    }
}

extern "C" fn sigint_handler(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// `--live`: bounded capture tee → optional streaming AEC (mic) → local live transcription, printing words to
/// stdout as they arrive. The mic channel is echo-cancelled and labelled `Me`; the tap channel is a second
/// transcriber labelled `Them`. AEC warm-up (`CORTI_AEC_LOOKAHEAD_SECS`, default 5 s) delays the first mic
/// words.
#[cfg(all(feature = "live", target_os = "macos"))]
fn run_live(args: &Args) -> anyhow::Result<()> {
    use anyhow::Context;
    use corti_aec::{AecConfig, StreamingAec};
    use corti_capture::{CaptureChunk, CaptureTee, Recorder};
    use corti_core::OwningApp;
    use corti_transcribe_local::{LocalConfig, LocalTranscriber};
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};
    use std::time::Duration;

    // Load the resident engine first: a missing model cache fails fast, before the mic is ever opened.
    let engine = LocalTranscriber::new(LocalConfig::default())
        .live_engine()
        .context("loading local models (run crates/corti-transcribe-local/fetch-models.sh)")?;
    let mut mic_live = if args.no_mic {
        None
    } else {
        Some(engine.channel()?)
    };
    let mut them_live = engine.channel()?;

    // Bounded, lossy tee: ~32 chunks ≈ 2.7 s of slack at 48 kHz before it starts dropping.
    let (tx, rx) = sync_channel::<CaptureChunk>(32);
    let tee = CaptureTee::new(tx);
    let dropped = tee.dropped_counter();

    let app = OwningApp {
        bundle_id: None,
        name: args.label.clone(),
    };
    let recorder = if args.no_mic {
        Recorder::start_tap_only_with_tee(&app, None, tee)
    } else {
        Recorder::start_with_tee(&app, None, tee)
    }
    .context("starting capture (is the audio-capture TCC permission granted?)")?;
    let rate = recorder.sample_rate();

    // Streaming AEC on the mic (skipped under --no-mic). Honors CORTI_AEC_LOOKAHEAD_SECS.
    let mut aec = if args.no_mic {
        None
    } else {
        Some(StreamingAec::new(rate, AecConfig::default()))
    };

    eprintln!("recording — live transcript (Ctrl-C to stop)");
    eprintln!("  WAV: {}", recorder.output_path().display());
    if aec.is_some() {
        eprintln!(
            "  note: AEC lookahead delays the first mic words (~CORTI_AEC_LOOKAHEAD_SECS, default 5 s)"
        );
    }

    install_sigint_handler();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                process_live_chunk(chunk, aec.as_mut(), mic_live.as_mut(), &mut them_live, rate);
                // Chunks arrive continuously (~85 ms), so the Timeout arm rarely fires — check STOP here
                // too or Ctrl-C could never break the loop.
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    eprintln!("\nstopping capture…");
    let wav = if args.no_mic {
        recorder.finish_tap_only()
    } else {
        recorder.finish()
    }
    .context("finishing capture")?;

    // Drain chunks teed after STOP (stopping the recorder drops the tee sender).
    while let Ok(chunk) = rx.try_recv() {
        process_live_chunk(chunk, aec.as_mut(), mic_live.as_mut(), &mut them_live, rate);
    }

    // Flush the AEC tail into the mic transcriber, then finish both channels.
    if let (Some(aec), Some(mic)) = (aec.take(), mic_live.as_mut()) {
        let tail = aec.finish();
        if !tail.is_empty() {
            mic.push(&tail, rate);
        }
    }
    if let Some(mic) = mic_live.as_mut() {
        print_words("Me", &mic.finish());
    }
    print_words("Them", &them_live.finish());

    let n = dropped.load(Ordering::Relaxed);
    if n > 0 {
        eprintln!(
            "warning: dropped {n} live tee chunk(s) — transcript may have gaps (consumer fell behind)"
        );
    }
    eprintln!("wrote {}", wav.display());
    Ok(())
}

/// Feed one downmixed capture chunk to the live transcribers, printing any words that fell out. The mic side
/// is echo-cancelled first (empty output while the filter warms); the tap side is transcribed raw.
#[cfg(all(feature = "live", target_os = "macos"))]
fn process_live_chunk(
    chunk: corti_capture::CaptureChunk,
    aec: Option<&mut corti_aec::StreamingAec>,
    mic_live: Option<&mut corti_transcribe_local::LiveTranscriber>,
    them_live: &mut corti_transcribe_local::LiveTranscriber,
    rate: u32,
) {
    if let Some(mic) = mic_live {
        // Gate the AEC/mic side on the actual data, not `--no-mic`: a mic-mode capture can still deliver an
        // empty (or length-mismatched) mic channel, and `StreamingAec::push` asserts `mic.len() == far.len()`.
        let clean = match aec {
            Some(aec) if !chunk.mic.is_empty() && chunk.mic.len() == chunk.tap.len() => {
                aec.push(&chunk.mic, &chunk.tap) // cleaned mic (empty while warming up)
            }
            Some(_) => Vec::new(), // no usable mic data this chunk — skip so the assert stays unreachable
            None => chunk.mic.clone(),
        };
        if !clean.is_empty() {
            mic.push(&clean, rate);
        }
        if let Some(words) = mic.poll_words() {
            print_words("Me", &words);
        }
    }
    them_live.push(&chunk.tap, rate);
    if let Some(words) = them_live.poll_words() {
        print_words("Them", &words);
    }
}

/// Print each recognized word on its own line, `[Label   sec] text`, flushing so it appears live.
#[cfg(all(feature = "live", target_os = "macos"))]
fn print_words(label: &str, words: &[corti_transcribe_local::Word]) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for w in words {
        let _ = writeln!(out, "[{label} {:>7.2}] {}", w.start, w.text);
    }
    let _ = out.flush();
}

/// Which backend `--inbox` transcribes with. Variants track the compiled features so a slim build cannot
/// name a backend it does not link.
#[cfg(feature = "inbox")]
#[derive(Clone, Copy)]
enum InboxBackend {
    #[cfg(feature = "inbox-aws")]
    Aws,
    #[cfg(feature = "inbox-local")]
    Local,
}

/// Resolve the backend from `CORTI_TRANSCRIBE_BACKEND`, reusing the app's `aws` | `local` grammar. Unset
/// prefers `local` exactly as the app does, so the CLI transcribes with no cloud account out of the box.
#[cfg(feature = "inbox")]
fn inbox_backend() -> anyhow::Result<InboxBackend> {
    match std::env::var("CORTI_TRANSCRIBE_BACKEND")
        .ok()
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some("aws") => {
            #[cfg(feature = "inbox-aws")]
            return Ok(InboxBackend::Aws);
            #[cfg(not(feature = "inbox-aws"))]
            anyhow::bail!("CORTI_TRANSCRIBE_BACKEND=aws needs a build with `--features inbox-aws`");
        }
        Some("local") => {
            #[cfg(feature = "inbox-local")]
            return Ok(InboxBackend::Local);
            #[cfg(not(feature = "inbox-local"))]
            anyhow::bail!(
                "CORTI_TRANSCRIBE_BACKEND=local needs a build with `--features inbox-local`"
            );
        }
        Some(other) => {
            anyhow::bail!("unknown CORTI_TRANSCRIBE_BACKEND `{other}` (expected `aws` or `local`)")
        }
        None => {
            #[cfg(feature = "inbox-local")]
            return Ok(InboxBackend::Local);
            #[cfg(all(not(feature = "inbox-local"), feature = "inbox-aws"))]
            return Ok(InboxBackend::Aws);
            #[cfg(all(not(feature = "inbox-local"), not(feature = "inbox-aws")))]
            anyhow::bail!("--inbox needs a build with `inbox-aws` or `inbox-local`");
        }
    }
}

#[cfg(feature = "inbox")]
fn preflight_inbox() -> anyhow::Result<()> {
    use anyhow::Context;

    match inbox_backend()? {
        // The local backend resolves its own model cache; only the cloud path needs configuration here.
        #[cfg(feature = "inbox-local")]
        InboxBackend::Local => {}
        #[cfg(feature = "inbox-aws")]
        InboxBackend::Aws => {
            std::env::var("CORTI_AWS_BUCKET")
                .context("--inbox with the AWS backend requires CORTI_AWS_BUCKET")?;
        }
    }
    corti_vagus::Vagus::discover().context("--inbox requires vagus on PATH")?;
    Ok(())
}

#[cfg(feature = "inbox")]
fn file_to_inbox(label: &str, wav: &std::path::Path) -> anyhow::Result<()> {
    use corti_core::{OwningApp, RecordingMeta};
    use corti_vagus::Vagus;

    let meta = RecordingMeta {
        started_at: chrono::Local::now(),
        ended_at: Some(chrono::Local::now()),
        owning_app: OwningApp {
            bundle_id: None,
            name: label.to_string(),
        },
        audio_path: wav.to_path_buf(),
    };

    let (transcript, provenance) = match inbox_backend()? {
        #[cfg(feature = "inbox-local")]
        InboxBackend::Local => transcribe_local(wav, &meta)?,
        #[cfg(feature = "inbox-aws")]
        InboxBackend::Aws => transcribe_aws(wav, &meta)?,
    };

    eprintln!("filing note…");
    let vagus = Vagus::discover()?;
    let note = vagus.file_recording(&meta, &transcript, &provenance)?;
    eprintln!("note: {}", note.display());

    Ok(())
}

/// Shared provenance fields for both backends: this CLI never runs AEC and always files a completed file.
/// `segment_cleanup` mirrors `app/src/provenance.rs`, so a note filed by `corti-tap --inbox` describes the
/// same rule set as one filed by the app.
#[cfg(feature = "inbox")]
fn base_configuration(
    language: Option<&str>,
    cleanup: &corti_transcribe::segment::CleanupConfig,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut configuration = std::collections::BTreeMap::new();
    if let Some(language) = language {
        configuration.insert(
            "language".into(),
            serde_json::Value::String(language.to_string()),
        );
    }
    configuration.insert(
        "input".into(),
        serde_json::Value::String("completed_recording".into()),
    );
    configuration.insert(
        "aec".into(),
        serde_json::json!({ "enabled": false, "mode": "disabled" }),
    );
    configuration.insert("segment_cleanup".into(), segment_cleanup_json(cleanup));
    configuration
}

/// The effective cleanup rules, or `"off"`. Same shape as the app's `segment_cleanup` provenance value.
#[cfg(feature = "inbox")]
fn segment_cleanup_json(cleanup: &corti_transcribe::segment::CleanupConfig) -> serde_json::Value {
    if cleanup.is_noop() {
        return serde_json::Value::String("off".into());
    }
    serde_json::json!({
        "rules": corti_transcribe::segment::CLEANUP_RULES_VERSION,
        "echo_drop": cleanup.echo_drop,
        "echo_window_seconds": cleanup.echo_window_seconds,
        "echo_containment": cleanup.echo_containment,
        "merge_gap_seconds": cleanup.merge_gap_seconds,
        "drop_backchannels": cleanup.drop_backchannels,
        "echo_audio_margin_db": cleanup.echo_audio_margin_db,
        // This CLI always files a completed recording, so there is no live publication to be early for.
        "live_early_drop": false,
        // `--inbox` never runs AEC, so there are no per-block statistics for the echo pass to consult:
        // every echo drop in a note filed here came from the text rules alone.
        "audio_evidence": false,
    })
}

/// Run the shipping segment cleanup over a freshly transcribed timeline. `corti-tap` has no `config.toml`,
/// so it uses the crate defaults — the same rules the app ships with.
#[cfg(feature = "inbox")]
fn clean_segments(
    transcript: &mut corti_core::DiarizedTranscript,
    cleanup: &corti_transcribe::segment::CleanupConfig,
) {
    let (segments, stats) =
        corti_transcribe::segment::cleanup(std::mem::take(&mut transcript.segments), cleanup, &[]);
    transcript.segments = segments;
    eprintln!(
        "segment cleanup: {} echo (me) / {} echo (them) / {} merged / {} backchannel",
        stats.echo_dropped_me, stats.echo_dropped_them, stats.merged, stats.backchannels_dropped
    );
}

#[cfg(feature = "inbox-aws")]
fn transcribe_aws(
    wav: &std::path::Path,
    meta: &corti_core::RecordingMeta,
) -> anyhow::Result<(
    corti_core::DiarizedTranscript,
    corti_vagus::provenance::TranscriptProvenance,
)> {
    use anyhow::Context;
    use aws_config::BehaviorVersion;
    use corti_transcribe::Transcriber;
    use corti_transcribe_aws::{AwsOptions, AwsTranscriber};
    use corti_vagus::provenance::{
        GenerationMode, ModelIdentity, TranscriptModels, TranscriptProvenance,
    };

    let bucket = std::env::var("CORTI_AWS_BUCKET").unwrap();
    let language = std::env::var("CORTI_LANGUAGE").unwrap_or_else(|_| "en-US".to_string());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let sdk = rt.block_on(async { aws_config::defaults(BehaviorVersion::latest()).load().await });

    eprintln!("transcribing via AWS Transcribe…");
    let opts = AwsOptions {
        language: language.clone(),
        ..AwsOptions::new(bucket)
    };
    let mut transcript = AwsTranscriber::new(&sdk, opts)
        .transcribe(wav, meta)
        .context("transcription failed")?;

    let cleanup = corti_transcribe::segment::CleanupConfig::default();
    clean_segments(&mut transcript, &cleanup);

    let mut configuration = base_configuration(Some(&language), &cleanup);
    configuration.insert(
        "speaker_attribution".into(),
        serde_json::Value::String("channel_identification_for_multichannel".into()),
    );
    let provenance = TranscriptProvenance::new(
        GenerationMode::Batch,
        "aws",
        TranscriptModels {
            asr: ModelIdentity::new("aws/transcribe-default", None::<String>),
            vad: None,
            diarization: None,
            speaker_embedding: None,
        },
        configuration,
    );
    Ok((transcript, provenance))
}

#[cfg(feature = "inbox-local")]
fn transcribe_local(
    wav: &std::path::Path,
    meta: &corti_core::RecordingMeta,
) -> anyhow::Result<(
    corti_core::DiarizedTranscript,
    corti_vagus::provenance::TranscriptProvenance,
)> {
    use anyhow::Context;
    use corti_transcribe::Transcriber;
    use corti_transcribe_local::{LocalConfig, LocalTranscriber, models};
    use corti_vagus::provenance::{
        GenerationMode, ModelIdentity, TranscriptModels, TranscriptProvenance,
    };

    // The same `CORTI_LOCAL_*` knobs the app reads; everything else stays at the shipping defaults.
    let asr_engine = std::env::var("CORTI_LOCAL_ASR_ENGINE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| LocalConfig::default().asr_engine);
    let cfg = LocalConfig {
        model_dir: std::env::var_os("CORTI_LOCAL_MODEL_DIR").map(std::path::PathBuf::from),
        asr_engine: asr_engine.clone(),
        ..LocalConfig::default()
    };
    let wants_ggml = asr_engine == models::GGML_ASR_ENGINE;

    eprintln!(
        "transcribing locally via Parakeet ({})…",
        if wants_ggml { "Metal" } else { "CPU" }
    );
    let mut transcript = LocalTranscriber::new(cfg)
        .transcribe(wav, meta)
        .context("transcription failed")?;

    let cleanup = corti_transcribe::segment::CleanupConfig::default();
    clean_segments(&mut transcript, &cleanup);

    let mut configuration = base_configuration(None, &cleanup);
    configuration.insert(
        "asr_engine".into(),
        serde_json::Value::String(asr_engine.clone()),
    );
    configuration.insert(
        "speaker_attribution".into(),
        serde_json::Value::String("channels".into()),
    );
    let provenance = TranscriptProvenance::new(
        GenerationMode::Batch,
        "local",
        TranscriptModels {
            asr: ModelIdentity::new(
                models::PARAKEET_MODEL_ID,
                Some(if wants_ggml {
                    models::GGML_FILE.to_string()
                } else {
                    models::PARAKEET_DIR.to_string()
                }),
            ),
            vad: Some(ModelIdentity::new(
                models::VAD_MODEL_ID,
                Some(models::VAD_FILE),
            )),
            diarization: None,
            speaker_embedding: None,
        },
        configuration,
    );
    Ok((transcript, provenance))
}
