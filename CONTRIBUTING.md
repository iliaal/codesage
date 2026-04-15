# Contributing to CodeSage

CodeSage is pre-1.0 and under active development. The bar for contribution is simple: small, focused changes with tests.

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

Always build with `--features cuda` when you want GPU. Without it, a GPU-configured project fails loudly at runtime, which is on purpose: a silent CPU fallback would mix incompatible embeddings into the same index.

## Tests

Every bug fix and every feature needs a test. The workspace has ~120; new code should grow that number. Use integration tests for anything that crosses a crate boundary (parsing plus storage, indexing plus search).

Run `cargo test --workspace` before you send a pull request. Tests that need GPU are gated, so the suite passes on CPU-only machines.

## Conventions

- Rust 2024 edition.
- `anyhow` for error handling in binaries. Domain types live in the `protocol` crate.
- Tree-sitter queries are `.scm` files under `crates/parser/src/queries/`, embedded via `include_str!`.
- All query commands emit JSON with `--json`.
- Write a comment only when the reason isn't obvious from the code. If the next reader would ask "why is this here?", add the comment. Otherwise, let the code speak.

## Commit messages

Imperative mood, under 70 characters in the subject. A body is optional but useful when the *why* isn't obvious from the diff. Conventional prefixes (`feat:`, `fix:`, `docs:`, `test:`, `chore:`) help with later parsing.

## CHANGELOG

Every user-visible change updates `CHANGELOG.md` under `## [Unreleased]` in the same commit. User-visible means: new CLI flags or subcommands, new or changed MCP tools, behavior changes, breaking changes, schema migrations, hook template changes, config surface changes, or security fixes. Pure internal refactors, test-only changes, and doc-only changes don't need an entry.

One bullet per change. Describe what a user can now do, not how you implemented it.

## Pull requests

Keep them focused. One topic per PR. Aim for under ~300 lines of diff; split if it grows. Explain the *why* in the PR description, the *what* in the commits, and the *how* in the code. CI must be green before review.

## License

By contributing, you agree your contributions will be licensed under the MIT license, as described in [LICENSE](LICENSE).
