---
name: codesage-review
description: Review codesage feature slices in parallel via subagents, persist findings to .codesage/findings/
argument-hint: "<project-path> [--limit N] [--jobs N] [--feature <id>] [--kind route|library|cli-command|test-suite] [--severity low|medium|high] [--categories bug,security,perf,maintainability] [--deep] [--no-verify] [--model <m>] [--verify-model <m>]"
---

# Review codesage feature slices

Fan out a code review across the project's mapped feature slices using subagents. Each subagent reviews one slice in its own context; this command coordinates, mechanically checks the evidence, adversarially verifies new findings, and persists the survivors.

## Inputs

Parse `$ARGUMENTS`. First positional argument MUST be an absolute project path that's already onboarded to codesage (has `.codesage/index.db`). If not, stop and tell the user to run `/codesage-onboard <path>` first.

Flags:
- `--limit N` — cap the number of features reviewed in one run (default: 50)
- `--jobs N` — parallel subagents (default: 4; clamp to 8)
- `--feature <id>` — review one specific feature (skips the discovery step)
- `--kind <k>` — filter features by kind (`route`, `library`, `cli-command`, `test-suite`, `service`, `config`, `job`)
- `--severity <s>` — minimum severity to report (`low`/`medium`/`high`, default `medium`)
- `--categories <c,c,...>` — comma-separated list of `bug`/`security`/`perf`/`maintainability`/`style` (default: `bug,security`). `perf` and `maintainability` are opt-in: the reviewer reads slices statically and never profiles, so perf findings are inherently speculative and the highest false-positive category. The default keeps the two categories where a finding is almost always actionable. Add `perf` explicitly (`--categories bug,security,perf`) when you have a concrete hotspot concern; the rubric still gates it to demonstrable pessimizations.
- `--deep` — multi-lens mode: high-risk slices get 2-3 parallel reviewers with distinct lenses instead of one generalist (see Step 4). Costs roughly 2× on the slices it triggers for.
- `--no-verify` — skip the adversarial verification stage (Step 6). Verification is on by default; skip it only for quick informal sweeps.
- `--model <m>` — model for the reviewer subagents (pass as `model` on the Agent call). Default: inherit the session model.
- `--verify-model <m>` — model for the verifier subagents. Default: inherit the session model. When running a cheap sweep (`--model haiku`), keep the verifier on a stronger model — the model you trust for judgment belongs in the verifier seat.

## Step 1: Discover features

If `--feature` is set, skip this step.

Otherwise call MCP `mcp__codesage__list_features(project, kind?, limit=200)` to get the feature inventory. Then filter to features that need review:

1. Drop features already reviewed since their last code change. A feature is up-to-date when its `.codesage/findings/<feature_id>.json` mtime is newer than the entry file's last commit time (`git -C <project> log -1 --format=%ct -- <entry_path>`) AND the most recent `.codesage/reviews/*.json` record that planned this feature has `"status": "complete"`. `FeatureRecord` carries no timestamp, so use the file mtime and git history rather than a record field.
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
  "filters": { "limit": 35, "jobs": 4, "kind": null, "severity": "medium", "categories": ["bug", "security"], "deep": false, "verify": true },
  "features_planned": ["feat_aaa", "feat_bbb", ...],
  "status": "in_progress"
}
```

## Step 3: Load prior findings and compute the diff base per feature

For each feature in the plan, read `.codesage/findings/<feature_id>.json` if it exists. Pass the `findings[]` array (where `status` is `open`, `false-positive`, or `wont-fix`) to the subagent as `prior_findings` so it can dedupe and respect prior triage.

If no file exists yet, pass `prior_findings: []`.

When a prior findings file exists, also compute the commit the slice was last reviewed at, so the subagent can focus depth on what changed since:

```bash
LAST_TS=$(stat -c %Y "$PROJECT/.codesage/findings/<feature_id>.json")
BASE=$(git -C "$PROJECT" rev-list -1 --before=@$LAST_TS HEAD)
```

Pass `$BASE` as `last_reviewed_base` in the subagent prompt (omit the line entirely on first review or if the command fails). The subagent diffs the bundle's files against it itself.

## Step 4: Dispatch subagents in parallel batches

Process features in batches of `--jobs`. For each batch, send all Agent tool calls in a SINGLE message with multiple tool-use blocks so they run concurrently. When `--model` is set, pass it as `model` on every reviewer Agent call.

**Standard dispatch** (the default) — one reviewer per feature:

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
Last reviewed base: <sha>        # omit this line on first review
Prior findings: <inline JSON array, possibly empty>

Call mcp__codesage__feature_bundle({project: "...", feature_id: "...", include_callers: true, include_callees: true}) for context, then mcp__codesage__assess_risk on the entry file, then Read on relevant files. Return strict JSON per your agent definition.
```

Don't add extra commentary in the subagent prompt — the agent file already has the full spec. Keep the dispatch prompt short and unambiguous.

**Deep dispatch** (`--deep` only): before dispatching, call `mcp__codesage__assess_risk(project, entry_path)` for each planned feature. A feature is deep-eligible when `score >= 0.5` OR it crosses 3+ trust boundaries. Deep-eligible features get 2-3 reviewers in the same batch, identical prompts except one extra line each — `Lens: correctness`, `Lens: security`, `Lens: lifecycle` (drop the `security` lens if `security` isn't in `--categories`; drop `lifecycle` for pure-config slices). Non-eligible features get the standard single reviewer. Lens results for one feature merge in Step 5 exactly like one response with the findings concatenated — the ID computation dedupes overlaps.

## Step 5: Parse, mechanically check, and merge

Each subagent returns a single JSON block. Extract it (`{ ... }` between ```json fences).

**Parse repair:** if extraction or JSON parsing fails, don't give up on the feature yet. Dispatch one minimal repair subagent (`model: haiku`, general-purpose) with the raw output and the instruction: "Below is the raw output of a code-review agent that was supposed to return one fenced JSON object. Extract and syntax-repair that object. Output ONLY the fenced JSON — do not add, remove, or reword findings." If the repaired output parses, continue with it; if not, record the raw output to `.codesage/reviews/<run_id>-<feature_id>-raw.txt` and mark that feature as `error` in the run record. Don't crash the run — process other features and surface the failures at the end.

For each successfully parsed response, run these gates **in order**:

### 5a. Evidence check (mechanical, no LLM)

For every finding, verify the quoted evidence against the actual file. For each line in `evidence[]`, whitespace-normalize it (collapse runs of whitespace, trim) and search the whitespace-normalized content of `<project>/<file>`:

- At least one non-trivial evidence line (>10 chars after normalization) must appear in the file. If none does, **drop the finding** — the quote is fabricated or stale. Count it under `evidence_rejected` in the run record, with feature_id, file, title.
- If the evidence matches but sits outside `line ± 15`, correct the finding's `line` to where the evidence actually is and keep it.

Do this with a small python3/bash check, not by eyeballing. This gate is what makes the pipeline safe to run on weaker reviewer models.

### 5b. Compute finding IDs (orchestrator-side)

Reviewers do not mint IDs. For each surviving finding **without** a `finding_id` (i.e. not an echoed prior):

```bash
fnd_$(printf '%s' "<feature_id>|<file>|<category>|<line>" | sha256sum | cut -c1-8)
```

If two findings in one response collide on the tuple, append `|2`, `|3`, ... to the hashed string for the second and later ones (the ID format stays `fnd_<8hex>`).

### 5c. Merge with prior findings (exact, then fuzzy)

Merge with the existing `.codesage/findings/<feature_id>.json`:

1. **Exact match:** the finding's `finding_id` (echoed or computed) exists in the prior file → preserve `status` and `history` from the prior, update `last_seen_at` to now.
2. **Fuzzy match:** no exact ID match, but a prior finding has the same `file` + `category` and `|line delta| <= 10` → treat as the same finding: **keep the prior `finding_id`**, preserve `status`/`history`, update `line`, `title`, `summary`, `evidence` from the new response, set `last_seen_at` to now. This is what keeps identity stable across model changes (different title wording) and small code motion.
3. **New:** no match → set `status: open`, `first_seen_at: now`, `last_seen_at: now`, append `{action: "review", at: now, run_id: <run>}` to `history`. Queue it for Step 6 verification.
4. For each prior finding NOT in the response: mark `last_seen_at: now-1` (older than the run), append `{action: "not-seen", at: now, run_id: <run>}` to `history`. Don't auto-resolve — that's `/codesage-revalidate`'s job.

Don't write the merged file yet — Step 6 may still remove new findings.

## Step 6: Adversarial verification (default on; skipped by --no-verify)

Only **new** findings (Step 5c case 3) are verified — recurring findings already survived a prior round and the user's triage. Dispatch one `codesage-finding-verifier` subagent per new finding, batched by `--jobs`, all calls in one message per batch. When `--verify-model` is set, pass it as `model` on each verifier call. Prompt template:

```
Verify this code-review finding in project <abs project path>.

Finding:
<inline JSON of the finding, including its computed finding_id>

Try to refute it per your agent definition. Return strict JSON.
```

Apply verdicts:

- `confirmed` — keep; append `{action: "verify", at: now, run_id: <run>, result: "confirmed"}` to its history.
- `uncertain` — keep; append `{action: "verify", result: "uncertain", note: <note>}`. It stays `open`; the user adjudicates.
- `refuted` — **drop the finding** from the merge; record it in the run record under `refuted[]` with feature_id, title, and the verifier's `refutation` text so the user can spot-check the kills.

If a verifier errors or returns unparseable output, treat the finding as `uncertain` (keep it) — verification failure must not silently delete findings.

Now write the merged file for each feature and snapshot the per-run state to `.codesage/findings/history/<feature_id>-<run_id>.json`.

## Step 7: Write the run completion record

Update `.codesage/reviews/<run_id>.json`:

```json
{
  ...,
  "completed_at": "...",
  "status": "complete",
  "stats": {
    "features_reviewed": 34,
    "features_errored": 1,
    "features_deep_reviewed": 5,
    "findings_new": 23,
    "findings_recurring": 9,
    "findings_evidence_rejected": 3,
    "findings_refuted": 4,
    "findings_by_severity": { "high": 3, "medium": 18, "low": 11 },
    "findings_by_category": { "bug": 14, "security": 8, "perf": 6, "maintainability": 4 }
  },
  "evidence_rejected": [ { "feature_id": "...", "file": "...", "title": "..." } ],
  "refuted": [ { "feature_id": "...", "finding_id": "...", "title": "...", "refutation": "..." } ]
}
```

## Step 8: Report

Print a terminal summary:

```
Review run run_20260516T203000Z complete.
  Features: 34 reviewed (5 deep), 1 errored (see .codesage/reviews/<run_id>.json)
  Findings: 23 new (19 confirmed, 4 uncertain), 9 recurring, 32 total open
  Dropped: 3 failed evidence check, 4 refuted by verifier
  By severity: 3 high, 18 medium, 11 low
  By category: bug 14, security 8, perf 6, maintainability 4
  Top features by finding count: feat_abc (5), feat_def (4), feat_xyz (3)

Next: /codesage-report --project <path>  to render Markdown
      /codesage-triage --finding <id> --status false-positive
      /codesage-revalidate --feature <id>  to re-check after a fix
```

If any severity-high findings exist, list them with `finding_id` + `file:line` + title so the user can act immediately. If any findings were refuted, list their titles with one-line refutations — a wrong kill is worth catching.

## Notes

- The reviewer and verifier subagents run READ-ONLY. They must not edit, write, or commit. Their `autoApprove: read` setting enforces this.
- If `--jobs` × token budget × feature count looks excessive, warn the user upfront ("This will dispatch 35 subagents in batches of 4 plus ~1 verifier per new finding; expect ~50 LLM invocations and a few minutes of wall-clock"). `--deep` roughly doubles the reviewer count on the slices it triggers for.
- Subagent dispatch inherits the host's model unless `--model` / `--verify-model` override it. The economical sweep on a large project: `--model haiku --verify-model opus` — cheap finders, strong judge. The evidence check (5a) and verification (Step 6) exist precisely so weaker reviewer models don't degrade what lands in `.codesage/findings/`.
- The `.codesage/findings/` and `.codesage/reviews/` directories are gitignored automatically. `codesage init` writes `.codesage/.gitignore` containing `*` (plus `!.gitignore`), so the whole `.codesage/` tree stays out of version control.
