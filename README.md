# CodeSage

Code intelligence engine for AI coding agents. Combines structural graph queries (symbols, references, dependencies) with semantic search (embedding-based retrieval + cross-encoder reranking) in a single Rust binary.

## What it does

- **Semantic search** -- find code by natural language query ("where does auth happen?", "error handling in the GC")
- **Symbol lookup** -- find definitions by name across a codebase
- **Reference tracing** -- trace imports, calls, inheritance for any symbol
- **Dependency mapping** -- map import/include relationships between files
- **Change impact analysis** -- estimate which files are affected by changing a symbol or file
- **Context export** -- build curated code bundles ready for LLM consumption (JSON, markdown, or gitingest-style flat-text)
- **Git history intelligence** -- per-file churn, fix ratio, historical co-change, risk score (V2b slice 1)
- **MCP interface** -- all tools exposed via Model Context Protocol for AI agents

## Supported languages

PHP, Python, C, Rust, JavaScript, TypeScript.

## Getting started

```bash
# Build (GPU support)
cargo build --release -p codesage --features cuda

# Initialize a project
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

# Git history intelligence (V2b): churn, fix ratio, co-change, risk score
codesage git-index                                          # initial populate (auto mode); hooks auto-refresh
codesage git-index --full                                   # force full rescan (weekly hygiene)
codesage coupling src/auth/session.ts --limit 5             # files that historically change with this
codesage risk src/auth/session.ts                           # score + decomposition

# MCP server for Claude Code / Codex / Cursor (global, serves every onboarded project)
claude mcp add --scope user codesage -- codesage mcp

# Auto-reindex on git operations
codesage install-hooks

# Diagnose installation
codesage doctor
```

## Claude Code plugin

`plugins/codesage-tools/` is a Claude Code plugin that wraps the above into one command per task. Marketplace manifest at the repo root.

```bash
claude plugin marketplace add /path/to/codesage
claude plugin install codesage-tools@codesage
/codesage-onboard /path/to/project
```

Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`. The plugin handles global MCP registration, per-project init, indexing, git hook install (Husky-aware), and writes a `.claude/CLAUDE.md` hint teaching the agent how to route MCP calls.

## Search pipeline

Retrieval combines multiple signals:

1. **Embedding search** -- MiniLM-L6-v2 (22M params, 384d) via ONNX Runtime with CUDA
2. **Structural augmentation** -- file path + symbol context prepended to chunks before embedding
3. **Symbol boost** -- query tokens matching known symbols get a relevance boost
4. **Cross-encoder reranking** -- ms-marco-MiniLM-L6-v2 re-scores top candidates, blended 50/50 with semantic score
5. **Symbol annotation** -- results annotated with overlapping function/class names

The reranker is configurable per-project. Without it, embedding search + symbol boost still runs.

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

Models download automatically from HuggingFace on first use.

## Architecture

Rust workspace with 6 crates:

| Crate | Role |
|-------|------|
| `protocol` | Shared types (Symbol, Reference, SearchResult) |
| `parser` | File discovery, tree-sitter parsing, symbol/reference extraction |
| `storage` | SQLite + sqlite-vec KNN + FTS5 |
| `embed` | ONNX embedding inference + cross-encoder reranking + text chunking |
| `graph` | Indexing orchestration + search pipeline |
| `cli` | Binary with CLI subcommands + MCP server |

Storage: single SQLite database per project (`.codesage/index.db`) with structural tables (symbols, refs, files) and model-specific vector tables for embeddings.

## Retrieval benchmarks

Benchmark runner and eval-case extractor live under `bench/`:

- `bench/codesage-bench-runner` — runs a YAML corpus of ground-truth cases through `codesage search` and reports miss rate, median first-hit, and recall at 5/10.
- `bench/extract-eval-cases.py` — mines eval cases from Claude Code session transcripts and git commit history; emits a corpus YAML.

Bring your own corpus; results are not bundled.

## License

MIT
