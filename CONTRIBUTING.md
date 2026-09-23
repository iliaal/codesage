# Contributing to CodeSage

CodeSage is pre-1.0 and under active development. Send small, focused changes with tests.

## Reporting a bug

File an issue with:

- The exact command or MCP tool call you ran.
- What you expected.
- What happened instead. Paste `codesage doctor` output if the problem looks environmental.
- Your OS, GPU model if you run with CUDA, and `codesage --version`.

A minimal reproducer (a small test project that triggers the issue) saves a lot of back-and-forth.

## Proposing a feature

Open an issue tagged `proposal` and describe the user-facing problem before writing code. A new MCP tool needs a plausible agent use case. A new CLI flag needs a command shape that fits the existing pattern.

## Building

```bash
cargo build                                         # all crates, debug
cargo build --release -p codesage --features cuda   # release binary with GPU
cargo test --workspace                              # full test suite
cargo clippy --workspace                            # lint
```

Build with `--features cuda` when you want GPU. Without it, a GPU-configured project fails at runtime on purpose: a silent CPU fallback would mix incompatible embeddings into the same index.

## Tests

Every bug fix and feature needs a test. Use integration tests for anything that crosses a crate boundary (parsing plus storage, indexing plus search).

Run `cargo test --workspace` before you send a pull request. Tests that need GPU are gated, so the suite passes on CPU-only machines.

## Conventions

- Rust 2024 edition.
- `anyhow` for error handling workspace-wide. Shared domain types live in the `protocol` crate.
- Tree-sitter queries are `.scm` files under `crates/parser/src/queries/`, embedded via `include_str!`.
- All query commands emit JSON with `--json`.
- Write a comment only when the reason isn't obvious from the code, that is, when the next reader would ask "why is this here?".

## Commit messages

Use the imperative mood and keep the subject under ~100 characters (~72 preferred for `git log --oneline`). The limit is 100 rather than 70 so conventional `type(scope):` prefixes fit. Add a body when the *why* isn't obvious from the diff. Conventional prefixes (`feat:`, `fix:`, `docs:`, `test:`, `chore:`) help with later parsing.

## CHANGELOG

Every user-visible change updates `CHANGELOG.md` under `## [Unreleased]` in the same commit. User-visible means: new CLI flags or subcommands, new or changed MCP tools, behavior changes, breaking changes, schema migrations, hook template changes, config surface changes, or security fixes. Pure internal refactors, test-only changes, and doc-only changes don't need an entry. The CI `check-changelog` job validates the `## [Unreleased]` block on PRs. There is no local hook for this, since hooks are bypassable and slow down every commit.

One bullet per change. Describe what a user can now do, not how you implemented it.

## Pull requests

Keep each PR to one topic. Aim for under ~300 lines of diff; split if it grows. Explain the *why* in the PR description, the *what* in the commits, and the *how* in the code. CI must be green before review.

## Keeping private data out of commits

CodeSage is dogfooded against private codebases. Two mechanisms keep that work out of the public repo.

### Pre-commit leak check

`codesage install-hooks --with-leak-check` installs a `pre-commit` hook that runs `scripts/leak-check.sh` over staged content. Plain `codesage install-hooks` does not add it: the hook execs a repo-shipped script, so wiring it automatically would hand a fresh clone of a malicious repo code execution on your next commit. When `scripts/leak-check.sh` is present, plain `install-hooks` prints a notice pointing you at the `--with-leak-check` flag. The script scans against extended-regex patterns in two files:

- `scripts/leak-patterns.txt`: tracked, shared. Generic secret formats (private keys, AWS/GitHub/Slack tokens).
- `.git/info/leak-patterns.txt`: local-only, per-developer. Add your own entries here: private repo names, internal domain terms, absolute home paths. This file never enters git.

The script prints the offending `file:line` and blocks the commit. Bypass with `git commit --no-verify` only when you're sure the match is a false positive, and refine the pattern afterwards.

### External test data via env vars

Any path that points at private test data lives outside the repo and gets injected via environment variable. The existing example is `CODESAGE_BENCH_CORPUS_DIR` (default: `./bench-corpora`); set it to wherever your corpora live. Do not hardcode paths in tests, fixtures, or plugin commands.

Test fixtures under `crates/parser/tests/fixtures/` must be synthetic code, not copied from real repositories. If you need a fixture with a specific shape, write it; don't paste it in.

## License

By contributing, you agree your contributions will be licensed under the MIT license, as described in [LICENSE](LICENSE).
