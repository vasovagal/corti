//! Prove the vagus shell-out end to end: file a canned diarized transcript as a real note.
//!
//! Run against a throwaway vault so it never touches your real second brain:
//!
//! ```sh
//! VAGUS_VAULT=/tmp/corti-demo-vault VAGUS_DATA_DIR=/tmp/corti-demo-data \
//!   cargo run -p corti-vagus --example file_canned
//! ```

use std::path::PathBuf;

use corti_core::{DiarizedTranscript, OwningApp, RecordingMeta, Speaker, TranscriptSegment};
use corti_vagus::Vagus;

fn main() -> anyhow::Result<()> {
    let meta = RecordingMeta {
        started_at: chrono::Local::now(),
        ended_at: Some(chrono::Local::now()),
        owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
        audio_path: PathBuf::from("/tmp/corti/recordings/demo.wav"),
    };

    let transcript = DiarizedTranscript::new(vec![
        TranscriptSegment {
            speaker: Speaker::Me,
            start: 0.0,
            end: 3.2,
            text: "Morning everyone, let's kick off the Viasat sync.".into(),
        },
        TranscriptSegment {
            speaker: Speaker::Other("Speaker 1".into()),
            start: 3.6,
            end: 7.1,
            text: "Sounds good — I pushed the NDP terraform changes last night.".into(),
        },
        TranscriptSegment {
            speaker: Speaker::Me,
            start: 7.5,
            end: 10.0,
            text: "Nice. Any blockers on the prod rollout?".into(),
        },
    ]);

    let vagus = Vagus::discover()?;
    let path = vagus.file_recording(&meta, &transcript)?;
    println!("filed note: {}", path.display());
    Ok(())
}
