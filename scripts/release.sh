#!/usr/bin/env bash
# scripts/release.sh [-y|--yes] [--include-approved-prose] X.Y.Z
# Run the shared release workflow's lint/tests first. This script prepares
# metadata and a CUDA build, confirms commit/tag and push separately, then
# refreshes the writable PATH install and daemon. --yes confirms both prompts.
# --include-approved-prose includes approved README.md/CHANGELOG.md edits.
# Both agent plugins must refresh before pushing; a tagged but unpushed release
# can resume after repair.

set -euo pipefail

die() {
	echo "release: $*" >&2
	exit 1
}

ASSUME_YES=0
INCLUDE_APPROVED_PROSE=0
while [[ $# -gt 0 ]]; do
	case "$1" in
	-y | --yes)
		ASSUME_YES=1
		shift
		;;
	--include-approved-prose)
		INCLUDE_APPROVED_PROSE=1
		shift
		;;
	-*) die "unknown flag: $1" ;;
	*) break ;;
	esac
done

VERSION="${1:-}"
[[ -n "$VERSION" ]] || die "usage: scripts/release.sh [-y|--yes] [--include-approved-prose] X.Y.Z"
[[ $# -eq 1 ]] || die "expected exactly one version argument"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z (got: $VERSION)"

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

branch=$(git rev-parse --abbrev-ref HEAD)
[[ "$branch" == "master" ]] || die "not on master (current: $branch)"

if [[ "$INCLUDE_APPROVED_PROSE" -eq 1 ]]; then
	for prose in README.md CHANGELOG.md; do
		[[ -f "$prose" && ! -L "$prose" ]] || die "approved prose must be a regular file: $prose"
		git ls-files --error-unmatch -- "$prose" >/dev/null 2>&1 ||
			die "approved prose must already be tracked: $prose"
	done
	if ! git diff --quiet -- . ':(exclude)README.md' ':(exclude)CHANGELOG.md' ||
		! git diff --cached --quiet -- . ':(exclude)README.md' ':(exclude)CHANGELOG.md'; then
		die "working tree has changes outside approved README.md/CHANGELOG.md prose"
	fi
	prose_worktree_summary=$(git diff --summary -- README.md CHANGELOG.md)
	prose_staged_summary=$(git diff --cached --summary -- README.md CHANGELOG.md)
	[[ -z "$prose_worktree_summary" && -z "$prose_staged_summary" ]] ||
		die "approved prose may change contents only, not file types, paths, or modes"
else
	if ! git diff --quiet || ! git diff --cached --quiet; then
		die "working tree has uncommitted changes"
	fi
fi

command -v codex >/dev/null 2>&1 ||
	die "required 'codex' CLI not found on PATH; install it before releasing"
command -v claude >/dev/null 2>&1 ||
	die "required 'claude' CLI not found on PATH; install it before releasing"

git fetch origin master --quiet
local_sha=$(git rev-parse HEAD)
remote_sha=$(git rev-parse origin/master)

# Resume only when the existing tag is at HEAD and absent from origin.
RESUME=0
if git rev-parse "v$VERSION" >/dev/null 2>&1; then
	tag_sha=$(git rev-parse "v$VERSION^{commit}")
	if [[ "$tag_sha" != "$local_sha" ]]; then
		die "tag v$VERSION already exists and points at $tag_sha, not HEAD"
	fi
	if git ls-remote --exit-code --tags origin "refs/tags/v$VERSION" >/dev/null 2>&1; then
		die "tag v$VERSION already exists on origin"
	fi
	echo "Tag v$VERSION exists at HEAD and is not on origin -- resuming unpushed release."
	RESUME=1
fi

if [[ "$RESUME" -eq 1 ]]; then
	if ! git diff --quiet || ! git diff --cached --quiet; then
		die "cannot resume with uncommitted changes, including approved prose"
	fi
	git merge-base --is-ancestor origin/master HEAD ||
		die "cannot resume: local master and origin/master have diverged"
elif [[ "$local_sha" != "$remote_sha" ]]; then
	die "local master ($local_sha) differs from origin/master ($remote_sha)"
fi

# Validate before mutation so malformed entries cannot become release notes.
python3 "$ROOT/scripts/check-changelog.py" "$ROOT/CHANGELOG.md" ||
	die "CHANGELOG [Unreleased] failed validation (see above)"
python3 "$ROOT/scripts/check-plugin-versions.py" --root "$ROOT" ||
	die "plugin versions don't match the current workspace version"

if [[ "$RESUME" -eq 0 ]]; then
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
	if [[ "$INCLUDE_APPROVED_PROSE" -eq 1 ]]; then
		echo
		echo "--- Approved README.md diff ---"
		git --no-pager diff HEAD -- README.md
	fi
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
		# Treat EOF as rejection so set -e cannot bypass the explicit abort message.
		read -r -p "Proceed? [y/N] " ans || ans=""
	fi
	[[ "$ans" == "y" || "$ans" == "Y" ]] || die "aborted before commit"

	# Explicit paths exclude stray edits while including the CUDA lockfile refresh.
	RELEASE_FILES=(
		CHANGELOG.md
		Cargo.toml
		Cargo.lock
		plugins/codesage-tools/.codex-plugin/plugin.json
		plugins/codesage-tools/.claude-plugin/plugin.json
		.claude-plugin/marketplace.json
	)
	if [[ "$INCLUDE_APPROVED_PROSE" -eq 1 ]]; then
		RELEASE_FILES+=(README.md)
	fi
	EXISTING_FILES=()
	for f in "${RELEASE_FILES[@]}"; do
		[[ -e "$f" ]] && EXISTING_FILES+=("$f")
	done
	git add -- "${EXISTING_FILES[@]}"
	git commit -m "release: v$VERSION" -- "${EXISTING_FILES[@]}"
	git tag -a "v$VERSION" -m "codesage $VERSION"

	echo
	echo "Commit + tag created:"
	git --no-pager log -1 --oneline
	git --no-pager tag -v "v$VERSION" 2>/dev/null | head -5 || git --no-pager show "v$VERSION" --no-patch --oneline
fi

echo
if [[ "$ASSUME_YES" -eq 1 ]]; then
	echo "Push master + v$VERSION to origin? [y/N] y  (--yes)"
	ans=y
else
	read -r -p "Push master + v$VERSION to origin? [y/N] " ans || ans=""
fi
if [[ "$ans" == "y" || "$ans" == "Y" ]]; then
	echo
	echo "Refreshing Codex plugin codesage-tools@codesage ..."
	codex plugin marketplace add "${ROOT}" ||
		die "Codex marketplace refresh failed; release not pushed. Repair it, then rerun scripts/release.sh --yes ${VERSION}."
	codex plugin add codesage-tools@codesage ||
		die "Codex plugin refresh failed; release not pushed. Repair it, then rerun scripts/release.sh --yes ${VERSION}."
	echo "Codex plugin refreshed. Start a new Codex thread to load version ${VERSION}."

	echo
	echo "Refreshing Claude Code plugin codesage-tools@codesage ..."
	claude plugin update codesage-tools@codesage ||
		die "Claude Code plugin refresh failed; release not pushed. Repair it, then rerun scripts/release.sh --yes ${VERSION}."
	echo "Claude plugin refreshed. Restart Claude Code sessions to load version ${VERSION}."
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

# Replace the pathname without overwriting an inode held by running processes.
local_install="$(command -v codesage 2>/dev/null || true)"
if [[ -n "$local_install" && -w "$local_install" ]]; then
	if [[ -f target/release/codesage ]]; then
		built_version="$(target/release/codesage --version)" || die "could not check the release binary version"
		built_version="${built_version%%$'\n'*}"
		[[ "${built_version}" == "codesage ${VERSION} (release)" ]] ||
			die "release binary is '${built_version}', expected 'codesage ${VERSION} (release)'; rebuild before refreshing the local install"
		backup="${local_install}.old-pre-${VERSION}"
		echo
		echo "Refreshing local install at $local_install ..."
		mv "$local_install" "$backup"
		cp target/release/codesage "$local_install"
		installed_version="$("$local_install" --version)" ||
			die "could not check the installed binary version; original install is at ${backup}"
		installed_version="${installed_version%%$'\n'*}"
		[[ "${installed_version}" == "codesage ${VERSION} (release)" ]] ||
			die "installed binary is '${installed_version}', expected 'codesage ${VERSION} (release)'; original install is at ${backup}"
		rm -f "$backup"
		echo "Local install: $installed_version"
		echo "Restarting the shared MCP daemon ..."
		"${local_install}" daemon stop || die "daemon stop failed after local install refresh"
		python3 - "${local_install}" "${VERSION}" <<'PYEOF' || die "daemon verification failed after local install refresh"
import json
import os
import select
import subprocess
import sys
import time

binary, version = sys.argv[1:]

def verify_daemon():
    # The daemon must outlive the release terminal and this verification shim.
    proc = subprocess.Popen(
        [binary, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        start_new_session=True,
    )
    deadline = time.monotonic() + 10
    buffered = b""

    def send(message):
        proc.stdin.write(json.dumps(message).encode() + b"\n")
        proc.stdin.flush()

    def response(request_id):
        nonlocal buffered
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("MCP handshake timed out after 10 seconds")
            if b"\n" not in buffered:
                if not select.select([proc.stdout], [], [], remaining)[0]:
                    raise RuntimeError("MCP handshake timed out after 10 seconds")
                chunk = os.read(proc.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError("MCP shim closed before completing the handshake")
                buffered += chunk
                if len(buffered) > 1048576:
                    raise RuntimeError("MCP handshake response exceeded 1 MiB")
                continue
            line, buffered = buffered.split(b"\n", 1)
            message = json.loads(line)
            if not isinstance(message, dict):
                raise RuntimeError("MCP handshake returned a non-object message")
            if "id" not in message:
                continue
            if message.get("id") != request_id or message.get("jsonrpc") != "2.0":
                raise RuntimeError(f"unexpected MCP response: {message}")
            if "error" in message or not isinstance(message.get("result"), dict):
                raise RuntimeError(f"MCP request failed: {message}")
            return message["result"]

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "codesage-release", "version": version},
        }})
        initialized = response(1)
        if initialized.get("protocolVersion") != "2025-11-25" or not isinstance(initialized.get("capabilities"), dict):
            raise RuntimeError(f"invalid MCP initialize response: {initialized}")
        server = initialized.get("serverInfo", {})
        if not isinstance(server, dict) or server.get("name") != "codesage" or server.get("version") != version:
            raise RuntimeError(f"expected codesage {version} daemon, received {server}")
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "ping"})
        response(2)
    finally:
        proc.stdin.close()
        try:
            status = proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.terminate()
            try:
                status = proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                status = proc.wait()
        proc.stdout.close()
    if status != 0:
        raise RuntimeError(f"MCP verification shim exited with status {status}")

try:
    verify_daemon()
except (OSError, ValueError, RuntimeError) as error:
    sys.exit(f"daemon handshake failed: {error}")
PYEOF
		echo "Daemon restarted. Reconnect existing agent MCP sessions to use the new daemon."
	else
		echo
		echo "No target/release/codesage binary (resumed run?); skipping local install refresh."
		echo "Run: cargo build --release -p codesage --features cuda && cp target/release/codesage $local_install"
	fi
elif [[ -n "$local_install" ]]; then
	echo
	echo "Found $local_install on PATH but it is not writable; skipping local install."
	echo "Run: cp target/release/codesage $local_install"
else
	echo
	echo "No 'codesage' on PATH; skipping local install."
fi
