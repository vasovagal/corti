//! The single-experiment lock + tray guard.
//!
//! The harness runs on ONE Mac: one mic/speaker, one CPU, and peak-RSS measurement that only means
//! anything in isolation. So at most one experiment executes at a time, enforced *inside the binary* (not an
//! outer shell wrapper) via an advisory `flock` — there is no "forgot to wrap it" footgun. The lock is held
//! for the lifetime of the returned guard and released on drop (fd close).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// A held advisory lock. Drop releases it.
pub struct ExperimentLock {
    _file: File,
}

fn lock_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CORTI_BENCH_LOCK") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("corti-bench.lock")
}

/// Acquire the global single-experiment lock (blocking — concurrent callers queue rather than fail). Pass
/// `enabled = false` (the `--no-lock` escape hatch) to skip it for debugging.
pub fn acquire(enabled: bool) -> Result<Option<ExperimentLock>> {
    if !enabled {
        return Ok(None);
    }
    let path = lock_path();
    let file =
        File::create(&path).with_context(|| format!("creating lock file {}", path.display()))?;
    // LOCK_EX blocks until the lock is free; the kernel releases it if we crash (advisory, fd-scoped).
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        bail!(
            "failed to acquire the bench lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    tracing::debug!(target: "corti::bench", path = %path.display(), "acquired experiment lock");
    Ok(Some(ExperimentLock { _file: file }))
}

/// Hard-refuse to run while the Corti tray app is alive. Capture would fight it for the mic + aggregate
/// device, and any `--redo`/`--input`-style work shares on-disk writers (queue.db + clean-wav) — the
/// documented two-writer corruption hazard. Quit the tray first.
pub fn assert_tray_not_running() -> Result<()> {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("Corti.app/Contents/MacOS/corti")
        .output();
    if let Ok(o) = out
        && !o.stdout.is_empty()
    {
        let pids = String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the Corti tray app is running (pid {pids}). Quit it before capturing/processing — capture \
             fights it for the mic, and they share on-disk writers (queue.db + clean-wav corruption hazard)."
        );
    }
    Ok(())
}
