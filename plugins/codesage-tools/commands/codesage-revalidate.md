---
name: codesage-revalidate
description: Re-check findings after code changes without inferring fixes from omission
argument-hint: "<project-path> --finding <fnd_id> | --feature <feat_id> | --all [--status open|fixed|false-positive|wont-fix] [--max-verify-findings N]"
---

# Revalidate CodeSage findings

Review the owning feature again, apply the same evidence and identity gates as `/codesage-review`, and reconcile through `codesage-review-state`.

## Parse and resolve

Require an absolute onboarded project path and exactly one selector:

- `--finding <id>`: locate one finding under `.codesage/findings/*.json`.
- `--feature <id>`: select findings in one feature.
- `--all`: select across the project, grouped by feature.

`--status` defaults to `open`. `--max-verify-findings` defaults to `5` and caps new regression candidates sent to each feature verifier.

If no findings match, print the selector and available status counts, then stop. Warn before dispatch when the selection spans more than 50 findings or 20 features.

## Prepare feature-local inputs

Use `rev_$(date -u +%Y%m%dT%H%M%SZ)` as `RUN_ID`. For each feature:

1. Read `entry_path`, `title`, `kind`, and `feature_files` from the findings document. For legacy documents missing metadata, call `mcp__codesage__list_features(project, limit=200)` once and join by `feature_id`.
2. Write the feature record under `.codesage/reviews/<RUN_ID>/features/`.
3. Project only the targeted findings. Include ID, location, severity, category, title, summary, evidence, suggested fix, and status. Strip `history`. Write their ID array to `.codesage/reviews/<RUN_ID>/targets/<feature_id>.json`.
4. Call `mcp__codesage__assess_risk_batch` once for the feature's entry and owned paths.

## Dispatch one reviewer per feature

Prompt `codesage-feature-reviewer` with the same feature metadata and risk fields as `/codesage-review`, plus:

```text
Revalidate these prior findings. Return a prior finding with its existing finding_id only when current code and evidence show the same defect. Omit it when you can't find it. Report new regression findings without an ID.

Prior findings under revalidation: <projected JSON>
Severity threshold: low
Categories: bug,security,perf,maintainability
```

An omission is a review result, not proof of a fix.

## Validate, verify news, and merge

Save each parsed response and run `codesage-review-state validate` exactly as `/codesage-review` does. Send at most `--max-verify-findings` new findings to one `codesage-finding-verifier` per feature and save its verdict array. This applies the adversarial verifier to revalidation news instead of accepting regressions unchecked.

Merge with:

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state" merge \
  --project "$PROJECT" \
  --feature ".codesage/reviews/$RUN_ID/features/<feature_id>.json" \
  --validated ".codesage/reviews/$RUN_ID/validated/<feature_id>.json" \
  --verdicts ".codesage/reviews/$RUN_ID/verdicts/<feature_id>.json" \
  --run-id "$RUN_ID" \
  --mode revalidate \
  --target-ids ".codesage/reviews/$RUN_ID/targets/<feature_id>.json"
```

The deterministic outcomes are:

- Returned prior with valid evidence: `still-present`; preserve `open`, `false-positive`, or `wont-fix`. A returned `fixed` finding reopens to `open` because positive evidence disproves the fix claim.
- Missing `open`: keep `open` and record one `needs-confirmation` event. Repeated omissions don't grow history.
- Missing `fixed`, `false-positive`, or `wont-fix`: preserve status and history. Omission doesn't confirm or overturn user triage.
- New finding: apply the same verifier path as a normal review; keep unverified overflow open and label it.
- Unselected finding in the same feature: preserve it unchanged; it was outside this revalidation run.

## Report

List every selected finding as `still-present`, `reopened`, or `not-seen-needs-confirmation`. List new findings with verifier status. Never print `confirmed-fixed` unless the user supplied independent test or diff evidence outside this review run.
