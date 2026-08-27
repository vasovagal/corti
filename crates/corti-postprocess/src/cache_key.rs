use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::{Digest as _, Sha256};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroize;

use crate::{
    BillingBasis, CachePolicy, ConnectionScopeId, Lane, ModelId, PromptTask, ProviderId,
    SupportTier, TranscriptRow, TransportId,
};

type HmacSha256 = Hmac<Sha256>;

/// A 256-bit key used only as HMAC input. It cannot be cloned, displayed, serialized, or read back.
pub struct DigestKey([u8; 32]);

impl DigestKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Derive a persistable opaque identity for canonical content. Callers choose a versioned domain and
    /// never receive either the HMAC key or intermediate canonical bytes.
    pub fn fingerprint(&self, domain: &[u8], canonical_content: &[u8]) -> KeyedFingerprint {
        KeyedFingerprint::derive(self, domain, canonical_content)
    }

    pub(crate) fn hmac(&self, domain: &[u8]) -> HmacSha256 {
        let mut mac =
            HmacSha256::new_from_slice(&self.0).expect("SHA-256 HMAC accepts any key size");
        mac.update(domain);
        mac
    }
}

impl fmt::Debug for DigestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DigestKey(<redacted>)")
    }
}

impl Drop for DigestKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Keyed exact request identity. The digest is opaque and contains no readable prompt material.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestKey([u8; 32]);

impl RequestKey {
    pub fn derive(key: &DigestKey, material: &RequestKeyMaterial<'_>) -> Self {
        let mut encoder = CanonicalCborHmac::new(key, b"corti-postprocess-key-v2\0");
        material.encode(&mut encoder);
        Self(encoder.finish())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_base64url(self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn from_base64url(value: &str) -> Option<Self> {
        let decoded = URL_SAFE_NO_PAD.decode(value).ok()?;
        Some(Self(decoded.try_into().ok()?))
    }

    /// Bind the semantic request to the credential lease that was actually resolved before exact lookup.
    /// The identity is already opaque; a second keyed domain keeps it non-reversible in durable cache keys.
    pub fn bind_credential(self, key: &DigestKey, credential_identity: &[u8; 32]) -> Self {
        let mut encoder = CanonicalCborHmac::new(key, b"corti-postprocess-credential-bound-v1\0");
        encoder.map(2);
        encoder.key("request_key");
        encoder.byte_string(&self.0);
        encoder.key("credential_identity");
        encoder.byte_string(credential_identity);
        Self(encoder.finish())
    }
}

impl fmt::Debug for RequestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RequestKey(<opaque-hmac>)")
    }
}

/// Opaque provider stable-prefix key, encoded for provider APIs as base64url without padding.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProviderCacheKey(String);

impl ProviderCacheKey {
    pub fn derive(key: &DigestKey, material: &ProviderCacheKeyMaterial<'_>) -> Self {
        let mut encoder = CanonicalCborHmac::new(key, b"corti-provider-cache-key-v1\0");
        material.encode(&mut encoder);
        Self(URL_SAFE_NO_PAD.encode(encoder.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn bind_credential(&self, key: &DigestKey, credential_identity: &[u8; 32]) -> Self {
        let mut encoder =
            CanonicalCborHmac::new(key, b"corti-provider-cache-credential-bound-v1\0");
        encoder.map(2);
        encoder.key("provider_cache_key");
        encoder.text(&self.0);
        encoder.key("credential_identity");
        encoder.byte_string(credential_identity);
        Self(URL_SAFE_NO_PAD.encode(encoder.finish()))
    }
}

impl fmt::Debug for ProviderCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderCacheKey(<opaque-hmac>)")
    }
}

/// HMAC-derived fingerprint safe to place in content-free provenance or telemetry.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct KeyedFingerprint(String);

impl KeyedFingerprint {
    pub(crate) fn derive(key: &DigestKey, domain: &[u8], canonical_content: &[u8]) -> Self {
        let mut encoder = CanonicalCborHmac::new(key, domain);
        encoder.byte_string(canonical_content);
        Self(URL_SAFE_NO_PAD.encode(encoder.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for KeyedFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyedFingerprint(<opaque-hmac>)")
    }
}

/// All semantic inputs to one exact local result identity.
///
/// The type intentionally has no `Debug` or serialization implementation. Canonical bytes are streamed
/// directly into HMAC state and are never returned to callers.
#[derive(Clone, Copy)]
pub struct RequestKeyMaterial<'a> {
    pub provider: &'a ProviderId,
    pub transport: &'a TransportId,
    pub support_tier: SupportTier,
    pub connection_scope_id: &'a ConnectionScopeId,
    pub region: Option<&'a str>,
    pub exact_model_id: &'a ModelId,
    pub adapter_version: u32,
    pub prompt_template_version: u32,
    pub output_schema_version: u32,
    pub chunker_version: u32,
    pub lane: Lane,
    pub billing_basis: BillingBasis,
    pub cache_policy: CachePolicy,
    pub word_bank_canonical_digest: &'a str,
    pub effective_steering: &'a str,
    pub targets: &'a [TranscriptRow],
    pub context: &'a [TranscriptRow],
    /// Whether semantic context was omitted to fit the request budget. Two identical visible row slices can
    /// carry different grounding semantics, so this bit is part of exact local identity.
    pub context_truncated: bool,
    pub question: Option<&'a str>,
}

impl RequestKeyMaterial<'_> {
    fn encode(self, encoder: &mut CanonicalCborHmac) {
        // Schema-defined map order is part of key-v2. Values use shortest-form CBOR integers and NFC text.
        encoder.map(19);
        encoder.key("provider_id");
        encoder.text(self.provider.as_str());
        encoder.key("transport_id");
        encoder.text(self.transport.as_str());
        encoder.key("support_tier");
        encoder.text(support_tier_name(self.support_tier));
        encoder.key("connection_scope_uuid");
        encoder.text(self.connection_scope_id.as_str());
        encoder.key("region");
        encoder.optional_text(self.region);
        encoder.key("exact_model_id");
        encoder.text(self.exact_model_id.as_str());
        encoder.key("adapter_version");
        encoder.unsigned(u64::from(self.adapter_version));
        encoder.key("prompt_template_version");
        encoder.unsigned(u64::from(self.prompt_template_version));
        encoder.key("output_schema_version");
        encoder.unsigned(u64::from(self.output_schema_version));
        encoder.key("chunker_version");
        encoder.unsigned(u64::from(self.chunker_version));
        encoder.key("lane");
        encoder.text(lane_name(self.lane));
        encoder.key("billing_basis");
        encoder.text(billing_name(self.billing_basis));
        encoder.key("cache_policy");
        encode_cache_policy(encoder, self.cache_policy);
        encoder.key("word_bank_canonical_digest");
        encoder.text(self.word_bank_canonical_digest);
        encoder.key("steering_canonical_digest");
        let steering: String = self.effective_steering.nfc().collect();
        encoder.byte_string(&Sha256::digest(steering.as_bytes()));
        encoder.key("targets");
        encoder.rows(self.targets);
        encoder.key("context");
        encoder.rows(self.context);
        encoder.key("context_truncated");
        encoder.boolean(self.context_truncated);
        encoder.key("question_if_any");
        encoder.optional_text(self.question);
    }
}

/// Stable provider-prefix semantics. It deliberately cannot represent session, call, transcript, question,
/// or steering identity.
#[derive(Clone, Copy)]
pub struct ProviderCacheKeyMaterial<'a> {
    pub provider: &'a ProviderId,
    pub transport: &'a TransportId,
    pub support_tier: SupportTier,
    pub connection_scope_id: &'a ConnectionScopeId,
    pub region: Option<&'a str>,
    pub exact_model_id: &'a ModelId,
    pub adapter_version: u32,
    pub prompt_template_version: u32,
    pub output_schema_version: u32,
    pub prompt_task: PromptTask,
    pub provider_cache_mode: crate::ProviderCacheMode,
    pub word_bank_canonical_digest: &'a str,
}

impl ProviderCacheKeyMaterial<'_> {
    fn encode(self, encoder: &mut CanonicalCborHmac) {
        encoder.map(12);
        encoder.key("provider_id");
        encoder.text(self.provider.as_str());
        encoder.key("transport_id");
        encoder.text(self.transport.as_str());
        encoder.key("support_tier");
        encoder.text(support_tier_name(self.support_tier));
        encoder.key("connection_scope_uuid");
        encoder.text(self.connection_scope_id.as_str());
        encoder.key("region");
        encoder.optional_text(self.region);
        encoder.key("exact_model_id");
        encoder.text(self.exact_model_id.as_str());
        encoder.key("adapter_version");
        encoder.unsigned(u64::from(self.adapter_version));
        encoder.key("prompt_template_version");
        encoder.unsigned(u64::from(self.prompt_template_version));
        encoder.key("output_schema_version");
        encoder.unsigned(u64::from(self.output_schema_version));
        encoder.key("prompt_task");
        encoder.text(prompt_task_name(self.prompt_task));
        encoder.key("provider_cache_mode");
        encoder.text(provider_cache_name(self.provider_cache_mode));
        encoder.key("word_bank_canonical_digest");
        encoder.text(self.word_bank_canonical_digest);
    }
}

/// Minimal canonical CBOR encoder that streams directly into HMAC state. Only the definite-length types
/// needed by key-v1 are implemented; callers cannot obtain the sensitive canonical plaintext bytes.
struct CanonicalCborHmac {
    mac: HmacSha256,
}

impl CanonicalCborHmac {
    fn new(key: &DigestKey, domain: &[u8]) -> Self {
        Self {
            mac: key.hmac(domain),
        }
    }

    fn major_value(&mut self, major: u8, value: u64) {
        let prefix = major << 5;
        match value {
            0..=23 => self.mac.update(&[prefix | value as u8]),
            24..=0xff => self.mac.update(&[prefix | 24, value as u8]),
            0x100..=0xffff => {
                self.mac.update(&[prefix | 25]);
                self.mac.update(&(value as u16).to_be_bytes());
            }
            0x1_0000..=0xffff_ffff => {
                self.mac.update(&[prefix | 26]);
                self.mac.update(&(value as u32).to_be_bytes());
            }
            _ => {
                self.mac.update(&[prefix | 27]);
                self.mac.update(&value.to_be_bytes());
            }
        }
    }

    fn length(&mut self, major: u8, len: usize) {
        self.major_value(
            major,
            u64::try_from(len).expect("key material length fits canonical CBOR"),
        );
    }

    fn map(&mut self, len: usize) {
        self.length(5, len);
    }

    fn array(&mut self, len: usize) {
        self.length(4, len);
    }

    fn unsigned(&mut self, value: u64) {
        self.major_value(0, value);
    }

    fn byte_string(&mut self, bytes: &[u8]) {
        self.length(2, bytes.len());
        self.mac.update(bytes);
    }

    fn raw_text(&mut self, text: &str) {
        self.length(3, text.len());
        self.mac.update(text.as_bytes());
    }

    fn key(&mut self, key: &'static str) {
        self.raw_text(key);
    }

    fn text(&mut self, text: &str) {
        // NFC makes canonically equivalent Unicode requests exact-cache equivalent. Other whitespace and
        // punctuation remain byte-significant because they can change model behavior.
        let canonical: String = text.nfc().collect();
        self.raw_text(&canonical);
    }

    fn optional_text(&mut self, text: Option<&str>) {
        match text {
            None => self.mac.update(&[0xf6]), // CBOR null
            Some(text) => self.text(text),
        }
    }

    fn boolean(&mut self, value: bool) {
        self.mac.update(&[if value { 0xf5 } else { 0xf4 }]);
    }

    fn rows(&mut self, rows: &[TranscriptRow]) {
        self.array(rows.len());
        for row in rows {
            self.map(5);
            self.key("row_id");
            self.text(row.row_id.as_str());
            self.key("speaker");
            self.text(&row.speaker);
            self.key("start_ms");
            self.unsigned(row.start_ms);
            self.key("end_ms");
            self.unsigned(row.end_ms);
            self.key("raw_or_clean_text");
            self.text(&row.text);
        }
    }

    fn finish(self) -> [u8; 32] {
        self.mac.finalize().into_bytes().into()
    }
}

fn support_tier_name(value: SupportTier) -> &'static str {
    match value {
        SupportTier::Documented => "documented",
        SupportTier::Experimental => "experimental",
        SupportTier::Blocked => "blocked",
    }
}

fn lane_name(value: Lane) -> &'static str {
    match value {
        Lane::Live => "live",
        Lane::Final => "final",
        Lane::AdHocQuestion => "ad_hoc_question",
        Lane::PinnedQuestion => "pinned_question",
    }
}

fn billing_name(value: BillingBasis) -> &'static str {
    match value {
        BillingBasis::MeteredEstimate => "metered_estimate",
        BillingBasis::IncludedSubscription => "included_subscription",
        BillingBasis::NoProviderRequest => "no_provider_request",
        BillingBasis::Unknown => "unknown",
    }
}

fn prompt_task_name(value: PromptTask) -> &'static str {
    match value {
        PromptTask::Rewrite => "rewrite",
        PromptTask::Question => "question",
    }
}

fn local_cache_name(value: crate::LocalCacheMode) -> &'static str {
    match value {
        crate::LocalCacheMode::Reusable => "reusable",
        crate::LocalCacheMode::RecoveryOnly => "recovery_only",
        crate::LocalCacheMode::MemoryOnly => "memory_only",
    }
}

fn provider_cache_name(value: crate::ProviderCacheMode) -> &'static str {
    match value {
        crate::ProviderCacheMode::Off => "off",
        crate::ProviderCacheMode::ExplicitStablePrefix => "explicit_stable_prefix",
        crate::ProviderCacheMode::UnavoidableImplicit => "unavoidable_implicit",
        crate::ProviderCacheMode::Unavailable => "unavailable",
    }
}

fn encode_cache_policy(encoder: &mut CanonicalCborHmac, value: CachePolicy) {
    encoder.map(2);
    encoder.key("local");
    encoder.text(local_cache_name(value.local));
    encoder.key("provider");
    encoder.text(provider_cache_name(value.provider));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalCacheMode, ProviderCacheMode};

    fn id<T: TryFrom<&'static str>>(value: &'static str) -> T
    where
        T::Error: fmt::Debug,
    {
        T::try_from(value).unwrap()
    }

    fn row(text: &str) -> TranscriptRow {
        TranscriptRow {
            row_id: id("r-1"),
            speaker: "Speaker A".into(),
            start_ms: 10,
            end_ms: 20,
            text: text.into(),
        }
    }

    fn material<'a>(target: &'a [TranscriptRow]) -> RequestKeyMaterial<'a> {
        let provider = Box::leak(Box::new(id("provider")));
        let transport = Box::leak(Box::new(id("transport")));
        let scope = Box::leak(Box::new(id("scope-opaque")));
        let model = Box::leak(Box::new(id("model-exact")));
        RequestKeyMaterial {
            provider,
            transport,
            support_tier: SupportTier::Documented,
            connection_scope_id: scope,
            region: Some("region-a"),
            exact_model_id: model,
            adapter_version: 1,
            prompt_template_version: 1,
            output_schema_version: 1,
            chunker_version: 1,
            lane: Lane::Live,
            billing_basis: BillingBasis::MeteredEstimate,
            cache_policy: CachePolicy {
                local: LocalCacheMode::Reusable,
                provider: ProviderCacheMode::Off,
            },
            word_bank_canonical_digest: "bank-digest",
            effective_steering: "synthetic policy",
            targets: target,
            context: &[],
            context_truncated: false,
            question: None,
        }
    }

    #[test]
    fn canonical_unicode_equivalents_have_the_same_request_key() {
        let composed = [row("caf\u{e9}")];
        let decomposed = [row("cafe\u{301}")];
        let key = DigestKey::new([7; 32]);
        assert_eq!(
            RequestKey::derive(&key, &material(&composed)),
            RequestKey::derive(&key, &material(&decomposed))
        );
    }

    #[test]
    fn semantic_field_changes_invalidate_the_key() {
        let rows = [row("synthetic transcript")];
        let base = material(&rows);
        let key = DigestKey::new([9; 32]);
        let expected = RequestKey::derive(&key, &base);
        macro_rules! assert_changed {
            ($material:expr) => {
                assert_ne!(expected, RequestKey::derive(&key, &$material))
            };
        }

        let other_provider = ProviderId::new("provider-other").unwrap();
        let mut changed = base;
        changed.provider = &other_provider;
        assert_changed!(changed);
        let other_transport = TransportId::new("transport-other").unwrap();
        let mut changed = base;
        changed.transport = &other_transport;
        assert_changed!(changed);
        let other_scope = ConnectionScopeId::new("scope-other").unwrap();
        let mut changed = base;
        changed.connection_scope_id = &other_scope;
        assert_changed!(changed);
        let other_model = ModelId::new("model-other").unwrap();
        let mut changed = base;
        changed.exact_model_id = &other_model;
        assert_changed!(changed);

        for mutate in [
            |material: &mut RequestKeyMaterial<'_>| material.adapter_version += 1,
            |material: &mut RequestKeyMaterial<'_>| material.prompt_template_version += 1,
            |material: &mut RequestKeyMaterial<'_>| material.output_schema_version += 1,
            |material: &mut RequestKeyMaterial<'_>| material.chunker_version += 1,
        ] {
            let mut changed = base;
            mutate(&mut changed);
            assert_changed!(changed);
        }

        let mut changed = base;
        changed.support_tier = SupportTier::Experimental;
        assert_changed!(changed);
        let mut changed = base;
        changed.region = Some("region-other");
        assert_changed!(changed);
        let mut changed = base;
        changed.lane = Lane::Final;
        assert_changed!(changed);
        let mut changed = base;
        changed.billing_basis = BillingBasis::Unknown;
        assert_changed!(changed);
        let mut changed = base;
        changed.cache_policy.local = LocalCacheMode::RecoveryOnly;
        assert_changed!(changed);
        let mut changed = base;
        changed.cache_policy.provider = ProviderCacheMode::ExplicitStablePrefix;
        assert_changed!(changed);
        let mut changed = base;
        changed.word_bank_canonical_digest = "different-bank-digest";
        assert_changed!(changed);
        let mut changed = base;
        changed.effective_steering = "different synthetic policy";
        assert_changed!(changed);
        let mut changed = base;
        changed.question = Some("synthetic question");
        assert_changed!(changed);
        let mut changed = base;
        changed.context_truncated = true;
        assert_changed!(changed);

        let context = [row("synthetic context")];
        let mut changed = base;
        changed.context = &context;
        assert_changed!(changed);
        let changed_rows = [row("changed synthetic transcript")];
        let changed = material(&changed_rows);
        assert_changed!(changed);
    }

    #[test]
    fn actual_credential_identity_fences_local_and_provider_cache_keys() {
        let rows = [row("synthetic transcript")];
        let material = material(&rows);
        let key = DigestKey::new([0x31; 32]);
        let request = RequestKey::derive(&key, &material);
        let first = request.bind_credential(&key, &[1; 32]);
        let second = request.bind_credential(&key, &[2; 32]);
        assert_ne!(first, second);
        assert_eq!(
            RequestKey::from_base64url(&first.to_base64url()),
            Some(first)
        );

        let provider_material = ProviderCacheKeyMaterial {
            provider: material.provider,
            transport: material.transport,
            support_tier: material.support_tier,
            connection_scope_id: material.connection_scope_id,
            region: material.region,
            exact_model_id: material.exact_model_id,
            adapter_version: material.adapter_version,
            prompt_template_version: material.prompt_template_version,
            output_schema_version: material.output_schema_version,
            prompt_task: PromptTask::Rewrite,
            provider_cache_mode: ProviderCacheMode::ExplicitStablePrefix,
            word_bank_canonical_digest: material.word_bank_canonical_digest,
        };
        let provider = ProviderCacheKey::derive(&key, &provider_material);
        assert_ne!(
            provider.bind_credential(&key, &[1; 32]),
            provider.bind_credential(&key, &[2; 32])
        );
    }

    #[test]
    fn opaque_keys_do_not_leak_key_or_prompt_material() {
        let rows = [row("never-readable-synthetic-phrase")];
        let request_material = material(&rows);
        let key = DigestKey::new([0x41; 32]);
        let request = RequestKey::derive(&key, &request_material).to_base64url();
        assert!(!request.contains("never-readable"));
        assert!(!format!("{key:?}").contains("AAAA"));

        let provider_material = ProviderCacheKeyMaterial {
            provider: request_material.provider,
            transport: request_material.transport,
            support_tier: request_material.support_tier,
            connection_scope_id: request_material.connection_scope_id,
            region: request_material.region,
            exact_model_id: request_material.exact_model_id,
            adapter_version: request_material.adapter_version,
            prompt_template_version: request_material.prompt_template_version,
            output_schema_version: request_material.output_schema_version,
            prompt_task: PromptTask::Rewrite,
            provider_cache_mode: request_material.cache_policy.provider,
            word_bank_canonical_digest: "opaque-bank-digest",
        };
        let provider = ProviderCacheKey::derive(&key, &provider_material);
        assert!(!provider.as_str().contains("bank"));
        assert!(!provider.as_str().contains("scope"));

        let mut question_material = provider_material;
        question_material.prompt_task = PromptTask::Question;
        assert_ne!(
            provider,
            ProviderCacheKey::derive(&key, &question_material),
            "different immutable prompt policies must not share a provider prefix key"
        );
    }
}
