Claude plans should be mirrored to github issues and labeled with either `Feature` or `Bug Fix`. Issues should be sufficient for new claude sessions to pick up on the work

## Releasing

After pushing a `vX.Y.Z` tag, `release.yml` builds + publishes the ad-hoc-signed `.dmg` + `.app.zip` to the
GitHub release (no signing/notarization; no PAT). Once the public two-asset contract passes, its final job
updates `vasovagal.github.io` through the Corti-specific `LANDING_PAGE_DEPLOY_KEY`; that key can write only
the site repository, and the idempotent site commit is `corti bumped to X.Y.Z`.

After the public two-asset contract passes, an independent release job renders the cask and updates
`vasovagal/homebrew-tap/Casks/corti.rb` automatically. Re-runs are idempotent; older releases cannot
downgrade the tap. Provision the org GitHub App with **Contents: write** installed only on
`vasovagal/homebrew-tap`; set repository variable `HOMEBREW_PUBLISHER_CLIENT_ID` and secret
`HOMEBREW_PUBLISHER_PRIVATE_KEY` in this source repo. The short-lived token is minted only after the
public assets pass validation and the cask renders. If the job fails, repair it by re-running the release
workflow (or manually render with `VERSION=X.Y.Z scripts/render-cask.sh`). No PAT is required.

The tap also holds vagus's formula. Install UX: `brew tap vasovagal/tap` then
`brew install --cask corti`. See
[`design/adr/0006`](./design/adr/0006-distribution-unsigned-cask-tap.md).

## Screenshots

Marketing screenshots are generated from the real React frontend with deterministic Tauri IPC fixtures:

    cd screenshots
    npm install
    npm run capture

`./screenshots/update_site.py` captures and copies the PNGs into the sibling
`vasovagal.github.io/assets/screenshots/` checkout. Generated `screenshots/output/` files stay ignored;
commit the harness here and the copied images in the website repo. See [`screenshots/README.md`](./screenshots/README.md).

## Documentation

Write and review documentation with the [succinct-docs skill](.claude/skills/succinct-docs/SKILL.md)
and follow [AGENTS.md](AGENTS.md#documentation). Preserve the existing invariants above.
