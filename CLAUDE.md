Claude plans should be mirrored to github issues and labeled with either `Feature` or `Bug Fix`. Issues should be sufficient for new claude sessions to pick up on the work

## Releasing

After pushing a `vX.Y.Z` tag, `release.yml` builds + publishes the ad-hoc-signed `.dmg` + `.app.zip` to the
GitHub release (no signing/notarization; no PAT). **You must then update the Homebrew tap by hand — CI never
writes it.** Regenerate the cask and commit it to the shared tap `vasovagal/homebrew-tap`:

    VERSION=X.Y.Z scripts/render-cask.sh > <path-to>/homebrew-tap/Casks/corti.rb

Then `cd` into the tap clone, `git commit -am "corti X.Y.Z"`, `git push`. The tap also holds vagus's
formula. Install UX: `brew tap vasovagal/tap` then `brew install --cask corti`. See
[`design/adr/0006`](./design/adr/0006-distribution-unsigned-cask-tap.md).
