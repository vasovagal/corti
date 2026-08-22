//! Compile-time boundary for Corti's optional local JSONL tracing integration.
//!
//! Every constructor exposes only schema-v1 low-cardinality values. Application code never passes raw
//! errors, paths, recording/job identifiers, transcript text, app identity, or cloud configuration into
//! this module. With `offline-tracing` disabled the wrapper is zero-sized, does not consult activation
//! inputs, and expands to no `tracing` callsites.

#[cfg(feature = "offline-tracing")]
use std::time::Duration;

use tracing_subscriber::Registry;

/// The exact target excluded from Corti's independent diagnostics layers.
pub const TARGET: &str = "vasovagal::trace";

pub type TraceLayer = Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync>;

/// Deferred shared-crate initialization, kept type-stable when the integration is compiled out.
pub struct Prepared {
    pub layer: Option<TraceLayer>,
    #[cfg(feature = "offline-tracing")]
    pending: Option<vasovagal_tracing::PendingSession>,
}

impl Prepared {
    /// Tell the shared crate whether the composed global subscriber was installed.
    pub fn finish(self, installed: bool) -> Guard {
        #[cfg(feature = "offline-tracing")]
        {
            let mut this = self;
            let pending = this
                .pending
                .take()
                .expect("offline tracing pending session is consumed once");
            let (guard, _status) = pending.finish(installed);
            Guard { inner: Some(guard) }
        }
        #[cfg(not(feature = "offline-tracing"))]
        {
            let _ = installed;
            Guard {}
        }
    }
}

/// Resolve Corti activation without installing a subscriber.
///
/// The compiled-out branch deliberately calls no shared-crate API and reads no environment/configuration.
pub fn prepare() -> Prepared {
    #[cfg(feature = "offline-tracing")]
    {
        let prepared = vasovagal_tracing::prepare::<Registry>(vasovagal_tracing::InitOptions {
            service: vasovagal_tracing::Service::Corti,
            package_version: env!("CARGO_PKG_VERSION"),
            cli: vasovagal_tracing::CliActivation::Unspecified,
        });
        Prepared {
            layer: prepared.layer,
            pending: Some(prepared.pending),
        }
    }
    #[cfg(not(feature = "offline-tracing"))]
    {
        Prepared { layer: None }
    }
}

/// Owns the optional shared writer guard.
pub struct Guard {
    #[cfg(feature = "offline-tracing")]
    inner: Option<vasovagal_tracing::TraceGuard>,
}

impl Guard {
    /// Drain and close the session before a headless `process::exit` or after the tray event loop returns.
    pub fn shutdown(self) {
        #[cfg(feature = "offline-tracing")]
        {
            let mut this = self;
            if let Some(guard) = this.inner.take() {
                guard.shutdown(Duration::from_secs(2));
            }
        }
    }
}

/// A dispatcher captured at a thread-spawn boundary. The compiled-out value is zero-sized.
pub struct Dispatch {
    #[cfg(feature = "offline-tracing")]
    inner: tracing::Dispatch,
}

impl Dispatch {
    pub fn capture() -> Self {
        #[cfg(feature = "offline-tracing")]
        {
            Self {
                inner: tracing::dispatcher::get_default(Clone::clone),
            }
        }
        #[cfg(not(feature = "offline-tracing"))]
        {
            Self {}
        }
    }

    pub fn with_default<T>(&self, work: impl FnOnce() -> T) -> T {
        #[cfg(feature = "offline-tracing")]
        {
            tracing::dispatcher::with_default(&self.inner, work)
        }
        #[cfg(not(feature = "offline-tracing"))]
        {
            work()
        }
    }
}

/// Optional schema-v1 span. No callsite exists in a compiled-out build.
#[derive(Clone)]
pub struct Span {
    #[cfg(feature = "offline-tracing")]
    inner: tracing::Span,
}

impl Span {
    #[cfg(feature = "offline-tracing")]
    fn from_inner(inner: tracing::Span) -> Self {
        Self { inner }
    }

    #[cfg(feature = "offline-tracing")]
    pub(crate) fn inner(&self) -> &tracing::Span {
        &self.inner
    }

    /// Enter only around active work. Callers keep blocking channel receives outside this closure.
    pub fn in_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        #[cfg(feature = "offline-tracing")]
        {
            self.inner.in_scope(work)
        }
        #[cfg(not(feature = "offline-tracing"))]
        {
            work()
        }
    }

    pub fn ok(&self) {
        self.outcome("ok");
    }

    pub fn skipped(&self) {
        self.outcome("skipped");
    }

    pub fn fallback(&self) {
        self.outcome("fallback");
    }

    pub fn error(&self, code: ErrorCode) {
        #[cfg(feature = "offline-tracing")]
        {
            self.inner.record("outcome", "error");
            self.inner.record("error_code", code.as_str());
        }
        #[cfg(not(feature = "offline-tracing"))]
        let _ = code;
    }

    pub fn record_item_count(&self, value: usize) {
        self.record_bounded_count("item_count", value);
    }

    pub fn record_window_count(&self, value: usize) {
        self.record_bounded_count("window_count", value);
    }

    fn record_bounded_count(&self, field: &'static str, value: usize) {
        #[cfg(feature = "offline-tracing")]
        self.inner
            .record(field, u64::try_from(value).unwrap_or(u64::MAX));
        #[cfg(not(feature = "offline-tracing"))]
        let _ = (field, value);
    }

    fn outcome(&self, outcome: &'static str) {
        #[cfg(feature = "offline-tracing")]
        self.inner.record("outcome", outcome);
        #[cfg(not(feature = "offline-tracing"))]
        let _ = outcome;
    }
}

#[derive(Clone, Copy)]
pub enum ErrorCode {
    Storage,
    #[cfg(feature = "local")]
    DecodeFailed,
    #[cfg(feature = "local")]
    ResourceExhausted,
    BackendUnavailable,
    Other,
}

impl ErrorCode {
    #[cfg(feature = "offline-tracing")]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            #[cfg(feature = "local")]
            Self::DecodeFailed => "decode_failed",
            #[cfg(feature = "local")]
            Self::ResourceExhausted => "resource_exhausted",
            Self::BackendUnavailable => "backend_unavailable",
            Self::Other => "other",
        }
    }
}

/// Map application labels onto the immutable catalogue instead of forwarding arbitrary strings.
pub fn backend(value: &str) -> &'static str {
    match value {
        "aws" => "aws",
        "local" => "local",
        "apple" => "apple",
        "whisper" => "whisper",
        "whisper_cpp" => "whisper_cpp",
        "system" => "system",
        _ => "other",
    }
}

/// Map the selected local runtime onto the immutable engine catalogue.
#[cfg(feature = "local")]
pub fn engine(value: &str) -> &'static str {
    match value {
        "" | "sherpa" | "onnx" => "onnx",
        "whisper" => "whisper",
        "whisper_cpp" => "whisper_cpp",
        "system" => "system",
        _ => "other",
    }
}

#[cfg(feature = "offline-tracing")]
macro_rules! operation_span {
    ($parent:expr, $name:literal, $($field:tt)*) => {{
        let inner = match $parent {
            Some(parent) => tracing::span!(
                target: vasovagal_tracing::TARGET,
                parent: parent.inner(),
                tracing::Level::INFO,
                $name,
                $($field)*
            ),
            None => tracing::span!(
                target: vasovagal_tracing::TARGET,
                tracing::Level::INFO,
                $name,
                $($field)*
            ),
        };
        Span::from_inner(inner)
    }};
}

pub fn cli(command: &'static str) -> Span {
    #[cfg(feature = "offline-tracing")]
    return operation_span!(
        None::<&Span>,
        "corti.cli",
        command,
        outcome = tracing::field::Empty,
        error_code = tracing::field::Empty
    );
    #[cfg(not(feature = "offline-tracing"))]
    {
        let _ = command;
        Span {}
    }
}

pub fn pipeline_recording(
    parent: Option<&Span>,
    capture_mode: &'static str,
    backend: &'static str,
) -> Span {
    #[cfg(feature = "offline-tracing")]
    return operation_span!(
        parent,
        "corti.pipeline.recording",
        capture_mode,
        backend,
        outcome = tracing::field::Empty,
        error_code = tracing::field::Empty
    );
    #[cfg(not(feature = "offline-tracing"))]
    {
        let _ = (parent, capture_mode, backend);
        Span {}
    }
}

macro_rules! pipeline_phase {
    ($function:ident, $operation:literal) => {
        pub fn $function(parent: &Span, capture_mode: &'static str, backend: &'static str) -> Span {
            #[cfg(feature = "offline-tracing")]
            return operation_span!(
                Some(parent),
                $operation,
                capture_mode,
                backend,
                outcome = tracing::field::Empty,
                error_code = tracing::field::Empty
            );
            #[cfg(not(feature = "offline-tracing"))]
            {
                let _ = (parent, capture_mode, backend);
                Span {}
            }
        }
    };
}

pipeline_phase!(pipeline_queue, "corti.pipeline.queue");
pipeline_phase!(pipeline_checkpoint, "corti.pipeline.checkpoint");
pipeline_phase!(pipeline_cloud_cleanup, "corti.pipeline.cloud_cleanup");
pipeline_phase!(pipeline_vagus_file, "corti.pipeline.vagus_file");
pipeline_phase!(pipeline_complete, "corti.pipeline.complete");

pub fn transcription(parent: &Span, backend: &'static str, engine: &'static str) -> Span {
    #[cfg(feature = "offline-tracing")]
    return operation_span!(
        Some(parent),
        "corti.transcription",
        backend,
        engine,
        model_family = "speech_to_text",
        outcome = tracing::field::Empty,
        error_code = tracing::field::Empty
    );
    #[cfg(not(feature = "offline-tracing"))]
    {
        let _ = (parent, backend, engine);
        Span {}
    }
}

#[cfg(feature = "local")]
macro_rules! transcription_phase {
    ($function:ident, $operation:literal) => {
        pub fn $function(parent: &Span, backend: &'static str, engine: &'static str) -> Span {
            #[cfg(feature = "offline-tracing")]
            return operation_span!(
                Some(parent),
                $operation,
                backend,
                engine,
                model_family = "speech_to_text",
                item_count = tracing::field::Empty,
                outcome = tracing::field::Empty,
                error_code = tracing::field::Empty
            );
            #[cfg(not(feature = "offline-tracing"))]
            {
                let _ = (parent, backend, engine);
                Span {}
            }
        }
    };
}

pub fn transcription_aec(parent: &Span, backend: &'static str, engine: &'static str) -> Span {
    #[cfg(feature = "offline-tracing")]
    return operation_span!(
        Some(parent),
        "corti.transcription.aec",
        backend,
        engine,
        model_family = "aec",
        item_count = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error_code = tracing::field::Empty
    );
    #[cfg(not(feature = "offline-tracing"))]
    {
        let _ = (parent, backend, engine);
        Span {}
    }
}

#[cfg(feature = "local")]
transcription_phase!(transcription_decode, "corti.transcription.decode");
#[cfg(feature = "local")]
transcription_phase!(transcription_backend, "corti.transcription.backend");

#[cfg(feature = "local")]
pub fn live_session(capture_mode: &'static str, backend: &'static str) -> Span {
    #[cfg(feature = "offline-tracing")]
    return operation_span!(
        None::<&Span>,
        "corti.live.session",
        capture_mode,
        backend,
        window_count = tracing::field::Empty,
        item_count = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error_code = tracing::field::Empty
    );
    #[cfg(not(feature = "offline-tracing"))]
    {
        let _ = (capture_mode, backend);
        Span {}
    }
}

macro_rules! live_phase {
    ($function:ident, $operation:literal) => {
        pub fn $function(parent: &Span, capture_mode: &'static str, backend: &'static str) -> Span {
            #[cfg(feature = "offline-tracing")]
            return operation_span!(
                Some(parent),
                $operation,
                capture_mode,
                backend,
                window_count = tracing::field::Empty,
                item_count = tracing::field::Empty,
                outcome = tracing::field::Empty,
                error_code = tracing::field::Empty
            );
            #[cfg(not(feature = "offline-tracing"))]
            {
                let _ = (parent, capture_mode, backend);
                Span {}
            }
        }
    };
}

#[cfg(feature = "local")]
live_phase!(live_consume, "corti.live.consume");
live_phase!(live_window_flush, "corti.live.window_flush");
live_phase!(live_note_sync, "corti.live.note_sync");
live_phase!(live_finish, "corti.live.finish");

pub fn background_job(attempt_kind: &'static str, attempt_count: u32) -> Span {
    #[cfg(feature = "offline-tracing")]
    return operation_span!(
        None::<&Span>,
        "corti.background_job",
        attempt_kind,
        attempt_count = u64::from(attempt_count),
        outcome = tracing::field::Empty,
        error_code = tracing::field::Empty
    );
    #[cfg(not(feature = "offline-tracing"))]
    {
        let _ = (attempt_kind, attempt_count);
        Span {}
    }
}

macro_rules! background_phase {
    ($function:ident, $operation:literal) => {
        pub fn $function(parent: &Span, attempt_kind: &'static str, attempt_count: u32) -> Span {
            #[cfg(feature = "offline-tracing")]
            return operation_span!(
                Some(parent),
                $operation,
                attempt_kind,
                attempt_count = u64::from(attempt_count),
                outcome = tracing::field::Empty,
                error_code = tracing::field::Empty
            );
            #[cfg(not(feature = "offline-tracing"))]
            {
                let _ = (parent, attempt_kind, attempt_count);
                Span {}
            }
        }
    };
}

background_phase!(background_retry, "corti.background_job.retry");
background_phase!(background_cleanup, "corti.background_job.cleanup");
background_phase!(background_retention, "corti.background_job.retention");

#[cfg(all(test, feature = "offline-tracing"))]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::{Subscriber, span};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::{Layer, Registry};

    use super::*;

    type ParentRecords = Arc<Mutex<Vec<(String, Option<String>)>>>;

    #[derive(Clone, Default)]
    struct ParentCapture(ParentRecords);

    impl<S> Layer<S> for ParentCapture
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(
            &self,
            attrs: &span::Attributes<'_>,
            id: &span::Id,
            context: Context<'_, S>,
        ) {
            let parent = context
                .span(id)
                .and_then(|span| span.parent())
                .map(|span| span.metadata().name().to_string());
            self.0
                .lock()
                .unwrap()
                .push((attrs.metadata().name().to_string(), parent));
        }
    }

    #[test]
    fn worker_phase_uses_its_explicit_parent_without_an_entered_guard() {
        let captured = ParentCapture::default();
        let records = captured.0.clone();
        let subscriber = Registry::default().with(captured);
        tracing::subscriber::with_default(subscriber, || {
            let root = pipeline_recording(None, "mixed", "local");
            let child = transcription(&root, "local", "onnx");
            child.ok();
            root.ok();
        });

        let records = records.lock().unwrap();
        assert!(records.contains(&("corti.pipeline.recording".into(), None)));
        assert!(records.contains(&(
            "corti.transcription".into(),
            Some("corti.pipeline.recording".into())
        )));
    }
}
