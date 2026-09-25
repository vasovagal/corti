import { beforeEach, describe, expect, it, vi } from "vitest";

const bridge = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: bridge.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: bridge.listen }));

import {
  cancelHostedQuestion,
  clearBedrockSetup,
  clearProviderSecret,
  getHostedAssistant,
  patchHostedSettings,
  promptForProviderSecret,
  refreshHostedProvider,
  replaceHostedWordBank,
  runHostedSubscriptionNow,
  saveBedrockSetup,
  setHostedSubscriptions,
  submitHostedQuestion,
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

  it("uses secret-free scope, catalog refresh, and subscription commands", async () => {
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

    const subscription = {
      id: "asked-of-me",
      title: "Asked of me",
      template: "",
      enabled: true,
      preset: "asked_of_me" as const,
      output: "bullets" as const,
      trigger: { quiet_ms: 1_000, min_new_words: 1, min_new_speech_ms: 0, min_interval_ms: 0, on_speakers: "them" as const },
      context: { window: "last_minutes" as const, minutes: 5, rows: 0 },
      name_hints: ["Xavier"],
    };
    await setHostedSubscriptions(9, [subscription]);
    expect(bridge.invoke).toHaveBeenLastCalledWith("set_hosted_subscriptions", {
      request: {
        observed_state_revision: 9,
        subscriptions: [subscription],
      },
    });

    await runHostedSubscriptionNow("asked-of-me");
    expect(bridge.invoke).toHaveBeenLastCalledWith("run_hosted_subscription_now", {
      id: "asked-of-me",
    });
  });

  it("sends one revision-checked atomic Bedrock setup payload including region and setup name", async () => {
    await saveBedrockSetup({
      observed_state_revision: 21,
      mode: "assume_role",
      profile: "base-profile",
      role_arn: "arn:aws:iam::123456789012:role/corti",
      region: "us-west-2",
      setup_name: "Clinical Bedrock",
    });
    expect(bridge.invoke).toHaveBeenLastCalledWith("save_bedrock_setup", {
      request: {
        observed_state_revision: 21,
        mode: "assume_role",
        profile: "base-profile",
        role_arn: "arn:aws:iam::123456789012:role/corti",
        region: "us-west-2",
        setup_name: "Clinical Bedrock",
      },
    });

    await clearBedrockSetup({ observed_state_revision: 22 });
    expect(bridge.invoke).toHaveBeenLastCalledWith("clear_bedrock_setup", {
      request: { observed_state_revision: 22 },
    });
  });

  it("keeps Bedrock key add, replace, and remove operations on secret-only commands", async () => {
    await promptForProviderSecret({ provider: "aws", slot: "access_key_id" });
    expect(bridge.invoke).toHaveBeenLastCalledWith("prompt_for_provider_secret", {
      request: { provider: "aws", slot: "access_key_id" },
    });

    await clearProviderSecret({ provider: "aws", slot: "secret_access_key" });
    expect(bridge.invoke).toHaveBeenLastCalledWith("clear_provider_secret", {
      request: { provider: "aws", slot: "secret_access_key" },
    });
  });

  it("keeps session-only assistant content inside live-window commands", async () => {
    await getHostedAssistant();
    expect(bridge.invoke).toHaveBeenLastCalledWith("get_hosted_assistant");

    await submitHostedQuestion("Synthetic bounded question");
    expect(bridge.invoke).toHaveBeenLastCalledWith("submit_hosted_question", {
      question: "Synthetic bounded question",
    });

    await cancelHostedQuestion("fixture-call-id");
    expect(bridge.invoke).toHaveBeenLastCalledWith("cancel_hosted_question", {
      callId: "fixture-call-id",
    });
  });
});
