---
name: codesage-triage
description: Mark a codesage review finding open / false-positive / wont-fix / fixed with an optional note
argument-hint: "<project-path> --finding <fnd_id> --status open|false-positive|wont-fix|fixed [--magnitude N --metric NAME] [--note \"text\"]"
---

# Triage a codesage review finding

Update the lifecycle state of a finding produced by `/codesage-review`. No LLM call — this is a pure local state edit on `.codesage/findings/<feature_id>.json`.

## Parse arguments

From `$ARGUMENTS`:
- First positional argument: absolute project path (required).
- `--finding <fnd_id>`: required, must look like `fnd_<8hex>`.
- `--status <s>`: required, one of:
  - `open` — the finding is real and not yet acted on (the default state from review).
  - `false-positive` — the finding doesn't actually apply; subsequent reviews should not re-raise it.
  - `wont-fix` — the finding is real but won't be acted on. Later reviews suppress it while preserving the human decision.
  - `fixed` — the finding has been resolved in the source. A revalidation run reopens it only when current evidence shows the same defect is still present.
- `--note "<text>"`: optional. Free-form context — why this is a false positive, link to a ticket, whatever the user wants future-them to read.
- `--magnitude N --metric NAME`: optional together, only with `false-positive` or `wont-fix`. Acknowledge an explicitly measured quantity up to N. Use the finding's recorded metric and a finite, nonnegative value at least as large as its observed value. The source must still match the reviewed content. Do not invent a magnitude for a finding without one; revalidate with a reproducible measurement first.

Reject invalid combinations early (missing finding_id, unknown status). Don't proceed if any are malformed.

> **Before touching any `.codesage/` path:** `.codesage/` is repository content, so a cloned
> repo can ship it — or any directory under it — as a symlink. Refuse to read, write, create,
> or delete through one. Check with `test -L <path>` (not `test -e`, which follows links) on
> `.codesage` itself and on each subdirectory you are about to use, and stop with an error if
> any is a symlink. Apply the same check to every **leaf** you touch: a `*.json` findings
> file, or any temporary file you create beside it, may itself be a planted symlink or a
> directory. Read or write a leaf only if it is a regular file (or absent, when creating),
> and give temporary files a freshly generated unique name rather than a predictable one.

## Locate the finding

Walk `.codesage/findings/*.json` and find the file containing a finding with the matching `finding_id`. If no match, report `finding not found in <project>/.codesage/findings/` and stop. Suggest:

> Did you mean one of these? — list 3-5 findings with similar IDs (prefix match) or recent ones.

## Update the record

Run `${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state triage --project <absolute-path> --finding <id> --status <status>`, passing the supplied `--note`, `--magnitude`, and `--metric` as separate arguments. The helper requires exactly one matching record, validates magnitude and source freshness, preserves history, writes atomically, and invalidates the reviewed-state cache so the next review evaluates the acknowledgement. Do not edit the JSON manually.

## Report

```
Triaged fnd_abcd1234 in feat_xyz789
  was: open  → now: false-positive
  note: covered by integration tests in tests/api/auth_test.py
  file: src/api/handler.rs:142
  title: Unauthenticated path bypasses token check

Future /codesage-review runs will suppress this finding.
```

## Notes

- Without a magnitude, `false-positive` and `wont-fix` retain legacy suppression. A numeric acknowledgement suppresses an applicable finding while its current value is at or below the acknowledged value. An increase, a changed metric, or missing measurement reopens it with a reason. Include that qualification in the report when magnitude is supplied.
- Acknowledgements survive a rename only when the original path disappears and exactly one tracked or nonignored untracked Git file matches its last reviewed SHA256. Copies, ambiguous hashes or matching acknowledgements, rewritten files, and failed inventories never transfer acknowledgement. If the feature ID changes, review imports the unique matching acknowledgement under a new feature-local finding ID, preserving `ack_transferred_from`. The source record remains for audit and is flagged stale by the sweep; future transfers consult the latest destination, not the retained source. Use the destination ID for subsequent triage.
- Review summaries and persisted `ack_sweep` flag stale acknowledgements and findings not emitted by the current review. Omission alone never deletes an acknowledgement or marks the finding fixed.
- Run `${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state sweep-acks --project <absolute-path>` to inspect all documents, including retired feature IDs, for missing or changed source and persisted non-emission diagnostics. This read-only sweep never infers a new magnitude.
- Triaging without magnitude removes an existing numeric acknowledgement; `--status open` always returns the finding to open status.
- Transferred source IDs are historical and cannot be triaged; the helper names their destination. A rename back to an old feature never reactivates its source acknowledgement. Only the current destination can supply a numeric acknowledgement, so revocation survives return renames and longer chains.
- `fixed` is a soft assertion. `/codesage-revalidate --finding <id>` reopens it only when the reviewer returns the same ID with current evidence. Omission alone doesn't prove the fix.
- The status flip is reversible: `--status open` always works. The full `history[]` preserves every change so audit is intact.
