// The "How Corti Works" diagram: the fixed sequence of pipeline boxes plus the pure mapping from a
// backend stage id to the box(es) that pulse. Stage ids mirror Rust `imp::Stage` (app/src/main.rs);
// `idle` (and any unknown id) lights nothing.
//
// Detect + Capture are one live phase (`recording`), so both pulse together. The shipped pipeline folds
// echo cancellation into a single `transcribing` step (one `transcribe_recording` call), so `transcribing`
// lights both the Echo-cancel and Transcribe boxes; the standalone `cancelling_echo` id is honoured too in
// case the backend ever reports it on its own.

export interface PipelineBox {
  key: string;
  title: string;
  blurb: string;
  /** Backend stage ids that light this box. */
  stages: string[];
}

export const PIPELINE_BOXES: PipelineBox[] = [
  {
    key: "detect",
    title: "Detect",
    blurb: "Notices when another app starts using your microphone.",
    stages: ["recording"],
  },
  {
    key: "capture",
    title: "Capture",
    blurb: "Records both sides of the call to a two-track file on disk.",
    stages: ["recording"],
  },
  {
    key: "echo",
    title: "Echo-cancel",
    blurb: "Strips the far side bleeding into your mic before transcribing.",
    stages: ["cancelling_echo", "transcribing"],
  },
  {
    key: "transcribe",
    title: "Transcribe",
    blurb: "Turns the audio into a speaker-labelled transcript.",
    stages: ["transcribing"],
  },
  {
    key: "file",
    title: "File to vagus",
    blurb: "Hands the transcript to vagus, which writes the note.",
    stages: ["filing"],
  },
];

/** The keys of the boxes that pulse for a given backend stage id. `idle`/unknown ⇒ none. */
export function activeBoxKeys(stage: string): string[] {
  return PIPELINE_BOXES.filter((b) => b.stages.includes(stage)).map((b) => b.key);
}

// A live capture always pulses Detect + Capture, even if the backend `stage` has moved on: the global stage
// is last-writer-wins and an older job's worker can clobber a live recording's `recording` stage (see Rust
// `AppState::stage`), so `recording` is authoritative for those two boxes.
export function activeBoxKeysForActivity(stage: string, recording: boolean): string[] {
  const active = new Set(activeBoxKeys(stage));
  if (recording) {
    active.add("detect");
    active.add("capture");
  }
  return PIPELINE_BOXES.filter((b) => active.has(b.key)).map((b) => b.key);
}
