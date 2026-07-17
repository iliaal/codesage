---
name: codesage-review
description: Review CodeSage feature slices in parallel and persist validated findings
argument-hint: "<project-path> [--limit N] [--jobs N] [--feature <id>] [--kind <kind>] [--focus all|product] [--severity low|medium|high] [--categories bug,security,perf,maintainability] [--deep] [--no-verify] [--max-verify-findings N] [--model <m>] [--verify-model <m>]"
---

# Review CodeSage feature slices

Review mapped feature slices with read-only subagents. The subagents inspect code and return candidate findings. `${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state` validates evidence, assigns identities, compares feature content, merges prior state, and writes findings atomically.

## Parse inputs

The first positional argument must be an absolute onboarded project path containing `.codesage/index.db`.

Flags:

- `--limit N`: cap planned features after filtering and sorting. Default `50`.
- `--jobs N`: parallel reviewer or verifier calls. Default `4`, maximum `8`.
- `--feature <id>`: review one feature.
- `--kind <kind>`: filter to `route`, `library`, `cli-command`, `test-suite`, `service`, `config`, or `job`.
- `--focus all|product`: `all` reviews every mapped kind and is the default. `product` excludes `test-suite` plus entries under `bench/` and `scripts/`.
- `--severity low|medium|high`: minimum severity. Default `medium`.
- `--categories <list>`: any of `bug`, `security`, `perf`, `maintainability`. Default `bug,security`.
- `--deep`: use correctness, security, and lifecycle lenses on eligible slices.
- `--no-verify`: skip adversarial verification.
- `--max-verify-findings N`: maximum new findings sent to one feature's verifier. Default `5`, maximum `10`. Keep extra findings as `unverified` rather than dropping them.
- `--model <m>` and `--verify-model <m>`: reviewer and verifier model overrides.

Reject unknown values before dispatch. Set:

```bash
REVIEW_STATE="${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state"
test -x "$REVIEW_STATE"
PARAMS="categories=<sorted csv>;severity=<severity>;focus=<all|product>;deep=<0|1>"
```

`PARAMS` is the canonical review-scope string; pass the same value to every `feature-states`, `plan-feature`, and `merge` call so freshness fingerprints bind to this run's scope. Every `.codesage/...` path below lives under the project, not the session's working directory — always write it as `$PROJECT/.codesage/...`.

## 1. Create the run

Use `run_$(date -u +%Y%m%dT%H%M%SZ)` as `RUN_ID`. Create:

```text
$PROJECT/.codesage/findings/history/
$PROJECT/.codesage/reviews/<RUN_ID>/features/
$PROJECT/.codesage/reviews/<RUN_ID>/risk/
$PROJECT/.codesage/reviews/<RUN_ID>/changed/
$PROJECT/.codesage/reviews/<RUN_ID>/plans/
$PROJECT/.codesage/reviews/<RUN_ID>/responses/
$PROJECT/.codesage/reviews/<RUN_ID>/validated/
$PROJECT/.codesage/reviews/<RUN_ID>/verdicts/
```

Write `$PROJECT/.codesage/reviews/<RUN_ID>.json` with the run ID, start time, filters, and `status: in_progress`.

## 2. Discover, check freshness, and rank

Unless `--feature` names one slice, call `mcp__codesage__list_features(project, kind?, limit=500)`. If exactly 500 records return, the inventory may be truncated: record `inventory_truncated: true` in the run record and say so in the summary — coverage beyond the cap is not claimed. For `--feature`, run `codesage feature-show --json <feature_id>` from the project and fail if the ID is unknown. Write the inventory to `$PROJECT/.codesage/reviews/<RUN_ID>/features.json` and each complete feature record to `$PROJECT/.codesage/reviews/<RUN_ID>/features/<feature_id>.json`.

Hash freshness for the inventory in one process:

```bash
"$REVIEW_STATE" feature-states \
  --project "$PROJECT" \
  --features "$PROJECT/.codesage/reviews/$RUN_ID/features.json" \
  --findings-dir "$PROJECT/.codesage/findings" \
  --params "$PARAMS"
```

The helper reads each unique slice file once even when features overlap. Skip only records where `up_to_date` is true — except when `--feature` names one slice: an explicit request always re-reviews, even a fresh one. The fingerprint covers every entry, owned, context, and test file plus the run's `PARAMS`, so widening categories or lowering the severity floor re-reviews content-unchanged slices. Findings mtimes and triage edits don't affect it. Legacy documents without `reviewed_state` are reviewed once and upgraded during merge.

Apply `--focus` and `--kind`.

Collect every candidate's entry and owned paths. Call `mcp__codesage__assess_risk_batch` in chunks of at most 100 unique paths. Write each feature's batch-shaped subset to `$PROJECT/.codesage/reviews/<RUN_ID>/risk/<feature_id>.json`, attach each full risk record to its feature, and compute `max_owned_risk`.

Sort by:

1. product paths before `bench/`, `scripts/`, and `test-suite`;
2. `max_owned_risk` descending;
3. `route`, `service`, `cli-command`, `library`, then the remaining kinds;
4. high confidence first, then feature ID.

Apply `--limit`. This keeps full coverage by default while making a capped run reach risky product code first. Report discovered, fresh, filtered, and planned counts.

## 3. Prepare compact prompts

For each planned feature:

1. Generate feature-local priors:

   ```bash
   "$REVIEW_STATE" slim-priors \
     --findings "$PROJECT/.codesage/findings/<feature_id>.json"
   ```

   Use `[]` when the file doesn't exist. Never inline full histories or prior suggested fixes.
2. If the findings document has `reviewed_at_sha`, compute changed slice files in the orchestrator with `git -C "$PROJECT" diff --name-only <sha> -- <feature paths>`. Include committed and working-tree changes. Reviewers don't receive Bash. Write the JSON array, or `[]`, to `$PROJECT/.codesage/reviews/<RUN_ID>/changed/<feature_id>.json`.
3. Compute a bounded, deterministic coverage plan:

   ```bash
   "$REVIEW_STATE" plan-feature \
     --feature "$PROJECT/.codesage/reviews/$RUN_ID/features/<feature_id>.json" \
     --risk "$PROJECT/.codesage/reviews/$RUN_ID/risk/<feature_id>.json" \
     --changed "$PROJECT/.codesage/reviews/$RUN_ID/changed/<feature_id>.json" \
     --project "$PROJECT" \
     --params "$PARAMS" \
     --output "$PROJECT/.codesage/reviews/$RUN_ID/plans/<feature_id>.json"
   ```

   The plan always puts the entry first when present, then changed slice files of any role, then remaining entry/owned files by descending risk and path. Files deleted from disk are dropped from the plan rather than demanded of the reviewer. It requires at most five files normally and at most ten for high-risk, trust-heavy, large, or broadly changed slices. Unchanged context and test files remain available for targeted follow-up but aren't part of this bounded coverage contract. The plan also records `feature_state`, the dispatch-time content fingerprint that the merge stores — content edited after this point isn't stamped as reviewed.
4. Build a self-contained prompt with:

   ```text
   Review CodeSage feature <feature_id> in project <absolute path>.
   Severity threshold: <severity>
   Categories: <categories>
   Feature: <JSON containing id, title, kind, entry_path, files, trust_boundaries>
   Risk by file: <JSON from assess_risk_batch>
   Changed files since prior review: <JSON array, possibly empty>
   Must-read paths: <the plan's must_read JSON array>
   Prior findings: <slim JSON array, possibly empty>
   Lens: <optional lens>
   Follow your agent definition and return one strict JSON object.
   ```

## 4. Dispatch reviewers

Process features in batches of `--jobs`; send all Agent calls in one message per batch.

Standard mode uses one `codesage-feature-reviewer` per feature. Deep mode uses separate lenses when `max_owned_risk >= 0.5`, the feature crosses at least three trust boundaries, the feature has at least eight owned files, or the entry exceeds 800 lines. Use `correctness`, `security`, and `lifecycle`; omit security when the category is disabled and lifecycle for config slices.

Pass precomputed risk and only the plan's `must_read` path array in every prompt. Keep the remaining plan metadata in the run record. Reviewers must not repeat the batch risk call when risk is present. Every response declares the files actually inspected; bundle membership alone doesn't count as inspection.

Before dispatch, report the planned reviewer and verifier calls plus the worst-case conditional retry ceiling. The ceiling is two calls per initial reviewer, allowing one JSON repair, plus two calls per feature for one coverage retry and its JSON repair, plus at most one verifier per feature. Omit verifier calls under `--no-verify`. Warn when the ceiling exceeds 30 calls.

## 5. Parse and validate

Extract the first fenced JSON object from each response. If parsing fails, use one small repair call that may only repair JSON syntax. Save a second failure as `$PROJECT/.codesage/reviews/<RUN_ID>/responses/<feature_id>-raw.txt` and mark the feature errored.

For deep mode, save each lens response separately and combine them in fixed `correctness`, `security`, `lifecycle` order:

```bash
"$REVIEW_STATE" combine \
  --responses \
    "$PROJECT/.codesage/reviews/$RUN_ID/responses/<feature_id>-correctness.json" \
    "$PROJECT/.codesage/reviews/$RUN_ID/responses/<feature_id>-security.json" \
    "$PROJECT/.codesage/reviews/$RUN_ID/responses/<feature_id>-lifecycle.json" \
  --output "$PROJECT/.codesage/reviews/$RUN_ID/responses/<feature_id>.json"
```

Include only the lenses that were dispatched, and pass their paths explicitly in lens order rather than relying on glob order. `combine` rejects any lens response carrying an `error` field — a failed lens fails the feature (retry that lens once or mark the feature errored); never combine around it. The validation step below removes duplicate candidates by evidence fingerprint or close title/location identity before assigning IDs.

Write parsed JSON to `$PROJECT/.codesage/reviews/<RUN_ID>/responses/<feature_id>.json`, then run:

```bash
"$REVIEW_STATE" validate \
  --project "$PROJECT" \
  --response "$PROJECT/.codesage/reviews/$RUN_ID/responses/<feature_id>.json" \
  --findings "$PROJECT/.codesage/findings/<feature_id>.json" \
  --must-read "$PROJECT/.codesage/reviews/$RUN_ID/plans/<feature_id>.json" \
  --output "$PROJECT/.codesage/reviews/$RUN_ID/validated/<feature_id>.json"
```

The helper rejects fabricated or weakly-located evidence, corrects accepted loci, preserves identity by evidence fingerprint or close title match, prevents nearby defects from collapsing, guards echoed IDs against unrelated prior findings, and mints IDs for new findings. It also rejects a response whose `reviewed_files` omits any `must_read` path.

On that coverage error only, dispatch one focused `codesage-feature-reviewer` with the missing paths, feature metadata, risk records, and original review constraints. Ask it to inspect only those paths and return `reviewed_files` plus any additional findings. Combine the original and retry responses with `combine`, then validate once more. If required paths are still missing, or validation failed for another structural reason, mark the feature errored. Don't retry individual evidence rejections. Use `evidence_rejected`, `reviewed_files`, and the new/recurring ID lists in the run stats.

## 6. Verify new findings once per feature

Skip this step under `--no-verify`. Otherwise, for each feature with new IDs:

1. Sort new findings by severity, then file and line.
2. Take at most `--max-verify-findings`.
3. Dispatch one `codesage-finding-verifier` with the selected JSON array. Don't dispatch one verifier per finding.
4. Save its strict response as `$PROJECT/.codesage/reviews/<RUN_ID>/verdicts/<feature_id>.json`.

The verifier returns one verdict per selected ID. Findings beyond the cap or missing from a failed verifier response remain open with an `unverified` history event. Never delete them because verification failed.

## 7. Merge atomically

For every successfully reviewed feature, run:

```bash
"$REVIEW_STATE" merge \
  --project "$PROJECT" \
  --feature "$PROJECT/.codesage/reviews/$RUN_ID/features/<feature_id>.json" \
  --validated "$PROJECT/.codesage/reviews/$RUN_ID/validated/<feature_id>.json" \
  --verdicts "$PROJECT/.codesage/reviews/$RUN_ID/verdicts/<feature_id>.json" \
  --plan "$PROJECT/.codesage/reviews/$RUN_ID/plans/<feature_id>.json" \
  --params "$PARAMS" \
  --run-id "$RUN_ID" \
  --mode review
```

Omit `--verdicts` when verification is disabled. The helper writes `$PROJECT/.codesage/findings/<feature_id>.json` plus the immutable run snapshot; a snapshot-only write failure is a warning (`snapshot_error` in the output), not a merge failure. It stores feature metadata, trust boundaries, and the plan's dispatch-time content fingerprint. It appends one pending event when an open finding disappears, never marks it fixed, and adds no `not-seen` history to fixed, false-positive, or wont-fix findings. A recurring finding whose prior is `fixed` reopens to `open` with a `reopened` event — current evidence disproves the fix claim in review mode too.

## 8. Complete the run record

Build the completion record from helper outputs rather than recounting findings by hand. Include feature counts, new/recurring/unverified/refuted/evidence-rejected counts, severity/category totals, errors, and high-severity IDs. Set `status: complete` only after every successful feature has merged; otherwise set `status: partial` and list errors.

Print the same summary plus exact next commands for report, triage, and revalidation.

## Constraints

- Reviewer and verifier agents only use Read, Grep, and read-only CodeSage MCP tools.
- User triage owns `false-positive` and `wont-fix`; later reviews suppress both.
- Static review doesn't justify speculative performance claims. Keep `perf` opt-in.
- All state stays under ignored `.codesage/findings/` and `.codesage/reviews/`.
