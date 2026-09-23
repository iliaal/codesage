#!/usr/bin/env bash
# Local run of the gates CI enforces on push, including the fmt check that
# catches edits made after the last `cargo fmt` (commit a43c51d).
#
# Not installed as a git hook. Exits nonzero on the first failure, so it can
# be chained from a pre-push hook or alias.
#
# Usage:
#   bash scripts/sanity-check.sh          # fmt + clippy + tests + script regressions
#   bash scripts/sanity-check.sh --fast   # validation/lint gates only (skip tests)
#   bash scripts/sanity-check.sh --cuda   # also lint CUDA paths (release gate)

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

FAST=0
CUDA=0
for arg in "$@"; do
	case "$arg" in
	--fast) FAST=1 ;;
	--cuda) CUDA=1 ;;
	-h | --help)
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

step "python3 scripts/check-changelog.py"
python3 scripts/check-changelog.py

step "python3 scripts/check-plugin-versions.py --root ."
python3 scripts/check-plugin-versions.py --root .

step "cargo fmt --all -- --check"
cargo fmt --all -- --check

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

if [[ $CUDA -eq 1 ]]; then
	step "cargo clippy --workspace --all-targets --features codesage/cuda -- -D warnings"
	cargo clippy --workspace --all-targets --features codesage/cuda -- -D warnings
fi

step "shellcheck scripts/*.sh"
shellcheck scripts/*.sh

step "shfmt -d scripts/*.sh"
shfmt -d scripts/*.sh

if [[ $FAST -eq 0 ]]; then
	step "cargo test --workspace"
	cargo test --workspace

	step "bash scripts/regression-tests.sh"
	bash scripts/regression-tests.sh
else
	echo
	echo "skipping tests (--fast); CI will run them"
fi

echo
echo "✓ sanity checks passed"
