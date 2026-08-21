import { beforeEach, describe, expect, it, vi } from "vitest";

const bridge = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: bridge.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: bridge.listen }));

import {
  patchHostedSettings,
  refreshHostedProvider,
  replaceHostedWordBank,
  setHostedPinnedQuestion,
  updateHostedProviderScope,
  updateHostedSteering,
} from "./api";

describe("hosted Tauri command bindings", () => {
  beforeEach(() => {
    bridge.invoke.mockReset();
    bridge.invoke.mockResolvedValue(undefined);
  });

  it("sends revision-checked control and display patches to the real command shape", async () => {
    await patchHostedSettings(17, { kind: "set_lane_enabled", lane: "final", enabled: true });
    expect(bridge.invoke).toHaveBeenCalledWith("patch_hosted_settings", {
      request: {
        observed_state_revision: 17,
        patch: { kind: "set_lane_enabled", lane: "final", enabled: true },
      },
    });
  });

  it("keeps steering and word-bank content inside their narrow commands", async () => {
    await updateHostedSteering(4, "Synthetic steering", true);
    expect(bridge.invoke).toHaveBeenLastCalledWith("update_hosted_steering", {
      request: {
        observed_state_revision: 4,
        text: "Synthetic steering",
        persist_as_default: true,
      },
    });

    await replaceHostedWordBank(5, ["Corti", "Parakeet"]);
    expect(bridge.invoke).toHaveBeenLastCalledWith("replace_hosted_word_bank", {
      request: { observed_state_revision: 5, entries: ["Corti", "Parakeet"] },
    });
  });

  it("uses secret-free scope, catalog refresh, and pinned-template commands", async () => {
    await updateHostedProviderScope(8, {
      provider: "google",
      transport: "vertex_api",
      alias: "Fixture connection",
      project: "fixture-project",
      region: "global",
      quota_project: null,
    });
    expect(bridge.invoke).toHaveBeenLastCalledWith("update_hosted_provider_scope", {
      request: {
        observed_state_revision: 8,
        provider: "google",
        transport: "vertex_api",
        alias: "Fixture connection",
        project: "fixture-project",
        region: "global",
        quota_project: null,
      },
    });

    await refreshHostedProvider("google", "vertex_api");
    expect(bridge.invoke).toHaveBeenLastCalledWith("refresh_hosted_provider", {
      request: { provider: "google", transport: "vertex_api" },
    });

    await setHostedPinnedQuestion("Synthetic pinned question");
    expect(bridge.invoke).toHaveBeenLastCalledWith("set_hosted_pinned_question", {
      template: "Synthetic pinned question",
    });
  });
});
