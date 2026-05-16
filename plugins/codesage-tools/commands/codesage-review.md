---
name: codesage-review
description: Review codesage feature slices in parallel via subagents, persist findings to .codesage/findings/
argument-hint: "<project-path> [--limit N] [--jobs N] [--feature <id>] [--kind route|library|cli-command|test-suite] [--severity low|medium|high] [--categories bug,security,perf,maintainability]"
---

# Review codesage feature slices

Fan out a code review across the project's mapped feature slices using subagents. Each subagent reviews one slice in its own context; this command coordinates and persists findings.

## Inputs

Parse `$ARGUMENTS`. First positional argument MUST be an absolute project path that's already onboarded to codesage (has `.codesage/index.db`). If not, stop and tell the user to run `/codesage-onboard <path>` first.

Flags:
- `--limit N` — cap the number of features reviewed in one run (default: 50)
- `--jobs N` — parallel subagents (default: 4; clamp to 8)
- `--feature <id>` — review one specific feature (skips the discovery step)
- `--kind <k>` — filter features by kind (`route`, `library`, `cli-command`, `test-suite`, `service`, `config`, `infra`)
- `--severity <s>` — minimum severity to report (`low`/`medium`/`high`, default `medium`)
- `--categories <c,c,...>` — comma-separated list (default: `bug,security`)

## Step 1: Discover features

If `--feature` is set, skip this step.

Otherwise call MCP `mcp__codesage__list_features(project, kind?, limit=200)` to get the feature inventory. Then filter to features that need review:

1. Drop features whose `.codesage/findings/<feature_id>.json` is newer than the feature's `updated_at` from the record AND has `runs[-1].status == "complete"` (already reviewed since last change).
2. Sort by what the user is most likely to care about: `kind == "route"` > `kind == "cli-command"` > `kind == "service"` > rest. Then by `confidence: high` first.
3. Apply `--limit`.

Report the count: "Found 47 features; 12 already up-to-date; reviewing 35 (capped at --limit 35)."

## Step 2: Generate run ID and prep state dir

```bash
RUN_ID="run_$(date +%Y%m%dT%H%M%SZ -u)"
mkdir -p "$PROJECT/.codesage/findings" "$PROJECT/.codesage/findings/history" "$PROJECT/.codesage/reviews"
```

Write the run start record to `.codesage/reviews/$RUN_ID.json`:

```json
{
  "run_id": "run_20260516T203000Z",
  "started_at": "2026-05-16T20:30:00Z",
  "project": "/abs/path",
  "filters": { "limit": 35, "jobs": 4, "kind": null, "severity": "medium", "categories": ["bug", "security"] },
  "features_planned": ["feat_aaa", "feat_bbb", ...],
  "status": "in_progress"
}
```

## Step 3: Load prior findings per feature

For each feature in the plan, read `.codesage/findings/<feature_id>.json` if it exists. Pass the `findings[]` array (where `status` is `open`, `false-positive`, or `wont-fix`) to the subagent as `prior_findings` so it can dedupe and respect prior triage.

If no file exists yet, pass `prior_findings: []`.

## Step 4: Dispatch subagents in parallel batches

Process features in batches of `--jobs`. For each batch, send all Agent tool calls in a SINGLE message with multiple tool-use blocks so they run concurrently.

Each subagent invocation:

```
Agent({
  description: "Review feat_<id>",
  subagent_type: "codesage-feature-reviewer",
  prompt: <<SELF-CONTAINED PROMPT>>
})
```

The prompt must be fully self-contained (subagents don't see your context). Template:

```
Review codesage feature <feature_id> in project <abs project path>.

Severity threshold: <severity>
Categories: <comma-separated>
Prior findings: <inline JSON array, possibly empty>

Call mcp__codesage__feature_bundle({project: "...", feature_id: "...", include_callers: true, include_callees: true}) for context, then mcp__codesage__assess_risk on the entry file, then Read on relevant files. Return strict JSON per your agent definition.
```

Don't add extra commentary in the subagent prompt — the agent file already has the full spec. Keep the dispatch prompt short and unambiguous.

## Step 5: Parse subagent responses

Each subagent returns a single JSON block. Extract it (`{ ... }` between ```json fences). If parse fails for a subagent, record the raw output to `.codesage/reviews/<run_id>-<feature_id>-raw.txt` and mark that feature as `error` in the run record. Don't crash the run — process other features and surface the failures at the end.

For each successfully parsed response:

1. Merge with the existing `.codesage/findings/<feature_id>.json`:
   - For each finding in the response, check if `finding_id` exists in the prior file. If yes, preserve `status` and `history` from the prior, update `last_seen_at` to now. If no, it's new — set `status: open`, `first_seen_at: now`, `last_seen_at: now`, append `{action: "review", at: now, run_id: <run>}` to `history`.
   - For each prior finding NOT in the response: mark `last_seen_at: now-1` (older than the run), append `{action: "not-seen", at: now, run_id: <run>}` to `history`. Don't auto-resolve — that's `/codesage-revalidate`'s job.
2. Write the merged file.
3. Snapshot the per-run state to `.codesage/findings/history/<feature_id>-<run_id>.json`.

## Step 6: Write the run completion record

Update `.codesage/reviews/<run_id>.json`:

```json
{
  ...,
  "completed_at": "...",
  "status": "complete",
  "stats": {
    "features_reviewed": 34,
    "features_errored": 1,
    "findings_new": 23,
    "findings_recurring": 9,
    "findings_by_severity": { "high": 3, "medium": 18, "low": 11 },
    "findings_by_category": { "bug": 14, "security": 8, "perf": 6, "maintainability": 4 }
  }
}
```

## Step 7: Report

Print a terminal summary:

```
Review run run_20260516T203000Z complete.
  Features: 34 reviewed, 1 errored (see .codesage/reviews/<run_id>.json)
  Findings: 23 new, 9 recurring, 32 total open
  By severity: 3 high, 18 medium, 11 low
  By category: bug 14, security 8, perf 6, maintainability 4
  Top features by finding count: feat_abc (5), feat_def (4), feat_xyz (3)

Next: /codesage-report --project <path>  to render Markdown
      /codesage-triage --finding <id> --status false-positive
      /codesage-revalidate --feature <id>  to re-check after a fix
```

If any severity-high findings exist, list them with `finding_id` + `file:line` + title so the user can act immediately.

## Notes

- The subagent runs READ-ONLY. It must not edit, write, or commit. Its `autoApprove: read` setting enforces this.
- If `--jobs` × token budget × feature count looks excessive, warn the user upfront ("This will dispatch 35 subagents in batches of 4; expect ~12 LLM invocations and a few minutes of wall-clock").
- Subagent dispatch inherits the host's model. If you want a cheaper sweep, the user can prefix with `claude /codesage-review --model haiku` (handled at the Claude Code level, not by this skill).
- The `.codesage/findings/` and `.codesage/reviews/` directories should be gitignored by default. The plugin's onboard script will add the entries on next refresh.
