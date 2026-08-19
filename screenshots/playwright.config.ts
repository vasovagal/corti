import { defineConfig } from "@playwright/test";

// The app's Tauri dev session owns :1420. Screenshot capture defaults to a
// separate port so it can run beside a real Corti checkout without silently
// rendering the wrong frontend.
const BASE_URL = process.env.CORTI_TEST_URL ?? "http://127.0.0.1:1425";
const PORT = new URL(BASE_URL).port || "1425";

export default defineConfig({
  testDir: ".",
  testMatch: "capture.spec.ts",
  outputDir: "test-results",
  timeout: 60_000,
  workers: 1,
  retries: process.env.CI ? 2 : 0,
  use: {
    viewport: { width: 1200, height: 800 },
    deviceScaleFactor: 2,
    colorScheme: "dark",
    timezoneId: "America/New_York",
  },
  webServer: {
    command: `npm run dev -- --host 127.0.0.1 --port ${PORT} --strictPort`,
    cwd: "../app/ui",
    // Vite can answer `/` before its module graph is ready. Probe the entry
    // module so the first screenshot never catches a cold-start reload.
    url: `${BASE_URL}/src/main.tsx`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
