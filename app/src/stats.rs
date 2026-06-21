//! In-app performance stats: a COUNT-capped ring buffer of periodic [`StatsSnapshot`]s plus the Tauri
//! command the Resources panel polls. The buffer is DEDICATED — stats never go through the console log
//! ring (that ring is a 10 MB byte-capped FIFO; a 1 Hz heartbeat must not evict real log events).
//!
//! Mirrors the proven `console::ConsoleBuffer` discipline (Arc<Mutex<..>>, Clone, a poison-recovering
//! lock, FIFO eviction past a cap) but COUNT-capped, not byte-capped.
//!
//! ## corti conventions
//! - Serde-only (no `specta`); the TypeScript mirror is hand-written in `app/ui/src/lib/api.ts`.
//! - The sampler runs on its OWN `corti-stats` thread — never on the `corti-pipeline` worker (guardrail 9:
//!   the pipeline thread owns the Queue and runs transcription serially; it must never be blocked).
//! - Self-introspection (memory + per-thread CPU) is via `libc` proc_* syscalls; zero mach paths needed.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

use crate::imp::AppState; // AppState lives in `mod imp` (made pub(crate) in main.rs).
use crate::settings::SharedConfig;

/// How many snapshots to retain (newest evicts oldest past this). At 1 Hz this is ~5 min of history.
const MAX_SNAPSHOTS: usize = 300;
/// How many completed pipeline-stage samples to retain.
const MAX_STAGES: usize = 512;

// ---------------------------------------------------------------------------
// libc FFI: process memory + per-thread CPU. All signatures/consts verified against libc-0.2.186
// except PROC_PIDLISTTHREADS, which is ABSENT from the crate and declared here (XNU canonical = 6).
// ---------------------------------------------------------------------------

/// `proc_pidinfo` flavor that returns a packed array of u64 thread ids. NOT defined in libc-0.2.186
/// (verified absent); the value is the stable XNU constant from <sys/proc_info.h>.
const PROC_PIDLISTTHREADS: libc::c_int = 6;

/// Process memory: returns (phys_footprint_bytes, resident_size_bytes) from ONE `proc_pid_rusage`
/// call (RUSAGE_INFO_V2 covers both fields). `None` on failure.
fn read_mem() -> Option<(u64, u64)> {
    unsafe {
        let pid = libc::getpid(); // pid_t = i32
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        // proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut rusage_info_t);
        // rusage_info_t = *mut c_void, so cast a *mut rusage_info_v2 THROUGH it (double-indirection
        // naming footgun: this casts the v2 pointer AS a rusage_info_t value — correct).
        let rc = libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
        );
        if rc != 0 {
            return None;
        }
        Some((info.ri_phys_footprint, info.ri_resident_size))
    }
}

/// Per-thread CPU: returns Vec<(name, cpu_permille)>. `pth_cpu_usage` is i32 in 1/10-of-a-percent.
/// Tolerant of the thread set changing between the list/per-tid calls (TOCTOU): skips any tid whose
/// PROC_PIDTHREADINFO returns < full struct size.
fn read_threads() -> Vec<(String, i32)> {
    let mut out = Vec::new();
    unsafe {
        let pid = libc::getpid();

        // 1) Enumerate thread ids. NB: proc_pidinfo's LIST flavors do NOT support the "(null, 0) returns
        //    the needed size" idiom — they return only the bytes actually written into the buffer (0 for a
        //    0-size buffer). So over-provision a Vec<u64> and grow until the returned count is strictly
        //    less than capacity, which proves the list wasn't truncated. (Verified empirically: the
        //    (null,0) sizing call returns 0, yielding an empty thread table.)
        let mut cap: usize = 128;
        let tids = loop {
            let mut buf: Vec<u64> = vec![0u64; cap];
            let written = libc::proc_pidinfo(
                pid,
                PROC_PIDLISTTHREADS,
                0,
                buf.as_mut_ptr() as *mut libc::c_void,
                (cap * std::mem::size_of::<u64>()) as libc::c_int,
            );
            if written <= 0 {
                return out;
            }
            let got = (written as usize) / std::mem::size_of::<u64>();
            // got < cap => the buffer had spare room, so we have every tid. Cap at 8192 as a backstop.
            if got < cap || cap >= 8192 {
                buf.truncate(got);
                break buf;
            }
            cap *= 2;
        };

        // 2) Per-tid PROC_PIDTHREADINFO into a proc_threadinfo.
        let ti_size = std::mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
        for &tid in &tids {
            let mut ti: libc::proc_threadinfo = std::mem::zeroed();
            let n = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTHREADINFO, // = 5
                tid,
                &mut ti as *mut libc::proc_threadinfo as *mut libc::c_void,
                ti_size,
            );
            if n < ti_size {
                continue; // vanished/partial thread (also rejects 0/negative) — drop silently.
            }
            // pth_name is [c_char; 64]. XNU NUL-terminates within the buffer, but decode defensively:
            // scan at most 64 bytes for a NUL so this can NEVER read past the fixed array, even if the
            // kernel ever wrote 64 non-NUL bytes (proc_threadinfo is the last struct field).
            let raw: &[u8] =
                std::slice::from_raw_parts(ti.pth_name.as_ptr() as *const u8, ti.pth_name.len());
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let name = String::from_utf8_lossy(&raw[..end]).into_owned();
            out.push((name, ti.pth_cpu_usage));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Serde types (hand-mirrored in app/ui/src/lib/api.ts — keep field names in lockstep).
// ---------------------------------------------------------------------------

/// One OS thread at sample time. `cpu_pct` is the kernel's scheduler-maintained CPU estimate as a
/// percentage (per-core). NB: Mach `cpu_usage` is an exponentially-decaying average, not an exact
/// interval measure — it lags and smooths bursts, so treat it as a recent-load gauge, not a precise %.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadStat {
    /// Thread name, e.g. "corti-pipeline", "corti-stats", "corti-detect" (may be empty for unnamed threads).
    pub name: String,
    /// Scheduler-averaged CPU share, 0..100+ (per-core); kernel permille / 10.0.
    pub cpu_pct: f64,
}

/// One completed pipeline stage (recorded by the pipeline worker via [`StatsBuffer::record_stage`]).
/// No `Deserialize`: the `&'static str` fields are not deserializable, and the UI path is write-only.
#[derive(Debug, Clone, Serialize)]
pub struct StageSample {
    /// ISO-8601 UTC (jiff::Timestamp::now().to_string()).
    pub timestamp: String,
    /// "transcribe" (AEC + backend) | "file" (vagus filing).
    pub stage: &'static str,
    /// Low-cardinality backend label: "aws" | "local" | "none" | "vagus".
    pub backend: &'static str,
    /// Wall-clock duration of the stage in milliseconds.
    pub duration_ms: u64,
}

/// A single 1 Hz sample of app/runtime state. No `Deserialize`: it embeds `Vec<StageSample>`
/// (which is Serialize-only), and the UI only ever reads it.
#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    /// ISO-8601 UTC timestamp (matches ConsoleEntry's `jiff::Timestamp::now().to_string()`).
    pub timestamp: String,
    /// Active transcription backend label ("AWS Transcribe" / "Parakeet (local)" / "none").
    pub backend: String,
    /// Whether a detector (mic-triggered) capture is in flight.
    pub detector_recording: bool,
    /// Whether a manual webinar capture is in flight.
    pub webinar_recording: bool,
    /// Resident "footprint"/phys memory, mebibytes (proc_pid_rusage ri_phys_footprint / 1 MiB).
    pub phys_mb: f64,
    /// Resident set size, mebibytes (proc_pid_rusage ri_resident_size / 1 MiB).
    pub rss_mb: f64,
    /// Per-thread CPU at this sample.
    pub threads: Vec<ThreadStat>,
}

/// One coherent read of the stats buffer: the 1 Hz memory/thread `history` (oldest first) plus the
/// process-global `stages` timings (oldest first). Stages live ONCE here, not duplicated per snapshot.
/// No `Deserialize` (embeds `StageSample`, whose `&'static str` fields aren't deserializable; read-only).
#[derive(Debug, Clone, Serialize)]
pub struct StatsReport {
    pub history: Vec<StatsSnapshot>,
    pub stages: Vec<StageSample>,
}

// ---------------------------------------------------------------------------
// The dedicated COUNT-capped buffer. Holds BOTH the 1 Hz snapshots and the pipeline stage samples.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct BufferInner {
    snapshots: VecDeque<StatsSnapshot>,
    stages: VecDeque<StageSample>,
}

/// Thread-safe, COUNT-capped ring buffer. Cloning shares the same `Arc`, so the sampler thread, the
/// pipeline worker (stage timing), and the managed Tauri state all observe the same data.
#[derive(Debug, Clone)]
pub struct StatsBuffer {
    inner: Arc<Mutex<BufferInner>>,
}

impl Default for StatsBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BufferInner {
                snapshots: VecDeque::with_capacity(MAX_SNAPSHOTS),
                stages: VecDeque::with_capacity(MAX_STAGES),
            })),
        }
    }

    /// Append a snapshot, evicting the oldest FIFO once over [`MAX_SNAPSHOTS`]. Poison-recovering:
    /// a background sampler must never bring down the app.
    pub fn push(&self, snap: StatsSnapshot) {
        let mut buf = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        buf.snapshots.push_back(snap);
        while buf.snapshots.len() > MAX_SNAPSHOTS {
            buf.snapshots.pop_front();
        }
    }

    /// Record one finished pipeline stage. Called on the single pipeline worker thread. Never panics.
    pub fn record_stage(&self, stage: &'static str, dur: Duration, backend: &'static str) {
        let sample = StageSample {
            timestamp: jiff::Timestamp::now().to_string(),
            stage,
            backend,
            duration_ms: dur.as_millis() as u64,
        };
        let mut buf = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        buf.stages.push_back(sample);
        while buf.stages.len() > MAX_STAGES {
            buf.stages.pop_front();
        }
    }

    /// One coherent view of the buffer under a SINGLE lock acquisition: the snapshot history and the
    /// process-global stage timings (both oldest first). Stages are returned once — never duplicated
    /// into each snapshot — so the get_stats payload stays small and the lock is held only briefly.
    pub fn report(&self) -> StatsReport {
        let buf = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        StatsReport {
            history: buf.snapshots.iter().cloned().collect(),
            stages: buf.stages.iter().cloned().collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Build one snapshot from currently-observable app state + process self-introspection. Reads the
/// active backend from `SharedConfig` (poison-recovering, unlike settings::get_config's plain .unwrap())
/// and the recording flags from AppState.
fn sample(app: &tauri::AppHandle, cfg: &SharedConfig) -> StatsSnapshot {
    let backend = {
        let c = cfg.lock().unwrap_or_else(|e| e.into_inner());
        // Use backend_name() (feature-gated, returns "none" for an uncompiled backend) — NOT a bare
        // match on transcribe_backend, which would be non-exhaustive under single-feature builds.
        c.backend_name().to_string()
    };

    let (detector_recording, webinar_recording) = app
        .try_state::<AppState>()
        .map(|s| {
            use std::sync::atomic::Ordering;
            (
                s.detector_recording.load(Ordering::Relaxed),
                s.webinar_recording.load(Ordering::Relaxed),
            )
        })
        .unwrap_or((false, false));

    let (phys_mb, rss_mb) = read_mem()
        .map(|(phys, rss)| (phys as f64 / 1_048_576.0, rss as f64 / 1_048_576.0))
        .unwrap_or((0.0, 0.0));

    let threads = read_threads()
        .into_iter()
        .map(|(name, permille)| ThreadStat {
            name,
            cpu_pct: (permille as f64) / 10.0,
        })
        .collect();

    StatsSnapshot {
        timestamp: jiff::Timestamp::now().to_string(),
        backend,
        detector_recording,
        webinar_recording,
        phys_mb,
        rss_mb,
        threads,
    }
}

/// Spawn the 1 Hz `corti-stats` sampler. Runs OFF the pipeline thread. Holds clones of the shared
/// `StatsBuffer`, the app handle, and `SharedConfig`.
pub fn spawn_sampler(app: tauri::AppHandle, cfg: SharedConfig, buffer: StatsBuffer) {
    std::thread::Builder::new()
        .name("corti-stats".to_string())
        .spawn(move || loop {
            let started = Instant::now();
            let snap = sample(&app, &cfg);
            buffer.push(snap);
            // Steady 1 Hz cadence regardless of sample cost.
            let elapsed = started.elapsed();
            if elapsed < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_secs(1) - elapsed);
            }
        })
        .expect("spawning corti-stats sampler thread");
}

// ---------------------------------------------------------------------------
// Tauri command (registered in main.rs's invoke_handler). Mirrors get_console_logs.
// ---------------------------------------------------------------------------

/// One coherent read of the in-memory stats buffer: memory/thread history + global stage timings.
#[tauri::command]
pub fn get_stats(stats: State<'_, StatsBuffer>) -> StatsReport {
    stats.report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `proc_pid_rusage` must return a non-zero footprint + RSS for our own process. Guards against a
    /// silently-wrong flavor/struct/cast that would compile but read zeros (the panel would show 0.0 MiB).
    #[test]
    fn read_mem_returns_nonzero_footprint() {
        let (phys, rss) = read_mem().expect("proc_pid_rusage(self) should succeed");
        eprintln!(
            "read_mem: phys={:.1} MiB, rss={:.1} MiB",
            phys as f64 / 1_048_576.0,
            rss as f64 / 1_048_576.0
        );
        assert!(phys > 0, "phys_footprint should be > 0, got {phys}");
        assert!(rss > 0, "resident_size should be > 0, got {rss}");
    }

    /// A thread named via `Builder::name` (the exact mechanism `corti-pipeline` et al. use) must appear by
    /// that name in `read_threads`, with a sane permille. Exercises the whole tid-enumerate → per-tid →
    /// bounded `pth_name` decode path against the live kernel.
    #[test]
    fn read_threads_sees_named_thread() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("corti-stats-selftest".to_string())
            .spawn(move || {
                let _ = release_rx.recv(); // stay alive (and named) until the scan is done
            })
            .expect("spawn named test thread");
        std::thread::sleep(Duration::from_millis(100)); // let the kernel register the name

        let threads = read_threads();
        eprintln!("read_threads saw {} threads: {:?}", threads.len(), threads);

        assert!(!threads.is_empty(), "expected at least one thread");
        assert!(
            threads.iter().any(|(n, _)| n == "corti-stats-selftest"),
            "the Builder-named thread should be visible by name"
        );
        for (_, permille) in &threads {
            assert!(
                (0..=100_000).contains(permille),
                "permille out of sane range: {permille}"
            );
        }

        let _ = release_tx.send(());
        let _ = worker.join();
    }
}
