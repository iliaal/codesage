---
name: codesage-report
description: Render findings with the deterministic Python formatter; no model-generated report prose.
argument-hint: "<project-path> [--status open,wont-fix] [--severity high,medium] [--category bug,security] [--feature <id>] [--output <path>]"
---

# Render CodeSage findings

Run the packaged formatter with the user's arguments as distinct shell arguments:

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/codesage-review-state" report "$PROJECT"
```

Require an absolute project path. Pass optional `--status`, `--severity`, `--category`, `--feature`, and `--output` / `-o` directly. Do not generate, summarize, or rewrite the Markdown with a model. Return the helper's stdout verbatim; if it fails, report the error and do not fabricate a report.

Filters intersect. Defaults are statuses `open,wont-fix`, all three severities, and all four categories. Unknown filter values fail. Without `--output`, Markdown goes to stdout. With an output path, the helper atomically writes UTF-8 Markdown and prints the destination, selected finding count, and affected feature count. The destination's parent must already exist; symlink components and nonregular destinations are refused.

## Persisted state and coverage

Read `title`, `kind`, `entry_path`, and `feature_files` from findings documents. For legacy metadata gaps, the formatter makes one complete CLI inventory lookup (`features-list --json --limit 0`) and can join records by `feature_id`. Missing IDs or a failed lookup display `(metadata unavailable)`; never guess an entry from a finding's location. For repeatable archival rendering, pass `--features <inventory.json>` from the inventory helper instead of a live lookup. An explicitly supplied malformed or truncated inventory fails the report.

The helper checks `.codesage` path components and every findings JSON leaf, rejects symlinks and nonregular files, and fails on malformed state. It uses the latest persisted `reviewed_at` for the state timestamp, never the wall clock. Identical findings, metadata inventory, and source contents with identical arguments produce byte-identical output. Source contents matter because live acknowledgement diagnostics check whether cited files changed; these diagnostics can change even when findings JSON does not.

Transferred acknowledgements appear in a transfer audit, excluded from current totals. Persisted `ack_sweep` entries for selected feature documents appear independently of status, severity, and category filters, labeled as the last review scope. Live source checks appear separately. Neither kind of diagnostic declares an omitted finding fixed. Acknowledged selected findings include the accepted metric/value and current measurement, or explicitly state that the current measurement is unavailable.

An empty selection displays the requested filters and project-wide current totals, excluding transferred historical records. Feature filtering restricts findings and diagnostics; the empty-selection totals still cover the project.

## Rendering

The formatter orders features and findings deterministically. High-severity findings have full sections with evidence and suggested fixes. Medium findings use sections up to eight records, then a table. Low findings always use a table. A separate section retains acknowledgement values and triage notes even for table rows. Summary counts distinguish statuses; `wont-fix` is not counted as open. Trust-boundary counts count each affected feature once per boundary.
