//! Live mic-in-use probe: prints a line every time the microphone starts/stops being used, with
//! best-effort attribution of the owning app.
//!
//! Run it, then start/stop a Zoom or Slack huddle (or just open Photo Booth) to watch the transitions:
//!
//! ```sh
//! cargo run -p corti-coreaudio --bin probe
//! ```
//!
//! This is the manual verification for the detection milestone — the foundation the `corti-detect` state
//! machine is built on.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use corti_coreaudio::{MicMonitor, listener, process};

    let device = listener::default_input_device()?;
    println!("corti probe — default input device id: {device}");
    println!("initial mic-in-use: {}", listener::is_running(device)?);
    if listener::is_running(device)? {
        let owner = process::mic_owner();
        println!(
            "  current owner: {} ({:?}, pid {:?})",
            owner.app.name, owner.app.bundle_id, owner.pid
        );
    }
    println!("watching… (Ctrl-C to quit)\n");

    let _monitor = MicMonitor::new(|running| {
        if running {
            let owner = process::mic_owner();
            println!(
                "● MIC ON   owner: {} ({:?}, pid {:?})",
                owner.app.name, owner.app.bundle_id, owner.pid
            );
        } else {
            println!("○ mic off");
        }
    })?;

    // Keep the process alive so the HAL delivers listener callbacks. CFRunLoopRun is the canonical way to
    // park while CoreAudio notifications are processed.
    core_foundation::runloop::CFRunLoop::run_current();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("corti is macOS-only");
}
