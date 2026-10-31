---
name: succinct-docs
description: Write or rewrite Corti documentation and site copy as brief, authoritative text for skilled engineers and coding agents. Scores a document against a rubric taken from NASA TM-105419 before and after editing, and keeps every contract. Use when writing a new doc, rewriting or shortening an existing one, or reviewing a doc change.
---

# Succinct docs

Readers are users, maintainers and coding agents. State the prerequisites for the task;
do not assume they know Corti. Give them the shortest text that gets them running
correctly. Fewer tokens is a goal; a lost contract is a defect.

The hard rules are in [AGENTS.md#documentation](../../../AGENTS.md#documentation). This skill is
how to meet them.

## Repository rules

- **Describe verified behavior.** Repository docs describe the current tree; installation and
  download pages distinguish published releases from unreleased work. Keep future work labelled.
  Preserve compatibility constraints, upgrade and rollback steps, ADR decisions, and dated benchmark
  methodology. Remove obsolete narration only when no current contract or inbound link needs it.
- **Keep receipts out of product prose.** Put run IDs, checked source lines, rubric scores and test
  results in the PR or review report. Keep dated measurements with their method and limitations.
- **Code proves what exists.** Check every claim retained in a rewritten section against code,
  workflow steps or the relevant released artifact. Record the checked `file:line` and revision in
  the review report. Distinguish source inspection from a command or test actually run.
- **Preserve contracts.** Correct stale prose from verified implementation within the requested
  scope. Report implementation/policy conflicts or changes to contracts for the owner; do not edit
  runtime behavior to make prose true.
- **No backsliding.** Existing documents must not lose rubric points. New documents need at least
  40/50. A reviewer who did not write the change scores it before merge.

## Rubric

The source is Carol A. Vidoli, *Technical Report Writing*, NASA TM-105419, NASA Lewis Research
Center, 1992 (<https://ntrs.nasa.gov/citations/19930013813>). Score each criterion 1–5, where 5
means no violation is left (maximum 50). Score before and after editing, and cite one line for
every score below 5.

1. **Purpose first.** The opening says what the reader ends with and who the doc is for, then gives
   the step order or a table of contents. The body follows that plan. ("State your purpose or
   objective clearly and follow it with a concise description of the method you will use")
2. **Get to the point.** Every sentence is a step or a contract the reader must honour. Rationale,
   alternatives the reader will not take and asides go, or become a link. Keep one sentence of
   reason where a restriction looks arbitrary (often a security reason) or the obvious alternative
   is wrong. ("Technical reports are not mystery novels")
3. **Concise, not condensed.** Cut verification narration and receipts, but never below the floor.
   ("brief but comprehensible")
4. **Clear.** Every command, path, owner, mode, key and expected result is exact. No step depends
   on a guess.
5. **One fact, one place.** A fact appears once per doc. Across docs, choose the owning
   reference page and link to its precise heading. Keep installation and packaged READMEs usable
   on their own. ("never in more than one way")
6. **Natural words.** Short familiar words, no jargon that can mislead, nothing stilted.
   ("Write to express, not to impress.")
7. **Major ideas first.** Each section opens with its action or rule; detail follows in bullets.
8. **No rough-draft faults.** No noun stacks or prepositional chains, no abstract nouns where a
   verb works, parallel lists, no phrasing more complex than needed, no unnecessary words.
9. **Headings.** Equal-weight headings share a form unless that makes one unclear; at least two
   subheadings or none; at most three levels; Corti terms defined once.
10. **Objective.** Authoritative voice; each limit stated once beside the claim it qualifies; no
    hedging or selling.

## Floor: never cut

- Security, privacy and configuration contracts: file owners/modes, credential storage and
  exclusions, consent, local versus hosted processing, authority, spending and resource limits.
- CLI flags, config keys, defaults, formats, wire names, compatibility and platform constraints.
- Failure symptoms, the log or event that shows them, and the recovery action.
- Required installation, upgrade, rollback and cleanup steps.
- What a test or CI lane proves and what it does not; measured results and their limitations.
- Linked headings, HTML IDs and code strings that name a doc. Search this repository and relevant
  sibling repositories (`vasovagal/corti`, `vasovagal/vagus`, `vasovagal/vasovagal.github.io`) for
  inbound links, or report that check as unrun. Preserve or repair every affected destination.
- Text consumed by tests, packaging or release automation, including marked release fields and
  code/config examples. READMEs shipped in release archives must read on their own.

## Procedure

1. **Map the doc.** List its headings and every inbound link to them, including code strings that
   name the file. Read the owning docs it restates.
2. **Score it** against the rubric.
3. **Rewrite.**
   - Open with the purpose and the plan.
   - Make each section an action followed by its contract; turn paragraphs into steps, bullets
     or a table.
   - Replace restated facts with a link to the precise heading that owns them. Link only when the
     owner already states the fact.
   - Code and config blocks stay byte-identical unless one is wrong. Compare them with
     `git show HEAD:<file>` before finishing. A wrong block is a separate, verified fix.
4. **Check every claim a section keeps**, not only the ones you change, against its source and
   the failure modes below. Describe CI from the workflow step that runs it, not from a script's
   header or another doc.
5. **Run the gates** in [AGENTS.md#documentation-checks](../../../AGENTS.md#documentation-checks)
   and `git diff --check`. Check relative links and anchors you touched using GitHub Markdown slug
   rules and HTML IDs. For site edits, preview changed pages at desktop and mobile widths; check
   navigation, overflow, assets and console errors. Report unavailable checks as unrun.
6. **Score again**, then report:
   - scores and words, before and after;
   - each fact removed rather than reworded, with where it is still stated or why it was dropped;
   - headings renamed or removed, and what links to them now;
   - gate results as observed;
   - `Choices I made`: each place you read the brief's intent over its text.
7. **Review.** A reviewer who did not write the change (a human, or under an owner-approved plan a
   fresh agent) re-checks claims against the sources, not the editor's report. Each finding is
   `blocking` (a lost contract, a false claim, a step the reader can no longer follow) or
   `follow-up`. Fix blocking findings before merge.

## Attribution

Adapted from Dekopon’s NASA-derived skill. [Source and local adaptations](SOURCE.md) pin the
upstream commit and preserve its [license](LICENSE-MIT). The examples below come from upstream.

## Failure modes seen in rewrites

Each of these shipped in a draft and was caught only by review.

- **A qualified claim becomes universal.** "check accepts the sample IDs" became "check accepts
  every placeholder"; the binary refuses some.
- **A condition drops out.** "Rotate with both daemons running" lost its condition, but the reload
  only happens while the gateway is connected.
- **A fact moves into the wrong context.** A log path true under the service manager was attached
  to the foreground run, where output goes to the terminal.
- **Moving a block changes what a link lands on.** A block moved under a heading that another doc
  links to, so readers following that link got instructions the linking doc forbids.
- **A softer word becomes a stronger one.** "limit" became "cap" for a budget that can overshoot.
- **"Only" sneaks in.** A file that "holds the three lines" became one that "holds only" them,
  stricter than the code.
- **Links land on the parent section** instead of the sub-heading that states the fact.
- **Concurrent editors drift.** Two editors on docs that share facts each assume the other keeps a
  fact. Rewrite one doc at a time; split a very large doc into disjoint section ranges with one
  reviewer for the whole.
- **Carried-over text is wrong.** A rewrite kept three false chart claims from the old text because
  only changed claims were checked.
- **A conditional step disappears.** Cutting the old upgrade notes dropped "set `image.tag` or
  `image.digest` to the new release", the one step a pinned deployment needs.
- **An error message loses its explanation.** An error says "see docs/upgrading.md"; deleting the
  section it relied on left the pointer answering nothing.
- **History creeps back.** Another session re-added a migration section and CI run receipts days
  after a pass removed them. The rules live in `AGENTS.md` so every session sees them.
