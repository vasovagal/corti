// Test-only subscriber captures actual events, including fields recorded with Display/Debug. This is
// deliberately not a formatting test of the evidence types. Worker tests install the same Dispatch.
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone)]
struct Capture(Arc<Mutex<String>>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields<'a>(&'a mut String);
        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = writeln!(self.0, "{}={value:?}", field.name());
            }
        }
        event.record(&mut Fields(&mut self.0.lock().unwrap()));
    }
}

pub(crate) fn subscriber() -> (tracing::Dispatch, Arc<Mutex<String>>) {
    static INSTALLED: std::sync::OnceLock<(tracing::Dispatch, Arc<Mutex<String>>)> =
        std::sync::OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            let captured = Arc::new(Mutex::new(String::new()));
            let dispatch = tracing::Dispatch::new(
                tracing_subscriber::registry().with(Capture(captured.clone())),
            );
            // One lifetime across the parallel harness avoids rebuilding global callsite interest while
            // other tests execute these same actual functions without a thread-local subscriber.
            tracing::dispatcher::set_global_default(dispatch.clone()).unwrap();
            (dispatch, captured)
        })
        .clone()
}

pub(crate) fn assert_private(value: &str) {
    for forbidden in [
        "syntheticquartzfalcon",
        "invented-audio-content-violet",
        "invented-private-participant",
    ] {
        assert!(
            !value.contains(forbidden),
            "captured diagnostic exposed private fixture content"
        );
    }
}
