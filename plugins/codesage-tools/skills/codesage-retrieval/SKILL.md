---
name: codesage-retrieval
description: Route codebase retrieval through CodeSage in onboarded repositories. Use for semantic code search, symbol or reference tracing, feature-slice discovery, dependency and impact analysis, context export, historical coupling, change-risk assessment, edit-session regression checks, or focused test selection; also use before substantive code edits or reviews that need structural context.
---

# CodeSage Retrieval

## Establish the project boundary

Run from the repository under work:

```bash
test -f .codesage/index.db && codesage status
```

Use CodeSage only when both checks succeed. Otherwise use `rg`, or ask whether to onboard the repository when indexing would materially improve the task.

Resolve the repository root to an absolute path and pass that path as `project` on every CodeSage MCP call. Do not pass a parent workspace. For work spanning multiple repositories, make separate calls per repository. If the scope is unclear, establish the target repository before searching; never fan out across all indexed projects.

The CodeSage MCP is registered globally outside this plugin. Do not add per-project or per-tool entries to `~/.codex/config.toml`. After upgrading CodeSage, start a fresh Codex thread if the current session does not expose newly added tools.

## Choose the retrieval path

Use CodeSage for semantic and code-graph questions: behavior discovery, definitions, callers, imports, dependencies, feature ownership, blast radius, history-derived coupling, risk, and test selection.

Use `rg` for exact literal strings, error messages, configuration keys, test names, documentation, and generated files. Prefer the smallest query that answers the question; do not export a broad context bundle when a symbol or dependency lookup is enough.

## Route MCP calls

- Start unfamiliar-project orientation with `mcp__codesage__project_overview` when its combined language, freshness, feature, risk, and convention summary replaces several narrower calls.
- Use `mcp__codesage__find_symbol` for exact definitions and `mcp__codesage__find_references` for callers, imports, inheritance, and type uses.
- Use `mcp__codesage__search` for intent, behavior, or symptoms when the symbol name is unknown.
- Use `mcp__codesage__list_features` to discover mapped behavior slices and `mcp__codesage__find_feature` to identify the slice owning a file.
- Use `mcp__codesage__feature_bundle` after feature discovery when the entrypoint, owned files, tests, and context for one slice are needed together.
- Use `mcp__codesage__export_context` for a curated bundle around a free-form query or symbol when no mapped feature is the right anchor.
- Use `mcp__codesage__list_dependencies` for immediate imports and importers. Use `mcp__codesage__impact_analysis` before renames, broad changes, or API-contract edits to inspect downstream blast radius.
- Use `mcp__codesage__find_coupling` for historical co-change context. Read `response.coupled`; an empty list plus `note` can mean no history, no pair above the threshold, or a path mismatch.
- Use `mcp__codesage__assess_risk` for one file, `mcp__codesage__assess_risk_batch` for an explicit file set, and `mcp__codesage__assess_risk_diff` for a patch. Inspect `top_symbols` when present. Treat clustered directories and cycles touching the patch as evidence to investigate, not proof the patch introduced them.
- Use `mcp__codesage__session_start` before substantive edits when a structural baseline is useful, then `mcp__codesage__session_end` before completion to check for new cycles or top-risk-file drift.
- Use `mcp__codesage__recommend_tests` after edits to select focused tests from changed files. Run the relevant tests; recommendations are not verification.

## Use CLI fallbacks

When MCP tools are unavailable, run the corresponding CLI from the target repository:

```bash
codesage overview
codesage search --limit 5 'query'
codesage find-symbol <name>
codesage find-references <name>
codesage features-list
codesage feature-for <file>
codesage feature-bundle <feature_id>
codesage export 'query'
codesage dependencies <file>
codesage impact <symbol-or-file>
codesage coupling <file> --limit 5
codesage risk <file>
codesage risk-batch <files...>
git diff --name-only | codesage risk-diff
codesage session-start
codesage session-end
git diff --name-only | codesage tests-for
```

If daemon startup itself is the suspected failure, run `codesage mcp --direct` to exercise the single-process stdio path. Do not use that diagnostic as a second persistent MCP registration.
