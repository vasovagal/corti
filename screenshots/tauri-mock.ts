import { fixtures } from "./fixtures.js";

/**
 * Build the browser init script that supplies the tiny subset of Tauri's
 * JavaScript bridge used by Corti. The real React frontend runs unchanged;
 * only Rust IPC and event subscriptions are replaced by deterministic data.
 */
export function buildInitScript(overrides: Record<string, unknown> = {}): string {
  const fixtureJson = JSON.stringify({ ...fixtures, ...overrides });

  return `
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
      unregisterListener: function() {},
    };
    window.__TAURI_INTERNALS__ = {
      metadata: {
        currentWindow: { label: "screenshots" },
        currentWebview: { label: "screenshots" },
      },
      _cbCounter: 0,
      transformCallback: function(callback) {
        var id = ++window.__TAURI_INTERNALS__._cbCounter;
        window["_" + id] = callback;
        return id;
      },
      invoke: async function(command, args) {
        const fixtures = ${fixtureJson};
        if (command === "plugin:event|listen") return 0;
        if (command === "plugin:event|unlisten") return null;
        if (command in fixtures) return fixtures[command];
        console.warn("[corti-screenshot-mock] unhandled command", command, args);
        return null;
      },
      convertFileSrc: function(path) { return path; },
    };
  `;
}
