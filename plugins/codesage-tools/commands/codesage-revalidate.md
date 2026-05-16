---
name: codesage-revalidate
description: Re-check whether a finding still applies after a code change. Re-runs the review subagent on the affected feature slice.
argument-hint: "<project-path> --finding <fnd_id> | --feature <feat_id> | --all [--status open|fixed]"
---

# Revalidate codesage review findings

Re-run the review subagent against the slice that produced a finding and report whether the finding still surfaces. Lighter than a full `/codesage-review` — focused on one finding or one feature at a time.

## When to use

- After fixing a finding marked `fixed` to confirm the defect is gone.
- After a refactor of a feature slice to check whether prior findings still apply.
- When you triaged a finding as `false-positive` and want to confirm the next review won't re-raise it.

## Parse arguments

First positional: absolute project path (required).

One of these target selectors is required:
- `--finding <fnd_id>` — revalidate one specific finding.
- `--feature <feat_id>` — revalidate every open finding on a feature.
- `--all` — revalidate every finding matching `--status` across the project. Default `--status open`; pass `--status fixed` to confirm fix claims, `--status false-positive` to sanity-check triage decisions.

Reject any combination that selects more than one target type.

## Step 1: Resolve targets

For `--finding <id>`: walk `.codesage/findings/*.json`, locate the finding. Note its `feature_id`.

For `--feature <id>`: load `.codesage/findings/<feature_id>.json`. Collect all findings with `status: open` (or whatever `--status` filters to).

For `--all`: walk all `.codesage/findings/*.json`. Group findings by `feature_id` so we only spawn one subagent per feature even when several findings target the same slice.

If the target set is empty, report:

```
Nothing to revalidate.
  Tried: --status open in project /path
  Tip: --status fixed to verify fix claims, --status false-positive for triage sanity-check.
```

## Step 2: Dispatch subagents

For each distinct `feature_id` in the target set, spawn one `codesage-feature-reviewer` subagent (using the same Agent tool dispatch shape as `/codesage-review`). The prompt template:

```
Revalidate findings on codesage feature <feature_id> in project <abs path>.

You are re-running a review on this slice with a focused task: determine whether previously-recorded findings still apply.

Severity threshold: low (lower than normal so we don't accidentally mark a still-real finding as "no longer present" just because severity drifted)
Categories: bug,security,perf,maintainability  (all — match the original review's scope)

Prior findings under revalidation:
<JSON array of the findings being revalidated, each with finding_id, file, line, severity, category, title, summary, evidence, status>

Run feature_bundle + assess_risk as usual. For each prior finding:
  - If the same defect is still present at the same file:line: return it with the prior finding_id and the original title.
  - If the same defect is present but moved: return with the prior finding_id, updated file:line, and a one-line note in the summary noting the shift.
  - If the defect is gone: do NOT include it in your output.

Also report any NEW findings you'd flag — they may indicate the fix introduced a regression.

Return strict JSON per your agent definition. The orchestrator distinguishes "still present" vs "no longer present" by checking which prior finding_ids appear in your output.
```

Dispatch all subagent calls in a single message with multiple tool-use blocks (parallel).

## Step 3: Reconcile responses

For each subagent's response, compare against the prior findings list for that feature:

- **Still present** (finding_id appears in response): preserve prior `status`, append `history` entry:
  ```json
  {"at": "...", "action": "revalidate", "run_id": "rev_...", "result": "still-present", "moved_to": "src/foo.rs:147"}
  ```
- **Not seen** (prior finding_id missing from response):
  - If prior status was `open`: flip to `fixed` automatically, append `history` `{action: "revalidate", result: "no-longer-present"}`. The user can verify the diff.
  - If prior status was `fixed`: confirm fix. No status change; append `{action: "revalidate", result: "fix-confirmed"}`.
  - If prior status was `false-positive` or `wont-fix`: leave status alone, append `{action: "revalidate", result: "no-longer-present"}`. The triage decision still holds.
- **New finding** in the response that wasn't in the prior set: this is a regression OR a finding outside the revalidation scope. Append it to the file as a normal new finding with `first_seen_at: now`, `status: open`. Flag prominently in the report — the fix may have introduced this.

Snapshot the per-revalidation state to `.codesage/findings/history/<feature_id>-<revalidation_id>.json` where `revalidation_id = rev_<ts>`.

Write a revalidation record at `.codesage/reviews/rev_<ts>.json` (same shape as the review run record, with `type: "revalidate"` and a `targets[]` array of `finding_id`s).

## Step 4: Report

```
Revalidation rev_20260516T204500Z complete.
  Subagents dispatched: 3 (1 feature with 2 findings, 2 features with 1 finding each)

Per-finding outcomes:
  fnd_abc12345 (was: fixed)            confirmed-fixed     feat_xyz: src/auth.rs:142
  fnd_def67890 (was: open)             still-present       feat_xyz: src/auth.rs:163  ← unchanged
  fnd_ghi13579 (was: open)             still-present       feat_pqr: src/db.rs:88     ← moved from line 84
  fnd_jkl24680 (was: open)             no-longer-present   feat_pqr: status flipped to fixed

Regressions (new findings surfaced during revalidation): 1
  fnd_xxx99999  high  perf  feat_xyz: src/auth.rs:120  "N+1 query in token refresh path"

Next:
  /codesage-triage --finding fnd_xxx99999 --status open --note "regression introduced by fnd_abc12345 fix"
```

## Notes

- Revalidation does NOT mass-reopen `false-positive` or `wont-fix` findings even if the subagent re-surfaces them. The human triage stands; only their `history` gets a new entry noting the subagent saw it again.
- A finding being "no longer present" automatically flips `open` → `fixed`. This is a soft signal — the user should still review the diff to confirm the actual code change matches the claim.
- If `--all` selects more than 50 findings across more than 20 features, warn the user up front and ask for confirmation before dispatching (large parallel sweeps burn tokens).
