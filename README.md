# CodeSage

[![CI](https://github.com/iliaal/codesage/actions/workflows/ci.yml/badge.svg)](https://github.com/iliaal/codesage/actions/workflows/ci.yml)
[![Tests](https://github.com/iliaal/codesage/actions/workflows/tests.yml/badge.svg)](https://github.com/iliaal/codesage/actions/workflows/tests.yml)
[![Secret scan](https://github.com/iliaal/codesage/actions/workflows/secret-scan.yml/badge.svg)](https://github.com/iliaal/codesage/actions/workflows/secret-scan.yml)
[![Version](https://img.shields.io/github/v/release/iliaal/codesage)](https://github.com/iliaal/codesage/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Follow @iliaa](https://img.shields.io/badge/Follow-@iliaa-000000?style=flat&logo=x&logoColor=white)](https://x.com/intent/follow?screen_name=iliaa)

CodeSage is a code intelligence engine for AI coding agents. It combines structural graph queries (symbols, references, dependencies) and semantic search (embedding retrieval with cross-encoder reranking) in a single Rust binary, usable as a CLI or over MCP.

## What you can do with it

- Find code by natural-language query: "where does auth happen?", "error handling in the GC".
- Look up symbol definitions by name across a codebase.
- Trace imports, calls, and inheritance for any symbol.
- Map import and include relationships between files.
- Estimate which files a change breaks (change impact analysis).
- Build curated code bundles for LLM consumption in JSON, markdown, or flat-text (gitingest-style) form.
- Read per-file git history: churn, fix ratio, historical co-change, risk score.
- Expose all of the above over MCP so Claude Code, Codex, or Cursor can call them.

## Supported languages

PHP, Python, C, Rust, JavaScript, TypeScript.

## Getting started

```bash
# Build with GPU support
cargo build --release -p codesage --features cuda

# Initialize and index a project
cd /path/to/your/project
codesage init
codesage index

# Search
codesage search "authentication handler"
codesage search --json --limit 20 "database connection pooling"

# Structural queries
codesage find-symbol MyClass
codesage find-references some_function --kind call
codesage dependencies src/main.py

# Change impact analysis (who breaks if you touch this?)
codesage impact DocumentRepository --depth 2 --source-only
codesage impact src/auth/session.ts --json

# Context bundle for LLM consumption
codesage export "authentication flow" --limit 5 --callers
codesage export MyClass --symbol --format md
codesage export "auth flow" --format ingest    # gitingest-style flat-text bundle

# Git history: churn, fix ratio, co-change, risk score
codesage git-index                                          # initial populate; hooks keep it fresh
codesage git-index --full                                   # force full rescan (weekly hygiene)
codesage coupling src/auth/session.ts --limit 5             # files that historically change with this
codesage risk src/auth/session.ts                           # score with decomposition

# MCP server for Claude Code / Codex / Cursor (one global server, every onboarded project)
claude mcp add --scope user codesage -- codesage mcp

# Auto-reindex on git operations
codesage install-hooks

# Diagnose installation
codesage doctor
```

## Claude Code plugin

`plugins/codesage-tools/` wraps everything above into one command per task. The marketplace manifest lives at the repo root.

```bash
claude plugin marketplace add /path/to/codesage
claude plugin install codesage-tools@codesage
/codesage-onboard /path/to/project
```

Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`. The plugin handles global MCP registration, per-project init, indexing, git hook install (Husky-aware), and writes a `.claude/CLAUDE.md` hint teaching the agent how to route MCP calls.

## Search pipeline

A query flows through five stages:

1. Embed the query with MiniLM-L6-v2 (22M params, 384d) via ONNX Runtime.
2. Prepend file path and symbol context to chunks before embedding.
3. Boost chunks whose content matches known symbol names.
4. Re-score the top candidates with ms-marco-MiniLM-L6-v2 and blend 50/50 with the semantic score.
5. Annotate each result with overlapping function and class names.

The reranker is optional. Set or remove it in `config.toml`; stages 1-3 and the annotation still run without it.

## Configuration

`codesage init` generates `.codesage/config.toml`:

```toml
[project]
name = "my-project"

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"
device = "gpu"                                        # "gpu" or "cpu"
reranker = "cross-encoder/ms-marco-MiniLM-L6-v2"     # optional, remove to disable

[index]
exclude_patterns = [
  "**/tests/**", "**/vendor/**", "**/node_modules/**",
  "**/*.test.ts", "**/*Test.php", "**/*.phpt",
]
```

Models download from HuggingFace the first time you use them.

## Architecture

A Rust workspace with six crates:

| Crate | Role |
|-------|------|
| `protocol` | Shared types (Symbol, Reference, SearchResult) |
| `parser` | File discovery, tree-sitter parsing, symbol and reference extraction |
| `storage` | SQLite with sqlite-vec KNN and FTS5 |
| `embed` | ONNX embedding inference, cross-encoder reranking, chunking |
| `graph` | Indexing orchestration and search pipeline |
| `cli` | Binary with CLI subcommands and MCP server |

Storage is a single SQLite database per project at `.codesage/index.db`: structural tables (symbols, refs, files) plus model-specific vector tables for embeddings.

## Retrieval benchmarks

`bench/` holds the harness:

- `codesage-bench-runner` runs a YAML corpus of ground-truth cases through `codesage search` and reports miss rate, median first-hit, recall@5, and recall@10.
- `extract-eval-cases.py` mines eval cases from Claude Code session transcripts and git commit history.

Corpora aren't bundled. Bring your own, or point the plugin at `$CODESAGE_BENCH_CORPUS_DIR`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). In short: file an issue first, add a test, update `CHANGELOG.md` under `[Unreleased]` for user-visible changes.

## License

MIT
