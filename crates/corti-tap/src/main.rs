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

#[cfg(feature = "inbox")]
fn preflight_inbox() -> anyhow::Result<()> {
    use anyhow::Context;
    std::env::var("CORTI_AWS_BUCKET").context("--inbox requires CORTI_AWS_BUCKET")?;
    corti_vagus::Vagus::discover().context("--inbox requires vagus on PATH")?;
    Ok(())
}

#[cfg(feature = "inbox")]
fn file_to_inbox(label: &str, wav: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use aws_config::BehaviorVersion;
    use corti_core::{OwningApp, RecordingMeta};
    use corti_transcribe::Transcriber;
    use corti_transcribe_aws::{AwsOptions, AwsTranscriber};
    use corti_vagus::Vagus;

    let bucket = std::env::var("CORTI_AWS_BUCKET").unwrap();
    let language = std::env::var("CORTI_LANGUAGE").unwrap_or_else(|_| "en-US".to_string());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let sdk = rt.block_on(async { aws_config::defaults(BehaviorVersion::latest()).load().await });

    let meta = RecordingMeta {
        started_at: chrono::Local::now(),
        ended_at: Some(chrono::Local::now()),
        owning_app: OwningApp {
            bundle_id: None,
            name: label.to_string(),
        },
        audio_path: wav.to_path_buf(),
    };

    eprintln!("transcribing via AWS Transcribe…");
    let opts = AwsOptions {
        language,
        ..AwsOptions::new(bucket)
    };
    let transcript = AwsTranscriber::new(&sdk, opts)
        .transcribe(wav, &meta)
        .context("transcription failed")?;

    eprintln!("filing note…");
    let vagus = Vagus::discover()?;
    let note = vagus.file_recording(&meta, &transcript)?;
    eprintln!("note: {}", note.display());

    Ok(())
}
