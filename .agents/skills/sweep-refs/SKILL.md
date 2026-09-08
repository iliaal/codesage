---
name: sweep-refs
description: Refresh CodeSage reference mirrors, assess upstream changes, and create or update recommendation beads after auditing current CodeSage code. Use for a reference-tool sweep, optionally since YYYY-MM-DD.
---

# Reference sweep

This is the canonical workflow for Codex `$sweep-refs` and Claude `/sweep-refs`.
The Claude command only links here. Work from the CodeSage repository root; mirrors
live in `~/ai/codesage_ref/`. Accept no argument, `YYYY-MM-DD`, or
`since YYYY-MM-DD`; validate the calendar date and reject malformed or future
dates before mutations. Arguments are prompt input, not environment variables.

Deliver recommendations and sweep checkpoints through CodeSage's central `br`
ledger. Do not create, update, or stage landscape/recommendations Markdown.
Existing notes are read-only bootstrap history. Do not implement recommended
changes, delete legacy notes, commit, push, or post externally without separate
authorization. Preserve unrelated work. Use the writing skill for bead prose;
prefix shell commands with `rtk`, using `rtk proxy` for raw/machine-parsed data.

## Baseline and migration

1. Read the repository's beads protocol and the CLI-contract sections of
   `~/ai/wiki/tools/beads-review-ledger.md`. Run `rtk proxy br info --json` from
   CodeSage and confirm its central store. Never create `.beads` in this repo or
   bypass the wrapper. Capture initial Git status, CodeSage HEAD, and UTC start
   time. Verify mutation flags against current `br <command> --help`.
2. Read `rtk proxy br list --status all --deferred --limit 0 --json`; check `has_more` and
   paginate if needed, including closed/deferred records. Lists return an
   object with `issues`; `br show ID --json` returns a one-element array with
   comments. Missing `labels` means none. Search beyond sweep-labeled records.
3. Reuse the tracking task labeled `source:sweep-refs` and `sweep:tracker`.
   Create a P2 task `Reference sweep: checkpoint and next review` only if none
   exists. If several exist, reconcile their histories before selecting one.
   Keep it open: it stores inventory, migration mapping, and the next due date.
4. Use its latest **complete sweep** checkpoint as the baseline. Partial runs,
   migration-only audits, generic `updated_at`, and recommendation edit dates
   are not completed upstream sweeps. A supplied date overrides the comparison
   floor, not the carried-item audit.
   If migration is complete but no full sweep has run yet, use the historical
   floor preserved in the migration record. Its locally observed mirror HEADs
   are inventory evidence, not analyzed baselines.

Until the tracker records completed migration, read
`notes/20260411-code-intelligence-landscape.md` and the latest dated
`notes/*-reference-tool-recommendations.md`, following older references where
they contain a carried premise or decision. Bootstrap the date from explicit
last sweep, else newest `analysis-updated`, else oldest `added`. If no baseline
exists, inspect full available history and disclose it.

Map every carried item, dormant trigger, explicit negative, and open residual
to a bead before recording migration complete. Shipped headings may contain
unresolved residuals. Search for existing beads before importing; preserve their
newer decisions. Store the legacy-section-to-ID mapping and mirror inventory
in the tracker. Once migrated, beads are authoritative even if old files remain
or are later removed. Never repeatedly import old prose over newer evidence.

## Refresh and classify

1. Reconcile the tracker's inventory with every local Git repository under
   `~/ai/codesage_ref/` and, during bootstrap, every classified landscape repo.
   Preserve canonical upstream URLs and classification context. Clone missing
   mirrors only from verified URLs; do not infer a remote from a display name.
2. Run `rtk proxy bash ~/ai/codesage_ref/pull-all.sh`. Inspect per-repository
   failures as well as exit status. Preserve local edits and divergent branches;
   do not reset or clean mirrors. Record inspected HEADs and last commit dates.
3. Normally inspect each mirror's non-merge commits from its last completed
   HEAD to the captured new HEAD. If the prior commit is missing or not an
   ancestor, inspect the divergence and use the date floor as a disclosed
   fallback. A supplied date applies to every mirror. New mirrors need enough
   full-history/current-code inspection to establish relevance. Pin analysis
   to captured HEADs even if mirrors subsequently move.
4. Classify each mirror as material, non-material, or no change; independently
   flag stalled if its last commit is at least 90 days old. Record count, range,
   and evidence. Material means CodeSage should change what it borrows,
   benchmarks, or avoids. Dependency bumps, CI churn, docs edits, and cosmetics
   alone are not material. Read specific commits/code when headlines are
   insufficient, and preserve license constraints on borrowing.

Quiet-repo activity belongs in the tracker, not a new task per repository.
Only actionable recommendations or durable adoption decisions get separate beads.

## Audit current CodeSage

Read CodeSage's non-merge log for the window and `CHANGELOG.md` (`Unreleased`
and releases in the window). For **every open carried recommendation**, including
older unlabeled beads, check its premise against current code and tests. Use
exact `rg` for named paths/flags and the CodeSage retrieval skill for semantic
or graph questions. An empty search or commit title is not a verdict.

Record checked HEAD plus concrete file, symbol, test, or closing-commit evidence.
Separate shipped behavior from experiments, partial implementations, and unknowns.
Close only completed acceptance criteria; retain partial residuals and dormant
triggers. Correct stale claims with attributed evidence instead of carrying a
missing-feature claim that current code disproves.

## Maintain recommendation beads

Search all records by capability, upstream, legacy section, and cited commits
before creating. An equivalent existing bead wins without sweep labels. Read
its comments and disposition. Upstream shipping a previously rejected idea
does not itself justify reopening it; record what new evidence warrants a
user decision. Do not erase or bulk-close overlapping records; identify the
canonical ID and link the others in comments.

For new sweep-owned recommendations, use native `task` or `feature` (`bug`
only for an evidenced defect), `source:sweep-refs`, one category label
`sweep:borrow|evaluate|positioning|avoid|design`, and a stable
`sweep-key:<capability-slug>` independent of date/revision. Disambiguate distinct
scopes. These are planning tasks, not invented review cycles: do not fabricate
`cycle:*`/`state:*` on ordinary tasks. Preserve and follow those fields and human
gates on actual review findings. Preserve existing assignees, priorities,
acceptance criteria, and unrelated labels; add provenance without taking ownership.

New descriptions include the concrete outcome/current gap; upstream URL,
revision, and evidence; current CodeSage HEAD and implementation evidence;
proposed work or experiment and measurable acceptance criteria; counterevidence,
license constraints, uncertainty, and dormant/reconsideration triggers; legacy
sections and related IDs where applicable.

Attribute creates with `--actor` and comments with `--author` using the actual
agent identity. Use `--description-file` and `br comments add --file` with
temporary prose outside the repository. Prefer comments or `--append-notes`
for new evidence. Read the whole record before replacing its description.
Never automatically use `--force`, `--set-labels`, or `--claim`; add/remove
individual labels. Re-read before each write and deduplicate comments by
inspected revisions and substance. Reruns/resumes reuse IDs, including same-day
runs; new same-day evidence warrants a comment, not a duplicate backlog.

Keep unresolved/dormant work open with its trigger, never `in_progress` or
silently resolved. Preserve an existing explicit do-not-adopt decision as
closed `wont-fix` with rationale; a new proposed rejection stays open for the
user. Close verified completed work `fixed`, or a disproved premise
`false-positive`, after recording evidence. Auditing alone is not a fix. Use
`br close`, not `br update --status closed`; obey the repository's two-step
transition for review findings and preserve human decisions.

## Verify, checkpoint, and report

Read back every touched record: verify evidence, IDs, status, and no duplicates.
Reconcile every carried item and inventoried mirror. Run `rtk proxy br sync
--status --json` and report ledger health/export failures. Compare Git status
with the baseline; do not add Markdown or repo-local ledger artifacts.

Set and verify the tracker's `--due` to seven days after the sweep date with
explicit UTC time. Only then append a **complete sweep** checkpoint containing UTC
start time, date floor/override, CodeSage HEAD, full inventory (URLs, inspected
HEADs, ranges, counts, materiality/stalled flags), migration state, and touched
bead IDs and the verified due date. Read back the checkpoint. Reuse this task
instead of generating weekly reminders.

On failure, report the exact error and already-changed records. Preserve valid
partial writes and append a partial-run comment if the ledger remains usable.
Do not advance the completed baseline or fall back to Markdown. A migration-only
audit may record its mapping but must not claim a fresh upstream sweep. Resume
from the last complete baseline, re-reading partial records before writing.

Report a compact per-repo table (commits, materiality, stalled/new flags), the
tracker ID/due date, and counts/IDs of carried, created, updated, resolved, and
decision-pending recommendations. Name incomplete coverage explicitly.
