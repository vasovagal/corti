# Repository guidance

Read [CLAUDE.md](CLAUDE.md) and [design/guardrails.md](design/guardrails.md) before changing this repository. Those existing instructions and invariants remain binding.

## Documentation

Use [succinct-docs](.claude/skills/succinct-docs/SKILL.md) for new documentation, rewrites and reviews,
including site copy. Read the complete skill before editing; it defines the NASA-derived rubric,
source checks and review procedure.

- Preserve technical, privacy, security, compatibility and release contracts. State optional hosted
  processing beside local/offline claims, and distinguish current source from published releases.
- Keep commands and configuration blocks byte-identical during prose edits unless correcting a
  verified error; report each correction separately.
- Keep the fewest words that let the reader complete the task. Retain necessary failure/recovery
  steps, dated evidence, and inbound links. Place review receipts in the PR, not product prose.
- Score each changed document before and after; never lower its score. New documents need at least
  40/50. Include words, source checks, removed facts and renamed headings in the review report.
- Obtain an independent review against implementation before merge; resolve blocking findings.
- Report code/policy conflicts or proposed contract changes for owner decision. Documentation work
  does not authorize application changes, release, deployment or publishing settings changes.

## Documentation checks

- Run `git diff --check`.
- Check every changed relative link/anchor and inbound reference from this repository and relevant
  Vasovagal sibling repositories; report any unavailable cross-repository check.
- Check commands and configuration against their implementation; run safe examples in an isolated scratch directory when supported.
- Follow [CONTRIBUTING.md](CONTRIBUTING.md) for checks relevant to any changed executable example. Do not call source inspection a native macOS test.
- Report checks actually run, the tested commit, and any blocked or unrun checks.
