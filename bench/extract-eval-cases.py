#!/usr/bin/env python3
"""
Extract candidate eval cases from Claude Code session history.
Finds user queries and the files that were subsequently accessed.

Usage:
  python3 extract-eval-cases.py <session-dir> <project-root> [--min-files 1] [--max-cases 50]
  python3 extract-eval-cases.py <session-dir> <project-root> --yaml <out.yaml> [--project-name NAME]

Without --yaml, prints human-readable candidates to stdout for manual review.
With --yaml, writes a corpus YAML directly (project_root + scoring + cases).
"""

import argparse
import json
import re
import sys
from pathlib import Path


def parse_session(path: Path) -> list[dict]:
    messages = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
                messages.append(msg)
            except json.JSONDecodeError:
                continue
    return messages


def extract_tool_file_paths(msg: dict) -> set[str]:
    paths = set()
    content = msg.get("message", {}).get("content", [])
    if not isinstance(content, list):
        return paths
    for block in content:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "tool_use":
            inp = block.get("input", {})
            if isinstance(inp, dict):
                fp = inp.get("file_path", "")
                if fp:
                    paths.add(fp)
        if block.get("type") == "tool_result":
            result_content = block.get("content", "")
            if isinstance(result_content, str):
                for m in re.finditer(r'"file_path"\s*:\s*"([^"]+)"', result_content):
                    paths.add(m.group(1))
    return paths


def get_user_text(msg: dict) -> str:
    content = msg.get("message", {}).get("content", "")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, dict) and block.get("type") == "text":
                parts.append(block.get("text", ""))
            elif isinstance(block, str):
                parts.append(block)
        return " ".join(parts)
    return ""


def is_tool_result(msg: dict) -> bool:
    return msg.get("type") == "user" and "toolUseResult" in msg


def is_search_query(text: str) -> bool:
    text = re.sub(r'<[^>]+>.*?</[^>]+>', '', text, flags=re.DOTALL).strip()
    if len(text) < 15 or len(text) > 500:
        return False
    lower = text.lower().strip()
    skip_prefixes = (
        "continue", "yes", "no", "ok", "thanks", "go ahead", "stop",
        "git ", "cargo ", "npm ", "pip ", "/", "!", "base directory",
    )
    if any(lower.startswith(p) for p in skip_prefixes):
        return False

    search_signals = [
        "where", "find", "how does", "how do", "what is", "what are",
        "which file", "which class", "which function", "which method",
        "show me", "look at", "look for", "search",
        "implementation", "handler", "controller", "service",
        "component", "module", "route", "endpoint",
        "config", "setup", "initialize", "bootstrap",
        "error", "bug", "fix", "issue", "crash", "fail",
        "auth", "login", "session", "token", "permission",
        "database", "migration", "schema", "model",
        "debug", "trace", "log",
        "api", "request", "response", "middleware",
        "upload", "download", "document",
        "validation", "form", "input", "field",
        "can you", "need to", "want to", "trying to",
        "understand", "explain", "figure out", "investigate",
        "check", "review", "analyze", "explore",
    ]
    return any(sig in lower for sig in search_signals)


def normalize_path(path: str, project_root: str) -> str | None:
    if path.startswith(project_root):
        rel = path[len(project_root):].lstrip("/")
        if rel and not rel.startswith("."):
            return rel
    return None


def extract_cases(session_dir: Path, project_root: str, min_files: int, max_cases: int) -> list[dict]:
    candidates = []
    jsonl_files = sorted(session_dir.rglob("*.jsonl"), key=lambda p: p.stat().st_size, reverse=True)

    for jsonl_path in jsonl_files[:40]:
        if jsonl_path.stat().st_size < 3000:
            continue

        messages = parse_session(jsonl_path)
        if not messages:
            continue

        i = 0
        while i < len(messages):
            msg = messages[i]

            if msg.get("type") == "user" and not is_tool_result(msg):
                text = get_user_text(msg)
                text = re.sub(r'<system-reminder>.*?</system-reminder>', '', text, flags=re.DOTALL).strip()
                text = re.sub(r'<command-message>.*?</command-message>', '', text, flags=re.DOTALL).strip()
                text = re.sub(r'<command-name>.*?</command-name>', '', text, flags=re.DOTALL).strip()

                if is_search_query(text):
                    files = set()
                    for j in range(i + 1, min(i + 40, len(messages))):
                        next_msg = messages[j]
                        if next_msg.get("type") == "user" and not is_tool_result(next_msg):
                            break
                        if next_msg.get("type") == "assistant":
                            files.update(extract_tool_file_paths(next_msg))

                    rel_files = set()
                    for f in files:
                        rel = normalize_path(f, project_root)
                        if rel and "/" in rel:
                            ext = rel.rsplit(".", 1)[-1] if "." in rel else ""
                            if ext in ("php", "py", "js", "jsx", "ts", "tsx", "c", "h", "rs",
                                       "vue", "rb", "go", "java", "kt", "swift", "yml", "yaml",
                                       "json", "toml", "env", "conf"):
                                rel_files.add(rel)

                    if len(rel_files) >= min_files:
                        query = text[:300].strip()
                        query = re.sub(r'\s+', ' ', query)
                        candidates.append({
                            "query": query,
                            "files": sorted(rel_files),
                            "session": jsonl_path.name,
                            "file_count": len(rel_files),
                        })

            i += 1

        if len(candidates) >= max_cases * 3:
            break

    seen_queries = set()
    deduped = []
    for c in candidates:
        key = c["query"][:60].lower()
        if key not in seen_queries:
            seen_queries.add(key)
            deduped.append(c)

    deduped.sort(key=lambda c: c["file_count"], reverse=True)
    return deduped[:max_cases]


def _slugify(text: str, max_len: int = 60) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:max_len] or "case"


def write_yaml(cases: list[dict], project_root: str, project_name: str, out_path: Path) -> None:
    lines: list[str] = []
    lines.append(f"project_root: {project_root}")
    lines.append("description: |")
    lines.append(f"  Retrieval benchmark for {project_name}.")
    lines.append("  Session-based queries mined from real Claude Code session history by")
    lines.append("  bench/extract-eval-cases.py --yaml.")
    lines.append("")
    lines.append("scoring:")
    lines.append("  k_values: [5, 10]")
    lines.append("")
    lines.append("cases:")

    used_ids: set[str] = set()
    for c in cases:
        base = _slugify(c["query"])
        candidate = f"session-{base}"
        i = 2
        while candidate in used_ids:
            candidate = f"session-{base}-{i}"
            i += 1
        used_ids.add(candidate)

        query = c["query"].replace("\n", " ").strip()
        lines.append(f"  - id: {candidate}")
        lines.append("    source: session")
        # Quote the query defensively: YAML double-quoted string handles most user text
        # once we escape backslashes and double quotes.
        escaped = query.replace("\\", "\\\\").replace('"', '\\"')
        lines.append(f'    query: "{escaped}"')
        lines.append("    expected_files:")
        for f in c["files"]:
            lines.append(f"      - {f}")
        lines.append("")

    out_path.write_text("\n".join(lines))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("session_dir", type=Path)
    ap.add_argument("project_root", type=str)
    ap.add_argument("--min-files", type=int, default=1)
    ap.add_argument("--max-cases", type=int, default=50)
    ap.add_argument("--yaml", type=Path, default=None,
                    help="Write a corpus YAML to this path instead of printing candidates")
    ap.add_argument("--project-name", type=str, default=None,
                    help="Name used in YAML description (default: basename of project_root)")
    args = ap.parse_args()

    cases = extract_cases(args.session_dir, args.project_root, args.min_files, args.max_cases)

    if args.yaml is not None:
        if not cases:
            print("# no candidate cases found — nothing to write", file=sys.stderr)
            return 1
        project_name = args.project_name or Path(args.project_root).name or "project"
        write_yaml(cases, args.project_root, project_name, args.yaml)
        print(f"wrote {len(cases)} cases to {args.yaml}")
        return 0

    print(f"# Found {len(cases)} candidate cases\n")
    for i, c in enumerate(cases):
        print(f"## Case {i+1} ({c['file_count']} files, session: {c['session'][:12]}...)")
        print(f"Query: {c['query'][:200]}")
        print(f"Files:")
        for f in c["files"][:10]:
            print(f"  - {f}")
        if len(c["files"]) > 10:
            print(f"  ... and {len(c['files']) - 10} more")
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
