//! Private per-slot files for the fixed [`SecretPurpose`] slots.
//!
//! Each slot is one raw-bytes file under `~/.local/share/corti/hosted-secrets/` — directory mode 0700,
//! files mode 0600, written atomically through [`crate::private_file`], which also refuses symlinks, foreign
//! owners, and loosened modes on read. Callers get bytes or presence; the webview only ever learns presence.
//! ADR 0015 §5 as amended on 2026-08-26 grants this binding.
//!
//! # Why not the macOS Keychain
//!
//! A login-keychain item guards its secret with two checks keyed on code-signing identity: the decrypt ACL's
//! trusted-application list, and (since macOS 10.12) a partition list that securityd consults even when
//! that ACL trusts any application. Corti ships ad-hoc signed (ADR 0006), so its identity in both is the
//! cdhash, which changes on every build. Each rebuild or update is a stranger to every item, "Always Allow"
//! survives exactly one build, and because an ad-hoc requester carries no certificate to validate, macOS
//! escalates the confirmation dialog to demanding the login keychain password. Measured on macOS 26 for
//! #145: an item created by one ad-hoc build with a NULL (any-application) decrypt ACL still raised the
//! dialog when the next build read it. The data-protection keychain never prompts but needs an
//! application-identifier entitlement that ad-hoc signing cannot carry. An owner-only file offers the same
//! practical protection a "readable by any application" item would — the `~/.aws/credentials` posture —
//! without the dialog.
//!
//! Items earlier builds wrote under the Keychain service `com.vasovagal.corti.hosted` are neither read
//! (reading is the dialog) nor deleted (that needs the Security.framework binding this module retires); a
//! stale item is inert and Keychain Access removes it by hand.

use std::fs::Permissions;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use zeroize::{Zeroize as _, Zeroizing};

use crate::postprocess_config::SecretPurpose;
use crate::private_file::{atomic_write_private, read_private};

/// Directory under [`corti_queue::data_dir`] holding one file per slot.
pub(crate) const SECRETS_DIR_NAME: &str = "hosted-secrets";
const SECRETS_DIR_MODE: u32 = 0o700;
/// Generous bound for any slot; the largest is the ChatGPT rotating-credential document.
const MAX_SECRET_BYTES: usize = 64 * 1024;
const LABEL: &str = "hosted secret";

fn default_dir() -> Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join(SECRETS_DIR_NAME))
}

fn slot_path(dir: &Path, purpose: SecretPurpose) -> PathBuf {
    dir.join(purpose.slot_name())
}

/// Create the slot directory owner-only, or verify an existing one is a real directory and tighten a
/// loosened mode. A symlink or non-directory at the path fails closed.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {LABEL} parent {}", parent.display()))?;
    }
    match std::fs::DirBuilder::new()
        .mode(SECRETS_DIR_MODE)
        .create(dir)
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("creating {LABEL} directory {}", dir.display()));
        }
    }
    let metadata = std::fs::symlink_metadata(dir)
        .with_context(|| format!("inspecting {LABEL} directory {}", dir.display()))?;
    ensure!(
        metadata.is_dir(),
        "refusing non-directory {LABEL} path {}",
        dir.display()
    );
    // `mode` on creation is filtered by the process umask; pin the exact owner-only contract.
    if metadata.permissions().mode() & 0o777 != SECRETS_DIR_MODE {
        std::fs::set_permissions(dir, Permissions::from_mode(SECRETS_DIR_MODE))
            .with_context(|| format!("securing {LABEL} directory {}", dir.display()))?;
    }
    Ok(())
}

/// Read one slot. A missing file is `Ok(None)`; a file that fails the private-file checks is an error.
pub(crate) fn read(purpose: SecretPurpose) -> Result<Option<Vec<u8>>> {
    read_in(&default_dir()?, purpose)
}

fn read_in(dir: &Path, purpose: SecretPurpose) -> Result<Option<Vec<u8>>> {
    read_private(&slot_path(dir, purpose), LABEL, MAX_SECRET_BYTES)
        .context("reading a hosted secret")
}

/// Store one slot. Empty values are refused so [`is_present`] can never report a value that is not there.
pub(crate) fn write(purpose: SecretPurpose, value: &[u8]) -> Result<()> {
    write_in(&default_dir()?, purpose, value)
}

fn write_in(dir: &Path, purpose: SecretPurpose, value: &[u8]) -> Result<()> {
    if value.is_empty() {
        bail!("refusing to store an empty hosted secret");
    }
    if value.len() > MAX_SECRET_BYTES {
        bail!("refusing to store an oversized hosted secret");
    }
    ensure_private_dir(dir)?;
    atomic_write_private(&slot_path(dir, purpose), value, LABEL).context("storing a hosted secret")
}

/// Delete one slot. Deleting an absent slot succeeds, so "clear" is idempotent.
pub(crate) fn delete(purpose: SecretPurpose) -> Result<()> {
    delete_in(&default_dir()?, purpose)
}

fn delete_in(dir: &Path, purpose: SecretPurpose) -> Result<()> {
    match std::fs::remove_file(slot_path(dir, purpose)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("deleting a hosted secret"),
    }
}

/// Whether a readable, non-empty value is stored. This is the only fact ever projected to the webview.
pub(crate) fn is_present(purpose: SecretPurpose) -> bool {
    default_dir().is_ok_and(|dir| is_present_in(&dir, purpose))
}

fn is_present_in(dir: &Path, purpose: SecretPurpose) -> bool {
    match read_in(dir, purpose) {
        Ok(Some(mut bytes)) => {
            let present = !bytes.is_empty();
            bytes.zeroize();
            present
        }
        _ => false,
    }
}

/// The hosted store's master key as loaded or freshly generated.
pub(crate) struct MasterKey {
    pub(crate) bytes: Zeroizing<[u8; 32]>,
    /// No key existed and this one was just stored. Ciphertext sealed under any earlier key is now
    /// unrecoverable by construction.
    pub(crate) created: bool,
}

/// Load the master key, or generate one with `generate` and store it.
pub(crate) fn load_or_create_master_key(
    generate: impl FnOnce(&mut [u8]) -> Result<()>,
) -> Result<MasterKey> {
    load_or_create_master_key_in(&default_dir()?, generate)
}

fn load_or_create_master_key_in(
    dir: &Path,
    generate: impl FnOnce(&mut [u8]) -> Result<()>,
) -> Result<MasterKey> {
    let purpose = SecretPurpose::PostprocessCacheMasterKey;
    if let Some(existing) = read_in(dir, purpose)? {
        let existing = Zeroizing::new(existing);
        let bytes: [u8; 32] = existing
            .as_slice()
            .try_into()
            .context("hosted master key has an invalid length")?;
        return Ok(MasterKey {
            bytes: Zeroizing::new(bytes),
            created: false,
        });
    }
    let mut bytes = Zeroizing::new([0u8; 32]);
    generate(bytes.as_mut_slice())?;
    write_in(dir, purpose, bytes.as_slice())?;
    Ok(MasterKey {
        bytes,
        created: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A not-yet-existing directory under the system temp dir, so `ensure_private_dir` creates it.
    fn fresh_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("corti-secret-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn slots_address_distinct_files_under_one_directory() {
        let names = [
            SecretPurpose::OpenAiApiKey,
            SecretPurpose::ChatGptSubscriptionCredential,
            SecretPurpose::AnthropicApiKey,
            SecretPurpose::PostprocessCacheMasterKey,
            SecretPurpose::AwsAccessKeyId,
            SecretPurpose::AwsSecretAccessKey,
            SecretPurpose::AwsSessionToken,
        ]
        .map(SecretPurpose::slot_name);
        assert_eq!(
            names
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            names.len()
        );
        for name in names {
            assert!(!name.contains('/') && !name.starts_with('.'), "{name}");
        }
        assert_eq!(SECRETS_DIR_NAME, "hosted-secrets");
    }

    #[test]
    fn round_trips_and_clears_a_slot() {
        let dir = fresh_dir("round-trip");
        let purpose = SecretPurpose::AwsSessionToken;
        assert!(!is_present_in(&dir, purpose));
        assert_eq!(read_in(&dir, purpose).unwrap(), None);

        write_in(&dir, purpose, b"synthetic-round-trip-value").unwrap();
        assert!(is_present_in(&dir, purpose));
        assert_eq!(
            read_in(&dir, purpose).unwrap().as_deref(),
            Some(b"synthetic-round-trip-value".as_slice())
        );
        write_in(&dir, purpose, b"synthetic-replacement-value").unwrap();
        assert_eq!(
            read_in(&dir, purpose).unwrap().as_deref(),
            Some(b"synthetic-replacement-value".as_slice())
        );

        delete_in(&dir, purpose).unwrap();
        assert!(!is_present_in(&dir, purpose));
        assert_eq!(read_in(&dir, purpose).unwrap(), None);
        // Clearing twice must not fail.
        delete_in(&dir, purpose).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_and_files_are_owner_only() {
        let dir = fresh_dir("modes");
        let purpose = SecretPurpose::AnthropicApiKey;
        write_in(&dir, purpose, b"synthetic").unwrap();
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(mode_of(&slot_path(&dir, purpose)), 0o600);

        // A loosened directory mode is tightened again on the next write.
        std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        write_in(&dir, SecretPurpose::OpenAiApiKey, b"synthetic").unwrap();
        assert_eq!(mode_of(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_and_oversized_values_are_refused() {
        let dir = fresh_dir("refused");
        assert!(write_in(&dir, SecretPurpose::AwsAccessKeyId, b"").is_err());
        assert!(
            write_in(
                &dir,
                SecretPurpose::AwsAccessKeyId,
                &[0u8; MAX_SECRET_BYTES + 1]
            )
            .is_err()
        );
        assert!(!dir.join(SecretPurpose::AwsAccessKeyId.slot_name()).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loosened_or_symlinked_slots_fail_closed() {
        let dir = fresh_dir("fail-closed");
        let purpose = SecretPurpose::AwsSecretAccessKey;
        write_in(&dir, purpose, b"synthetic").unwrap();
        std::fs::set_permissions(slot_path(&dir, purpose), Permissions::from_mode(0o644)).unwrap();
        assert!(read_in(&dir, purpose).is_err());
        assert!(!is_present_in(&dir, purpose));

        let target = dir.join("elsewhere");
        std::fs::write(&target, b"synthetic").unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o600)).unwrap();
        delete_in(&dir, purpose).unwrap();
        std::os::unix::fs::symlink(&target, slot_path(&dir, purpose)).unwrap();
        assert!(read_in(&dir, purpose).is_err());
        assert!(!is_present_in(&dir, purpose));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlinked_directory_fails_closed() {
        let dir = fresh_dir("symlinked-dir");
        let target = fresh_dir("symlinked-dir-target");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        assert!(write_in(&dir, SecretPurpose::AwsAccessKeyId, b"synthetic").is_err());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn master_key_is_created_once_and_then_loaded() {
        let dir = fresh_dir("master-key");
        let first = load_or_create_master_key_in(&dir, |bytes| {
            bytes.copy_from_slice(&[7u8; 32]);
            Ok(())
        })
        .unwrap();
        assert!(first.created);
        assert_eq!(*first.bytes, [7u8; 32]);
        assert_eq!(
            mode_of(&slot_path(&dir, SecretPurpose::PostprocessCacheMasterKey)),
            0o600
        );

        let second =
            load_or_create_master_key_in(&dir, |_| panic!("an existing key is never regenerated"))
                .unwrap();
        assert!(!second.created);
        assert_eq!(*second.bytes, [7u8; 32]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn master_key_with_an_invalid_length_is_an_error_not_a_regeneration() {
        let dir = fresh_dir("master-key-length");
        write_in(&dir, SecretPurpose::PostprocessCacheMasterKey, &[1u8; 31]).unwrap();
        let error = load_or_create_master_key_in(&dir, |_| panic!("must not regenerate"))
            .err()
            .expect("a malformed key is an error");
        assert!(format!("{error:#}").contains("invalid length"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
