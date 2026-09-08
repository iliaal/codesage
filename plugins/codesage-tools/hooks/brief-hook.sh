#!/usr/bin/env bash
# Fail silently without blocking edits. PreToolUse context requires the JSON
# envelope; omit permissionDecision because this hook only supplies context.

exec 2>/dev/null

[[ ${CODESAGE_BRIEF_CANARY:-0} == 1 ]] || exit 0

INPUT=$(cat) || exit 0

# Reject escaped paths rather than misdecode them in the cheap prefilter.
[[ ${INPUT} =~ \"file_path\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]] || exit 0
FILE=${BASH_REMATCH[1]}
[[ ${FILE} == *\\* ]] && exit 0
[[ ${FILE} == /* ]] || exit 0
# Dot segments would defeat the lexical containment check.
[[ ${FILE} =~ (^|/)\.\.?(/|$) ]] && exit 0
# Reject symlinks and special files (FIFOs can hang); new Write targets are valid.
[[ -L ${FILE} || (-e ${FILE} && ! -f ${FILE}) ]] && exit 0

# No session means no enforceable cumulative context budget.
[[ ${INPUT} =~ \"session_id\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]] || exit 0
SESSION=${BASH_REMATCH[1]}
[[ ${SESSION} =~ ^[a-zA-Z0-9_-][a-zA-Z0-9_.-]{0,127}$ ]] || exit 0

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
command -v jq >/dev/null 2>&1 || exit 0
jq -e --arg file "${FILE}" --arg session "${SESSION}" \
	'.session_id == $session and .tool_input.file_path == $file and
   (.tool_name == "Edit" or .tool_name == "Write" or .tool_name == "MultiEdit")' \
	<<<"${INPUT}" >/dev/null || exit 0

# brief expects a project cwd and relative path; suppression prints nothing.
PAYLOAD=$(cd "${ROOT}" && codesage brief --session "${SESSION}" -- "${FILE#"${ROOT}"/}") || exit 0
[[ -n ${PAYLOAD} ]] || exit 0

jq -cn --arg ctx "${PAYLOAD}" \
	'{hookSpecificOutput: {hookEventName: "PreToolUse", additionalContext: $ctx}}'
exit 0
