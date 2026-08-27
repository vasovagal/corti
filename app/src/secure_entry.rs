//! App-owned AppKit secure entry for provider secrets.
//!
//! The typed value goes from `NSSecureTextField` straight into the private secret store on the main thread. It never
//! crosses the IPC boundary, so no browser field ever holds a key and React learns only presence. ADR
//! 0015 §5 grants this macOS binding.

use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSSecureTextField, NSTextField,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use tauri::AppHandle;
use zeroize::Zeroize as _;

use crate::{postprocess_config::SecretPurpose, secret_store};

/// Longest value the sheet will accept. AWS secret keys and API keys are far shorter; the cap only stops
/// a paste of something that could not possibly be a credential.
const MAX_SECRET_BYTES: usize = 8 * 1024;

/// What the user did with the sheet. No variant can carry the secret itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecureEntryOutcome {
    Stored,
    Cancelled,
    /// Submitted, but the value could not be a credential (empty, oversized, or not header-safe).
    Rejected,
}

/// Prompt for one secret and store it. Runs the sheet on the main thread and blocks the calling command
/// thread until it closes.
pub(crate) fn prompt_and_store(
    app: &AppHandle,
    purpose: SecretPurpose,
    title: &str,
    detail: &str,
) -> Result<SecureEntryOutcome> {
    let (tx, rx) = mpsc::channel();
    let (title, detail) = (title.to_owned(), detail.to_owned());
    app.clone()
        .run_on_main_thread(move || {
            let _ = tx.send(show_sheet(purpose, &title, &detail));
        })
        .context("scheduling the secure-entry sheet on the main thread")?;
    rx.recv()
        .context("the secure-entry sheet did not return an outcome")?
}

/// Must be called on the main thread.
fn show_sheet(purpose: SecretPurpose, title: &str, detail: &str) -> Result<SecureEntryOutcome> {
    let Some(mtm) = MainThreadMarker::new() else {
        bail!("the secure-entry sheet must run on the main thread");
    };
    // An Accessory app's modal can otherwise open behind the frontmost application.
    let ns_app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    ns_app.activateIgnoringOtherApps(true);

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let field: Retained<NSSecureTextField> = NSSecureTextField::new(mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(320.0, 24.0),
    ));
    let text_field: &NSTextField = &field;
    text_field.setPlaceholderString(Some(&NSString::from_str("Paste the value")));
    alert.setAccessoryView(Some(&field));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return Ok(SecureEntryOutcome::Cancelled);
    }

    let mut value = {
        use objc2_app_kit::NSControl;
        let control: &NSControl = &field;
        control.stringValue().to_string()
    };
    // Clear the field's own copy as soon as the value has been read out of AppKit.
    text_field.setStringValue(&NSString::from_str(""));

    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_SECRET_BYTES
        || !trimmed.bytes().all(|byte| byte.is_ascii_graphic())
    {
        value.zeroize();
        return Ok(SecureEntryOutcome::Rejected);
    }
    let mut stored = trimmed.to_owned();
    value.zeroize();
    let result = secret_store::write(purpose, stored.as_bytes());
    stored.zeroize();
    result?;
    Ok(SecureEntryOutcome::Stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_cannot_carry_the_secret() {
        // A compile-time-ish guard: the enum is Copy and field-free, so no rendering path can leak.
        for outcome in [
            SecureEntryOutcome::Stored,
            SecureEntryOutcome::Cancelled,
            SecureEntryOutcome::Rejected,
        ] {
            let rendered = format!("{outcome:?}");
            assert!(rendered.chars().all(|character| character.is_alphabetic()));
        }
        assert_eq!(std::mem::size_of::<SecureEntryOutcome>(), 1);
    }
}
