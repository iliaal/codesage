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
  - `wont-fix` — the finding is real but won't be acted on. Reviews continue to raise it; this just records the human decision.
  - `fixed` — the finding has been resolved in the source. The next `/codesage-revalidate` run should confirm it's gone; if it reappears, the status flips back to `open`.
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

Future /codesage-review runs will skip this finding unless its title/file/line shifts.
```

## Notes

- The triage state is owned by the user, not the reviewer. Subsequent `/codesage-review` runs MUST NOT auto-flip a `false-positive` or `wont-fix` back to `open` — the subagent receives prior findings and is told to respect their status (see the agent definition).
- `fixed` is a soft assertion. The user is claiming the underlying code change resolves the finding. To verify, run `/codesage-revalidate --finding <id>` which re-runs the review subagent on the slice and checks whether the same defect still surfaces.
- The status flip is reversible: `--status open` always works. The full `history[]` preserves every change so audit is intact.
