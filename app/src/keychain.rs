//! Narrow Security.framework wrapper over the fixed [`SecretPurpose`] slots.
//!
//! Every item is an explicitly non-synchronizing generic password, so nothing here can reach iCloud
//! Keychain. Callers get bytes or presence; the webview only ever learns presence. ADR 0015 §5 grants
//! this macOS binding.

use anyhow::{Context, Result};
use security_framework::passwords::{
    PasswordOptions, delete_generic_password, generic_password, set_generic_password_options,
};
use security_framework_sys::base::errSecItemNotFound;

use crate::postprocess_config::SecretPurpose;

pub(crate) const HOSTED_KEYCHAIN_SERVICE: &str = "com.vasovagal.corti.hosted";

fn options(purpose: SecretPurpose) -> PasswordOptions {
    let mut options =
        PasswordOptions::new_generic_password(HOSTED_KEYCHAIN_SERVICE, purpose.keychain_account());
    // Explicit false is important: hosted secrets must never enter iCloud Keychain synchronization.
    options.set_access_synchronized(Some(false));
    options
}

/// Read one slot. A missing item is `Ok(None)`; only a real Keychain failure is an error.
pub(crate) fn read(purpose: SecretPurpose) -> Result<Option<Vec<u8>>> {
    match generic_password(options(purpose)) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == errSecItemNotFound => Ok(None),
        Err(error) => Err(error).context("reading a non-synchronizing hosted Keychain item"),
    }
}

pub(crate) fn write(purpose: SecretPurpose, value: &[u8]) -> Result<()> {
    set_generic_password_options(value, options(purpose))
        .context("storing a non-synchronizing hosted Keychain item")
}

/// Delete one slot. Deleting an absent item succeeds, so "clear" is idempotent.
pub(crate) fn delete(purpose: SecretPurpose) -> Result<()> {
    match delete_generic_password(HOSTED_KEYCHAIN_SERVICE, purpose.keychain_account()) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == errSecItemNotFound => Ok(()),
        Err(error) => Err(error).context("deleting a non-synchronizing hosted Keychain item"),
    }
}

/// Whether a non-empty value is stored. This is the only fact ever projected to the webview.
pub(crate) fn is_present(purpose: SecretPurpose) -> bool {
    use zeroize::Zeroize as _;

    match read(purpose) {
        Ok(Some(mut bytes)) => {
            let present = !bytes.is_empty();
            bytes.zeroize();
            present
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_address_distinct_accounts_under_one_service() {
        let accounts = [
            SecretPurpose::AwsAccessKeyId,
            SecretPurpose::AwsSecretAccessKey,
            SecretPurpose::AwsSessionToken,
        ]
        .map(SecretPurpose::keychain_account);
        assert_eq!(
            accounts
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            accounts.len()
        );
        assert_eq!(HOSTED_KEYCHAIN_SERVICE, "com.vasovagal.corti.hosted");
    }

    /// Touches the real login Keychain, so it stays opt-in rather than running in CI.
    #[test]
    #[ignore = "requires an unlocked login Keychain"]
    fn round_trips_and_clears_a_slot() {
        let purpose = SecretPurpose::AwsSessionToken;
        write(purpose, b"synthetic-round-trip-value").unwrap();
        assert!(is_present(purpose));
        assert_eq!(
            read(purpose).unwrap().as_deref(),
            Some(b"synthetic-round-trip-value".as_slice())
        );
        delete(purpose).unwrap();
        assert!(!is_present(purpose));
        // Clearing twice must not fail.
        delete(purpose).unwrap();
    }
}
