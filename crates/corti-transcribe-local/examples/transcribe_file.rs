//! Transcribe a recorded 2-track WAV fully offline with the local (Parakeet/ONNX) backend and print the
//! diarized Markdown. Plays the app's role: ch0 → `Me`, ch1 → diarized far-end speakers (`Them 1/2/…`).
//!
//! ```sh
//! # models default to ~/Library/Caches/corti/models (fetch once with fetch-models.sh):
//! cargo run -p corti-transcribe-local --example transcribe_file -- /path/to/recording.wav
//! # or point at an explicit model dir, and pick the provider:
//! CORTI_LOCAL_PROVIDER=cpu \
//!   cargo run -p corti-transcribe-local --example transcribe_file -- recording.wav /path/to/models
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use corti_core::{OwningApp, RecordingMeta};
use corti_transcribe::Transcriber;
use corti_transcribe_local::{LocalConfig, LocalTranscriber};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let audio = PathBuf::from(
        args.next()
            .context("usage: transcribe_file <recording.wav> [model_dir]")?,
    );
    let model_dir = args.next().map(PathBuf::from);

    let cfg = LocalConfig {
        model_dir,
        provider: std::env::var("CORTI_LOCAL_PROVIDER").unwrap_or_else(|_| "cpu".to_string()),
        ..LocalConfig::default()
    };

    // The detector would supply real metadata; the local backend only needs the audio path.
    let meta = RecordingMeta {
        started_at: chrono::Local::now(),
        ended_at: Some(chrono::Local::now()),
        owning_app: OwningApp::unknown(),
        audio_path: audio.clone(),
    };

    eprintln!(
        "transcribing {} locally (provider={})…",
        audio.display(),
        cfg.provider
    );
    let transcript = LocalTranscriber::new(cfg).transcribe(&audio, &meta)?;
    print!("{}", transcript.to_markdown());
    Ok(())
}
