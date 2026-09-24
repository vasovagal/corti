//! One derivation of a lane's provider-side cache policy from the selected model's capabilities.
//!
//! Before this existed the policy was chosen in the Settings UI, re-checked in the app's selection
//! validator, in the coordinator and in every adapter — and a Gemini lane could never be saved because
//! the acknowledgement the persisted document demanded had no writer (#144). The rule now lives here and
//! the other checks merely confirm it.

use corti_postprocess::{ModelDescriptor, ProviderCacheMode, ProviderId};

/// Transport id of the native ChatGPT subscription path, whose private endpoint owns cache behaviour.
pub const CHATGPT_SUBSCRIPTION_TRANSPORT: &str = "chatgpt_subscription";

/// Why a model cannot be used under any policy the owner has agreed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachePolicyBlock {
    /// The model caches implicitly on the provider side whether or not Corti asks, so the owner must
    /// acknowledge provider-side retention before the lane may use it.
    AcknowledgementRequired { provider: ProviderId },
}

/// The provider cache mode a lane must carry for `model`, given whether the owner has acknowledged
/// provider-side caching for that provider.
///
/// - ChatGPT subscription: `Unavailable` — the endpoint decides; nothing to acknowledge.
/// - Implicit-cache models (Gemini): `UnavoidableImplicit` once acknowledged, otherwise blocked.
/// - Explicit-prefix models (OpenAI, Anthropic, Claude on Vertex): `ExplicitStablePrefix` once
///   acknowledged, `Off` until then — the lane works either way.
/// - Everything else: `Off`.
pub fn effective_provider_cache(
    model: &ModelDescriptor,
    acknowledged: bool,
) -> Result<ProviderCacheMode, CachePolicyBlock> {
    if model.transport.as_str() == CHATGPT_SUBSCRIPTION_TRANSPORT {
        return Ok(ProviderCacheMode::Unavailable);
    }
    if model.capabilities.implicit_cache_may_apply {
        return if acknowledged {
            Ok(ProviderCacheMode::UnavoidableImplicit)
        } else {
            Err(CachePolicyBlock::AcknowledgementRequired {
                provider: model.provider.clone(),
            })
        };
    }
    if model.capabilities.explicit_prefix_cache && acknowledged {
        return Ok(ProviderCacheMode::ExplicitStablePrefix);
    }
    Ok(ProviderCacheMode::Off)
}

/// Whether `stored` is a policy the adapters accept for `model` right now — used to flag a lane whose
/// saved policy predates the acknowledgement or the model's classification.
pub fn stored_policy_is_acceptable(
    model: &ModelDescriptor,
    stored: ProviderCacheMode,
    acknowledged: bool,
) -> bool {
    match effective_provider_cache(model, acknowledged) {
        Err(_) => false,
        Ok(ProviderCacheMode::Unavailable) => {
            matches!(
                stored,
                ProviderCacheMode::Unavailable | ProviderCacheMode::Off
            )
        }
        Ok(ProviderCacheMode::UnavoidableImplicit) => {
            stored == ProviderCacheMode::UnavoidableImplicit
        }
        Ok(ProviderCacheMode::ExplicitStablePrefix) => matches!(
            stored,
            ProviderCacheMode::ExplicitStablePrefix | ProviderCacheMode::Off
        ),
        Ok(ProviderCacheMode::Off) => stored == ProviderCacheMode::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_postprocess::{AdapterCapabilities, BillingBasis, ModelId, SupportTier, TransportId};

    fn model(transport: &str, implicit: bool, explicit: bool) -> ModelDescriptor {
        ModelDescriptor {
            provider: ProviderId::new("fixture").unwrap(),
            transport: TransportId::new(transport).unwrap(),
            support_tier: SupportTier::Documented,
            exact_model_id: ModelId::new("fixture-model").unwrap(),
            account_scoped_available: true,
            region: None,
            max_context_tokens: 100_000,
            max_output_tokens: 8_192,
            capabilities: AdapterCapabilities {
                text_input: true,
                text_output: true,
                streaming: true,
                structured_output: true,
                explicit_prefix_cache: explicit,
                implicit_cache_may_apply: implicit,
            },
            billing_basis: BillingBasis::MeteredEstimate,
            tariff_version: None,
            deprecated: false,
            benchmarked_for_live: false,
        }
    }

    #[test]
    fn effective_provider_cache_matrix() {
        let chatgpt = model(CHATGPT_SUBSCRIPTION_TRANSPORT, false, false);
        assert_eq!(
            effective_provider_cache(&chatgpt, false),
            Ok(ProviderCacheMode::Unavailable)
        );
        assert_eq!(
            effective_provider_cache(&chatgpt, true),
            Ok(ProviderCacheMode::Unavailable)
        );

        let gemini = model("vertex_api", true, false);
        assert!(matches!(
            effective_provider_cache(&gemini, false),
            Err(CachePolicyBlock::AcknowledgementRequired { .. })
        ));
        assert_eq!(
            effective_provider_cache(&gemini, true),
            Ok(ProviderCacheMode::UnavoidableImplicit)
        );

        let claude = model("vertex_api", false, true);
        assert_eq!(
            effective_provider_cache(&claude, false),
            Ok(ProviderCacheMode::Off)
        );
        assert_eq!(
            effective_provider_cache(&claude, true),
            Ok(ProviderCacheMode::ExplicitStablePrefix)
        );

        let plain = model("bedrock_runtime", false, false);
        assert_eq!(
            effective_provider_cache(&plain, true),
            Ok(ProviderCacheMode::Off)
        );
    }

    #[test]
    fn stored_policies_are_judged_against_the_effective_one() {
        let gemini = model("vertex_api", true, false);
        assert!(!stored_policy_is_acceptable(
            &gemini,
            ProviderCacheMode::Off,
            true
        ));
        assert!(!stored_policy_is_acceptable(
            &gemini,
            ProviderCacheMode::UnavoidableImplicit,
            false
        ));
        assert!(stored_policy_is_acceptable(
            &gemini,
            ProviderCacheMode::UnavoidableImplicit,
            true
        ));
        let claude = model("vertex_api", false, true);
        assert!(stored_policy_is_acceptable(
            &claude,
            ProviderCacheMode::Off,
            true
        ));
        assert!(!stored_policy_is_acceptable(
            &claude,
            ProviderCacheMode::ExplicitStablePrefix,
            false
        ));
        let chatgpt = model(CHATGPT_SUBSCRIPTION_TRANSPORT, false, false);
        assert!(stored_policy_is_acceptable(
            &chatgpt,
            ProviderCacheMode::Off,
            false
        ));
        assert!(!stored_policy_is_acceptable(
            &chatgpt,
            ProviderCacheMode::ExplicitStablePrefix,
            true
        ));
    }
}
