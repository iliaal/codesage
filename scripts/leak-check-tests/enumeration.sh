#!/usr/bin/env bash
# Isolated enumeration and rename regression fixtures; no workspace changes.
set -euo pipefail

script_dir="$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)"
checker="$script_dir/../leak-check.sh"
tmp="$(mktemp -d)"
trap 'rm -rf -- "$tmp"' EXIT
real_git="$(command -v git)"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR

new_repo() {
	mkdir "$tmp/$1"
	cd "$tmp/$1"
	git init -q
	git config user.email fixture@example.invalid
	git config user.name Fixture
	git config core.hooksPath /dev/null
	git config commit.gpgSign false
	git config diff.renames true
	mkdir scripts
	printf 'FORBIDDEN_TOKEN\n' >scripts/leak-patterns.txt
	# Keep the pattern itself out of --all while retaining the real policy loader.
	printf 'clean\n' >clean.txt
	git add clean.txt
	git commit -qm base
}

expect_scan() {
	local expected="$1" diagnostic="$2" status=0
	shift 2
	scan_output="$(bash "$checker" "$@" 2>&1)" || status=$?
	if [[ "$status" != "$expected" || "$scan_output" != *"$diagnostic"* ]]; then
		printf 'FAIL %s: expected status %s / %q, got %s\n%s\n' "${PWD##*/}" "$expected" "$diagnostic" "$status" "$scan_output" >&2
		exit 1
	fi
}

scan_change() {
	local mode="$1" base="$2" expected="$3" diagnostic="$4"
	if [[ "$mode" == staged ]]; then
		expect_scan "$expected" "$diagnostic"
	else
		git commit -qm change
		expect_scan "$expected" "$diagnostic" --range "$base..HEAD"
	fi
}

# Both early gates must scan exact and modified rename destinations, despite
# rename detection being enabled in repository configuration.
for mode in staged range; do
	for kind in filename content modified; do
		(
			new_repo "rename-$mode-$kind"
			printf 'unchanged filler line %s\n' {1..40} >source.txt
			if [[ "$kind" == content ]]; then
				printf 'FORBIDDEN_TOKEN\n' >>source.txt
			fi
			git add source.txt
			git commit -qm source
			base="$(git rev-parse HEAD)"
			destination=$'renamed file\t"quoted"\nname.txt'
			diagnostic='contains a forbidden pattern'
			if [[ "$kind" == filename ]]; then
				destination='.env'
				diagnostic='is denied by filename policy'
			fi
			git mv source.txt "$destination"
			if [[ "$kind" == modified ]]; then
				printf 'FORBIDDEN_TOKEN\n' >>"$destination"
				git add -- "$destination"
			fi
			scan_change "$mode" "$base" 1 "$destination $diagnostic"
		)
	done
	for kind in addition modification type-change; do
		(
			new_repo "$kind-$mode"
			file=$'file with\tspace\nand newline.txt'
			if [[ "$kind" == modification ]]; then
				printf 'clean\n' >"$file"
			elif [[ "$kind" == type-change ]]; then
				ln -s clean.txt "$file"
			fi
			if [[ "$kind" != addition ]]; then
				git add -- "$file"
				git commit -qm source
				rm -- "$file"
			fi
			base="$(git rev-parse HEAD)"
			printf 'FORBIDDEN_TOKEN\n' >"$file"
			git add -- "$file"
			scan_change "$mode" "$base" 1 "$file contains a forbidden pattern"
		)
	done
done

# Each mode must inspect its selected blob, not a clean working-tree version,
# with NUL-delimited paths; valid empty/clean enumerations must still succeed.
for mode in staged range all; do
	(
		new_repo "blobs-$mode"
		base="$(git rev-parse HEAD)"
		file=$'blob\twith\nwhitespace.txt'
		printf 'FORBIDDEN_TOKEN\n' >"$file"
		git add -- "$file"
		args=()
		if [[ "$mode" != staged ]]; then
			git commit -qm leak
			if [[ "$mode" == range ]]; then
				args=(--range "$base..HEAD")
			else
				args=(--all)
			fi
		fi
		printf 'clean\n' >"$file"
		expect_scan 1 "$file contains a forbidden pattern" "${args[@]}"
		git add -- "$file"
		if [[ "$mode" != staged ]]; then
			git commit -qm clean
		fi
		expect_scan 0 '' "${args[@]}"
	)
done

(
	new_repo invalid-range
	expect_scan 2 'failed to enumerate files' --range 'missing-start..HEAD'
)

# Inject a producer failure after partial NUL-delimited output. No partial
# scan may run, even when the already-emitted path contains a policy violation.
mkdir "$tmp/bin"
cat >"$tmp/bin/git" <<'WRAPPER'
#!/usr/bin/env bash
for argument in "$@"; do
	case "$argument" in
	--name-only | ls-files)
		printf 'leaking.txt\0'
		printf 'fixture enumeration failure\n' >&2
		exit 73
		;;
	esac
done
exec "$REAL_GIT" "$@"
WRAPPER
chmod +x "$tmp/bin/git"
for mode in staged range all; do
	(
		new_repo "failed-enumeration-$mode"
		printf 'FORBIDDEN_TOKEN\n' >leaking.txt
		git add leaking.txt
		git commit -qm leak
		args=()
		case "$mode" in
		range) args=(--range 'HEAD~1..HEAD') ;;
		all) args=(--all) ;;
		esac
		export REAL_GIT="$real_git" PATH="$tmp/bin:$PATH"
		expect_scan 2 'failed to enumerate files (git exit 73)' "${args[@]}"
		if [[ "$scan_output" == *'contains a forbidden pattern'* ]]; then
			printf 'FAIL: scanned partial enumeration before aborting\n%s\n' "$scan_output" >&2
			exit 1
		fi
	)
done
printf 'PASS: leak-check enumeration, rename, type-change, and blob fixtures\n'
