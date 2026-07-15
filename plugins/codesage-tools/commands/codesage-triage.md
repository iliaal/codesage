---
name: codesage-triage
description: Mark a codesage review finding open / false-positive / wont-fix / fixed with an optional note
argument-hint: "<project-path> --finding <fnd_id> --status open|false-positive|wont-fix|fixed [--note \"text\"]"
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

Reject invalid combinations early (missing finding_id, unknown status). Don't proceed if any are malformed.

## Locate the finding

Walk `.codesage/findings/*.json` and find the file containing a finding with the matching `finding_id`. If no match, report `finding not found in <project>/.codesage/findings/` and stop. Suggest:

> Did you mean one of these? — list 3-5 findings with similar IDs (prefix match) or recent ones.

## Update the record

1. Load the JSON.
2. Find the matching finding's index.
3. Set `status = <new>` and `triaged_at = <now>`.
4. Append a history entry:
   ```json
   {
     "at": "2026-05-16T20:35:00Z",
     "action": "triage",
     "from_status": "open",
     "to_status": "false-positive",
     "note": "covered by integration tests in tests/api/auth_test.py"
   }
   ```
5. Write the file atomically (write to a `.tmp`, then `mv`).

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

- The user owns `false-positive` and `wont-fix`. Later reviews don't reopen or re-echo either status.
- `fixed` is a soft assertion. `/codesage-revalidate --finding <id>` reopens it only when the reviewer returns the same ID with current evidence. Omission alone doesn't prove the fix.
- The status flip is reversible: `--status open` always works. The full `history[]` preserves every change so audit is intact.
