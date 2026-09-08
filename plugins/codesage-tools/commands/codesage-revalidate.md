---
name: codesage-revalidate
description: Re-check findings after code changes without inferring fixes from omission
argument-hint: "<project-path> --finding <fnd_id> | --feature <feat_id> | --all [--status open|fixed|false-positive|wont-fix] [--max-verify-findings N]"
---

# Revalidate CodeSage findings

Review the owning feature again, apply the same evidence and identity gates as `/codesage-review`, and reconcile through `codesage-review-state`.

## Parse and resolve

Require an absolute onboarded project path and exactly one selector:

- `--finding <id>`: locate one finding under `$PROJECT/.codesage/findings/*.json`.

Every `.codesage/...` path below lives under the project, not the session's working directory — always write it as `$PROJECT/.codesage/...`.
- `--feature <id>`: select findings in one feature.
- `--all`: select across the project, grouped by feature.

`--status` defaults to `open`. `--max-verify-findings` defaults to `5` and caps new regression candidates sent to each feature verifier.

If no findings match, print the selector and available status counts, then stop. Warn before dispatch when the selection spans more than 50 findings or 20 features.

## Prepare feature-local inputs

Use `rev_$(date -u +%Y%m%dT%H%M%SZ)` as `RUN_ID`. For each feature:

1. Run `codesage-review-state inventory --project "$PROJECT" --feature <feature_id> --output <run-inventory.json>` for fresh, complete feature metadata; do not use the budget-limited MCP list. If the CLI explicitly reports that this feature ID no longer exists, use the findings document's persisted `entry_path`, `title`, `kind`, and `feature_files` as a historical scope, record `metadata_source: persisted-retired-feature`, and disclose that it may not cover the current feature map. Other inventory errors fail this feature rather than silently falling back. If a retired legacy document lacks its file list, record a partial run and an unavailable scope; do not infer the slice from one finding.
2. Write the complete feature record under `$PROJECT/.codesage/reviews/<RUN_ID>/features/`. Normalize persisted `feature_files` to `files` when using historical scope. Union target finding paths missing from that list as explicit `context` paths so selected findings remain in the bounded plan, without pretending those paths are current mapped ownership.
3. Project only the targeted findings. Include ID, location, severity, category, title, summary, evidence, suggested fix, status, and optional `magnitude` and `acknowledgement` unchanged. Strip `history`. Run `codesage-review-state sweep-acks --project <absolute-path>` and exclude transferred source records from targets, directing explicit source-ID requests to their destination. Write their ID array to `$PROJECT/.codesage/reviews/<RUN_ID>/targets/<feature_id>.json`.
4. Collect existing entry and owned paths, save the requested path array, and call `mcp__codesage__assess_risk_batch` in batches of at most 100 unique paths. Run `codesage-review-state check-risk --paths <requested-paths.json> --risk <response.json>` for every batch before using scores. Truncation, missing or extra rows, duplicate paths, and invalid scores fail this check. Retry smaller batches or use CLI `risk-batch --json` from the project; never synthesize a zero score. If complete coverage remains unavailable, mark the feature errored and the run partial. Write the complete batch-shaped result to `$PROJECT/.codesage/reviews/<RUN_ID>/risk/<feature_id>.json`.
5. Compute changed slice paths since `reviewed_at_sha`, when present, and union them with the targeted findings' paths. Write the array to `$PROJECT/.codesage/reviews/<RUN_ID>/changed/<feature_id>.json`.
6. Run `codesage-review-state plan-feature` with the feature, risk, and changed/target path files, passing `--project "$PROJECT"` so deleted files drop out of the plan instead of deadlocking coverage. Write `$PROJECT/.codesage/reviews/<RUN_ID>/plans/<feature_id>.json` and pass it to the reviewer as `must_read`.

## Dispatch one reviewer per feature

Prompt `codesage-feature-reviewer` with the same feature metadata and risk fields as `/codesage-review`, plus:

```text
Revalidate these prior findings. Return a prior finding with its existing finding_id only when current code and evidence show the same defect. The echoed ID is advisory — the helper re-derives identity from evidence and location. These priors are targeted for revalidation regardless of status: return any of them whose defect is present, including false-positive and wont-fix ones; the standing suppression rule doesn't apply to targeted findings. Omit a finding when you can't find its defect. Report new regression findings without an ID.

Prior findings under revalidation: <projected JSON>
Severity threshold: low
Categories: bug,security,perf,maintainability
Must-read paths: <the plan's must_read JSON array>
```

An omission is a review result, not proof of a fix. The response must list every actually inspected path in `reviewed_files`; bundle membership doesn't count as inspection.

## Validate, verify news, and merge

Save each parsed response and run `codesage-review-state validate` exactly as `/codesage-review` does, including `--must-read "$PROJECT/.codesage/reviews/$RUN_ID/plans/<feature_id>.json"`. On a missing-coverage error only, make one focused reviewer retry for the missing paths, combine its response with the original, and validate once more. A second miss marks the feature errored. Send at most `--max-verify-findings` new findings to one `codesage-finding-verifier` per feature and save its verdict array. This applies the adversarial verifier to revalidation news instead of accepting regressions unchecked.

Merge with:

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state" merge \
  --project "$PROJECT" \
  --feature "$PROJECT/.codesage/reviews/$RUN_ID/features/<feature_id>.json" \
  --validated "$PROJECT/.codesage/reviews/$RUN_ID/validated/<feature_id>.json" \
  --verdicts "$PROJECT/.codesage/reviews/$RUN_ID/verdicts/<feature_id>.json" \
  --run-id "$RUN_ID" \
  --mode revalidate \
  --target-ids "$PROJECT/.codesage/reviews/$RUN_ID/targets/<feature_id>.json"
```

The deterministic outcomes are:

- Returned prior with valid evidence: `still-present`; preserve `open` and legacy triage without numeric acknowledgement. For a numeric acknowledgement, return the current measured magnitude: values at or below its threshold stay suppressed, while increases, missing measurements, or a changed metric reopen it. A returned `fixed` finding reopens to `open` because positive evidence disproves the fix claim.
- Missing `open`: keep `open` and record one `needs-confirmation` event. Repeated omissions don't grow history.
- Missing `fixed`, `false-positive`, or `wont-fix`: preserve status and history. Omission doesn't confirm or overturn user triage.
- Report all `ack_sweep` entries and `suppressed` counts. Acknowledged targeted findings omitted from the response are flagged `foreign`; findings outside the target set are not swept.
- New finding: apply the same verifier path as a normal review; keep unverified overflow open and label it.
- Returned finding matching an untargeted prior: ignored by the merge and listed under `out_of_scope` in its output — not an error, and the untargeted prior is untouched.
- Unselected finding in the same feature: preserve it unchanged; it was outside this revalidation run.

Revalidation never advances the slice's content fingerprint or `reviewed_at_sha`; only a full review merge does, so a targeted recheck can't make changed code look fresh to the next `/codesage-review`.

## Report

List every selected finding as `still-present`, `reopened`, or `not-seen-needs-confirmation`. List new findings with verifier status. Never print `confirmed-fixed` unless the user supplied independent test or diff evidence outside this review run.
