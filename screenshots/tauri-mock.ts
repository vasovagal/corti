import { fixtures } from "./fixtures.js";

/**
 * Build the browser init script that supplies the tiny subset of Tauri's
 * JavaScript bridge used by Corti. The real React frontend runs unchanged;
 * only Rust IPC and event subscriptions are replaced by deterministic data.
 */
export function buildInitScript(overrides: Record<string, unknown> = {}): string {
  const fixtureJson = JSON.stringify({ ...fixtures, ...overrides });

  return `
    (function installCortiFixtureBridge() {
      const fixtureState = ${fixtureJson};
      const listeners = new Map();
      const invocations = [];
      const heldCommands = new Set();
      const pendingCommands = new Map();
      let listenerId = 0;

      function clone(value) {
        return typeof structuredClone === "function" ? structuredClone(value) : value;
      }

      function removeListener(event, id) {
        const byId = listeners.get(event);
        if (!byId) return;
        byId.delete(id);
        if (byId.size === 0) listeners.delete(event);
      }

      window.__cortiSetFixture = function(command, value) {
        fixtureState[command] = value;
      };
      window.__cortiInvocations = function(command) {
        return invocations.filter(function(item) { return !command || item.command === command; });
      };
      window.__cortiHoldCommand = function(command) {
        heldCommands.add(command);
      };
      window.__cortiResolveCommand = function(command, value) {
        heldCommands.delete(command);
        const pending = pendingCommands.get(command) ?? [];
        const waiter = pending.shift();
        if (pending.length === 0) pendingCommands.delete(command);
        if (waiter) waiter.resolve(clone(value));
      };
      window.__cortiRejectCommand = function(command, reason) {
        heldCommands.delete(command);
        const pending = pendingCommands.get(command) ?? [];
        const waiter = pending.shift();
        if (pending.length === 0) pendingCommands.delete(command);
        if (waiter) waiter.reject(reason);
      };
      window.__cortiEmit = function(event, payload) {
        const byId = listeners.get(event);
        if (!byId) return;
        for (const [id, callbackId] of byId.entries()) {
          const callback = window["_" + callbackId];
          if (typeof callback === "function") callback({ event: event, id: id, payload: payload });
        }
      };

      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
        unregisterListener: removeListener,
      };
      window.__TAURI_INTERNALS__ = {
        metadata: {
          currentWindow: { label: "live" },
          currentWebview: { label: "live" },
        },
        _cbCounter: 0,
        transformCallback: function(callback) {
          var id = ++window.__TAURI_INTERNALS__._cbCounter;
          window["_" + id] = callback;
          return id;
        },
        invoke: async function(command, args) {
          invocations.push({ command: command, args: args ?? null });
          if (command === "plugin:event|listen") {
            const id = ++listenerId;
            const byId = listeners.get(args.event) ?? new Map();
            byId.set(id, args.handler);
            listeners.set(args.event, byId);
            return id;
          }
          if (command === "plugin:event|unlisten") {
            removeListener(args.event, args.eventId);
            return null;
          }
          if (heldCommands.has(command)) {
            return new Promise(function(resolve, reject) {
              const pending = pendingCommands.get(command) ?? [];
              pending.push({ resolve: resolve, reject: reject });
              pendingCommands.set(command, pending);
            });
          }
          if (command in fixtureState) {
            return clone(fixtureState[command]);
          }
          return null;
        },
        convertFileSrc: function(path) { return path; },
      };
    })();
  `;
}
