//! In-app diagnostics console: an in-memory ring buffer of `tracing` events plus the Tauri commands the
//! console webview calls (issue #76). The on-disk log sink lives alongside it (see [`init_file_layer`]).
//!
//! Two diagnostics `tracing_subscriber::Layer`s feed off the same event stream:
//! - [`ConsoleLayer`] captures each event into a size-capped [`ConsoleBuffer`] the UI polls over `invoke`.
//! - the file layer (built in [`init_file_layer`]) writes to a **daily-rolling** appender wrapped in a
//!   **non-blocking** writer, so the single pipeline worker thread never blocks on disk I/O.
//!
//! An optional third layer emits independent local schema traces. Every diagnostics layer has its own
//! `EnvFilter` and excludes that exact target; see ADR 0016.
//!
//! ## corti conventions (differ from the claria reference this was ported from)
//! - [`ConsoleEntry`] derives serde **only** — no `specta`. The TypeScript mirror is hand-written in
//!   `app/ui/src/lib/api.ts` and must track this struct's field names/types.
//! - The "Save log…" dialog uses host-side `rfd` (no tauri dialog plugin is wired in corti).
//!
//! ## Deadlock discipline
//! [`ConsoleLayer::on_event`] builds the entry **before** touching the buffer and holds the buffer mutex
//! only for the brief `push` (a `VecDeque` push + byte accounting). Nothing under that lock emits a
//! `tracing` event, so a log line fired from inside the lock can never re-enter and deadlock.

use std::{
    collections::VecDeque,
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::{Event, Metadata, Subscriber, field::Visit};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{Layer, layer::Context};

use crate::offline_trace;

/// Maximum approximate byte size of the ring buffer (10 MB). Oldest entries are evicted FIFO past this.
const MAX_BYTES: usize = 10 * 1024 * 1024;

/// A single log entry captured by the console ring buffer.
///
/// Serde-only (no `specta`): the TypeScript `ConsoleEntry` interface in `app/ui/src/lib/api.ts` mirrors
/// these four fields by hand — keep them in sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleEntry {
    /// ISO-8601 UTC timestamp (`jiff::Timestamp::now()`, e.g. `2026-06-18T17:04:05.123Z`).
    pub timestamp: String,
    /// Formatted `tracing` level (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`).
    pub level: String,
    /// The event's `target` (usually the module path).
    pub target: String,
    /// The event's `message` field, or a `field=value` concatenation when no explicit message was set.
    pub message: String,
}

impl ConsoleEntry {
    /// Approximate byte size of this entry for buffer-cap accounting (string payloads only).
    fn byte_size(&self) -> usize {
        self.timestamp.len() + self.level.len() + self.target.len() + self.message.len()
    }
}

impl fmt::Display for ConsoleEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {}: {}",
            self.timestamp, self.level, self.target, self.message
        )
    }
}

/// Thread-safe, size-capped ring buffer of log entries. Cloning shares the same underlying buffer (it is an
/// `Arc`), so the [`ConsoleLayer`] and the managed Tauri state observe the same entries.
#[derive(Debug, Clone)]
pub struct ConsoleBuffer {
    inner: Arc<Mutex<BufferInner>>,
}

#[derive(Debug)]
struct BufferInner {
    entries: VecDeque<ConsoleEntry>,
    total_bytes: usize,
}

impl Default for ConsoleBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BufferInner {
                entries: VecDeque::new(),
                total_bytes: 0,
            })),
        }
    }

    /// Append an entry, evicting the oldest entries FIFO until the buffer is back under [`MAX_BYTES`].
    ///
    /// Recovers from a poisoned lock instead of panicking: a logging path must never bring down the app.
    fn push(&self, entry: ConsoleEntry) {
        let entry_size = entry.byte_size();
        let mut buf = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        buf.entries.push_back(entry);
        buf.total_bytes += entry_size;

        while buf.total_bytes > MAX_BYTES {
            if let Some(removed) = buf.entries.pop_front() {
                buf.total_bytes = buf.total_bytes.saturating_sub(removed.byte_size());
            } else {
                break;
            }
        }
    }

    /// Returns a clone of all buffered entries (oldest first).
    pub fn entries(&self) -> Vec<ConsoleEntry> {
        let buf = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        buf.entries.iter().cloned().collect()
    }

    /// Formats all buffered entries as a plain-text string, one line per entry.
    pub fn to_text(&self) -> String {
        let buf = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        for entry in &buf.entries {
            out.push_str(&entry.to_string());
            out.push('\n');
        }
        out
    }
}

/// A `tracing_subscriber::Layer` that captures events into a [`ConsoleBuffer`].
pub struct ConsoleLayer {
    buffer: ConsoleBuffer,
}

impl ConsoleLayer {
    pub fn new(buffer: ConsoleBuffer) -> Self {
        Self { buffer }
    }
}

/// Visitor that extracts the `message` field from a tracing event, falling back to a `field=value`
/// concatenation of the event's other fields when no explicit `message` is present.
struct MessageVisitor {
    message: String,
}

impl MessageVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else if self.message.is_empty() {
            // Fall back to the first field if no explicit "message".
            self.message = format!("{}={:?}", field.name(), value);
        } else {
            self.message
                .push_str(&format!(" {}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if self.message.is_empty() {
            self.message = format!("{}={}", field.name(), value);
        } else {
            self.message
                .push_str(&format!(" {}={}", field.name(), value));
        }
    }
}

impl<S: Subscriber> Layer<S> for ConsoleLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Build the whole entry first; the buffer mutex is taken only for the brief `push` below.
        let metadata = event.metadata();
        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);

        let entry = ConsoleEntry {
            timestamp: jiff::Timestamp::now().to_string(),
            level: metadata.level().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message,
        };

        self.buffer.push(entry);
    }
}

/// Build the on-disk log layer: a **daily-rolling** appender under `<data_dir>/logs/` wrapped in a
/// **non-blocking** writer so the pipeline worker thread never blocks on disk I/O.
///
/// Returns the boxed layer (to add to the registry) and the [`WorkerGuard`]. **The guard must be kept alive
/// for the process lifetime** (store it in managed state / a `static`): dropping it stops the background
/// writer thread and the final buffered lines are lost. Returns `None` if the log directory can't be
/// created (e.g. a read-only sandbox) — the in-memory console keeps working without a file sink.
pub fn init_file_layer<S>() -> Option<(Box<dyn Layer<S> + Send + Sync>, WorkerGuard)>
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let dir = logs_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[corti] could not create log dir {}: {e}", dir.display());
        return None;
    }

    let appender = tracing_appender::rolling::daily(&dir, "corti.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false) // no escape codes in a file
        .boxed();

    Some((layer, guard))
}

/// The diagnostics log directory: `<corti-queue data_dir>/logs/` (honors `$CORTI_DATA_DIR`). `None` if the
/// data dir can't be resolved (e.g. no `$HOME`).
pub fn logs_dir() -> Option<PathBuf> {
    corti_queue::data_dir().ok().map(|d| d.join("logs"))
}

/// Owns both non-blocking writers for exactly as long as the selected app mode runs.
pub struct SubscriberGuards {
    diagnostics: Option<WorkerGuard>,
    offline: offline_trace::Guard,
}

impl SubscriberGuards {
    /// Gracefully drain offline records, then drop the diagnostics writer guard. Headless callers invoke
    /// this before `process::exit`; the tray invokes it after `app.run` returns.
    pub fn shutdown(self) {
        let Self {
            diagnostics,
            offline,
        } = self;
        offline.shutdown();
        drop(diagnostics);
    }
}

/// RUST_LOG wins; fall back to CORTI_LOG; then preserve the historical `info` default. A fresh value is
/// installed on every diagnostics layer so these directives cannot gate the independent offline layer.
fn diagnostics_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_env("CORTI_LOG"))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// The schema target is never formatted as a log line or copied into the diagnostics UI.
fn is_diagnostic(metadata: &Metadata<'_>) -> bool {
    metadata.target() != offline_trace::TARGET
}

/// Install the tray subscriber. Each diagnostics sink owns both its own `EnvFilter` and an exact-target
/// exclusion; the optional shared layer is composed directly onto the registry and remains independent of
/// `RUST_LOG`/`CORTI_LOG`. Installation uses `try_init`, so a pre-existing subscriber disables offline
/// storage without panicking or preventing Corti startup.
pub fn init_tracing() -> (ConsoleBuffer, SubscriberGuards) {
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let buffer = ConsoleBuffer::new();
    let mut prepared = offline_trace::prepare();
    let trace_layer = prepared.layer.take();

    let console_layer = ConsoleLayer::new(buffer.clone())
        .with_filter(diagnostics_env_filter())
        .with_filter(filter_fn(is_diagnostic));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(diagnostics_env_filter())
        .with_filter(filter_fn(is_diagnostic));
    let (file_layer, diagnostics) = match init_file_layer() {
        Some((layer, guard)) => (
            Some(
                layer
                    .with_filter(diagnostics_env_filter())
                    .with_filter(filter_fn(is_diagnostic)),
            ),
            Some(guard),
        ),
        None => (None, None),
    };

    let installed = tracing_subscriber::registry()
        .with(trace_layer)
        .with(console_layer)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();
    let offline = prepared.finish(installed);

    (
        buffer,
        SubscriberGuards {
            diagnostics,
            offline,
        },
    )
}

/// Install the headless subscriber: an independently filtered stderr diagnostics layer plus the optional
/// local JSONL layer. Both are composed in one `try_init`; the returned guards must be shut down before the
/// caller's unconditional `process::exit`.
pub fn init_cli_tracing() -> SubscriberGuards {
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let mut prepared = offline_trace::prepare();
    let trace_layer = prepared.layer.take();
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(diagnostics_env_filter())
        .with_filter(filter_fn(is_diagnostic));
    let installed = tracing_subscriber::registry()
        .with(trace_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();

    SubscriberGuards {
        diagnostics: None,
        offline: prepared.finish(installed),
    }
}

// ---------------------------------------------------------------------------
// Tauri commands (registered in `main.rs`'s invoke_handler). The console webview calls these by name.
// ---------------------------------------------------------------------------

/// Snapshot of the in-memory console ring buffer (oldest first).
#[tauri::command]
pub fn get_console_logs(console: State<'_, ConsoleBuffer>) -> Vec<ConsoleEntry> {
    console.entries()
}

/// The same buffer rendered as one log line per entry (for copy-to-clipboard in the UI).
#[tauri::command]
pub fn get_console_logs_text(console: State<'_, ConsoleBuffer>) -> String {
    console.to_text()
}

/// Open a native "Save log…" panel and write the current console text to the chosen file. Returns `Ok(true)`
/// if a file was written, `Ok(false)` if the user cancelled, `Err` on a write failure.
#[tauri::command]
pub fn save_console_logs(console: State<'_, ConsoleBuffer>) -> Result<bool, String> {
    let text = console.to_text();
    let date = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();

    let path = rfd::FileDialog::new()
        .set_file_name(format!("corti-diagnostics-{date}.log"))
        .add_filter("Log files", &["log", "txt"])
        .save_file();

    match path {
        Some(p) => {
            std::fs::write(&p, text).map_err(|e| e.to_string())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::Layer;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[test]
    fn diagnostics_preserve_normal_format_and_exclude_exact_offline_target() {
        let buffer = ConsoleBuffer::new();
        let layer = ConsoleLayer::new(buffer.clone())
            .with_filter(tracing_subscriber::EnvFilter::new("trace"))
            .with_filter(filter_fn(is_diagnostic));
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "corti::test", count = 3_u64, "unchanged diagnostic");
            tracing::info!(
                target: offline_trace::TARGET,
                operation = "must-not-reach-diagnostics"
            );
        });

        let entries = buffer.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, "corti::test");
        assert_eq!(entries[0].message, "unchanged diagnostic count=3");
    }
}
