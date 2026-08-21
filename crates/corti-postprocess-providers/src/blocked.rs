use corti_postprocess::{ErrorCode, KnownTransport, ProviderDescriptor, SupportTier};

/// Descriptor-only representation of Claude Free/Pro/Max subscription routing.
///
/// This type intentionally implements neither `ProviderAdapter` nor any connect/execute trait. Direct
/// Anthropic API billing is represented by the separate documented Anthropic Messages adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSubscriptionDescriptor {
    provider: ProviderDescriptor,
    policy_code: ErrorCode,
}

impl ClaudeSubscriptionDescriptor {
    pub fn provider(&self) -> &ProviderDescriptor {
        &self.provider
    }

    pub const fn policy_code(&self) -> ErrorCode {
        self.policy_code
    }

    pub const fn connect_available(&self) -> bool {
        false
    }

    pub const fn execute_available(&self) -> bool {
        false
    }
}

pub fn claude_subscription_descriptor() -> ClaudeSubscriptionDescriptor {
    let provider = KnownTransport::ClaudeSubscription.descriptor();
    debug_assert_eq!(provider.support_tier, SupportTier::Blocked);
    debug_assert!(!provider.adapter_available);
    ClaudeSubscriptionDescriptor {
        provider,
        policy_code: ErrorCode::PolicyBlocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_subscription_is_descriptor_only_and_blocked() {
        let descriptor = claude_subscription_descriptor();
        assert_eq!(descriptor.provider().support_tier, SupportTier::Blocked);
        assert!(!descriptor.provider().adapter_available);
        assert!(!descriptor.connect_available());
        assert!(!descriptor.execute_available());
        assert_eq!(descriptor.policy_code(), ErrorCode::PolicyBlocked);
    }
}
