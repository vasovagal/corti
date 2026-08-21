//! Provider-edge crate scaffolding.
//!
//! Adapter implementations intentionally do not land in the domain-core slice. Future adapters must use
//! injected HTTP/process/clock/credential seams and fixture-only tests; this crate performs no ambient
//! credential discovery and cannot make a provider request in its current form.

#![forbid(unsafe_code)]

pub use corti_postprocess::{
    CancellationToken, CredentialSource, HostedRequest, ModelCatalog, PostprocessError,
    ProviderAdapter, ProviderDescriptor, ProviderEvent, ProviderEventSink, ProviderScope,
    ProviderTerminal,
};

/// Codex app-server support is experimental and remains off in default builds.
pub const CODEX_APP_SERVER_EXPERIMENTAL: bool = true;
pub const CODEX_APP_SERVER_DEFAULT_ENABLED: bool = false;
pub const CODEX_APP_SERVER_COMPILED: bool = cfg!(feature = "codex-experimental");

/// Claude Free/Pro/Max routing has no adapter without written Anthropic permission.
pub const CLAUDE_SUBSCRIPTION_ADAPTER_BLOCKED: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_subscription_paths_cannot_be_accidentally_defaulted() {
        assert!(CODEX_APP_SERVER_EXPERIMENTAL);
        assert!(!CODEX_APP_SERVER_DEFAULT_ENABLED);
        assert_eq!(
            CODEX_APP_SERVER_COMPILED,
            cfg!(feature = "codex-experimental")
        );
        assert!(CLAUDE_SUBSCRIPTION_ADAPTER_BLOCKED);
    }
}
