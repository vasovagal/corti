import { expect, test, type Page } from "@playwright/test";
import { buildInitScript } from "./tauri-mock.js";

const BASE_URL = process.env.CORTI_TEST_URL ?? "http://127.0.0.1:1425";
const SCREENSHOT_TIME = new Date("2026-08-18T20:00:00-04:00");

test.beforeEach(async ({ page }) => {
  // Stable relative times, colors, motion, and IPC data make reruns byte-level useful.
  await page.clock.setFixedTime(SCREENSHOT_TIME);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.addInitScript({ content: buildInitScript() });
});

async function capture(page: Page, filename: string, fullPage = false) {
  await page.screenshot({
    path: `output/${filename}`,
    fullPage,
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

test("hosted rewrite preferences", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto(`${BASE_URL}/?view=settings&section=hosted`);
  await expect(page.getByRole("heading", { name: "Hosted rewrite", exact: true })).toBeVisible();
  await expect(page.getByRole("alert")).toHaveText("gcloud token isn't armed");
  await expect(page.locator("#hosted-model-final")).toHaveValue("gpt-5.6-luna");
  await capture(page, "settings-hosted.png", true);
});
