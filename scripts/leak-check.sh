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

set -eu

mode="staged"
range=""

while [ $# -gt 0 ]; do
    case "$1" in
        --range)
            shift
            [ $# -gt 0 ] || { echo "leak-check: --range needs an argument" >&2; exit 2; }
            range="$1"
            mode="range"
            ;;
        --all)
            mode="all"
            ;;
        -h|--help)
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
    sed -E 's/[[:space:]]*#.*$//; s/^[[:space:]]+//; s/[[:space:]]+$//' "$file" \
        | grep -v '^$' || true
}

patterns="$(
    { collect_patterns "$shared_patterns"; collect_patterns "$local_patterns"; } \
        | paste -sd '|' -
)"

if [ -z "$patterns" ]; then
    exit 0
fi

# Discover files to scan and the source for their content.
case "$mode" in
    staged)
        files="$(git diff --cached --name-only --diff-filter=AM)"
        content_ref=""   # ":FILE" syntax for staged content
        ;;
    range)
        files="$(git diff --name-only --diff-filter=AM "$range")"
        content_ref="HEAD"   # use working HEAD content for changed files
        ;;
    all)
        files="$(git ls-files)"
        content_ref="HEAD"
        ;;
esac

if [ -z "$files" ]; then
    exit 0
fi

# Resolve the content source per file. In `staged` mode the blob is at `:FILE`;
# in `range` and `all` modes it's at `HEAD:FILE`.
content_source() {
    local file="$1"
    if [ "$mode" = "staged" ]; then
        git show ":$file" 2>/dev/null
    else
        git show "HEAD:$file" 2>/dev/null
    fi
}

# Detect binary additions in staged mode via numstat. In other modes, fall back
# to a heuristic: if the file contains a NUL byte in the first 8KB, treat as binary.
is_binary() {
    local file="$1"
    if [ "$mode" = "staged" ]; then
        local added
        added="$(git diff --cached --numstat -- "$file" | awk 'NR==1{print $1}')"
        [ "$added" = "-" ]
    else
        content_source "$file" | head -c 8192 | grep -q $'\x00'
    fi
}

found=0
while IFS= read -r file; do
    [ -z "$file" ] && continue

    if echo "$file" | grep -qE -- "$FILENAME_ALLOW_RE"; then
        : # explicitly allowed, fall through to content scan
    elif echo "$file" | grep -qE -- "$FILENAME_BLOCK_RE"; then
        echo "leak-check: $file is denied by filename policy (secret/credential pattern)" >&2
        found=1
        continue
    fi

    if is_binary "$file"; then
        continue
    fi

    matches="$(content_source "$file" | grep -nE -e "$patterns" || true)"
    if [ -n "$matches" ]; then
        echo "leak-check: $file contains a forbidden pattern:" >&2
        printf '%s\n' "$matches" | head -5 | sed "s|^|  $file:|" >&2
        found=1
    fi
done <<EOF
$files
EOF

if [ "$found" -eq 1 ]; then
    echo >&2
    case "$mode" in
        staged)
            echo "leak-check: commit blocked. Options:" >&2
            echo "  1. Remove the flagged content from the staged files." >&2
            echo "  2. Refine the pattern in .git/info/leak-patterns.txt if it's a false positive." >&2
            echo "  3. Bypass with 'git commit --no-verify' (use with intent)." >&2
            ;;
        range|all)
            echo "leak-check: scan failed in $mode mode." >&2
            echo "Either remove the flagged content, or refine the pattern in scripts/leak-patterns.txt." >&2
            ;;
    esac
    exit 1
fi

exit 0
