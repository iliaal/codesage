# brief-efficacy

Measures observed follow-up actions after the `codesage brief` PreToolUse hook
(`plugins/codesage-tools/hooks/brief-hook.sh`) changes agent behavior, or just
spends context. Stdlib-only Python:

```bash
python3 bench/brief-efficacy/analyze.py [--ledger-dir DIR] [--projects-dir DIR] [--min-served 50] [--json]
```

## Opt-in canary

Set `CODESAGE_BRIEF_CANARY=1` in the environment that launches Claude Code to
enable the hook. It is disabled by default. Every invocation requires a valid
session ID and uses `brief --session` for deduplication, cooldown, and the
1,500-token estimate (characters divided by four). Missing sessions, failures,
and ambiguous paths stay silent;
the hook never blocks edits. Disabled invocations exit before reading stdin.
State and its lock remain in the runtime directory so resuming an older session
does not reset its budget. Clearing that directory, including at reboot on tmpfs,
resets the gate. Corrupt state and concurrent lock contention suppress output.

Run `python3 bench/brief-efficacy/test_canary.py` for hook and scorer regressions.
These use controlled fixtures; they are not efficacy evidence.

### Verify collection before accumulating a sample

Start a new Claude Code session with the canary enabled and the CodeSage plugin
loaded. From this checkout, you can load the local plugin for that session:

```bash
CODESAGE_BRIEF_CANARY=1 claude --plugin-dir "$PWD/plugins/codesage-tools"
```

Work on an actual editing task in an onboarded project. Keep the session
transcript and the persistent fire ledger. Do not use `--bare` or
`--no-session-persistence`: the former skips hooks, and the latter discards the
transcript required to score exposure. A linked worktree needs its own index;
the hook resolves the project from the edited file's ancestors.

After a session finishes, run the analyzer and inspect the first served row
before collecting more sessions. `served_scored.n` counts all served ledger
rows; `served_scored.scoreable_n` counts only acted, ambiguous, and no-op rows.
`required_scoreable_serves` reports the requested threshold, and
`observational_sample_ready` reports whether that threshold was reached. None
of these fields establishes causal efficacy.

For an unmatched row, `unmatched_reason` distinguishes a transcript that could
not be resolved (`transcript-not-resolved`, including ambiguous session paths)
from a transcript without a matching successful exposure (`exposure-not-found`).
`transcript-read-failed` means the transcript could not be read completely;
partial contents, malformed JSON, and invalid UTF-8 are excluded from scoring
and the base rate. A failed read anywhere in the resolved session family
leaves that session's served rows unmatched.
For `exposure-not-found`, check for the brief hook's successful attachment and decoded
`additionalContext`. Running `codesage brief --session` manually writes a served
ledger row, but a command result is not proof that the PreToolUse hook injected
context. Keep these rows unmatched; do not reconstruct attachment records.

Native Codex subagents do not exercise this Claude PreToolUse hook. Their edits
cannot supply its sample unless a separately verified integration records real
exposure and follow-up actions.

### Collect a controlled comparison

Use real, independently useful tasks with acceptance checks defined before the
runs. Give each task independent clean checkouts at the same source revision,
with equivalent fresh indexes and history, the same model and effort, the same
tool permissions, and the same task instructions. Randomize the order of the
canary-enabled and disabled runs. Keep session budgets and deduplication intact;
do not split a task into sessions merely to inflate the number of serves.

Keep a private mapping of task, arm, order, source and binary revisions, session
ID, and outcome. Evaluate both arms with the same acceptance checks without
showing the evaluator the arm. Report task success, tool calls, tokens, latency,
and cost alongside matched exposure counts. A served-file action can be
correlated with the hook while task quality stays unchanged or worsens.

The existing `bench/agent-task-runner` compares MCP availability on read-only
retrieval tasks and disables Edit/Write. It cannot run this hook comparison
unchanged. Select and bound the editing workload before starting billed model
runs. Fifty scoreable serves make the observational sample reviewable; they do
not establish fifty independent tasks or replace the controlled comparison.

## Data sources

1. **Fire ledger** — `brief-fires.jsonl` (and rotation sibling
   `brief-fires.jsonl.1`) in the CodeSage state dir (`$XDG_STATE_HOME/codesage`,
   else `~/.local/state/codesage`; relative or empty values are ignored, and
   if neither resolves the ledger falls back to the runtime dir). Not the
   runtime dir by default: that is tmpfs on
   systemd hosts and WSL2 and is wiped at boot, which is where every fire
   before 2026-09-06 went. Unless `--ledger-dir` is given, the analyzer also
   reads every runtime-dir candidate (`$CODESAGE_DAEMON_RUNTIME_DIR`,
   `$XDG_RUNTIME_DIR/codesage`, `/tmp/codesage-$UID`, `$TMPDIR/codesage-$UID`)
   and merges them, so rows
   written before the move still count until those dirs are wiped.
   Written by `codesage brief --session`, one line per
   fire, silent fires included: `t` (epoch secs), `s` (session id), `p`
   (project root), `f` (repo-relative file), `d` (decision:
   served/empty/repeat/cooldown/budget/unavailable/error), and on non-empty
   payloads `h` (FNV-1a 64-bit hex over the rendered payload text, per
   `crates/cli/src/brief_gate.rs`) and `tok` (chars/4).
2. **Session transcripts** — `~/.claude/projects/<munged-cwd>/<session>.jsonl`,
   where `<munged-cwd>` is the project root with non-alphanumerics replaced by
   `-`. The served payload text lands in the transcript verbatim (the hook
   injects it as `additionalContext`), which is what makes the digest join
   possible.

## What the analyzer settles

**(a) Decision mix** per session and per project. ~90% of fires are silent by
design; the mix tells an over-firing surface from a well-gated one.

**(b) Per-serve scoring.** Each `served` ledger row is joined to its
transcript by session id, then to the exact injected text by recomputing the
FNV-1a digest over candidate payload blocks found in the transcript. Scored
strictly:

- **acted** — after the serve, a Bash tool call runs one of the served test
  paths (a test-runner invocation naming the path), or a later full Read/Edit/Write
  touches one of the served co-change files.
  Collection, help, and version requests do not count. Shell control flow, redirects,
  heredocs, and substitutions are not scored; a simple `cd DIR &&` prefix and
  RTK wrappers are supported. Tool calls establish attempted actions, not test
  success.
  Test-path scoring recognizes direct Python scripts, `python -m pytest` /
  `unittest`, `pytest`, `phpunit`, `jest`, `vitest run`, and `node --test` with
  known execution options. Option values are not test selectors. Unknown flags
  and wrappers remain uncredited. Cargo filters, Go package selectors, and
  npm/pnpm/yarn scripts are not resolved to served test files; their path mentions
  cannot establish which tests they select. This makes the observed rate a
  conservative lower bound for the supported forms.
- **ambiguous** — the only post-serve touch of a served co-change file is a
  *ranged* Read (`offset`/`limit` present). A ranged read after a serve is
  consistent with acting on the brief but also with ordinary navigation, so it
  never counts as acted.
- **no-op** — none of the served content was exercised afterwards.
- **hotspot-only** — the payload named no tests and no co-change files, so
  there is no detectable action; excluded from the acted/no-op denominator.
- **branch-only** — the payload reports branch overlap without tests or
  co-change files; excluded because branch-related actions are not scored.
- **unmatched** — transcript missing or the digest never found (e.g. the
  session ran on another machine, or the transcript was pruned).

Only successful Edit/Write/MultiEdit PreToolUse attachments count as exposure.
The analyzer decodes JSON
`additionalContext` from `attachment.stdout`, and collapses duplicate hook
records sharing a tool-use ID and payload digest. Ordinary quoted payload text
does not establish exposure. The resolved session includes its parent transcript
and native `SESSION/subagents/agent-*.jsonl` files. Each exposure is scored only
against later actions in the transcript that received it. A parent copy with
the same tool-use ID as a child attachment is ignored. If distinct transcripts
contain the same digest after that deduplication, `exposure-ambiguous` leaves
the row unmatched instead of guessing its recipient or interleaving actions.
Symlinked transcript files and session/subagent directories are excluded.
A session that edited a sibling project can be matched across project
directories only when its session family is unique. The descriptive base rate
still uses parent transcripts only; it is not a matched control for child actions.

**Compliance ≠ adoption.** "acted" means the named action occurred after the
serve — it does not prove the brief *caused* it. The agent may have run those
tests anyway. The observational base rate cannot establish causality.

## Base rate

The honest counterfactual — "for non-served edits, would the agent have run
the tests a brief *would have* named?" — requires rebuilding would-have-served
payloads from the index at each historical edit, which is out of scope for a
transcript-only harness. Instead the analyzer computes the **unconditioned
rate of file-named-test-following behavior**: across all transcripts of the
ledger's projects, the fraction of edited files (first edit per file per
session) whose name stem later appears in a Bash command matching a
file-named-test pattern (`{stem}Test`, `test_{stem}`, `{stem}_test`,
`{stem}.test.`, `{stem}.spec.`) or a test-runner invocation naming the stem.

**Known biases, stated plainly:**

- The base-rate population includes edits where no file-named test *exists*;
  the served population by construction had something to name. This deflates
  the base rate and flatters the hook. Treat a *small* positive gap as noise.
- The base rate only covers the test half of "acted"; co-change-file follow-up
  has no clean unconditioned analogue (agents read neighboring files
  constantly), so serves scored acted purely via co-change files have a weaker
  counterfactual than serves scored via tests.
- Stem substring matching over-counts short or generic stems; stems under 3
  characters are excluded from the base rate. Served-test scoring requires the
  actual named path.

## Decision rule

After **≥ 50 scoreable served fires** (acted + ambiguous + no-op; not
hotspot-only, branch-only, or unmatched), the observational sample is available for review.
Ambiguous counts against the hook. The descriptive z-score compares biased,
non-equivalent populations; it cannot decide whether to keep the hook or enable
it by default. Default-on requires randomized exposure or controlled A/B task
runs, as specified in the design memo. The analyzer always reports
`default_on_ready: false` because it does not collect such controlled evidence.

See [recorded evidence](RESULTS.md) for the retained sample and controlled-run
outcomes, with production observations and historical experiments kept separate.
