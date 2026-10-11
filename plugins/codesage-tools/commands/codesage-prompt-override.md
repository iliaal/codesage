---
name: codesage-prompt-override
description: Print a compatibility pointer to the canonical CodeSage retrieval skill
---

# CodeSage routing pointer

Run `bin/codesage-prompt-override` under the installed plugin root and show its
stdout. Invoke the executable with its path as one quoted argument. The command
prints a pointer to the current retrieval skill; it no longer maintains a second
tool-selection policy. Existing callers can continue to consume its text output.

Load the named skill, or read the emitted JSON-string path as file-path data.
Do not execute that path as shell source. The skill contains tool selection,
freshness, uncertainty, recovery, ordering, and measured-cost guidance.

Printing the pointer does not change host configuration. Obtain the user's
authorization before installing it into a persistent prompt setting.
