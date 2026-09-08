#!/usr/bin/env bash
# Regression tests for repository maintenance scripts.

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

test_leak_check_range_uses_range_endpoint() {
	local tmp base leak
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' RETURN

	cd "$tmp"
	git init -q
	git config user.email test@example.com
	git config user.name Test
	mkdir scripts
	cp "$repo_root/scripts/leak-check.sh" scripts/leak-check.sh
	printf 'FORBIDDEN_TOKEN\n' >scripts/leak-patterns.txt
	printf 'clean\n' >sample.txt
	git add scripts/leak-check.sh scripts/leak-patterns.txt sample.txt
	git commit -q -m base
	base="$(git rev-parse HEAD)"

	printf 'FORBIDDEN_TOKEN\n' >sample.txt
	git add sample.txt
	git commit -q -m leak
	leak="$(git rev-parse HEAD)"
	git checkout -q "$base"

	if ./scripts/leak-check.sh --range "$base..$leak" >"$tmp/leak-check-range.out" 2>&1; then
		printf 'expected leak-check --range to fail on endpoint content\n' >&2
		cat "$tmp/leak-check-range.out" >&2
		return 1
	fi
	cd "$repo_root"
}

test_leak_check_invalid_regex_fails_closed() {
	local tmp status
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' RETURN

	cd "$tmp"
	git init -q
	git config user.email test@example.com
	git config user.name Test
	mkdir scripts
	cp "$repo_root/scripts/leak-check.sh" scripts/leak-check.sh
	printf '(\n' >scripts/leak-patterns.txt
	printf 'FORBIDDEN_TOKEN\n' >sample.txt
	git add scripts/leak-check.sh scripts/leak-patterns.txt sample.txt
	git commit -q -m base

	set +e
	./scripts/leak-check.sh --all >"$tmp/leak-check-invalid-regex.out" 2>&1
	status=$?
	set -e

	if [ "$status" -eq 0 ]; then
		printf 'expected leak-check to fail closed on invalid regex\n' >&2
		cat "$tmp/leak-check-invalid-regex.out" >&2
		return 1
	fi
	if ! grep -q 'invalid forbidden pattern regex' "$tmp/leak-check-invalid-regex.out"; then
		printf 'expected invalid regex diagnostic\n' >&2
		cat "$tmp/leak-check-invalid-regex.out" >&2
		return 1
	fi
	if ! grep -Eq '^  .+' "$tmp/leak-check-invalid-regex.out"; then
		printf 'expected grep to explain the invalid regex\n' >&2
		cat "$tmp/leak-check-invalid-regex.out" >&2
		return 1
	fi
	cd "$repo_root"
}

test_release_script_updates_changelog_links() {
	local tmp origin fake_bin missing_codex_bin missing_claude_bin release_script version changelog codex_calls claude_calls codex_version claude_version marketplace_metadata_version marketplace_plugin_version remote_before_failed_refresh remote_after_failed_refresh head_before_missing_cli
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' RETURN
	origin="${tmp}/origin.git"
	fake_bin="${tmp}/bin"
	codex_calls="${tmp}/codex-calls"
	claude_calls="${tmp}/claude-calls"
	release_script="$repo_root/scripts/release.sh"
	version="1.2.3"

	mkdir -p "${fake_bin}"
	printf '#!/usr/bin/env bash\nexit 0\n' >"${fake_bin}/cargo"
	printf '#!/usr/bin/env bash\nprintf "codesage fake\\n"\n' >"${fake_bin}/codesage"
	cat >"${fake_bin}/codex" <<'EOF'
#!/usr/bin/env bash
if [[ "${FAIL_CODEX_REFRESH:-0}" == "1" ]]; then
	printf 'simulated Codex refresh failure\n' >&2
	exit 43
fi
if [[ "$1" == "plugin" ]]; then
	local_head="$(git rev-parse HEAD)"
	remote_head="$(git ls-remote origin refs/heads/master | awk '{print $1}')"
	if [[ "$local_head" == "$remote_head" ]]; then
		printf 'Codex refresh ran after release push\n' >&2
		exit 42
	fi
fi
printf '%s\n' "$*" >>"$CODEX_CALLS_FILE"
EOF
	cat >"${fake_bin}/claude" <<'EOF'
#!/usr/bin/env bash
if [[ "${FAIL_CLAUDE_REFRESH:-0}" == "1" ]]; then
	printf 'simulated Claude refresh failure\n' >&2
	exit 44
fi
if [[ "$1" == "plugin" ]]; then
	local_head="$(git rev-parse HEAD)"
	remote_head="$(git ls-remote origin refs/heads/master | awk '{print $1}')"
	if [[ "$local_head" == "$remote_head" ]]; then
		printf 'Claude refresh ran after release push\n' >&2
		exit 42
	fi
fi
printf '%s\n' "$*" >>"$CLAUDE_CALLS_FILE"
EOF
	chmod +x "${fake_bin}/cargo" "${fake_bin}/codesage" "${fake_bin}/codex" "${fake_bin}/claude"
	chmod a-w "${fake_bin}/codesage"

	git init --bare -q "$origin"
	mkdir "$tmp/work"
	cd "$tmp/work"
	git init -q
	git checkout -q -b master
	git config user.email test@example.com
	git config user.name Test
	git remote add origin "$origin"
	cat >Cargo.toml <<'EOF'
[workspace.package]
version = "1.2.2"
EOF
	cat >CHANGELOG.md <<'EOF'
# Changelog

## [Unreleased]

### Fixed

- Example fix.

## [1.2.2] - 2026-01-01

### Fixed

- Prior fix.

[Unreleased]: https://github.com/iliaal/codesage/compare/v1.2.2...HEAD
[1.2.2]: https://github.com/iliaal/codesage/releases/tag/v1.2.2
EOF
	mkdir -p plugins/codesage-tools/.codex-plugin
	cat >plugins/codesage-tools/.codex-plugin/plugin.json <<'EOF'
{
  "name": "codesage-tools",
  "version": "1.2.2"
}
EOF
	mkdir -p plugins/codesage-tools/.claude-plugin .claude-plugin
	cp plugins/codesage-tools/.codex-plugin/plugin.json plugins/codesage-tools/.claude-plugin/plugin.json
	cat >.claude-plugin/marketplace.json <<'EOF'
{
  "name": "codesage",
  "metadata": {"version": "1.2.2"},
  "plugins": [
    {
      "name": "codesage-tools",
      "version": "1.2.2",
      "source": "./plugins/codesage-tools"
    }
  ]
}
EOF
	# release.sh runs scripts/check-changelog.py as a pre-flight against the
	# repo root it is invoked in; provision (and commit, to keep the tree clean)
	# the lints so the fake repo mirrors a real checkout.
	mkdir -p scripts
	cp "$repo_root/scripts/check-changelog.py" scripts/check-changelog.py
	cp "$repo_root/scripts/check-plugin-versions.py" scripts/check-plugin-versions.py
	printf '# Example\n' >README.md
	git add README.md Cargo.toml CHANGELOG.md .claude-plugin/marketplace.json scripts/check-changelog.py scripts/check-plugin-versions.py plugins/codesage-tools/.codex-plugin/plugin.json plugins/codesage-tools/.claude-plugin/plugin.json
	git commit -q -m initial
	git push -q origin master

	missing_codex_bin="${tmp}/bin-no-codex"
	missing_claude_bin="${tmp}/bin-no-claude"
	mkdir -p "${missing_codex_bin}" "${missing_claude_bin}"
	ln -s "$(command -v git)" "${missing_codex_bin}/git"
	ln -s "$(command -v git)" "${missing_claude_bin}/git"
	ln -s "${fake_bin}/codex" "${missing_claude_bin}/codex"
	head_before_missing_cli="$(git rev-parse HEAD)"
	if PATH="${missing_codex_bin}" /bin/bash "${release_script}" --yes "${version}" >"${tmp}/release-script-missing-codex.out" 2>&1; then
		printf 'release script continued without the Codex CLI\n' >&2
		cat "${tmp}/release-script-missing-codex.out" >&2
		return 1
	fi
	grep -Fq "required 'codex' CLI not found on PATH" "${tmp}/release-script-missing-codex.out"
	[[ "$(git rev-parse HEAD)" == "${head_before_missing_cli}" ]]
	if PATH="${missing_claude_bin}" /bin/bash "${release_script}" --yes "${version}" >"${tmp}/release-script-missing-claude.out" 2>&1; then
		printf 'release script continued without the Claude CLI\n' >&2
		cat "${tmp}/release-script-missing-claude.out" >&2
		return 1
	fi
	grep -Fq "required 'claude' CLI not found on PATH" "${tmp}/release-script-missing-claude.out"
	[[ "$(git rev-parse HEAD)" == "${head_before_missing_cli}" ]]
	if git rev-parse "v${version}" >/dev/null 2>&1; then
		printf 'missing-CLI preflight created tag v%s\n' "${version}" >&2
		return 1
	fi

	printf '\nApproved prose.\n' >>README.md
	if PATH="${fake_bin}:${PATH}" "${release_script}" --yes "${version}" >"${tmp}/prose-unapproved.out" 2>&1; then
		printf 'release accepted dirty prose without its explicit option\n' >&2
		return 1
	fi
	grep -Fq 'working tree has uncommitted changes' "${tmp}/prose-unapproved.out"
	printf '\n# unrelated\n' >>Cargo.toml
	git add Cargo.toml
	git show HEAD:Cargo.toml >Cargo.toml
	if PATH="${fake_bin}:${PATH}" "${release_script}" --include-approved-prose --yes "${version}" >"${tmp}/prose-mixed.out" 2>&1; then
		printf 'release accepted an unrelated staged change canceled in the worktree\n' >&2
		return 1
	fi
	grep -Fq 'changes outside approved' "${tmp}/prose-mixed.out"
	git restore --staged Cargo.toml
	chmod +x README.md
	if PATH="${fake_bin}:${PATH}" "${release_script}" --include-approved-prose --yes "${version}" >"${tmp}/prose-mode.out" 2>&1; then
		printf 'release accepted prose mode change\n' >&2
		return 1
	fi
	grep -Fq 'contents only' "${tmp}/prose-mode.out"
	chmod -x README.md
	mv README.md "${tmp}/approved-readme"
	ln -s "${tmp}/approved-readme" README.md
	if PATH="${fake_bin}:${PATH}" "${release_script}" --include-approved-prose --yes "${version}" >"${tmp}/prose-type.out" 2>&1; then
		printf 'release accepted symlinked prose\n' >&2
		return 1
	fi
	grep -Fq 'must be a regular file' "${tmp}/prose-type.out"
	rm README.md
	if PATH="${fake_bin}:${PATH}" "${release_script}" --include-approved-prose --yes "${version}" >"${tmp}/prose-deleted.out" 2>&1; then
		printf 'release accepted deleted prose\n' >&2
		return 1
	fi
	grep -Fq 'must be a regular file' "${tmp}/prose-deleted.out"
	mv "${tmp}/approved-readme" README.md
	git add README.md
	python3 - <<'PYEOF'
from pathlib import Path
path = Path("CHANGELOG.md")
path.write_text(path.read_text().replace("- Example fix.", "- Approved example fix."))
PYEOF
	CODEX_CALLS_FILE="${codex_calls}" CLAUDE_CALLS_FILE="${claude_calls}" PATH="${fake_bin}:${PATH}" \
		"${release_script}" --include-approved-prose --yes "${version}" >"${tmp}/release-script.out" 2>&1
	[[ "$(git show HEAD:README.md)" == $'# Example\n\nApproved prose.' ]]
	git show HEAD:CHANGELOG.md | grep -Fq -- '- Approved example fix.'
	git diff --quiet
	git diff --cached --quiet

	changelog="$(cat CHANGELOG.md)"
	[[ "$changelog" == *"[Unreleased]: https://github.com/iliaal/codesage/compare/v$version...HEAD"* ]]
	[[ "$changelog" == *"[$version]: https://github.com/iliaal/codesage/releases/tag/v$version"* ]]
	if grep -q 'compare/v1.2.2...HEAD' CHANGELOG.md; then
		printf 'release script left stale Unreleased compare link\n' >&2
		cat CHANGELOG.md >&2
		return 1
	fi
	codex_version="$(git show HEAD:plugins/codesage-tools/.codex-plugin/plugin.json | python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])')"
	claude_version="$(git show HEAD:plugins/codesage-tools/.claude-plugin/plugin.json | python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])')"
	marketplace_metadata_version="$(git show HEAD:.claude-plugin/marketplace.json | python3 -c 'import json, sys; print(json.load(sys.stdin)["metadata"]["version"])')"
	marketplace_plugin_version="$(git show HEAD:.claude-plugin/marketplace.json | python3 -c 'import json, sys; print(json.load(sys.stdin)["plugins"][0]["version"])')"
	if [[ "${codex_version}" != "${version}" || "${claude_version}" != "${version}" || "${marketplace_metadata_version}" != "${version}" || "${marketplace_plugin_version}" != "${version}" ]]; then
		printf 'release commit left plugin versions at codex=%s claude=%s metadata=%s marketplace=%s, expected %s\n' \
			"${codex_version}" "${claude_version}" "${marketplace_metadata_version}" "${marketplace_plugin_version}" "${version}" >&2
		cat "${tmp}/release-script.out" >&2
		return 1
	fi
	[[ "$(grep -Fxc "plugin marketplace add ${tmp}/work" "${codex_calls}")" -eq 1 ]]
	[[ "$(grep -Fxc 'plugin add codesage-tools@codesage' "${codex_calls}")" -eq 1 ]]
	[[ "$(grep -Fxc 'plugin update codesage-tools@codesage' "${claude_calls}")" -eq 1 ]]

	python3 - <<'PYEOF'
from pathlib import Path

path = Path("CHANGELOG.md")
text = path.read_text()
text = text.replace(
    "## [Unreleased]\n\n",
    "## [Unreleased]\n\n### Fixed\n\n- Second example fix.\n\n",
    1,
)
path.write_text(text)
PYEOF
	git add CHANGELOG.md
	git commit -q -m 'prepare declined-push release'
	git push -q origin master
	: >"${codex_calls}"
	: >"${claude_calls}"
	printf 'y\nn\n' | CODEX_CALLS_FILE="${codex_calls}" CLAUDE_CALLS_FILE="${claude_calls}" PATH="${fake_bin}:${PATH}" \
		"${release_script}" 1.2.4 >"${tmp}/release-script-no-push.out" 2>&1
	if [[ -s "${codex_calls}" ]]; then
		printf 'release script refreshed Codex after push was declined\n' >&2
		cat "${codex_calls}" >&2
		return 1
	fi
	if [[ -s "${claude_calls}" ]]; then
		printf 'release script refreshed Claude after push was declined\n' >&2
		cat "${claude_calls}" >&2
		return 1
	fi
	remote_before_failed_refresh="$(git ls-remote origin refs/heads/master | awk '{print $1}')"
	printf '\nToo late for tagged release.\n' >>README.md
	if PATH="${fake_bin}:${PATH}" "${release_script}" --include-approved-prose --yes 1.2.4 >"${tmp}/prose-resume.out" 2>&1; then
		printf 'release resumed an existing tag with uncommitted prose\n' >&2
		return 1
	fi
	grep -Fq 'cannot resume with uncommitted changes' "${tmp}/prose-resume.out"
	git restore README.md
	if FAIL_CODEX_REFRESH=1 CODEX_CALLS_FILE="${codex_calls}" CLAUDE_CALLS_FILE="${claude_calls}" PATH="${fake_bin}:${PATH}" \
		"${release_script}" --yes 1.2.4 >"${tmp}/release-script-refresh-failure.out" 2>&1; then
		printf 'release script continued after a Codex plugin refresh failure\n' >&2
		cat "${tmp}/release-script-refresh-failure.out" >&2
		return 1
	fi
	remote_after_failed_refresh="$(git ls-remote origin refs/heads/master | awk '{print $1}')"
	[[ "${remote_after_failed_refresh}" == "${remote_before_failed_refresh}" ]]
	grep -Fq 'Codex marketplace refresh failed; release not pushed.' "${tmp}/release-script-refresh-failure.out"
	[[ ! -s "${claude_calls}" ]]
	: >"${codex_calls}"
	if FAIL_CLAUDE_REFRESH=1 CODEX_CALLS_FILE="${codex_calls}" CLAUDE_CALLS_FILE="${claude_calls}" PATH="${fake_bin}:${PATH}" \
		"${release_script}" --yes 1.2.4 >"${tmp}/release-script-claude-refresh-failure.out" 2>&1; then
		printf 'release script continued after a Claude plugin refresh failure\n' >&2
		cat "${tmp}/release-script-claude-refresh-failure.out" >&2
		return 1
	fi
	remote_after_failed_refresh="$(git ls-remote origin refs/heads/master | awk '{print $1}')"
	[[ "${remote_after_failed_refresh}" == "${remote_before_failed_refresh}" ]]
	grep -Fq 'Claude Code plugin refresh failed; release not pushed.' "${tmp}/release-script-claude-refresh-failure.out"

	local daemon_calls daemon_failure expected_calls actual_calls daemon_status scenario
	local target_version installed_version mcp_mode expected_error install_before install_after
	daemon_calls="${tmp}/daemon-calls"
	mkdir -p target/release
	cat >target/release/codesage <<'EOF'
#!/usr/bin/env python3
import json
import os
import pathlib
import sys

command = " ".join(sys.argv[1:])
if command == "--version":
    is_target = pathlib.Path(sys.argv[0]).resolve() == pathlib.Path("target/release/codesage").resolve()
    version = os.environ.get("TARGET_VERSION" if is_target else "INSTALLED_VERSION", "1.2.4")
    print(f"codesage {version} (release)\n  target: x86_64-unknown-linux\n  features compiled: cpu, cuda\n  device configured: cpu")
    sys.exit(0)

def log(message):
    with open(os.environ["DAEMON_CALLS_FILE"], "a") as calls:
        print(message, file=calls)

log(f"1.2.4 {sys.argv[0]} {command}")
if command == os.environ.get("FAIL_DAEMON_COMMAND"):
    sys.exit(f"simulated daemon command failure: {command}")
if command == "daemon status":
    print("reachable: no")
    sys.exit(0)
if command == "daemon stop":
    sys.exit(0)
if command != "mcp":
    sys.exit(f"unexpected command: {command}")

mode = os.environ.get("MCP_MODE", "success")
if mode == "empty":
    sys.exit(0)
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    log(f"rpc {method}")
    if method == "initialize":
        assert request["params"]["clientInfo"]["version"] == "1.2.4"
        if mode == "malformed":
            print("not-json", flush=True)
            continue
        result = {
            "protocolVersion": "invalid" if mode == "wrong-protocol" else request["params"]["protocolVersion"],
            "capabilities": {},
            "serverInfo": {"name": "codesage", "version": "1.2.3" if mode == "wrong-version" else "1.2.4"},
        }
    elif method == "notifications/initialized":
        continue
    elif method == "ping":
        if mode == "timeout":
            continue
        result = {}
    else:
        sys.exit(f"unexpected MCP method: {method}")
    response = {"jsonrpc": "2.0", "id": request["id"], "result": result}
    if mode == "protocol-error" or (mode == "ping-error" and method == "ping"):
        response = {"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "simulated failure"}}
    print(json.dumps(response), flush=True)
EOF
	chmod +x target/release/codesage
	chmod u+w "${fake_bin}/codesage"
	for scenario in success stop startup empty malformed protocol-error wrong-version wrong-protocol ping-error timeout stale-target wrong-install; do
		daemon_failure=''
		target_version=1.2.4
		installed_version=1.2.4
		mcp_mode="${scenario}"
		expected_error=''
		case "${scenario}" in
		success) ;;
		stop)
			daemon_failure='daemon stop'
			expected_error='daemon stop failed'
			;;
		startup)
			daemon_failure=mcp
			expected_error='MCP shim closed before completing the handshake'
			;;
		empty)
			expected_error='MCP shim closed before completing the handshake'
			DAEMON_CALLS_FILE="${daemon_calls}" "${fake_bin}/codesage" daemon status >"${tmp}/unreachable-status.out"
			grep -Fxq 'reachable: no' "${tmp}/unreachable-status.out"
			;;
		malformed) expected_error='daemon handshake failed: Expecting value' ;;
		protocol-error | ping-error) expected_error='MCP request failed:' ;;
		wrong-version) expected_error='expected codesage 1.2.4 daemon' ;;
		wrong-protocol) expected_error='invalid MCP initialize response:' ;;
		timeout) expected_error='MCP handshake timed out after 10 seconds' ;;
		stale-target)
			target_version=1.2.3
			expected_error="release binary is 'codesage 1.2.3 (release)', expected 'codesage 1.2.4 (release)'"
			printf '\n# existing-install-%s\n' "${tmp}" >>"${fake_bin}/codesage"
			;;
		wrong-install)
			installed_version=1.2.3
			expected_error="installed binary is 'codesage 1.2.3 (release)', expected 'codesage 1.2.4 (release)'"
			;;
		*)
			printf 'unknown release verification scenario: %s\n' "${scenario}" >&2
			return 1
			;;
		esac
		install_before="$(sha256sum "${fake_bin}/codesage")"
		: >"${daemon_calls}"
		daemon_status=0
		printf 'n\n' | FAIL_DAEMON_COMMAND="${daemon_failure}" DAEMON_CALLS_FILE="${daemon_calls}" \
			TARGET_VERSION="${target_version}" INSTALLED_VERSION="${installed_version}" MCP_MODE="${mcp_mode}" PATH="${fake_bin}:${PATH}" \
			"${release_script}" 1.2.4 >"${tmp}/release-script-daemon.out" 2>&1 || daemon_status=$?
		if [[ "${scenario}" == success ]]; then
			[[ "${daemon_status}" -eq 0 ]]
			grep -Fq 'Daemon restarted.' "${tmp}/release-script-daemon.out"
		else
			if [[ "${daemon_status}" -eq 0 ]]; then
				printf 'release script accepted failed daemon verification: %s\n' "${scenario}" >&2
				cat "${tmp}/release-script-daemon.out" >&2
				return 1
			fi
			grep -Fq "${expected_error}" "${tmp}/release-script-daemon.out"
			if grep -Fq 'Daemon restarted.' "${tmp}/release-script-daemon.out"; then
				printf 'release script claimed a restart after failed verification: %s\n' "${scenario}" >&2
				return 1
			fi
		fi
		expected_calls="1.2.4 ${fake_bin}/codesage daemon stop"
		if [[ "${scenario}" == stale-target || "${scenario}" == wrong-install ]]; then
			expected_calls=''
		elif [[ "${scenario}" != stop ]]; then
			expected_calls+=$'\n'"1.2.4 ${fake_bin}/codesage mcp"
			if [[ "${scenario}" != startup && "${scenario}" != empty ]]; then
				expected_calls+=$'\n'"rpc initialize"
			fi
			if [[ "${scenario}" == success || "${scenario}" == ping-error || "${scenario}" == timeout ]]; then
				expected_calls+=$'\n'"rpc notifications/initialized"$'\n'"rpc ping"
			fi
		fi
		actual_calls="$(cat "${daemon_calls}")"
		if [[ "${actual_calls}" != "${expected_calls}" ]]; then
			printf 'release did not verify versions, stop, and complete the daemon handshake in order (scenario=%s)\n' "${scenario}" >&2
			cat "${daemon_calls}" "${tmp}/release-script-daemon.out" >&2
			return 1
		fi
		if [[ "${scenario}" == stale-target ]]; then
			install_after="$(sha256sum "${fake_bin}/codesage")"
			[[ "${install_after}" == "${install_before}" ]]
		fi
	done
	cd "$repo_root"
}

test_check_changelog_terseness() {
	local tmp checker status
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' RETURN
	checker="$repo_root/scripts/check-changelog.py"

	# A justification tail and a bold lead-in must both be rejected.
	cat >"$tmp/bad.md" <<'EOF'
# Changelog

## [Unreleased]

### Fixed

- **Thing:** it broke, so users waited longer than before.
EOF
	set +e
	python3 "$checker" "$tmp/bad.md" >"$tmp/bad.out" 2>&1
	status=$?
	set -e
	if [ "$status" -eq 0 ]; then
		printf 'expected check-changelog to reject a non-terse bullet\n' >&2
		cat "$tmp/bad.out" >&2
		return 1
	fi
	if ! grep -q 'bold lead-in' "$tmp/bad.out" || ! grep -q 'explanation' "$tmp/bad.out"; then
		printf 'expected both bold-lead-in and explanation diagnostics\n' >&2
		cat "$tmp/bad.out" >&2
		return 1
	fi

	# A terse single change plus a consolidated semicolon list must pass.
	cat >"$tmp/good.md" <<'EOF'
# Changelog

## [Unreleased]

### Changed

- `codesage export --format` rejects an unknown value instead of rendering markdown.

### Fixed

- Feature mapper: Laravel `prefix()->group()` inner routes inherit the prefix; `#` in a CMake string no longer truncates targets; Rust workspace-member bins stay out of library slices; Next.js pages-router skips special files.
EOF
	if ! python3 "$checker" "$tmp/good.md" >"$tmp/good.out" 2>&1; then
		printf 'expected check-changelog to accept terse + consolidated bullets\n' >&2
		cat "$tmp/good.out" >&2
		return 1
	fi
	cd "$repo_root"
}

test_leak_check_range_uses_range_endpoint
test_leak_check_invalid_regex_fails_closed
test_release_script_updates_changelog_links
test_check_changelog_terseness
run_python_suite() {
	python3 - "$1" <<-'EOF'
		import sys
		import unittest

		suite = unittest.defaultTestLoader.discover(sys.argv[1], pattern="test_*.py")
		result = unittest.TextTestRunner(verbosity=1).run(suite)
		if result.testsRun == 0:
		    print(f"no tests discovered under {sys.argv[1]}", file=sys.stderr)
		    sys.exit(1)
		sys.exit(0 if result.wasSuccessful() else 1)
	EOF
}

run_python_suite "$repo_root/scripts/tests"
run_python_suite "$repo_root/plugins/codesage-tools/tests"

printf 'script and plugin regression tests passed\n'
