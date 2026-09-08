#!/usr/bin/env bash
# PreToolUse hook (Edit|Write|MultiEdit): serve `codesage brief` for the file
# about to be edited, as hookSpecificOutput.additionalContext.
#
# Contract: this hook must never block the tool call. Every path exits 0, and
# every failure is silence — an error surfaced here lands in the agent's
# context as noise it cannot act on. `codesage brief --session` applies its
# own repeat/cooldown/budget gate and prints nothing when suppressed, so an
# empty capture below is the common case, not a failure.
#
# PreToolUse plain stdout is NOT shown to the model (only UserPromptSubmit /
# SessionStart stdout is); context must go through the JSON envelope. No
# permissionDecision is emitted: this hook takes no stance on the tool call,
# and combining permissionDecision with other fields has known footguns under
# bypassPermissions.

exec 2>/dev/null

[[ ${CODESAGE_BRIEF_CANARY:-0} == 1 ]] || exit 0

INPUT=$(cat) || exit 0

# Field extraction stays pure-bash so the no-op path costs no subprocess.
# A path containing an escape sequence would need real JSON decoding; those
# are rare enough that bailing silently is cheaper than being wrong.
[[ ${INPUT} =~ \"file_path\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]] || exit 0
FILE=${BASH_REMATCH[1]}
[[ ${FILE} == *\\* ]] && exit 0
[[ ${FILE} == /* ]] || exit 0
# Reject `.`/`..` segments: with those gone, the lexical prefix strip below is
# real containment, so `<root>/../outside` can never reach codesage as a
# root-relative path.
[[ ${FILE} =~ (^|/)\.\.?(/|$) ]] && exit 0
# An existing target must be a regular file (test -f follows symlinks): a FIFO
# would stall codesage's read until the hook timeout. A not-yet-existing path
# is fine — that is every Write of a new file.
[[ -L ${FILE} || (-e ${FILE} && ! -f ${FILE}) ]] && exit 0

# No session means no enforceable cumulative context budget.
[[ ${INPUT} =~ \"session_id\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]] || exit 0
SESSION=${BASH_REMATCH[1]}
[[ ${SESSION} =~ ^[a-zA-Z0-9_-][a-zA-Z0-9_.-]{0,127}$ ]] || exit 0

# Onboarded-project check: walk up from the file's directory for
# .codesage/index.db. Stat-only; exits before any process is spawned when the
# file is outside every onboarded project.
ROOT=""
DIR=${FILE%/*}
while [[ -n ${DIR} ]]; do
	[[ -L ${DIR} ]] && exit 0
	if [[ -e "${DIR}/.codesage/index.db" ]]; then
		ROOT=${DIR}
		break
	fi
	[[ ${DIR} == "${DIR%/*}" ]] && break
	DIR=${DIR%/*}
done
[[ -n ${ROOT} ]] || exit 0

command -v codesage >/dev/null 2>&1 || exit 0
# jq builds the JSON envelope below, so it is a hard prerequisite — without
# it the hook stays silent (install jq to enable brief context).
command -v jq >/dev/null 2>&1 || exit 0
jq -e --arg file "${FILE}" --arg session "${SESSION}" \
	'.session_id == $session and .tool_input.file_path == $file and
   (.tool_name == "Edit" or .tool_name == "Write" or .tool_name == "MultiEdit")' \
	<<<"${INPUT}" >/dev/null || exit 0

# `codesage brief` resolves the project by walking up from cwd, and expects a
# root-relative path. Suppressed or empty briefs print nothing.
PAYLOAD=$(cd "${ROOT}" && codesage brief --session "${SESSION}" -- "${FILE#"${ROOT}"/}") || exit 0
[[ -n ${PAYLOAD} ]] || exit 0

jq -cn --arg ctx "${PAYLOAD}" \
	'{hookSpecificOutput: {hookEventName: "PreToolUse", additionalContext: $ctx}}'
exit 0
