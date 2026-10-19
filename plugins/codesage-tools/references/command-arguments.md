# Pass command arguments as data

Use this procedure for `codesage-onboard`, `codesage-eval`, and `codesage-bench`.
The host supplies `$ARGUMENTS` as task text. Parse the requested options and their
values, preserving quoted values, whitespace, and shell metacharacters literally.
Do not execute command substitutions, expand environment variables, or interpret
shell operators in those values. If argument boundaries are ambiguous, ask for
the intended values before running the command.

1. With the host's Write tool or native file-editing tool, create a unique
   temporary UTF-8 JSON file containing an array of argument strings, such as
   `["--corpus-dir", "a directory"]`.
   Use `[]` when there are no arguments. Do not create this file through Bash,
   `echo`, `printf`, a heredoc, or a shell command assembled from the argument
   values.
2. Run `python3` with the currently installed
   `${CLAUDE_PLUGIN_ROOT}/bin/codesage-invoke`, the fixed command identifier
   (`onboard`, `eval`, or `bench`), and the temporary JSON file path. Pass these
   as separate arguments. If the host exposes only a shell-command string,
   shell-quote each path as one literal word, including embedded single quotes.
   The command string must contain no user argument values or JSON document.
   Run the dispatcher as a single command; do not append an `echo`, cleanup, or
   other command that would replace its exit status. Read the status from the
   host's tool result and perform cleanup separately.
   The dispatcher reads the array and replaces itself with the matching plugin
   script through a native argv call; it never evaluates argument text as shell
   source. It preserves the script's exit status.
3. Remove only the temporary argument file you created after the command ends.
   Preserve it until a background command has finished reading it.

Keep the host's execution permission checks in force. If Bash or the equivalent
native execution tool is denied, report that limitation and stop. Do not switch
tools or execute through a file-writing tool to evade the denial. If the host
cannot write a data file without shell interpolation, report that limitation
instead of falling back to raw argument interpolation.
