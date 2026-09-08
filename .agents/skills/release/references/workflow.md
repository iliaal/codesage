---
description: Cut a codesage release (writing audit, fmt/clippy/tests, then scripts/release.sh)
argument-hint: X.Y.Z
---

Cut a codesage release for version `$ARGUMENTS`.

Gate the release behind a prose audit and the canonical sanity checks,
including CUDA lint. If any gate fails, stop and report — do not run `scripts/release.sh`.
If every gate passes, hand off to the script, which handles the CHANGELOG
rewrite, version bump, release build, commit, tag, and (after a prompt) the
push.

Steps:

1. Validate that `$ARGUMENTS` looks like `X.Y.Z`. If empty or malformed,
   stop and tell the user the expected form.

2. Prose audit via the `whetstone:ia-writing` skill. These changes
   ship publicly in the GitHub Release notes and on the repo landing page,
   so they need the same bar as other outgoing comms:
   - Extract the `## [Unreleased]` block from `CHANGELOG.md` (everything
     between that header and the next `## [` header). Run the writing
     skill against that block.
   - Resolve the most recent tag with `git describe --tags --abbrev=0`, then
     inspect `git diff <tag> -- README.md`, including the working contents
     that will enter the release. If no tag exists, audit the current README.
     If the diff is non-empty, run the writing skill against the changed
     sections (the added/modified lines, with enough surrounding context
     to judge them).
   - Apply rewrites already authorized by the user. Otherwise present the
     concrete suggested changes for approval before applying them. If the
     user declines, continue; the audit does not require accepting rewrites.
   - Keep accepted changes to `README.md` and `CHANGELOG.md` uncommitted for
     the release transaction. Pass `--include-approved-prose` to the script
     when including these changes. It rejects other tracked changes and
     prose deletions, type changes, or mode changes. Do not separately commit
     the prose: a new release requires HEAD to equal origin/master.
   - Skip this step cleanly if `[Unreleased]` is empty; `scripts/release.sh`
     will fail the release in that case with a clearer error.

3. Run `bash scripts/sanity-check.sh --cuda`. This is the canonical recipe
   for changelog/plugin validation, formatting, default and CUDA clippy,
   shell lint/format checks, workspace tests, and script/plugin regressions.
   Do not use `--fast` for a release. On failure, report the failing gate;
   repair and rerun when that work is authorized, otherwise stop before release.

4. If the recipe passes, run `bash scripts/release.sh $ARGUMENTS`, inserting
   `--include-approved-prose` before the version when carrying approved prose.
   Resume of an already tagged, unpushed release still requires a clean tree;
   the prose option does not amend an existing release tag. The script
   is interactive: it prints diffs and asks twice (once before commit+tag,
   once before push). If the user has already authorized both commit/tag and
   push, use the existing `--yes` option. Otherwise relay only the prompt for
   the action whose authority is still missing; do not request approval again
   for an already authorized action.
   After refreshing the installed binary, it stops the shared daemon, starts
   it through the new binary's MCP shim, and checks reachability. Report any
   restart failure; existing agent MCP sessions may need to reconnect.

5. After the script exits, summarize in two lines: what version was cut
   and whether it was pushed. If it wasn't pushed, remind the user of the
   manual push commands.
