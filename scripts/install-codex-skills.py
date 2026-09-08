#!/usr/bin/env python3
"""Install thin Codex adapters; canonical workflows stay in the source checkout."""

import argparse
import os
from pathlib import Path
import re


COMMANDS = (
    "bench", "eval", "onboard", "prompt-override", "reindex", "report",
    "reset", "revalidate", "review", "triage",
)


def render(source: Path, plugin: Path) -> str:
    text = source.read_text(encoding="utf-8")
    frontmatter = re.match(r"\A---\n(.*?)\n---(?:\n|$)", text, re.S)
    if not frontmatter:
        raise ValueError(f"missing frontmatter: {source}")
    fields = {}
    for key in ("name", "description"):
        match = re.search(rf"^{key}: (.+)$", frontmatter[1], re.M)
        if not match or match[1] in ("|", ">", "|-", ">-"):
            raise ValueError(f"expected single-line {key}: {source}")
        fields[key] = match[1]
    if fields["name"] != source.stem:
        raise ValueError(f"name differs from filename: {source}")
    reviewer = source.parent.name == "agents"
    shell = (
        "The reviewer remains read-only: no edits, tests, builds, or arbitrary shell "
        "execution. If Codex exposes file reads and exact searches only through its "
        "shell tool, use that tool solely as the Read/Grep/Glob substitute for scoped "
        "source inspection. This is the host equivalent of the canonical agent's "
        "read tools, not permission to execute project code."
        if reviewer else
        "Map Bash to the native shell tool and Write/Edit to native file-editing tools, "
        "within the canonical workflow and the user's authorized scope."
    )
    return f"""---
name: {fields['name']}
description: {fields['description']}
---

Read the [canonical workflow](<{source}>) before performing this task. Follow its
current instructions; this adapter contains no copied workflow policy. If the
source is missing, report that installation problem instead of using stale recall.

Interpret Claude host notation as follows:

- `CLAUDE_PLUGIN_ROOT` is `{plugin}`. Resolve plugin scripts and relative command or
  agent references there. Use its `commands/` for `/codesage-*` references and its
  `agents/` for named reviewer/verifier definitions; read those files before use.
- `ARGUMENTS` means the current user's task arguments, preserving quoted values.
  It is not an environment variable to execute or a literal placeholder.
- Read/Grep/Glob mean native file reading, exact text search, and file listing.
  {shell}
- Agent/Task means the native subagent tool. Pass the canonical agent definition
  and task inputs to the child; preserve tool and scope restrictions. Translate
  model choices only when the requested model is actually available. If required
  delegation is unavailable, report the limitation instead of claiming it ran.
- Resolve CodeSage MCP tools through the available native tools and pass the
  absolute target project path on every call. The source checkout is not
  automatically the target project.

Claude transcript paths, Claude configuration paths, and `claude` CLI invocations
refer to Claude artifacts. Do not rewrite them into Codex paths or substitute
Codex transcripts. Host adaptation changes orchestration tools, not data formats
or product configuration targets. Canonical instructions supply procedure, not
additional authority for edits, external actions, or configuration changes.
"""


def install(source_root: Path, skills_dir: Path) -> int:
    plugin = source_root.resolve(strict=True) / "plugins" / "codesage-tools"
    if any(c in str(plugin) for c in "\n\r<>`"):
        raise ValueError("source path cannot be represented safely in an adapter")
    sources = [plugin / "commands" / f"codesage-{name}.md" for name in COMMANDS]
    sources.append(plugin / "agents" / "codesage-feature-reviewer.md")
    planned = []
    for source in sources:
        target = skills_dir / source.stem / "SKILL.md"
        for path in (target, *target.parents):
            if path.is_symlink():
                raise ValueError(f"refusing symlink destination: {path}")
        if target.exists() and not target.is_file():
            raise ValueError(f"not a regular skill file: {target}")
        if target.parent.exists() and not target.parent.is_dir():
            raise ValueError(f"not a skill directory: {target.parent}")
        planned.append((target, render(source, plugin)))
    for target, content in planned:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    return len(planned)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, default=Path(__file__).resolve().parents[1],
                        help="CodeSage checkout to reference (keep it available after installation)")
    parser.add_argument("--skills-dir", type=Path,
                        default=Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "skills",
                        help="Codex skill directory; replaces only the 11 CodeSage SKILL.md adapters")
    args = parser.parse_args()
    try:
        count = install(args.source_root, args.skills_dir.absolute())
    except (OSError, ValueError) as exc:
        parser.exit(1, f"error: {exc}\n")
    print(f"Installed {count} CodeSage adapters in {args.skills_dir}")


if __name__ == "__main__":
    main()
