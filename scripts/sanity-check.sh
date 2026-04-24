#!/usr/bin/env bash
# Local sanity checks — same gates CI will enforce on push. Run before
# pushing when you've made code changes to avoid the "CI caught a diff
# after local fmt then later edit" class of break (see commit a43c51d
# for the incident that motivated this).
#
# Not auto-installed as a git hook; invoke manually. Exits nonzero on
# any failure so you can chain it in a pre-push or wrap it in an alias.
#
# Usage:
#   bash scripts/sanity-check.sh          # fmt + clippy + tests
#   bash scripts/sanity-check.sh --fast   # fmt + clippy only (skip tests)

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

FAST=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1 ;;
        -h|--help)
            sed -n '2,16p' "$0"
            exit 0
            ;;
        *)
            echo "unknown flag: $arg" >&2
            exit 2
            ;;
    esac
done

step() { printf '\n── %s ──\n' "$1"; }

step "cargo fmt --all -- --check"
cargo fmt --all -- --check

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

if [[ $FAST -eq 0 ]]; then
    step "cargo test --workspace"
    cargo test --workspace
else
    echo
    echo "skipping tests (--fast); CI will run them"
fi

echo
echo "✓ sanity checks passed"
