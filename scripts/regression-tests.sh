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
	cd "$repo_root"
}

test_release_script_updates_changelog_links() {
	local tmp origin fake_bin release_script version changelog
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' RETURN
	origin="$tmp/origin.git"
	fake_bin="$tmp/bin"
	release_script="$repo_root/scripts/release.sh"
	version="1.2.3"

	mkdir -p "$fake_bin"
	printf '#!/usr/bin/env bash\nexit 0\n' >"$fake_bin/cargo"
	printf '#!/usr/bin/env bash\nprintf "codesage fake\\n"\n' >"$fake_bin/codesage"
	chmod +x "$fake_bin/cargo" "$fake_bin/codesage"
	chmod a-w "$fake_bin/codesage"

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
	git add Cargo.toml CHANGELOG.md
	git commit -q -m initial
	git push -q origin master

	PATH="$fake_bin:$PATH" "$release_script" --yes "$version" >"$tmp/release-script.out" 2>&1

	changelog="$(cat CHANGELOG.md)"
	[[ "$changelog" == *"[Unreleased]: https://github.com/iliaal/codesage/compare/v$version...HEAD"* ]]
	[[ "$changelog" == *"[$version]: https://github.com/iliaal/codesage/releases/tag/v$version"* ]]
	if grep -q 'compare/v1.2.2...HEAD' CHANGELOG.md; then
		printf 'release script left stale Unreleased compare link\n' >&2
		cat CHANGELOG.md >&2
		return 1
	fi
	cd "$repo_root"
}

test_leak_check_range_uses_range_endpoint
test_leak_check_invalid_regex_fails_closed
test_release_script_updates_changelog_links

printf 'script regression tests passed\n'
