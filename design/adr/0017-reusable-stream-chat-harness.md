# ADR 0017 — Reusable stream-chat harness: row-scoped fences, question subscriptions, learned lexicon

- **Status:** Accepted (2026-09-24)
- **Amends:** ADR 0015 §6 (result fences), ADR 0014 (provenance records cleanup rules 3 and the lexicon),
  guardrail 11 (hosted egress) and guardrail 12 (segment cleanup)
- **References:** [design 07](../07-paid-model-post-processing.md) §6, §7.2, §7.5, §9;
  [#158](https://github.com/vasovagal/corti/issues/158) (umbrella),
  [#144](https://github.com/vasovagal/corti/issues/144),
  [#154](https://github.com/vasovagal/corti/issues/154),
  [#157](https://github.com/vasovagal/corti/issues/157)

## Context

v0.14–v0.17 shipped hosted Live cleanup, a Final rewrite, one pinned question and ad-hoc questions over
five provider transports (ADR 0015). In practice, with Vertex (Gemini / Claude-on-Vertex) or the native
ChatGPT-subscription transport configured, the owner only ever saw raw text. The mechanisms were sound
from a validated result to the UI; the failures were upstream and mostly silent:

1. **A global transcript-revision fence with newest-only targeting.** A Live result applied only if no new
   row batch had arrived since the request was built. `Me` and `Them` rows land as separate batches every
   few seconds and a round trip takes 1.5–6 s, so nearly every success was discarded as superseded, and a
   superseded batch was never retried.
2. **Deadlines tuned for non-thinking models** (first text 2 s, terminal 5 s) while ChatGPT sent no
   reasoning effort and Gemini no thinking control.
3. **A Gemini lane could not be saved** (#144): the persisted document demanded a provider-side caching
   acknowledgement that nothing could write.
4. **Adapter strictness and sticky auth:** exact served-model equality, ChatGPT refusing a lane saved with
   cache mode `off`, and any refresh/resolution error permanent for the process.
5. **Silent drops** at every submission boundary.
6. **Questions** were single-line plain text (no running bullet list), exactly one pinned question was
   hard-coded end to end, and nothing streamed to the UI.
7. **No correction rules** anywhere: the word bank is a spelling list, cleanup never substituted text, and
   #154 (fillers) and #157 (unsorted turns) were open.

The owner also asked for the chat machinery to be reusable, for "subscribed questions" (a running summary,
a topic watch, and above all "what are they asking me?" as a quick catch-up after tuning out), and for the
cleaning process to learn the owner's own corrections from old transcripts.

## Decision

1. **The coordinator is a platform-independent crate.** `crates/corti-chat` owns request fences, lane
   scheduling, cancellation, exact-cache ordering, Vertex catch-up, the Live batcher, the cache-policy
   derivation and question subscriptions. It depends on `corti-postprocess` and the provider adapters, never
   on Tauri, and builds and tests on every platform; the app keeps a re-export shim and the live store.
2. **A Live result is fenced per target row, not per transcript revision.** It applies while controls and
   the session generation are current and every target row still exists with identical id, speaker and
   timing. Rows arriving after the request never invalidate it. The Live lane is FIFO and never drops: a
   pure `LiveBatcher` accumulates finalized rows (quiet 400 ms, max wait 2 s, 8 rows / 4 KiB with the head
   row always included), keeps exactly one Live batch outstanding, releases rows it cannot send as raw with
   a loud log, and releases the oldest backlog as raw past 32 rows / 45 s so the view never falls minutes
   behind. Deadlines are configurable (`LaneDeadlines`, default 8 s first text / 20 s terminal / 45 s
   question) and a queued Live successor survives the in-flight call.
3. **Per-transport body flags are catalog facts, not runtime retries.** Gemini's thinking control is
   inferred from the model id class and sent for the Google publisher only, with a per-document
   `thinking = "omit"` escape hatch; ChatGPT sends `reasoning.effort = low` and no `max_output_tokens`;
   OpenAI direct sends `reasoning.effort = low` for reasoning ids. Served-model leniency is limited to a
   dated or numbered snapshot suffix of the requested id. Auth errors re-arm after an exponential backoff
   (30 s → 10 min) instead of blocking the process.
4. **Provider caching policy is derived in one place** (`effective_provider_cache`): ChatGPT subscription →
   unavailable; implicit-cache models → unavoidable-implicit once the owner acknowledged provider-side
   caching for that provider, otherwise blocked with that reason; explicit-prefix models → explicit once
   acknowledged, off until then. The acknowledgement is a real per-provider control; lane selection
   normalises to the derived policy and adapters and the document validator merely confirm it. hosted.toml
   moves to schema 2 (migration between parse and validate; an older binary refuses it and runs with hosted
   egress off); a document that fails to load is a visible Settings banner, never a silent off.
5. **Questions are subscriptions.** The one pinned template becomes a saved set of subscriptions, each with
   a trigger (new words / new speech from all, `Them` or `Me` rows, a quiet period, spacing), a context
   window (whole session, last N minutes or rows) and an answer layout. Presets: `asked_of_me` (`Them` rows
   only, 1 s quiet, a cheap question pre-filter before any paid call, last five minutes, bullets),
   `running_summary`, `topic_watch`, custom. The wire lane stays `PinnedQuestion`; the subscription id rides
   `target_id`; scheduling is pull-based (the coordinator names what is due, the app builds it); each
   subscription is single-flight, changing its question cancels its in-flight run, and "Catch up now" runs
   it regardless of thresholds. Question answers may span lines with plain `- `/`1. ` markers; all other
   markup stays refused. Streamed text deltas of a question call are kept as a bounded partial answer.
   Questions read the cleaned ledger (accepted Live text is copied in).
6. **Cleanup is ordered and learns.** `cleanup` sorts by start first (#157), then echo → merge →
   backchannel → filler/stutter stripping (#154, `strip_fillers`, on by default: owner decision) → the
   learned lexicon through a `TextRule` seam. `CLEANUP_RULES_VERSION` is 3 and provenance records
   `strip_fillers` and the lexicon's revision/digest/rule count. `crates/corti-lexicon` holds a
   digest-verified rule document (word and phrase corrections, whole-word, case-adapting), one idempotent
   longest-first pass that refuses self-matching rule sets, review heuristics and a scripted-IO review
   session. Corrections also ride the hosted prompt's stable prefix as delimited untrusted data, and the
   lexicon digest is part of every request and provider cache key. Word bank and lexicon are re-read at
   every hosted session begin. The lexicon is **not** applied to Live Transcript store rows: raw stays
   verbatim (guardrail 11).
7. **Reviewing old transcripts grows the lexicon; it never rewrites them.** `corti --review <note>…` parses
   the note (the tolerant inverse of `to_markdown`), flags near-misses to known terms, spelling variants,
   unknown capitalised words and filler residue, and persists each accepted rule immediately after a
   re-read and revision check. `corti --lexicon list|add|remove|test` edits by hand. Batch rewrites of
   historical notes are a separate, later job.
8. **Every drop is loud.** Discards, expiries, refusals and handoff failures log under `corti::hosted` with
   the call id, lane, code and whether the request reached the provider; the Live banner hides only
   explicit cancels.

## Consequences

- The Live view flips to clean text in a real call; a Live result is no longer lost to the next batch.
  Perceived latency is bounded by the batcher and the terminal deadline, and a slow provider is visible
  as a backlog notice rather than as silence.
- Effective concurrency is documented: with `MAX_PROVIDER_CALLS = 2` per provider shared with Live, one
  provider runs Live plus one question at a time.
- hosted.toml schema 2 is one-way for v0.17 binaries. `pinned_question_template` is a read-only migration
  sink; `pinned_auto_enabled` keeps its name as the global "automatic questions" switch.
- `CLEANUP_RULES_VERSION: 3` and `strip_fillers: true` are recorded on every new note; older notes are
  untouched and a `--review` run never edits a note.
- Prompt bytes are unchanged when the lexicon is empty, so existing caches survive; a non-empty lexicon is
  a different request (and a different provider prefix) by construction.
- Two ADRs in flight share the 001x range: PR #123 claims ADR 0016, so this decision is 0017.
- Not decided here (follow-ups): a SQLite-backed local exact cache, hosted suggestions inside `--review`,
  a deterministic clean overlay at live row minting, a `--hosted-check` preflight, and batch application
  of the lexicon to historical notes.
