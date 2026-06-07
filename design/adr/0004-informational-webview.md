# ADR 0004 — on-demand informational webview windows + a React/Vite frontend

- **Status:** Accepted (2026-06-07)

## Context

corti shipped as a **deliberately windowless** Tauri 2 menu-bar agent: `ActivationPolicy::Accessory`,
`app.windows: []`, no `frontendDist`, no JS/bundler (see `05-app-tauri.md`). That is the right shape for the
detect → capture → transcribe → vagus pipeline, which has no UI surface beyond the tray.

Two needs push against pure windowlessness:

- An **Ethics & Legality Guide** (issue #29): a tray-opened, content-heavy screen teaching users the
  legality, ethics, and cultural norms of recording people, plus an interactive cross-jurisdiction consent
  calculator. This is genuinely a *document with one interactive widget* — far past what a tray menu can show.
- The long-planned **settings screen** (`config.rs` header: "a settings screen … is the planned consumer of
  the runtime backend seam") will need real UI to write persisted config.

The windowless choice was documented, so adding a window warrants an ADR. A second question rides along: a
rich screen wants HTML/CSS/JS, which means a frontend toolchain in a so-far pure-Rust repo.

## Decision

- **Permit on-demand, informational webview windows.** The tray/pipeline stay windowless; windows are created
  at runtime from a tray action (never declared in `tauri.conf.json`, so nothing opens at launch). The app
  stays `Accessory` at startup and only flips to `ActivationPolicy::Regular` while a window is open (so it can
  take focus + show a Dock icon), reverting to `Accessory` once the last window closes. This preserves the
  menu-bar-only identity for the normal recording flow.
- **Adopt a React + Vite + TypeScript frontend** at `app/ui/`, wired via `tauri.conf.json`'s
  `frontendDist`/`devUrl`/`beforeDevCommand`/`beforeBuildCommand`. The Ethics guide is the first consumer; the
  settings screen will reuse the same foundation. This is **not** a guardrail #3 breach: React runs inside the
  system WKWebView — no new third-party macOS-binding crate (`objc2-*`/`coreaudio-sys`) enters the tree.
- **All reference data lives in auditable JSON** (`app/ui/src/data/*.json`), typed by a values-free
  `types.ts`; components are thin renderers. Legal data must be reviewable/editable without reading code.
- **Static-content windows need no capability.** Window open/focus is driven from Rust, and the page makes no
  `invoke()` calls, so the JS→core IPC bridge is unused. A minimal `core:default` capability scoped to the
  window label is added only for forward-compat.

## Consequences

- **Node/npm is now a build dependency.** `cargo tauri dev`/`build` shell out to npm; CI must install Node and
  run `npm ci` in `app/ui`. Plain `cargo build`/`clippy`/`test` do **not** run the frontend hooks but **do**
  embed `frontendDist` at `generate_context!()`, so `app/ui/dist` must exist (run `npm run build` once, or
  `cargo tauri build`) before bare cargo commands — otherwise the embed step fails. Document in README + CI.
- **Capture/TCC/entitlements are untouched** — a webview window neither records nor needs a new TCC identity
  (guardrail #10). Guardrails #2 (Apple-Silicon/latest-macOS) and #6 (pluggable backends) are unaffected.
- `app/ui/node_modules` and `app/ui/dist` are git-ignored. `app/gen/` (regenerated ACL/capability schemas)
  already is.
- **Relaxation point:** if the app ever needs a persistent always-present window, revisit the
  Accessory↔Regular toggle here; the windowless-*pipeline* policy stays.
