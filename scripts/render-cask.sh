#!/usr/bin/env bash
# Render Casks/corti.rb for a published release, to stdout.
# Computes sha256 from the PUBLIC GitHub release dmg (no token needed for public repos). Used by
# release.yml's update-cask job AND by hand for the first release (before the workflow runs against a tag).
#
#   VERSION=0.1.0 scripts/render-cask.sh > Casks/corti.rb
#   VERSION=0.1.0 REPO=vasovagal/corti scripts/render-cask.sh > Casks/corti.rb
#
# corti ships an UNSIGNED (ad-hoc) .app, so the cask strips the quarantine xattr in a postflight to skip
# Gatekeeper's "Open Anyway" wall. A private tap may do this; the official homebrew-cask repo may not.
set -euo pipefail

VERSION="${VERSION:?set VERSION, e.g. VERSION=0.1.0}"
REPO="${REPO:-vasovagal/corti}"
BASE="https://github.com/${REPO}/releases/download/v${VERSION}"

# Tauri v2 default dmg name for an aarch64 build: Corti_<version>_aarch64.dmg
DMG="Corti_${VERSION}_aarch64.dmg"
URL="${BASE}/${DMG}"

# Robustness against Tauri naming drift: if the canonical name isn't downloadable, discover the single
# .dmg asset on the release via gh (needs GH_TOKEN in CI; uses the logged-in gh locally).
if ! curl -fsIL "$URL" >/dev/null 2>&1; then
  if command -v gh >/dev/null 2>&1; then
    DMG="$(gh release view "v${VERSION}" --repo "$REPO" \
            --json assets --jq '.assets[].name | select(endswith(".dmg"))' | head -1 || true)"
  fi
  [ -n "${DMG:-}" ] || { echo "no .dmg asset found on v${VERSION} of ${REPO}" >&2; exit 1; }
  URL="${BASE}/${DMG}"
fi

SHA="$(curl -fsSL "$URL" | shasum -a 256 | awk '{print $1}')"
[ -n "$SHA" ] || { echo "failed to compute sha256 for ${URL}" >&2; exit 1; }

cat <<RB
cask "corti" do
  version "${VERSION}"
  sha256 "${SHA}"

  url "${URL}"
  name "Corti"
  desc "Menu-bar app that auto-records meetings and files transcript notes to vagus"
  homepage "https://github.com/${REPO}"

  # Apple Silicon only, latest macOS only. The app's minimumSystemVersion is 14.2; Homebrew's macOS gate
  # is per-major, so ">= :sonoma" (14.x) is the closest expressible floor — the real 14.2 floor stays in
  # the bundle's LSMinimumSystemVersion (see the tap README / ADR 0006).
  depends_on macos: ">= :sonoma"
  depends_on arch: :arm64

  app "Corti.app"

  # UNSIGNED (ad-hoc) build: strip the quarantine xattr so Gatekeeper doesn't force the "Open Anyway" dance
  # on first launch. macOS still prompts for Microphone + System Audio Recording on first run — and again
  # after each update, because the ad-hoc cdhash changes every release (see ADR 0006).
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-r", "-d", "com.apple.quarantine", "#{appdir}/Corti.app"],
                   sudo: false
  end

  zap trash: [
    "~/.local/share/corti",
    "~/Library/Application Support/com.vasovagal.corti",
    "~/Library/Caches/com.vasovagal.corti",
    "~/Library/Caches/corti",
    "~/Library/HTTPStorages/com.vasovagal.corti",
    "~/Library/Preferences/com.vasovagal.corti.plist",
    "~/Library/Saved Application State/com.vasovagal.corti.savedState",
    "~/Library/WebKit/com.vasovagal.corti",
  ]
end
RB
