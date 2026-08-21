use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{BillingBasis, ModelId, ProviderId, SupportTier};

const TOKEN_RATE_DENOMINATOR: u128 = 1_000_000;

/// Nullable, nonnegative terminal usage normalized across providers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub usage_complete: bool,
}

impl NormalizedUsage {
    pub const fn unknown() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
            reasoning_tokens: None,
            usage_complete: false,
        }
    }
}

/// Signed provider values before normalization. Negative values are rejected, never clamped to zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_read_tokens: Option<i64>,
    pub cached_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub usage_complete: bool,
}

impl TryFrom<RawUsage> for NormalizedUsage {
    type Error = PricingError;

    fn try_from(raw: RawUsage) -> Result<Self, Self::Error> {
        fn nonnegative(value: Option<i64>) -> Result<Option<u64>, PricingError> {
            value
                .map(|value| u64::try_from(value).map_err(|_| PricingError::NegativeUsage))
                .transpose()
        }
        Ok(Self {
            input_tokens: nonnegative(raw.input_tokens)?,
            output_tokens: nonnegative(raw.output_tokens)?,
            cached_read_tokens: nonnegative(raw.cached_read_tokens)?,
            cached_write_tokens: nonnegative(raw.cached_write_tokens)?,
            reasoning_tokens: nonnegative(raw.reasoning_tokens)?,
            usage_complete: raw.usage_complete,
        })
    }
}

/// Whether the normalized input count includes cached-read/write classes or is already uncached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTokenAccounting {
    IncludesCached,
    ClassesDisjoint,
}

/// Currency code constrained to three uppercase ASCII letters.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurrencyCode(String);

impl CurrencyCode {
    pub fn new(value: impl Into<String>) -> Result<Self, PricingError> {
        let value = value.into();
        if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_uppercase()) {
            return Err(PricingError::InvalidCurrency);
        }
        Ok(Self(value))
    }

    pub fn usd() -> Self {
        Self("USD".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CurrencyCode").field(&self.0).finish()
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Currency micros charged per one million tokens for each independently reported class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TariffRates {
    pub input_micros_per_million: Option<u64>,
    pub output_micros_per_million: Option<u64>,
    pub cached_read_micros_per_million: Option<u64>,
    pub cached_write_micros_per_million: Option<u64>,
    pub reasoning_micros_per_million: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tariff {
    pub tariff_id: String,
    pub provider: ProviderId,
    pub exact_model_id: ModelId,
    pub region: Option<String>,
    pub support_tier: SupportTier,
    /// Inclusive UTC Unix millisecond boundary.
    pub effective_from_unix_ms: i64,
    /// Exclusive UTC Unix millisecond boundary; `None` means open-ended within catalog validity.
    pub effective_until_unix_ms: Option<i64>,
    pub currency: CurrencyCode,
    pub input_accounting: InputTokenAccounting,
    pub rates: TariffRates,
}

impl Tariff {
    fn matches(&self, query: &PricingQuery<'_>) -> bool {
        &self.provider == query.provider
            && &self.exact_model_id == query.exact_model_id
            && self.region.as_deref() == query.region
            && self.support_tier == query.support_tier
            && query.dispatch_unix_ms >= self.effective_from_unix_ms
            && self
                .effective_until_unix_ms
                .is_none_or(|until| query.dispatch_unix_ms < until)
    }

    fn estimate_micros(&self, usage: &NormalizedUsage) -> Result<Option<u64>, PricingError> {
        if !usage.usage_complete {
            return Ok(None);
        }

        let input = match usage.input_tokens {
            Some(value) => value,
            None => return Ok(None),
        };
        let output = match usage.output_tokens {
            Some(value) => value,
            None => return Ok(None),
        };

        let cached_read = resolve_optional_class(
            usage.cached_read_tokens,
            self.rates.cached_read_micros_per_million,
        )?;
        let cached_write = resolve_optional_class(
            usage.cached_write_tokens,
            self.rates.cached_write_micros_per_million,
        )?;
        let reasoning = resolve_optional_class(
            usage.reasoning_tokens,
            self.rates.reasoning_micros_per_million,
        )?;
        let Some((cached_read_tokens, cached_read_rate)) = cached_read else {
            return Ok(None);
        };
        let Some((cached_write_tokens, cached_write_rate)) = cached_write else {
            return Ok(None);
        };
        let Some((reasoning_tokens, reasoning_rate)) = reasoning else {
            return Ok(None);
        };

        let uncached_input = match self.input_accounting {
            InputTokenAccounting::ClassesDisjoint => input,
            InputTokenAccounting::IncludesCached => {
                let cached = cached_read_tokens
                    .checked_add(cached_write_tokens)
                    .ok_or(PricingError::ArithmeticOverflow)?;
                input
                    .checked_sub(cached)
                    .ok_or(PricingError::InconsistentUsage)?
            }
        };

        let Some(input_rate) = required_rate(uncached_input, self.rates.input_micros_per_million)
        else {
            return Ok(None);
        };
        let Some(output_rate) = required_rate(output, self.rates.output_micros_per_million) else {
            return Ok(None);
        };

        let classes = [
            (uncached_input, input_rate),
            (output, output_rate),
            (cached_read_tokens, cached_read_rate),
            (cached_write_tokens, cached_write_rate),
            (reasoning_tokens, reasoning_rate),
        ];
        let numerator = classes.iter().try_fold(0u128, |sum, (tokens, rate)| {
            let charge = u128::from(*tokens)
                .checked_mul(u128::from(*rate))
                .ok_or(PricingError::ArithmeticOverflow)?;
            sum.checked_add(charge)
                .ok_or(PricingError::ArithmeticOverflow)
        })?;
        let rounded = if numerator == 0 {
            0
        } else {
            numerator
                .checked_add(TOKEN_RATE_DENOMINATOR - 1)
                .ok_or(PricingError::ArithmeticOverflow)?
                / TOKEN_RATE_DENOMINATOR
        };
        u64::try_from(rounded)
            .map(Some)
            .map_err(|_| PricingError::ArithmeticOverflow)
    }
}

/// `Some((tokens, rate))` means priced; `None` means usage/tariff is incomplete.
fn resolve_optional_class(
    tokens: Option<u64>,
    rate: Option<u64>,
) -> Result<Option<(u64, u64)>, PricingError> {
    Ok(match (tokens, rate) {
        (Some(tokens), Some(rate)) => Some((tokens, rate)),
        (None, None) => Some((0, 0)),
        (Some(0), None) => Some((0, 0)),
        (Some(_), None) | (None, Some(_)) => None,
    })
}

fn required_rate(tokens: u64, rate: Option<u64>) -> Option<u64> {
    match (tokens, rate) {
        (_, Some(rate)) => Some(rate),
        (0, None) => Some(0),
        (_, None) => None,
    }
}

/// Exact matching inputs for the dispatch instant. No model or region fallback is permitted.
#[derive(Debug, Clone, Copy)]
pub struct PricingQuery<'a> {
    pub provider: &'a ProviderId,
    pub exact_model_id: &'a ModelId,
    pub region: Option<&'a str>,
    pub support_tier: SupportTier,
    pub dispatch_unix_ms: i64,
    pub billing_basis: BillingBasis,
}

/// Runtime-free pricing seam used by coordinators and deterministic fixtures.
pub trait PricingCatalog: Send + Sync {
    fn estimate(
        &self,
        query: PricingQuery<'_>,
        usage: &NormalizedUsage,
    ) -> Result<CostEstimate, PricingError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TariffCatalog {
    pub version: String,
    pub source_url: String,
    pub retrieved_at_unix_ms: i64,
    /// Last dispatch instant for which this reviewed catalog may be used.
    pub valid_until_unix_ms: i64,
    pub tariffs: Vec<Tariff>,
}

impl TariffCatalog {
    pub fn estimate(
        &self,
        query: PricingQuery<'_>,
        usage: &NormalizedUsage,
    ) -> Result<CostEstimate, PricingError> {
        match query.billing_basis {
            BillingBasis::IncludedSubscription => return Ok(CostEstimate::included_subscription()),
            BillingBasis::NoProviderRequest => return Ok(CostEstimate::no_provider_request()),
            BillingBasis::Unknown => return Ok(CostEstimate::unavailable()),
            BillingBasis::MeteredEstimate => {}
        }

        if query.dispatch_unix_ms > self.valid_until_unix_ms {
            return Ok(CostEstimate::unavailable());
        }
        let mut matching = self.tariffs.iter().filter(|tariff| tariff.matches(&query));
        let Some(tariff) = matching.next() else {
            return Ok(CostEstimate::unavailable());
        };
        if matching.next().is_some() {
            return Err(PricingError::AmbiguousTariff);
        }
        let Some(cost_micros) = tariff.estimate_micros(usage)? else {
            return Ok(CostEstimate::unavailable());
        };
        Ok(CostEstimate {
            billing_basis: BillingBasis::MeteredEstimate,
            cost_micros: Some(cost_micros),
            currency: Some(tariff.currency.clone()),
            pricing_catalog_version: Some(self.version.clone()),
            tariff_id: Some(tariff.tariff_id.clone()),
            tariff_effective_at_unix_ms: Some(tariff.effective_from_unix_ms),
        })
    }
}

impl PricingCatalog for TariffCatalog {
    fn estimate(
        &self,
        query: PricingQuery<'_>,
        usage: &NormalizedUsage,
    ) -> Result<CostEstimate, PricingError> {
        TariffCatalog::estimate(self, query, usage)
    }
}

/// Truthful normalized cost result. Constructors preserve null-vs-zero semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEstimate {
    billing_basis: BillingBasis,
    cost_micros: Option<u64>,
    currency: Option<CurrencyCode>,
    pricing_catalog_version: Option<String>,
    tariff_id: Option<String>,
    tariff_effective_at_unix_ms: Option<i64>,
}

impl CostEstimate {
    pub const fn billing_basis(&self) -> BillingBasis {
        self.billing_basis
    }

    pub const fn cost_micros(&self) -> Option<u64> {
        self.cost_micros
    }

    pub fn currency(&self) -> Option<&CurrencyCode> {
        self.currency.as_ref()
    }

    pub fn pricing_catalog_version(&self) -> Option<&str> {
        self.pricing_catalog_version.as_deref()
    }

    pub fn tariff_id(&self) -> Option<&str> {
        self.tariff_id.as_deref()
    }

    pub const fn tariff_effective_at_unix_ms(&self) -> Option<i64> {
        self.tariff_effective_at_unix_ms
    }

    pub fn included_subscription() -> Self {
        Self::without_cost(BillingBasis::IncludedSubscription)
    }

    pub fn no_provider_request() -> Self {
        Self::without_cost(BillingBasis::NoProviderRequest)
    }

    pub fn unavailable() -> Self {
        Self::without_cost(BillingBasis::Unknown)
    }

    fn without_cost(billing_basis: BillingBasis) -> Self {
        Self {
            billing_basis,
            cost_micros: None,
            currency: None,
            pricing_catalog_version: None,
            tariff_id: None,
            tariff_effective_at_unix_ms: None,
        }
    }

    /// Exact truthful label for one call. Unknown/subscription/local outcomes never render as zero.
    pub fn render(&self) -> String {
        match self.billing_basis {
            BillingBasis::IncludedSubscription => "Included subscription · cost unavailable".into(),
            BillingBasis::NoProviderRequest => "Local cache · no provider request".into(),
            BillingBasis::Unknown => "Cost unavailable".into(),
            BillingBasis::MeteredEstimate => match (&self.currency, self.cost_micros) {
                (Some(currency), Some(micros)) if currency.as_str() == "USD" => {
                    format!("Estimated ${}", format_micros(micros))
                }
                (Some(currency), Some(micros)) => {
                    format!("Estimated {} {}", currency.as_str(), format_micros(micros))
                }
                _ => "Cost unavailable".into(),
            },
        }
    }
}

fn format_micros(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let fractional = micros % 1_000_000;
    let mut value = format!("{whole}.{fractional:06}");
    while value.ends_with('0') && value.len() - value.find('.').unwrap_or(value.len()) - 1 > 4 {
        value.pop();
    }
    value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PricingError {
    #[error("provider usage contained a negative token count")]
    NegativeUsage,
    #[error("currency code must be three uppercase ASCII letters")]
    InvalidCurrency,
    #[error("cached token counts exceed total input tokens")]
    InconsistentUsage,
    #[error("tariff arithmetic overflow")]
    ArithmeticOverflow,
    #[error("more than one tariff matches the exact dispatch dimensions")]
    AmbiguousTariff,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> TariffCatalog {
        TariffCatalog {
            version: "pricing-test-v1".into(),
            source_url: "https://example.invalid/pricing".into(),
            retrieved_at_unix_ms: 100,
            valid_until_unix_ms: 2_000,
            tariffs: vec![Tariff {
                tariff_id: "tariff-a".into(),
                provider: ProviderId::new("provider-a").unwrap(),
                exact_model_id: ModelId::new("model-a").unwrap(),
                region: Some("region-a".into()),
                support_tier: SupportTier::Documented,
                effective_from_unix_ms: 500,
                effective_until_unix_ms: Some(1_500),
                currency: CurrencyCode::usd(),
                input_accounting: InputTokenAccounting::IncludesCached,
                rates: TariffRates {
                    input_micros_per_million: Some(2_000_000),
                    output_micros_per_million: Some(4_000_000),
                    cached_read_micros_per_million: Some(500_000),
                    cached_write_micros_per_million: Some(2_500_000),
                    reasoning_micros_per_million: None,
                },
            }],
        }
    }

    fn query<'a>(provider: &'a ProviderId, model: &'a ModelId) -> PricingQuery<'a> {
        PricingQuery {
            provider,
            exact_model_id: model,
            region: Some("region-a"),
            support_tier: SupportTier::Documented,
            dispatch_unix_ms: 1_000,
            billing_basis: BillingBasis::MeteredEstimate,
        }
    }

    #[test]
    fn raw_usage_rejects_negative_values() {
        assert_eq!(
            NormalizedUsage::try_from(RawUsage {
                input_tokens: Some(-1),
                usage_complete: true,
                ..RawUsage::default()
            }),
            Err(PricingError::NegativeUsage)
        );
    }

    #[test]
    fn exact_tariff_uses_checked_cache_class_formula() {
        let provider = ProviderId::new("provider-a").unwrap();
        let model = ModelId::new("model-a").unwrap();
        let usage = NormalizedUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(200),
            cached_read_tokens: Some(400),
            cached_write_tokens: Some(0),
            reasoning_tokens: None,
            usage_complete: true,
        };
        let estimate = catalog()
            .estimate(query(&provider, &model), &usage)
            .unwrap();
        // 600 uncached input * $2/M + 400 cached * $0.50/M + 200 output * $4/M.
        assert_eq!(estimate.cost_micros(), Some(2_200));
        assert_eq!(estimate.render(), "Estimated $0.0022");
        assert_eq!(estimate.pricing_catalog_version(), Some("pricing-test-v1"));
    }

    #[test]
    fn stale_mismatched_or_incomplete_data_is_unknown_not_zero() {
        let provider = ProviderId::new("provider-a").unwrap();
        let model = ModelId::new("model-a").unwrap();
        let mut stale_query = query(&provider, &model);
        stale_query.dispatch_unix_ms = 2_001;
        assert_eq!(
            catalog()
                .estimate(stale_query, &NormalizedUsage::unknown())
                .unwrap()
                .render(),
            "Cost unavailable"
        );

        let incomplete = NormalizedUsage {
            input_tokens: Some(1),
            output_tokens: None,
            usage_complete: true,
            ..NormalizedUsage::default()
        };
        assert_eq!(
            catalog()
                .estimate(query(&provider, &model), &incomplete)
                .unwrap()
                .billing_basis(),
            BillingBasis::Unknown
        );
    }

    #[test]
    fn subscription_and_local_labels_are_exact_and_null() {
        let included = CostEstimate::included_subscription();
        assert_eq!(included.cost_micros(), None);
        assert_eq!(
            included.render(),
            "Included subscription · cost unavailable"
        );
        let local = CostEstimate::no_provider_request();
        assert_eq!(local.cost_micros(), None);
        assert_eq!(local.render(), "Local cache · no provider request");
    }

    #[test]
    fn inconsistent_and_overflowing_usage_never_wraps() {
        let provider = ProviderId::new("provider-a").unwrap();
        let model = ModelId::new("model-a").unwrap();
        let inconsistent = NormalizedUsage {
            input_tokens: Some(1),
            output_tokens: Some(0),
            cached_read_tokens: Some(2),
            cached_write_tokens: Some(0),
            reasoning_tokens: None,
            usage_complete: true,
        };
        assert_eq!(
            catalog().estimate(query(&provider, &model), &inconsistent),
            Err(PricingError::InconsistentUsage)
        );

        let mut overflowing = catalog();
        overflowing.tariffs[0].rates.input_micros_per_million = Some(u64::MAX);
        let huge = NormalizedUsage {
            input_tokens: Some(u64::MAX),
            output_tokens: Some(0),
            cached_read_tokens: Some(0),
            cached_write_tokens: Some(0),
            reasoning_tokens: None,
            usage_complete: true,
        };
        assert_eq!(
            overflowing.estimate(query(&provider, &model), &huge),
            Err(PricingError::ArithmeticOverflow)
        );
    }
}
