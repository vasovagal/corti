import { expect, test, type Locator, type Page } from "@playwright/test";
import {
  syntheticGapEvent,
  syntheticGapSnapshot,
  syntheticLiveOverrides,
  syntheticLiveTerminal,
  syntheticVertexNotice,
  syntheticVertexReadySettings,
} from "./fixtures.js";
import { buildInitScript } from "./tauri-mock.js";

const BASE_URL = process.env.CORTI_TEST_URL ?? "http://127.0.0.1:1425";
const SCREENSHOT_TIME = new Date("2026-08-18T20:00:00-04:00");

test.beforeEach(async ({ page }, testInfo) => {
  // Stable relative times, colors, motion, and injected IPC data make reruns byte-level useful.
  await page.clock.setFixedTime(SCREENSHOT_TIME);
  await page.emulateMedia({ reducedMotion: "reduce" });
  const liveExperience = [
    "live rewrite changes and assistant",
    "live diff and cost narrow",
    "desktop sidebar and pinned answer",
    "reduced motion live rewrite desktop",
    "reduced motion live rewrite narrow",
    "repairs revision gaps and shows the exact warning",
    "narrow assistant drawer restores focus",
    "accepted rewrite wash is one shot",
    "live controls and pinned debounce use narrow commands",
    "forced colors keeps changed tokens visible",
  ].includes(testInfo.title);
  await page.addInitScript({
    content: buildInitScript(liveExperience ? syntheticLiveOverrides : undefined),
  });
  // Product captures are fixture-only. Even if the machine has ambient provider
  // credentials, the browser may talk only to the loopback Vite server.
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    if (
      !["http:", "https:"].includes(url.protocol) ||
      ["127.0.0.1", "localhost", "::1"].includes(url.hostname)
    ) {
      await route.continue();
      return;
    }
    await route.abort("blockedbyclient");
  });
});

async function capture(page: Page, filename: string, fullPage = false) {
  await page.screenshot({
    path: `output/${filename}`,
    fullPage,
    animations: "disabled",
    caret: "hide",
  });
}

async function captureElement(locator: Locator, filename: string) {
  await locator.screenshot({
    path: `output/${filename}`,
    animations: "disabled",
    caret: "hide",
  });
}

test("live transcript", async ({ page }) => {
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByRole("heading", { name: "Zoom · live transcript" })).toBeVisible();
  await expect(page.getByText("The inbox note is already open", { exact: false })).toBeVisible();
  await capture(page, "live-transcript.png");
});

test("live rewrite changes and assistant", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 820 });
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByRole("heading", { name: "Synthetic planning call · live transcript" })).toBeVisible();
  await expect(page.getByText("Raw text appears first", { exact: false })).toBeVisible();

  await emitFixture(page, "hosted-state-changed", {
    event: "lane_state",
    lane: "live",
    state: "rewriting",
    code: null,
  });
  await emitFixture(page, "hosted-state-changed", {
    event: "accounting",
    call_id: "synthetic-live-call-42",
    lane: "live",
    finality: "provisional",
    usage: {
      input_tokens: 160,
      output_tokens: 12,
      cached_read_tokens: 96,
      cached_write_tokens: null,
      reasoning_tokens: null,
      usage_complete: false,
    },
    cost: {
      billing_basis: "metered_estimate",
      cost_micros: 600,
      currency: "USD",
      pricing_catalog_version: "synthetic-tariff-v1",
      tariff_id: "synthetic-live-rate",
      tariff_effective_at_unix_ms: 1787000000000,
    },
    late: false,
  });
  await expect(page.getByText("Provisional", { exact: true })).toBeVisible();
  await expect(page.locator(".live-call-cost")).toContainText("Estimated $0.0006");
  await emitFixture(page, "hosted-state-changed", syntheticLiveTerminal);
  await page.getByRole("radio", { name: "Changes" }).click();

  await expect(page.locator("del[data-diff-kind='removed']")).toContainText("teh");
  await expect(page.locator("ins[data-diff-kind='added']").first()).toContainText("the");
  await expect(page.getByText("TTFT", { exact: true })).toBeVisible();
  await expect(page.getByText("80ms", { exact: true })).toBeVisible();
  const details = page.getByRole("checkbox", { name: "Details" });
  await details.uncheck();
  await expect(page.getByRole("heading", { name: "Hosted call details" })).toBeHidden();
  await details.check();
  await expect(page.getByRole("heading", { name: "Hosted call details" })).toBeVisible();
  await expect(page.getByText("Estimated $0.0012", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("The fixture supports a Friday release", { exact: false })).toBeVisible();
  await expect(page.getByText("Running…", { exact: true })).toBeVisible();
  await expect(page.getByText("Provider cache read", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("Raw fallback", { exact: true })).toBeVisible();
  await expect(page.locator(".live-answer-canceled")).toContainText("No answer applied");
  await expect(page.locator(".live-answer-failed")).toContainText("Earlier transcript omitted");
  await expect(page.locator(".live-line-active")).toHaveCSS("animation-name", "none");
  await capture(page, "live-rewriting-assistant.png");
  await capture(page, "live-diff-cost-desktop.png");
});

test("live diff and cost narrow", async ({ page }) => {
  await page.setViewportSize({ width: 430, height: 932 });
  await prepareLiveRewrite(page);
  await expect(page.locator("del[data-diff-kind='removed']")).toContainText("teh");
  await expect(page.locator(".live-call-cost")).toContainText("Estimated $0.0012");
  await capture(page, "live-diff-cost-narrow.png");
});

test("desktop sidebar and pinned answer", async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 });
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByRole("heading", { name: "Synthetic planning call · live transcript" })).toBeVisible();
  await expect(page.getByText("The fixture supports a Friday release", { exact: false })).toBeVisible();
  await capture(page, "assistant-pinned-desktop.png");
});

test("reduced motion live rewrite desktop", async ({ page }) => {
  await page.setViewportSize({ width: 1100, height: 700 });
  await prepareLiveRewrite(page, false);
  await expect(page.locator(".live-line-active")).toHaveCSS("animation-name", "none");
  await expect(page.locator(".live-scroll")).toHaveCSS("scroll-behavior", "auto");
  await capture(page, "reduced-motion-desktop.png");
});

test("reduced motion live rewrite narrow", async ({ page }) => {
  await page.setViewportSize({ width: 430, height: 800 });
  await prepareLiveRewrite(page, false);
  await expect(page.locator(".live-line-active")).toHaveCSS("animation-name", "none");
  await capture(page, "reduced-motion-narrow.png");
});

test("repairs revision gaps and shows the exact warning", async ({ page }) => {
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByText("Raw text appears first", { exact: false })).toBeVisible();
  await page.evaluate((next) => {
    const bridge = window as unknown as {
      __cortiSetFixture: (command: string, value: unknown) => void;
    };
    bridge.__cortiSetFixture("get_live_transcript", next);
  }, syntheticGapSnapshot);
  await emitFixture(page, "live-transcript-changed", syntheticGapEvent);
  await expect(page.getByText("The repaired synthetic row is contiguous again.", { exact: true })).toBeVisible();
  await expect.poll(async () =>
    page.evaluate(() => {
      const bridge = window as unknown as {
        __cortiInvocations: (command: string) => unknown[];
      };
      return bridge.__cortiInvocations("get_live_transcript").length;
    }),
  ).toBeGreaterThanOrEqual(2);

  await emitFixture(page, "hosted-state-changed", syntheticVertexNotice);
  await expect(page.getByRole("alert")).toHaveText("gcloud token isn't armed");
});

test("narrow assistant drawer restores focus", async ({ page }) => {
  await page.setViewportSize({ width: 430, height: 900 });
  await page.goto(`${BASE_URL}/?view=live`);
  const trigger = page.getByRole("button", { name: /Assistant/u });
  await expect(trigger).toBeVisible();
  await trigger.click();
  const drawer = page.getByRole("dialog", { name: "Transcript assistant" });
  await expect(drawer).toBeVisible();
  await expect(page.getByRole("button", { name: "Close assistant" })).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(drawer).toBeHidden();
  await expect(trigger).toBeFocused();
  await trigger.click();
  await expect(page.getByText("The fixture supports a Friday release", { exact: false })).toBeVisible();
  await capture(page, "live-assistant-drawer.png");
  await capture(page, "assistant-pinned-narrow.png");
});

test("accepted rewrite wash is one shot", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByText("Raw text appears first", { exact: false })).toBeVisible();
  await emitFixture(page, "live-transcript-changed", {
    ...syntheticLiveTranscriptEventBase(),
    from_revision: 42,
    revision: 43,
    line: {
      seq: 3,
      row_id: "synthetic-row-3",
      speaker: "Me",
      start_sec: 14.1,
      end_sec: 20.3,
      text: "Raw text appears first while the next cleanup is queued.",
      clean_text: "Raw text appears first while the next cleanup is queued.",
      rewrite_state: "clean",
      commit_epoch: 43,
    },
  });
  const wash = page.locator(".live-accepted-wash");
  await expect(wash).toHaveCount(1);
  const animation = await wash.evaluate((element) => {
    const style = getComputedStyle(element, "::before");
    return { name: style.animationName, count: style.animationIterationCount };
  });
  expect(animation).toEqual({ name: "live-accepted-edge", count: "1" });

  await emitFixture(page, "live-transcript-changed", {
    ...syntheticLiveTranscriptEventBase(),
    from_revision: 43,
    revision: 44,
    line: null,
  });
  await expect(page.locator(".live-accepted-wash")).toHaveCount(0);
});

test("live controls and pinned debounce use narrow commands", async ({ page }) => {
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByText("Raw text appears first", { exact: false })).toBeVisible();

  await page.getByRole("button", { name: "Steering" }).click();
  const steering = page.getByRole("dialog", { name: "Session steering" });
  await steering.getByRole("textbox").fill("Synthetic session-only steering.");
  await steering.getByRole("button", { name: "Apply to next request" }).click();
  await expect.poll(() => invocationCount(page, "update_hosted_steering")).toBe(1);
  const steeringInvocation = await lastInvocation(page, "update_hosted_steering");
  expect(steeringInvocation).toMatchObject({
    args: {
      request: {
        text: "Synthetic session-only steering.",
        persist_as_default: false,
      },
    },
  });

  await page.getByRole("switch", { name: "Live" }).click();
  await expect.poll(() => invocationCount(page, "patch_hosted_settings")).toBe(1);
  expect(await lastInvocation(page, "patch_hosted_settings")).toMatchObject({
    args: {
      request: {
        patch: { kind: "set_lane_enabled", lane: "live", enabled: false },
      },
    },
  });

  const pinned = page.getByLabel("Replace or edit the one pinned question");
  await pinned.fill("What is the debounced synthetic answer?");
  await page.waitForTimeout(350);
  expect(await invocationCount(page, "set_hosted_pinned_question")).toBe(0);
  await expect.poll(() => invocationCount(page, "set_hosted_pinned_question")).toBe(1);
  expect(await lastInvocation(page, "set_hosted_pinned_question")).toMatchObject({
    args: { template: "What is the debounced synthetic answer?" },
  });
});

test("forced colors keeps changed tokens visible", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce", forcedColors: "active" });
  await page.goto(`${BASE_URL}/?view=live`);
  await page.getByRole("radio", { name: "Changes" }).click();
  await expect(page.locator("ins[data-diff-kind='added']").first()).toHaveCSS("border-bottom-style", "solid");
  await expect(page.locator(".live-line-accepted").first()).toHaveCSS("border-left-style", "solid");
});

test("recording queue", async ({ page }) => {
  await page.goto(`${BASE_URL}/?view=queue`);
  await expect(page.getByRole("heading", { name: "Recording Queue" })).toBeVisible();
  await expect(page.getByText("Transcribed 23 min call in 6 s")).toBeVisible();
  await capture(page, "recording-queue.png");
});

test("live pipeline", async ({ page }) => {
  await page.goto(`${BASE_URL}/?view=how`);
  await expect(page.getByRole("heading", { name: "How Corti Works" })).toBeVisible();
  await expect(page.getByText("Zoom · transcribing locally", { exact: false })).toBeVisible();
  await capture(page, "pipeline.png");
});

test("local settings", async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 1200 });
  await page.goto(`${BASE_URL}/?view=settings`);
  await expect(page.getByText("NVIDIA Parakeet-TDT runs fully offline", { exact: false })).toBeVisible();
  await expect(page.getByText("Live inbox filing", { exact: true })).toBeVisible();
  await capture(page, "settings-local.png");
});

async function prepareLiveRewrite(page: Page, terminal = true) {
  await page.goto(`${BASE_URL}/?view=live`);
  await expect(page.getByText("Raw text appears first", { exact: false })).toBeVisible();
  await emitFixture(page, "hosted-state-changed", {
    event: "lane_state",
    lane: "live",
    state: "rewriting",
    code: null,
  });
  await emitFixture(page, "hosted-state-changed", {
    event: "accounting",
    call_id: "synthetic-live-call-42",
    lane: "live",
    finality: "provisional",
    usage: {
      input_tokens: 160,
      output_tokens: 12,
      cached_read_tokens: 96,
      cached_write_tokens: null,
      reasoning_tokens: null,
      usage_complete: false,
    },
    cost: {
      billing_basis: "metered_estimate",
      cost_micros: 600,
      currency: "USD",
      pricing_catalog_version: "synthetic-tariff-v1",
      tariff_id: "synthetic-live-rate",
      tariff_effective_at_unix_ms: 1787000000000,
    },
    late: false,
  });
  if (terminal) await emitFixture(page, "hosted-state-changed", syntheticLiveTerminal);
  await page.getByRole("radio", { name: "Changes" }).click();
}

function syntheticLiveTranscriptEventBase() {
  return {
    protocol_version: 2,
    process_epoch: 71,
    session_generation: 3,
    session_id: "synthetic-live-session",
    mode: "call",
    status: "listening",
    title: "Synthetic planning call · live transcript",
    detail: "Recording · raw rows publish before optional cleanup",
    active: true,
    evicted_lines: 0,
    retained_from_seq: 1,
    reset: false,
  };
}

async function invocationCount(page: Page, command: string): Promise<number> {
  return page.evaluate((name) => {
    const bridge = window as unknown as {
      __cortiInvocations: (command: string) => unknown[];
    };
    return bridge.__cortiInvocations(name).length;
  }, command);
}

async function lastInvocation(page: Page, command: string): Promise<unknown> {
  return page.evaluate((name) => {
    const bridge = window as unknown as {
      __cortiInvocations: (command: string) => unknown[];
    };
    const values = bridge.__cortiInvocations(name);
    return values[values.length - 1];
  }, command);
}

async function emitFixture(page: Page, event: string, payload: unknown) {
  await page.evaluate(
    ({ eventName, eventPayload }) => {
      const bridge = window as unknown as {
        __cortiEmit: (name: string, value: unknown) => void;
      };
      bridge.__cortiEmit(eventName, eventPayload);
    },
    { eventName: event, eventPayload: payload },
  );
}

async function setFixture(page: Page, command: string, value: unknown) {
  await page.evaluate(
    ({ fixtureCommand, fixtureValue }) => {
      const bridge = window as unknown as {
        __cortiSetFixture: (name: string, next: unknown) => void;
      };
      bridge.__cortiSetFixture(fixtureCommand, fixtureValue);
    },
    { fixtureCommand: command, fixtureValue: value },
  );
}

test("hosted rewrite preferences", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto(`${BASE_URL}/?view=settings&section=hosted`);
  await expect(page.getByRole("heading", { name: "Hosted rewrite", exact: true })).toBeVisible();
  await expect(page.getByRole("alert")).toHaveText("gcloud token isn't armed");
  await expect(page.locator("#hosted-model-final")).toHaveValue("gpt-5.6-luna");
  await capture(page, "preferences-desktop.png");
  await capture(page, "settings-hosted.png", true);
});

test("hosted rewrite preferences narrow", async ({ page }) => {
  await page.setViewportSize({ width: 430, height: 900 });
  await page.goto(`${BASE_URL}/?view=settings&section=hosted`);
  await expect(page.getByRole("heading", { name: "Hosted rewrite", exact: true })).toBeVisible();
  await capture(page, "preferences-narrow.png");
});

test("Vertex warning and recovery", async ({ page }) => {
  const vertexCard = page.locator(".hosted-provider-card").filter({
    has: page.getByRole("heading", { name: "Google Vertex direct API" }),
  });
  await page.setViewportSize({ width: 1200, height: 1000 });
  await page.goto(`${BASE_URL}/?view=settings&section=hosted`);
  // Element screenshots can be taller than the viewport; keep the sticky tab
  // bar from overlaying the card while Playwright scrolls it into view.
  await page.addStyleTag({
    content: ".settings-tabs, .hosted-status-banner { position: static !important; }",
  });
  await expect(vertexCard.getByRole("alert")).toHaveText("gcloud token isn't armed");
  await captureElement(vertexCard, "vertex-warning-desktop.png");

  await page.setViewportSize({ width: 430, height: 900 });
  await captureElement(vertexCard, "vertex-warning-narrow.png");
  await setFixture(page, "get_hosted_settings", syntheticVertexReadySettings);
  await vertexCard.getByRole("button", { name: "Refresh status & catalog" }).click();
  await expect(vertexCard.getByText("Armed · token only", { exact: true })).toBeVisible();
  await expect(vertexCard.getByRole("alert")).toHaveCount(0);
  await captureElement(vertexCard, "vertex-recovery-narrow.png");

  await page.setViewportSize({ width: 1200, height: 1000 });
  await captureElement(vertexCard, "vertex-recovery-desktop.png");
});
