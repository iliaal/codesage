---
name: codesage-report
description: Render a Markdown findings report from .codesage/findings/. No LLM call — pure formatter.
argument-hint: "<project-path> [--status open,wont-fix] [--severity high,medium] [--category bug,security] [--feature <id>] [--output <path>]"
---

# Render codesage findings as Markdown

Format the project's findings into a Markdown report suitable for pasting into a PR description, a ticket, or a stakeholder email. No LLM call — this is a pure file walk + format.

## Parse arguments

First positional: absolute project path (required).

Filters (intersected — multiple filters AND together):
- `--status <s,s>` — comma-separated, default `open,wont-fix`. Triaged-out states (`false-positive`, `fixed`) excluded unless explicitly named.
- `--severity <s,s>` — default `high,medium,low` (all).
- `--category <c,c>` — default all.
- `--feature <id>` — restrict to one feature's findings.

Output:
- `--output <path>` / `-o <path>` — write Markdown to this path. Without it, print to stdout.

## Walk the findings

Read every `.codesage/findings/<feature_id>.json` in the project. For each finding, apply filters. Drop anything that doesn't match.

If the resulting set is empty after filtering, write:

```
No findings match the requested filters.
  Project: <path>
  Filters: status=<>, severity=<>, category=<>
```

Then list the unfiltered totals so the user can adjust:

```
Total findings in project: 47
  By status: open: 23, fixed: 12, false-positive: 8, wont-fix: 4
  By severity: high: 5, medium: 28, low: 14
  By category: bug: 19, security: 11, perf: 9, maintainability: 8
```

## Pull feature metadata

Read `title`, `kind`, `entry_path`, and `feature_files` from each findings document. Current review runs persist these fields during merge, so reporting needs no MCP lookup.

For a legacy document missing metadata, call `mcp__codesage__list_features(project, limit=200)` once and join records by `feature_id`. Don't guess an entry path from an arbitrary finding file. If the join misses, render the feature ID with `(metadata unavailable)`.

## Render Markdown

Output layout:

```markdown
# Code review findings — <project basename>

State updated 2026-05-16T20:50:00Z in .codesage/findings/.

## Summary

- **23 open findings** across 11 feature slices.
- By severity: **3 high**, **15 medium**, **5 low**.
- By category: bug (10), security (7), perf (4), maintainability (2).
- Trust-boundary distribution on affected features: network 8, secrets 5, filesystem 4, process-exec 3, database 2.

## High-severity findings

### fnd_abc12345 — Unauthenticated path bypasses token check

- **Feature:** `feat_xyz789` (route — `GET /api/users/{id}`)
- **File:** `src/api/handler.rs:142`
- **Trust boundaries crossed:** network, secrets, user-input
- **Status:** open (first seen 2026-05-16T20:30, last seen 2026-05-16T20:30)

The token validator branches on `headers.get("x-api-key")` but returns the row if the header is absent. An unauthenticated request returns a 200 with the user record body.

```rust
  let token = req.headers.get("x-api-key").unwrap_or(&Default::default());
  // ... handler proceeds to load and return the user row
```

**Fix:** Reject with 401 when the header is missing OR `validate_token()` returns false. The existing `auth::require_authenticated` middleware (used in 7 other handlers) is the pattern.

---

[... next high-severity finding ...]

## Medium-severity findings

[... compact table or full sections, see "Density rule" below ...]

## Low-severity findings

[... compact table ...]

## Triaged-out (informational)

[Only when --status includes false-positive / wont-fix / fixed]

- **fnd_def67890** (false-positive, 2026-05-16): `feat_abc/src/db.rs:84` — *"Race in connection pool"*. Note: "pool is single-threaded by construction; the comment in connection.rs:12 explains the invariant."
```

## Density rule

- **High severity:** full section per finding (the example above).
- **Medium severity:** compact one-paragraph form OR a table if there are more than 8. Table columns: `finding_id`, `file:line`, `title`, `feature` (short kind+name).
- **Low severity:** always a table.

## Step output

If `--output` is set, write to the path AND print:

```
Wrote findings report: <output path>
  Findings: 23 (3 high, 15 medium, 5 low)
  Features touched: 11
```

If not, print the rendered Markdown directly.

## Notes

- Use the latest persisted `reviewed_at` value for the `State updated` line. Don't insert the current wall clock; identical findings state must render byte-identical output.
- The trust-boundary distribution comes from each feature's record (`trust_boundaries: Vec<TrustBoundary>`), aggregated across the filtered features. It gives a quick read on whether the open findings cluster in security-sensitive code.
- For just the high-severity section, run `--severity high`. For an audit-trail report including triaged-out findings, run `--status open,wont-fix,false-positive,fixed`.
