# Corti screenshot capture

Deterministic product screenshots for [vasovagal.github.io](https://vasovagal.github.io), using the same pattern as the sister Claria project.

Playwright runs Corti's **real React frontend** from `app/ui/`. A browser init script replaces only Tauri IPC and event subscriptions with fixed, non-personal fixtures, so no Rust build, microphone permission, model download, recording, or vagus vault is needed.

## Run

```sh
cd screenshots
npm install
npx playwright install chromium   # first run only
npm run capture
```

The harness uses `http://127.0.0.1:1425` so it can run beside `cargo tauri dev` on `:1420`. Override both the navigation URL and Vite port with `CORTI_TEST_URL=http://127.0.0.1:1430`.

Generated Retina PNGs land in ignored `screenshots/output/`:

- `live-transcript.png`
- `live-rewriting-assistant.png`
- `live-assistant-drawer.png`
- `recording-queue.png`
- `pipeline.png`
- `settings-local.png`
- `settings-hosted.png`

Dates, relative times, color scheme, viewport, reduced motion, and all IPC results are fixed. Fixture text is intentionally synthetic and safe to publish.

## Refresh the website

With the sibling site checked out at `../vasovagal.github.io`:

```sh
./screenshots/update_site.py
```

That captures all views and copies the PNGs into `vasovagal.github.io/assets/screenshots/`. Use `--skip-capture` only when the current output was generated and reviewed immediately beforehand. The site itself is static and intentionally rebuilt/deployed manually for now.

## Add a view

1. Add any missing Tauri command result to `fixtures.ts`.
2. Add a test to `capture.spec.ts` and wait for content that proves the view settled.
3. Run `npm run capture` and inspect the image at 100%.
4. Reference the image from the site, then run `update_site.py` on future refreshes.

An unhandled IPC command returns `null`; required fixture drift should therefore fail a visibility assertion rather than silently publish a loading screen. Tests can update fixture values and emit synthetic Tauri events through the injected bridge without touching ambient credentials or provider networks.
