// Only host/UI boundaries are replaced. The production pipeline, backend dispatch, capture quality
// storage, queue, checkpoint, transcript rendering, note rewrite, and completion functions are copied
// from this checkout by run.py. A local retry must never invoke hosted postprocessing.
mod lexicon {
    pub fn load_compiled() -> Option<corti_lexicon::CompiledLexicon> {
        None
    }
    pub fn provenance_summary() -> serde_json::Value {
        serde_json::Value::Null
    }
}

mod stats {
    #[derive(Default)]
    pub struct StatsBuffer;
    impl StatsBuffer {
        pub fn record_stage(&self, _: &str, _: std::time::Duration, _: &str) {}
    }
}

mod live {
    use std::path::PathBuf;
    pub struct LiveManager;
    // @LIVE_OUTCOME@
}

mod settings {
    pub type SharedConfig = std::sync::Arc<std::sync::Mutex<crate::config::AppConfig>>;
}

mod tray {
    pub fn update_history(
        _: &(),
        _: &str,
        _: corti_core::JobStatus,
        _: Option<chrono::DateTime<chrono::Local>>,
        _: Option<String>,
        _: Option<std::path::PathBuf>,
    ) {
    }
    pub fn set_status(_: &(), _: String) {}
}

mod postprocess_app {
    use corti_core::DiarizedTranscript;
    use corti_postprocess::{CallId, ErrorCode};
    use corti_vagus::provenance::{AppliedPostprocessProvenance, ProvenanceFingerprint};

    // The exact production SettledFinalTranscript definition is inserted here.
    // @SETTLED_FINAL_TRANSCRIPT@

    pub struct HostedHandle;
    pub struct Snapshot {
        pub control: Control,
    }
    pub struct Control {
        pub master_enabled: bool,
        pub final_lane: Lane,
    }
    pub struct Lane {
        pub enabled: bool,
    }
    impl HostedHandle {
        pub fn snapshot(&self) -> Snapshot {
            Snapshot {
                control: Control {
                    master_enabled: false,
                    final_lane: Lane { enabled: false },
                },
            }
        }
        pub fn finalize(&self, _: &str, _: DiarizedTranscript, _: bool) -> SettledFinalTranscript {
            panic!("unexpected hosted path in synthetic local retry")
        }
        pub fn mark_final_checkpointed(&self, _: &[CallId]) -> Result<(), &str> {
            Ok(())
        }
        pub fn mark_final_applied(&self, _: &[CallId]) -> Result<(), &str> {
            Ok(())
        }
        pub fn abandon_final_result(&self, _: &[CallId]) -> Result<(), &str> {
            Ok(())
        }
    }
}
