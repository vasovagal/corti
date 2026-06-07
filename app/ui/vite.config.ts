/// <reference types="vitest/config" />
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Tauri wires this in via tauri.conf.json (build.devUrl = http://localhost:1420,
// build.frontendDist = ui/dist). `base: "./"` makes built asset URLs relative so the
// embedded bundle loads correctly over the tauri:// protocol.
export default defineConfig({
  plugins: [react()],
  base: "./",
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true, // fail loudly if 1420 is taken (devUrl must match)
  },
  build: {
    outDir: "dist",
    target: "safari15", // Apple-Silicon / latest-macOS WKWebView (guardrail #2)
    emptyOutDir: true,
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
