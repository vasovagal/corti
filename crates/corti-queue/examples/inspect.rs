//! Inspect the corti durable queue: list every tracked recording and what would resume after a crash.
//!
//! ```sh
//! cargo run -p corti-queue --example inspect
//! # or point at a throwaway DB:
//! CORTI_DATA_DIR=/tmp/corti-data cargo run -p corti-queue --example inspect
//! ```

use anyhow::Result;
use corti_queue::Queue;

fn main() -> Result<()> {
    let queue = Queue::open()?;

    let all = queue.all()?;
    if all.is_empty() {
        println!("queue is empty");
        return Ok(());
    }

    println!("{} recording(s):", all.len());
    for job in &all {
        println!(
            "  {:<22} {:<22} {}  →  {}",
            format!("{:?}", job.status),
            job.owning_app,
            job.id,
            job.audio_path.display()
        );
        if let Some(err) = &job.error {
            println!("      error: {err}");
        }
        if let Some(secs) = job.transcribe_secs {
            println!("      transcribed in {secs:.1}s");
        }
    }

    let resumable = queue.resumable()?;
    println!(
        "\n{} resumable (non-terminal) on next startup:",
        resumable.len()
    );
    for job in &resumable {
        println!("  {} — {:?}", job.id, job.status);
    }
    Ok(())
}
