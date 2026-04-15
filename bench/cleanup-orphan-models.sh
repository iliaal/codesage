#!/bin/bash
# cleanup-orphan-models.sh <repo-path> [--dry-run]
#
# Wrapper around `codesage cleanup`. Drops orphaned model-specific vec0
# tables from a repo's CodeSage index, keeping only the model referenced
# in .codesage/config.toml. Runs VACUUM to reclaim disk space.
#
# Delegates to the codesage binary because dropping vec0 tables requires
# sqlite-vec to be loaded (the sqlite3 CLI doesn't have it).

set -euo pipefail

usage() {
    cat >&2 <<EOF
usage: cleanup-orphan-models.sh <repo-path> [--dry-run] [--codesage-bin PATH]

Keeps only the vec0 table matching the active model in .codesage/config.toml.
Drops all other chunks_<model>_<dim> tables. Structural tables and FTS5 are
preserved.
EOF
    exit 2
}

repo=""
args=()
codesage_bin="${CODESAGE_BIN:-codesage}"

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) args+=("--dry-run") ;;
        --codesage-bin) shift; codesage_bin="$1" ;;
        -h|--help) usage ;;
        *) [ -z "$repo" ] && repo="$1" || usage ;;
    esac
    shift
done
[ -z "$repo" ] && usage

if [ ! -d "$repo/.codesage" ]; then
    echo "error: no .codesage/ at $repo" >&2
    exit 1
fi

echo "==> $repo"
( cd "$repo" && "$codesage_bin" cleanup "${args[@]}" )
