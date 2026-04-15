#!/bin/bash
# Pre-commit leak check.
#
# Scans content staged for commit against extended-regex patterns from:
#   - scripts/leak-patterns.txt (tracked, shared)
#   - .git/info/leak-patterns.txt (local-only, per-developer)
#
# Blocks the commit if any staged line matches any pattern.
# Bypass deliberately with: git commit --no-verify

set -eu

repo_root="$(git rev-parse --show-toplevel)"
git_dir="$(git rev-parse --git-dir)"
case "$git_dir" in
    /*) ;;
    *) git_dir="$repo_root/$git_dir" ;;
esac

shared_patterns="$repo_root/scripts/leak-patterns.txt"
local_patterns="$git_dir/info/leak-patterns.txt"

collect_patterns() {
    local file="$1"
    [ -f "$file" ] || return 0
    # Strip comments (everything from '#' onward) and trim whitespace.
    # Skip blank lines.
    sed -E 's/[[:space:]]*#.*$//; s/^[[:space:]]+//; s/[[:space:]]+$//' "$file" \
        | grep -v '^$' || true
}

# Combine both files into a single pattern alternation for grep -E.
patterns="$(
    { collect_patterns "$shared_patterns"; collect_patterns "$local_patterns"; } \
        | paste -sd '|' -
)"

if [ -z "$patterns" ]; then
    exit 0
fi

staged="$(git diff --cached --name-only --diff-filter=AM)"
if [ -z "$staged" ]; then
    exit 0
fi

found=0
while IFS= read -r file; do
    [ -z "$file" ] && continue
    # Skip binary files: git's numstat reports "-" for binary additions/deletions.
    added="$(git diff --cached --numstat -- "$file" | awk 'NR==1{print $1}')"
    if [ "$added" = "-" ]; then
        continue
    fi
    matches="$(git show ":$file" 2>/dev/null | grep -nE -e "$patterns" || true)"
    if [ -n "$matches" ]; then
        echo "leak-check: $file contains a forbidden pattern:" >&2
        printf '%s\n' "$matches" | head -5 | sed "s|^|  $file:|" >&2
        found=1
    fi
done <<EOF
$staged
EOF

if [ "$found" -eq 1 ]; then
    echo >&2
    echo "leak-check: commit blocked. Options:" >&2
    echo "  1. Remove the flagged content from the staged files." >&2
    echo "  2. Refine the pattern in .git/info/leak-patterns.txt if it's a false positive." >&2
    echo "  3. Bypass with 'git commit --no-verify' (use with intent)." >&2
    exit 1
fi

exit 0
