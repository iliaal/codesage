#!/bin/bash
# Leak check.
#
# Three modes:
#   (no args)         - scan staged content (pre-commit hook use)
#   --range A..B      - scan files changed in the git range A..B (CI use)
#   --all             - scan every tracked file (CI baseline / full audit)
#
# Patterns come from:
#   - scripts/leak-patterns.txt (tracked, shared)
#   - .git/info/leak-patterns.txt (local-only, per-developer)
#
# Exits non-zero on the first match (stops the commit / fails the CI job).
# Bypass deliberately with: git commit --no-verify

set -euo pipefail

mode="staged"
range=""
range_tip=""

while [ $# -gt 0 ]; do
	case "$1" in
	--range)
		shift
		[ $# -gt 0 ] || {
			echo "leak-check: --range needs an argument" >&2
			exit 2
		}
		range="$1"
		mode="range"
		;;
	--all)
		mode="all"
		;;
	-h | --help)
		sed -n '2,12p' "$0" >&2
		exit 0
		;;
	*)
		echo "leak-check: unknown option: $1" >&2
		exit 2
		;;
	esac
	shift
done

repo_root="$(git rev-parse --show-toplevel)"
git_dir="$(git rev-parse --git-dir)"
case "$git_dir" in
/*) ;;
*) git_dir="$repo_root/$git_dir" ;;
esac

shared_patterns="$repo_root/scripts/leak-patterns.txt"
local_patterns="$git_dir/info/leak-patterns.txt"

# Filenames that should never be committed regardless of their content.
# Allowlist takes precedence so templates (.env.example etc.) stay committable.
FILENAME_BLOCK_RE='(^|/)\.env$|(^|/)\.env\..+|(^|/)\.secret$|(^|/)\.secrets$|(^|/)\.secrets/|\.pem$|\.p12$|\.pfx$|(^|/)id_(rsa|dsa|ecdsa|ed25519)$|(^|/)id_(rsa|dsa|ecdsa|ed25519)\.|(^|/)credentials\.json$|(^|/)service-account.*\.json$'
FILENAME_ALLOW_RE='(^|/)\.env\.(example|template|sample)$|(^|/)id_(rsa|dsa|ecdsa|ed25519)\.pub$'

collect_patterns() {
	local file="$1"
	[ -f "$file" ] || return 0
	sed -E 's/[[:space:]]*#.*$//; s/^[[:space:]]+//; s/[[:space:]]+$//' "$file" |
		grep -v '^$' || true
}

patterns="$(
	{
		collect_patterns "$shared_patterns"
		collect_patterns "$local_patterns"
	} |
		paste -sd '|' -
)"

if [ -z "$patterns" ]; then
	exit 0
fi

set +e
regex_error="$(grep -E -e "$patterns" </dev/null 2>&1 >/dev/null)"
regex_status=$?
set -e
if [ "$regex_status" -gt 1 ]; then
	echo "leak-check: invalid forbidden pattern regex in $shared_patterns or $local_patterns:" >&2
	printf '%s\n' "$regex_error" | sed 's/^/  /' >&2
	exit 2
fi

# Resolve the content ref per mode and validate the range. `content_ref` is
# empty in staged mode (blob lives at ":FILE"), the range endpoint in range
# mode, and HEAD in all mode.
case "$mode" in
staged)
	content_ref="" # ":FILE" syntax for staged content
	;;
range)
	range_tip="${range##*..}"
	if [ -z "$range_tip" ] || [ "$range_tip" = "$range" ]; then
		echo "leak-check: --range must be A..B or A...B (got: $range)" >&2
		exit 2
	fi
	content_ref="$(git rev-parse --verify "${range_tip}^{commit}")"
	;;
all)
	content_ref="HEAD"
	;;
esac

# Emit the NUL-delimited file list for the active mode. core.quotepath=false +
# -z keeps non-ASCII paths verbatim; the default C-quoting would rename them
# (e.g. "p\303\242th.txt") so the later `git show ":$file"` would miss and the
# file would be scanned as empty — a silent secret-scan bypass.
list_files() {
	case "$mode" in
	staged)
		git -c core.quotepath=false diff -z --cached --name-only --diff-filter=AM
		;;
	range)
		git -c core.quotepath=false diff -z --name-only --diff-filter=AM "$range"
		;;
	all)
		git -c core.quotepath=false ls-files -z
		;;
	esac
}

# Materialize one file's blob into $content_file. In `staged` mode the blob is
# at `:FILE`; otherwise it's at `$content_ref:FILE`. Returns git show's exit
# status so the caller can fail loudly rather than treat an unreadable blob as
# empty.
read_content() {
	local file="$1"
	if [ "$mode" = "staged" ]; then
		git show ":$file" >"$content_file" 2>/dev/null
	else
		git show "$content_ref:$file" >"$content_file" 2>/dev/null
	fi
}

# Detect binary additions in staged mode via numstat. In other modes, let GNU
# grep classify the already-materialized blob as text or binary.
is_binary() {
	local file="$1"
	if [ "$mode" = "staged" ]; then
		local added
		added="$(git diff --cached --numstat -- "$file" | awk 'NR==1{print $1}')"
		[ "$added" = "-" ]
	else
		if grep -Iq . "$content_file"; then
			return 1
		fi
		return 0
	fi
}

content_file="$(mktemp)"
trap 'rm -f "$content_file"' EXIT

found=0
while IFS= read -r -d '' file; do
	[ -z "$file" ] && continue

	if printf '%s\n' "$file" | grep -qE -- "$FILENAME_ALLOW_RE"; then
		: # explicitly allowed, fall through to content scan
	elif printf '%s\n' "$file" | grep -qE -- "$FILENAME_BLOCK_RE"; then
		echo "leak-check: $file is denied by filename policy (secret/credential pattern)" >&2
		found=1
		continue
	fi

	# Fail loudly if the blob can't be read: silently skipping a listed file
	# would let a secret through unscanned.
	set +e
	read_content "$file"
	show_status=$?
	set -e
	if [ "$show_status" -ne 0 ]; then
		echo "leak-check: failed to read content for $file (git show exit $show_status); refusing to skip it" >&2
		exit 2
	fi

	if is_binary "$file"; then
		continue
	fi

	set +e
	matches="$(grep -nI -E -e "$patterns" "$content_file")"
	grep_status=$?
	set -e
	if [ "$grep_status" -gt 1 ]; then
		echo "leak-check: grep failed while scanning $file" >&2
		exit 2
	fi
	if [ -n "$matches" ]; then
		echo "leak-check: $file contains a forbidden pattern:" >&2
		printf '%s\n' "$matches" | head -5 | sed "s|^|  $file:|" >&2
		found=1
	fi
done < <(list_files)

if [ "$found" -eq 1 ]; then
	echo >&2
	case "$mode" in
	staged)
		echo "leak-check: commit blocked. Options:" >&2
		echo "  1. Remove the flagged content from the staged files." >&2
		echo "  2. Refine the pattern in .git/info/leak-patterns.txt if it's a false positive." >&2
		echo "  3. Bypass with 'git commit --no-verify' (use with intent)." >&2
		;;
	range | all)
		echo "leak-check: scan failed in $mode mode." >&2
		echo "Either remove the flagged content, or refine the pattern in scripts/leak-patterns.txt." >&2
		;;
	esac
	exit 1
fi

exit 0
