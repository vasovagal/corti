//! The app that owns the microphone during a recording.
//!
//! corti attributes a recording to whichever process grabbed the mic (best-effort). We carry the macOS
//! bundle identifier and resolve a friendly display name for known conferencing apps; anything unknown
//! falls back to a humanized form of the bundle id (or "Unknown app" when we couldn't attribute at all).

use serde::{Deserialize, Serialize};

/// The application that held the microphone for a recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwningApp {
    /// macOS bundle identifier, e.g. `us.zoom.xos`. `None` when attribution failed entirely.
    pub bundle_id: Option<String>,
    /// Friendly display name, e.g. `Zoom`. Always populated (falls back to a humanized bundle id).
    pub name: String,
}

impl OwningApp {
    /// Build from a bundle id, mapping known conferencing apps to a friendly name and humanizing the rest.
    pub fn from_bundle_id(bundle_id: impl Into<String>) -> Self {
        let bundle_id = bundle_id.into();
        let name = friendly_name(&bundle_id).unwrap_or_else(|| humanize(&bundle_id));
        Self {
            bundle_id: Some(bundle_id),
            name,
        }
    }

    /// Attribution failed — we know the mic is in use but not by whom.
    pub fn unknown() -> Self {
        Self {
            bundle_id: None,
            name: "Unknown app".to_string(),
        }
    }
}

/// Whether a bundle id is a recognized conferencing/communication app (vs. a random or system process).
/// Used by attribution to prefer the real meeting app over system audio helpers (`com.apple.CoreSpeech`,
/// etc.) when several processes hold the mic at once.
pub fn is_known_conferencing_app(bundle_id: &str) -> bool {
    friendly_name(bundle_id).is_some()
}

/// Known apps, matched by bundle-id prefix. Electron/Chromium apps (Slack, Discord, Chrome, Teams) open
/// the mic from a *helper* process whose bundle id is the base id plus a suffix
/// (`com.tinyspeck.slackmacgap.helper (Renderer)`), so we match `base` or anything under `base.`.
///
/// Order matters: more specific ids first (Chrome Canary before Chrome).
const KNOWN_APPS: &[(&str, &str)] = &[
    ("us.zoom.xos", "Zoom"),
    ("com.tinyspeck.slackmacgap", "Slack"),
    ("com.google.Chrome.canary", "Chrome Canary"),
    ("com.google.Chrome", "Chrome"),
    ("org.mozilla.firefox", "Firefox"),
    ("com.hnc.Discord", "Discord"),
    ("com.microsoft.teams2", "Microsoft Teams"),
    ("com.microsoft.teams", "Microsoft Teams"),
    ("com.apple.FaceTime", "FaceTime"),
    ("com.apple.Safari", "Safari"),
    ("com.cisco.webexmeetingsapp", "Webex"),
];

/// Friendly name for a known conferencing/communication app, if recognized (matched by prefix so helper
/// processes resolve to their parent app).
fn friendly_name(bundle_id: &str) -> Option<String> {
    KNOWN_APPS
        .iter()
        .find(|(base, _)| bundle_id == *base || bundle_id.starts_with(&format!("{base}.")))
        .map(|(_, name)| name.to_string())
}

/// Turn an unrecognized bundle id into a readable name: take the last dot-segment and title-case it.
/// `com.acme.SuperPhone` → `SuperPhone`; `org.example.cool-app` → `Cool App`.
fn humanize(bundle_id: &str) -> String {
    let last = bundle_id.rsplit('.').next().unwrap_or(bundle_id);
    if last.is_empty() {
        return bundle_id.to_string();
    }
    last.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_apps_map_to_friendly_names() {
        assert_eq!(OwningApp::from_bundle_id("us.zoom.xos").name, "Zoom");
        assert_eq!(
            OwningApp::from_bundle_id("com.tinyspeck.slackmacgap").name,
            "Slack"
        );
        assert_eq!(OwningApp::from_bundle_id("com.hnc.Discord").name, "Discord");
    }

    #[test]
    fn electron_helper_ids_resolve_to_parent_app() {
        // The real id seen in a live Slack huddle.
        assert_eq!(
            OwningApp::from_bundle_id("com.tinyspeck.slackmacgap.helper").name,
            "Slack"
        );
        assert_eq!(
            OwningApp::from_bundle_id("com.tinyspeck.slackmacgap.helper (Renderer)").name,
            "Slack"
        );
        assert_eq!(
            OwningApp::from_bundle_id("com.google.Chrome.helper").name,
            "Chrome"
        );
        assert!(is_known_conferencing_app("com.hnc.Discord.helper (GPU)"));
    }

    #[test]
    fn chrome_canary_is_not_shadowed_by_chrome() {
        assert_eq!(
            OwningApp::from_bundle_id("com.google.Chrome.canary").name,
            "Chrome Canary"
        );
    }

    #[test]
    fn unknown_bundle_id_is_humanized() {
        assert_eq!(
            OwningApp::from_bundle_id("com.acme.SuperPhone").name,
            "SuperPhone"
        );
        assert_eq!(
            OwningApp::from_bundle_id("org.example.cool-app").name,
            "Cool App"
        );
    }

    #[test]
    fn unknown_attribution_has_no_bundle_id() {
        let app = OwningApp::unknown();
        assert!(app.bundle_id.is_none());
        assert_eq!(app.name, "Unknown app");
    }

    #[test]
    fn bundle_id_is_preserved() {
        let app = OwningApp::from_bundle_id("us.zoom.xos");
        assert_eq!(app.bundle_id.as_deref(), Some("us.zoom.xos"));
    }
}
