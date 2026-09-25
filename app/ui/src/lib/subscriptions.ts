import type {
  HostedAssistantSubscription,
  HostedSubscription,
  HostedSubscriptionPreset,
} from "./api";

export const MAX_SUBSCRIPTIONS = 16;
export const MAX_SUBSCRIPTION_TEMPLATE_BYTES = 8 * 1024;

/** Presets in the order the "Add" controls offer them. */
export const SUBSCRIPTION_PRESETS: HostedSubscriptionPreset[] = [
  "asked_of_me",
  "running_summary",
  "topic_watch",
  "none",
];

export function presetLabel(preset: HostedSubscriptionPreset): string {
  switch (preset) {
    case "asked_of_me":
      return "Asked of me";
    case "running_summary":
      return "Running summary";
    case "topic_watch":
      return "Topic watch";
    case "none":
      return "Custom question";
  }
}

export function presetDescription(preset: HostedSubscriptionPreset): string {
  switch (preset) {
    case "asked_of_me":
      return "The last things the other speakers asked you. Runs after a question is heard from them; small context window.";
    case "running_summary":
      return "A running bullet list of what has been discussed. Runs after about 40 new words or 30 s of speech.";
    case "topic_watch":
      return "What is being said about one topic you name in the template.";
    case "none":
      return "Your own question, re-asked as the transcript grows.";
  }
}

/** Mirrors the backend preset defaults (corti-chat `SubscriptionPreset`). The template stays empty for
 * presets so the backend's default question is used until the owner writes their own. */
export function presetDefaults(preset: HostedSubscriptionPreset, id: string): HostedSubscription {
  const base: HostedSubscription = {
    id,
    title: presetLabel(preset),
    template: "",
    enabled: true,
    preset,
    output: preset === "none" ? "paragraph" : "bullets",
    trigger: {
      quiet_ms: 750,
      min_new_words: 40,
      min_new_speech_ms: 30_000,
      min_interval_ms: 0,
      on_speakers: "all",
    },
    context: { window: "whole", minutes: 0, rows: 0 },
    name_hints: [],
  };
  if (preset === "asked_of_me") {
    return {
      ...base,
      trigger: { quiet_ms: 1_000, min_new_words: 1, min_new_speech_ms: 0, min_interval_ms: 0, on_speakers: "them" },
      context: { window: "last_minutes", minutes: 5, rows: 0 },
    };
  }
  return base;
}

/** A fresh id for `preset` that does not collide with `existing`: `asked-of-me`, `asked-of-me-2`, … */
export function newSubscriptionId(preset: HostedSubscriptionPreset, existing: string[]): string {
  const stem = preset === "none" ? "question" : preset.replace(/_/gu, "-");
  if (!existing.includes(stem)) return stem;
  for (let index = 2; index < 1_000; index += 1) {
    const candidate = `${stem}-${index}`;
    if (!existing.includes(candidate)) return candidate;
  }
  return `${stem}-${Date.now()}`;
}

/** Client-side mirror of the backend validator: a human-readable reason or null. */
export function subscriptionProblem(subscription: HostedSubscription): string | null {
  if (!/^[a-z0-9-]{1,32}$/u.test(subscription.id)) return "Id must be 1–32 characters of a–z, 0–9 or '-'.";
  if (subscription.title.length > 128) return "Title is too long.";
  const template = subscription.template.trim();
  if (subscription.preset === "none" && !template) return "A custom question needs a template.";
  if (new TextEncoder().encode(subscription.template).byteLength > MAX_SUBSCRIPTION_TEMPLATE_BYTES) {
    return "Template is too long.";
  }
  if (subscription.name_hints.length > 16 || subscription.name_hints.some((hint) => !hint.trim() || hint.length > 64)) {
    return "Name hints must be 1–16 short names.";
  }
  return null;
}

export function subscriptionSetProblem(subscriptions: HostedSubscription[]): string | null {
  if (subscriptions.length > MAX_SUBSCRIPTIONS) return `At most ${MAX_SUBSCRIPTIONS} questions can be saved.`;
  const seen = new Set<string>();
  for (const subscription of subscriptions) {
    if (seen.has(subscription.id)) return `Duplicate id "${subscription.id}".`;
    seen.add(subscription.id);
    const problem = subscriptionProblem(subscription);
    if (problem) return `${subscription.title || subscription.id}: ${problem}`;
  }
  return null;
}

/** The assistant shows "Asked of me" first (that is the tune-out catch-up), then the rest in saved order. */
export function orderAssistantCards<T extends { preset: HostedSubscriptionPreset }>(cards: T[]): T[] {
  const first = cards.filter((card) => card.preset === "asked_of_me");
  const rest = cards.filter((card) => card.preset !== "asked_of_me");
  return [...first, ...rest];
}

/** Bullet answers arrive as `- item` lines; anything else renders as one paragraph per line. */
export function splitAnswerLines(answer: string): { bullets: boolean; lines: string[] } {
  const lines = answer
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  const bulletLike = lines.length > 0 && lines.every((line) => /^(- |\d{1,3}\. )/u.test(line));
  if (!bulletLike) return { bullets: false, lines };
  return { bullets: true, lines: lines.map((line) => line.replace(/^(- |\d{1,3}\. )/u, "")) };
}

/** The text a card should show while a rerun is in progress: the streamed partial, else the previous
 * accepted answer, else nothing. */
export function displayedAnswer(card: HostedAssistantSubscription): {
  text: string | null;
  kind: "answer" | "partial" | "previous" | "none";
} {
  const exchange = card.exchange;
  if (!exchange) return { text: null, kind: "none" };
  if (exchange.answer) return { text: exchange.answer, kind: "answer" };
  if (exchange.partial_answer) return { text: exchange.partial_answer, kind: "partial" };
  if (exchange.previous_answer) return { text: exchange.previous_answer, kind: "previous" };
  return { text: null, kind: "none" };
}

export function parseNameHints(value: string): string[] {
  return value
    .split(/[,\n]/u)
    .map((hint) => hint.trim())
    .filter((hint) => hint.length > 0)
    .slice(0, 16);
}
