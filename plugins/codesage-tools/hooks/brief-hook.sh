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

INPUT=$(cat) || exit 0

# Field extraction stays pure-bash so the no-op path costs no subprocess.
# A path containing an escape sequence would need real JSON decoding; those
# are rare enough that bailing silently is cheaper than being wrong.
[[ $INPUT =~ \"file_path\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]] || exit 0
FILE=${BASH_REMATCH[1]}
[[ $FILE == *\\* ]] && exit 0
[[ $FILE == /* ]] || exit 0
# Reject `.`/`..` segments: with those gone, the lexical prefix strip below is
# real containment, so `<root>/../outside` can never reach codesage as a
# root-relative path.
[[ $FILE =~ (^|/)\.\.?(/|$) ]] && exit 0
# An existing target must be a regular file (test -f follows symlinks): a FIFO
# would stall codesage's read until the hook timeout. A not-yet-existing path
# is fine — that is every Write of a new file.
[[ -e $FILE && ! -f $FILE ]] && exit 0

# session_id is optional: a hook payload without one (or with an escaped,
# non-decodable value) falls back to a session-less `codesage brief` call
# below — ungated render, no repeat/cooldown/budget suppression — rather
# than going fully silent. jq stays a hard prereq: without it the JSON
# envelope cannot be built, so the hook stays silent per the never-block
# contract above.
[[ $INPUT =~ \"session_id\"[[:space:]]*:[[:space:]]*\"([^\"]*)\" ]] && SESSION=${BASH_REMATCH[1]} || SESSION=""
[[ $SESSION == *\\* ]] && SESSION=""

# Onboarded-project check: walk up from the file's directory for
# .codesage/index.db. Stat-only; exits before any process is spawned when the
# file is outside every onboarded project.
ROOT=""
DIR=${FILE%/*}
while [[ -n $DIR ]]; do
  if [[ -e "$DIR/.codesage/index.db" ]]; then
    ROOT=$DIR
    break
  fi
  [[ $DIR == "${DIR%/*}" ]] && break
  DIR=${DIR%/*}
done
[[ -n $ROOT ]] || exit 0

command -v codesage >/dev/null 2>&1 || exit 0
# jq builds the JSON envelope below, so it is a hard prerequisite — without
# it the hook stays silent (install jq to enable brief context).
command -v jq >/dev/null 2>&1 || exit 0

# `codesage brief` resolves the project by walking up from cwd, and expects a
# root-relative path. Suppressed or empty briefs print nothing. Without a
# session id the --session flag is omitted (session-less render).
if [[ -n ${SESSION:-} ]]; then
	PAYLOAD=$(cd "$ROOT" && codesage brief --session "$SESSION" -- "${FILE#"$ROOT"/}") || exit 0
else
	PAYLOAD=$(cd "$ROOT" && codesage brief -- "${FILE#"$ROOT"/}") || exit 0
fi
[[ -n $PAYLOAD ]] || exit 0

jq -cn --arg ctx "$PAYLOAD" \
  '{hookSpecificOutput: {hookEventName: "PreToolUse", additionalContext: $ctx}}'
exit 0
