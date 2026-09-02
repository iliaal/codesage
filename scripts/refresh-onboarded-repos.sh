#!/usr/bin/env bash
# Refresh every onboarded CodeSage repo after a binary upgrade:
#   1. Run `codesage index` (migrations + feature mapping + trust-boundary backfill)
#   2. Re-run `codesage install-hooks` (idempotent; protects against binary path moves)
#   3. Re-run the plugin's onboard script with --refresh-hint to update
#      the per-repo .claude/CLAUDE.md so the agent learns about new tools
#
# Excludes ~/ai/bench-repos/* and ~/ai/codesage_ref/* per the
# `feedback_bench_repos_test_only` rule. Bench fixtures and competitor
# mirrors should not be touched by routine upgrade sweeps.
#
# Usage:
#   bash scripts/refresh-onboarded-repos.sh             # do it
#   bash scripts/refresh-onboarded-repos.sh --dry-run   # list what would run
#   bash scripts/refresh-onboarded-repos.sh --no-index  # skip the index step
#                                                      # (just refresh hints)
#   bash scripts/refresh-onboarded-repos.sh --full --no-semantic
#                                                      # force a full structural
#                                                      # reparse (after a parser
#                                                      # upgrade) without redoing
#                                                      # embeddings

set -euo pipefail

dry_run=0
do_index=1
index_args=()
while [ $# -gt 0 ]; do
	case "$1" in
	--dry-run) dry_run=1 ;;
	--no-index) do_index=0 ;;
	--full) index_args+=(--full) ;;
	--no-semantic) index_args+=(--no-semantic) ;;
	-h | --help)
		sed -n '2,16p' "$0" >&2
		exit 0
		;;
	*)
		echo "unknown flag: $1" >&2
		exit 2
		;;
	esac
	shift
done

repo_root() {
	# echo the project root for a .codesage dir
	dirname "$1"
}

is_excluded() {
	case "$1" in
	*/bench-repos/* | */codesage_ref/* | */.codesage/*) return 0 ;;
	*) return 1 ;;
	esac
}

codesage_bin="$(command -v codesage || true)"
if [ -z "$codesage_bin" ]; then
	echo "error: 'codesage' not in PATH. Install the 0.7.0 binary first." >&2
	exit 1
fi

onboard_bin="$(dirname "$(readlink -f "$0")")/../plugins/codesage-tools/bin/codesage-onboard"
if [ ! -x "$onboard_bin" ]; then
	echo "error: $onboard_bin not executable; refresh-hint step will be skipped" >&2
	onboard_bin=""
fi

# Discover onboarded repos. `find` is bounded to common roots to avoid a
# whole-filesystem scan; extend the list as needed.
# Build the find-root list, dropping any that don't exist so find won't
# error out on missing dirs (e.g. ~/projects/ on a machine that doesn't
# use that convention).
search_roots=()
for r in "$HOME/ai" "$HOME/cred" "$HOME/php-src" "$HOME"/php-src-* "$HOME/projects"; do
	[ -d "$r" ] && search_roots+=("$r")
done
if [ "${#search_roots[@]}" -eq 0 ]; then
	candidates=()
else
	mapfile -t candidates < <(
		find "${search_roots[@]}" -maxdepth 4 -name ".codesage" -type d 2>/dev/null |
			sort -u
	)
fi

active=()
skipped=()
for c in "${candidates[@]:-}"; do
	[ -z "$c" ] && continue
	root="$(repo_root "$c")"
	if is_excluded "$root"; then
		skipped+=("$root")
	else
		active+=("$root")
	fi
done

echo "==> codesage binary: $codesage_bin"
echo "==> onboard helper:  ${onboard_bin:-<missing>}"
echo "==> active repos: ${#active[@]}"
for r in "${active[@]:-}"; do echo "    + $r"; done
if [ "${#skipped[@]}" -gt 0 ]; then
	echo "==> skipped (bench/study mirrors): ${#skipped[@]}"
	for r in "${skipped[@]}"; do echo "    - $r"; done
fi
echo

if [ "$dry_run" -eq 1 ]; then
	echo "(--dry-run, exiting)"
	exit 0
fi

# `codesage index` exits 75 (EX_TEMPFAIL) when another indexer held the
# project lock for the whole wait: nothing was indexed and nothing broke.
# That repo is reported for a later retry, and its hook and hint refresh
# still run — they do not need the index.
EXIT_LOCK_HELD=75

failures=()
retry_later=()
for root in "${active[@]:-}"; do
	[ -z "$root" ] && continue
	echo "--- $root ---"

	if [ "$do_index" -eq 1 ]; then
		echo "    [1/3] codesage index ${index_args[*]:-}"
		rc=0
		(cd "$root" && "$codesage_bin" index ${index_args[@]+"${index_args[@]}"} 2>&1 | sed 's/^/        /') || rc=$?
		if [ "$rc" -eq "$EXIT_LOCK_HELD" ]; then
			retry_later+=("$root (index: lock held, retry later)")
			echo "        LOCK HELD (exit $rc): retry later"
		elif [ "$rc" -ne 0 ]; then
			failures+=("$root (index, exit $rc)")
			echo "        FAILED (exit $rc)"
			continue
		fi
	else
		echo "    [1/3] (skipped per --no-index)"
	fi

	if [ -d "$root/.git" ]; then
		echo "    [2/3] codesage install-hooks"
		if ! (cd "$root" && "$codesage_bin" install-hooks 2>&1 | sed 's/^/        /'); then
			failures+=("$root (install-hooks)")
			echo "        FAILED"
		fi
	else
		echo "    [2/3] (not a git repo, skipping hooks)"
	fi

	if [ -n "$onboard_bin" ]; then
		echo "    [3/3] refresh hint"
		# --no-mcp / --no-hooks: those steps were just done by `index` and
		# `install-hooks` above; skip the duplicate. Pass --no-mcp at the
		# global level only if it would otherwise re-register; the onboard
		# script is idempotent so this is safe either way. We keep --no-hooks
		# to avoid re-running install-hooks twice.
		if ! "$onboard_bin" --refresh-hint --no-mcp --no-hooks "$root" 2>&1 | sed 's/^/        /'; then
			failures+=("$root (hint)")
			echo "        FAILED"
		fi
	fi
done

echo
if [ "${#retry_later[@]}" -gt 0 ]; then
	echo "==> retry later (another indexer held the lock; nothing was indexed there):"
	for f in "${retry_later[@]}"; do echo "    - $f"; done
fi
if [ "${#failures[@]}" -eq 0 ]; then
	echo "==> done. ${#active[@]} repo(s) refreshed."
	[ "${#retry_later[@]}" -eq 0 ] || exit "$EXIT_LOCK_HELD"
else
	echo "==> done with failures:"
	for f in "${failures[@]}"; do echo "    - $f"; done
	exit 1
fi
