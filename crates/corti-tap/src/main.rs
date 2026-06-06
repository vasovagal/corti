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
    #[cfg(feature = "inbox")]
    inbox: bool,
}

impl Args {
    fn parse() -> Self {
        let mut label = "System audio".to_string();
        let mut no_mic = false;
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
                #[cfg(feature = "inbox")]
                "--inbox" => inbox = true,
                "--help" | "-h" => {
                    eprintln!("usage: corti-tap [--label <name>] [--no-mic] [--inbox]");
                    eprintln!("  --label <name>  recording label (default: \"System audio\")");
                    eprintln!(
                        "  --no-mic        tap-only 1-channel WAV; mic never opened (no orange dot)"
                    );
                    #[cfg(feature = "inbox")]
                    eprintln!("  --inbox         transcribe + file as a vagus note");
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    eprintln!("usage: corti-tap [--label <name>] [--no-mic] [--inbox]");
                    std::process::exit(1);
                }
            }
        }
        Self {
            label,
            no_mic,
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
