#!/usr/bin/env bash
# Release ceremony for codesage.
#
#   scripts/release.sh [-y|--yes] X.Y.Z
#
# Does:
#   1. Pre-flight checks (on master, clean tree, in sync with origin, tag free)
#   2. Move `## [Unreleased]` content into a new `## [X.Y.Z] - YYYY-MM-DD` block
#      and append the matching link reference.
#   3. Bump `[workspace.package].version`, both plugin manifests, and the
#      Claude marketplace version.
#   4. Build the release binary with `--features cuda` so Cargo.lock is up to date.
#   5. Prompt, then commit + tag.
#   6. Prompt, then refresh the local Codex and Claude Code plugins and
#      push master + tag.
#   7. Refresh whichever `codesage` is on PATH so the maintainer's local install
#      jumps to the new version. Skipped silently if no install is found or the
#      binary path is not writable.
#
# The two prompts are deliberate: every hard-to-reverse step stops and asks.
# Pass `-y` / `--yes` to auto-confirm both prompts when driving the script from
# a non-interactive context (e.g. an agent that has already run the lint/tests
# gate via `.claude/commands/release.md`).
# Pre-release lint/tests are the wrapper's job (see `.claude/commands/release.md`).

set -euo pipefail

die() {
	echo "release: $*" >&2
	exit 1
}

ASSUME_YES=0
while [[ $# -gt 0 ]]; do
	case "$1" in
	-y | --yes)
		ASSUME_YES=1
		shift
		;;
	-*) die "unknown flag: $1" ;;
	*) break ;;
	esac
done

VERSION="${1:-}"
[[ -n "$VERSION" ]] || die "usage: scripts/release.sh [-y|--yes] X.Y.Z"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z (got: $VERSION)"

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

branch=$(git rev-parse --abbrev-ref HEAD)
[[ "$branch" == "master" ]] || die "not on master (current: $branch)"

git diff-index --quiet HEAD -- || die "working tree has uncommitted changes"

git fetch origin master --quiet
local_sha=$(git rev-parse HEAD)
remote_sha=$(git rev-parse origin/master)
if [[ "$local_sha" != "$remote_sha" ]]; then
	die "local master ($local_sha) differs from origin/master ($remote_sha)"
fi

if git rev-parse "v$VERSION" >/dev/null 2>&1; then
	die "tag v$VERSION already exists"
fi

# CHANGELOG section ordering / validity gate (shared iliaal/* convention).
# Runs before any mutation so a malformed [Unreleased] block stops the release
# cleanly rather than getting stamped into a version section.
python3 "$ROOT/scripts/check-changelog.py" "$ROOT/CHANGELOG.md" ||
	die "CHANGELOG [Unreleased] failed validation (see above)"
python3 "$ROOT/scripts/check-plugin-versions.py" --root "$ROOT" ||
	die "plugin versions don't match the current workspace version"

current=$(awk -F'"' '/^\[workspace\.package\]/{f=1;next} f && /^version *=/{print $2; exit}' Cargo.toml)
[[ -n "$current" ]] || die "could not read current version from Cargo.toml"
echo "Current version: $current"
echo "Target version:  $VERSION"

DATE=$(date +%Y-%m-%d)

python3 - "$VERSION" "$DATE" <<'PYEOF'
import json, pathlib, re, sys

version, date = sys.argv[1], sys.argv[2]

cl = pathlib.Path("CHANGELOG.md")
text = cl.read_text()

m = re.search(r'## \[Unreleased\]\n(.*?)(?=\n## \[)', text, flags=re.DOTALL)
if not m:
    sys.exit("release: could not locate [Unreleased] block in CHANGELOG.md")
body = m.group(1).strip()
if not body:
    sys.exit("release: [Unreleased] is empty -- add changelog entries before releasing")

new_block = (
    f"## [Unreleased]\n\n"
    f"## [{version}] - {date}\n\n"
    f"{body}\n"
)
text = text.replace(m.group(0), new_block, 1)
text = re.sub(r'\n?\[Unreleased\]: [^\n]*\n?', '\n', text)
text = re.sub(rf'\n?\[{re.escape(version)}\]: [^\n]*\n?', '\n', text)
text = text.rstrip() + (
    f"\n\n[Unreleased]: https://github.com/iliaal/codesage/compare/v{version}...HEAD\n"
    f"[{version}]: https://github.com/iliaal/codesage/releases/tag/v{version}\n"
)
cl.write_text(text)

ct = pathlib.Path("Cargo.toml")
ctext = ct.read_text()
pattern = re.compile(
    r'(\[workspace\.package\][^\[]*?\nversion\s*=\s*")[^"]+(")',
    flags=re.DOTALL,
)
new_ctext, n = pattern.subn(rf'\g<1>{version}\g<2>', ctext, count=1)
if n != 1:
    sys.exit("release: failed to bump [workspace.package].version in Cargo.toml")
ct.write_text(new_ctext)

def load_json(path, label):
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        sys.exit(f"release: could not read {label}: {exc}")

def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")

for path, label in (
    (pathlib.Path("plugins/codesage-tools/.codex-plugin/plugin.json"), "Codex plugin manifest"),
    (pathlib.Path("plugins/codesage-tools/.claude-plugin/plugin.json"), "Claude plugin manifest"),
):
    plugin_data = load_json(path, label)
    if plugin_data.get("name") != "codesage-tools":
        sys.exit(f"release: {label} has unexpected name")
    plugin_data["version"] = version
    write_json(path, plugin_data)

marketplace_path = pathlib.Path(".claude-plugin/marketplace.json")
marketplace = load_json(marketplace_path, "Claude marketplace manifest")
marketplace.setdefault("metadata", {})["version"] = version
entries = [
    plugin
    for plugin in marketplace.get("plugins", [])
    if plugin.get("name") == "codesage-tools"
]
if len(entries) != 1:
    sys.exit(
        f"release: Claude marketplace has {len(entries)} codesage-tools entries; expected 1"
    )
entries[0]["version"] = version
write_json(marketplace_path, marketplace)
PYEOF

python3 "$ROOT/scripts/check-plugin-versions.py" --root "$ROOT" ||
	die "release mutation left plugin versions inconsistent"

echo
echo "--- Cargo.toml diff ---"
git --no-pager diff Cargo.toml
echo
echo "--- CHANGELOG.md diff (head) ---"
git --no-pager diff CHANGELOG.md | head -80
echo
echo "--- Plugin manifest diffs ---"
git --no-pager diff \
	plugins/codesage-tools/.codex-plugin/plugin.json \
	plugins/codesage-tools/.claude-plugin/plugin.json \
	.claude-plugin/marketplace.json
echo

echo "Building release binary with --features cuda (refreshes Cargo.lock)..."
cargo build --release -p codesage --features cuda

echo
echo "Ready to commit + tag:"
echo "  commit message: release: v$VERSION"
echo "  tag:            v$VERSION  (annotated: 'codesage $VERSION')"
if [[ "$ASSUME_YES" -eq 1 ]]; then
	echo "Proceed? [y/N] y  (--yes)"
	ans=y
else
	read -r -p "Proceed? [y/N] " ans
fi
[[ "$ans" == "y" || "$ans" == "Y" ]] || die "aborted before commit"

git commit -am "release: v$VERSION"
git tag -a "v$VERSION" -m "codesage $VERSION"

echo
echo "Commit + tag created:"
git --no-pager log -1 --oneline
git --no-pager tag -v "v$VERSION" 2>/dev/null | head -5 || git --no-pager show "v$VERSION" --no-patch --oneline

echo
if [[ "$ASSUME_YES" -eq 1 ]]; then
	echo "Push master + v$VERSION to origin? [y/N] y  (--yes)"
	ans=y
else
	read -r -p "Push master + v$VERSION to origin? [y/N] " ans
fi
if [[ "$ans" == "y" || "$ans" == "Y" ]]; then
	if command -v codex >/dev/null 2>&1; then
		echo
		echo "Refreshing Codex plugin codesage-tools@codesage ..."
		codex plugin marketplace add "${ROOT}" || die "failed to register the local CodeSage marketplace"
		codex plugin add codesage-tools@codesage || die "failed to refresh the Codex plugin"
		echo "Codex plugin refreshed. Start a new Codex thread to load version ${VERSION}."
	else
		echo
		echo "No 'codex' on PATH; skipping Codex plugin refresh."
	fi
	if command -v claude >/dev/null 2>&1; then
		echo
		echo "Refreshing Claude Code plugin codesage-tools@codesage ..."
		claude plugin update codesage-tools@codesage || die "failed to refresh the Claude Code plugin"
		echo "Claude plugin refreshed. Restart Claude Code sessions to load version ${VERSION}."
	else
		echo
		echo "No 'claude' on PATH; skipping Claude plugin refresh."
	fi
	git push origin master
	git push origin "v$VERSION"
	echo
	echo "Pushed. The Release workflow will extract [${VERSION}] from CHANGELOG.md"
	echo "and create the GitHub Release."
	echo "  https://github.com/iliaal/codesage/releases/tag/v$VERSION"
else
	echo "Skipped push. Run manually when ready:"
	echo "  git push origin master && git push origin v$VERSION"
fi

# Refresh the local install if there's already a `codesage` on PATH.
# The mv-then-cp dance avoids "Text file busy" when an MCP server (or any
# other long-running `codesage` process) is holding the old binary's inode:
# the running process keeps the old inode alive until exit, the new binary
# lands at the original path, and the next session picks it up.
local_install="$(command -v codesage 2>/dev/null || true)"
if [[ -n "$local_install" && -w "$local_install" ]]; then
	backup="${local_install}.old-pre-${VERSION}"
	echo
	echo "Refreshing local install at $local_install ..."
	mv "$local_install" "$backup"
	cp target/release/codesage "$local_install"
	rm -f "$backup"
	installed_version="$("$local_install" --version 2>/dev/null || echo '?')"
	echo "Local install: $installed_version"
elif [[ -n "$local_install" ]]; then
	echo
	echo "Found $local_install on PATH but it is not writable; skipping local install."
	echo "Run: cp target/release/codesage $local_install"
else
	echo
	echo "No 'codesage' on PATH; skipping local install."
fi
