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
	git add Cargo.toml CHANGELOG.md .claude-plugin/marketplace.json scripts/check-changelog.py scripts/check-plugin-versions.py plugins/codesage-tools/.codex-plugin/plugin.json plugins/codesage-tools/.claude-plugin/plugin.json
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

	CODEX_CALLS_FILE="${codex_calls}" CLAUDE_CALLS_FILE="${claude_calls}" PATH="${fake_bin}:${PATH}" \
		"${release_script}" --yes "${version}" >"${tmp}/release-script.out" 2>&1

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
