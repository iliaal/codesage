#!/usr/bin/env python3

import argparse
import json
from pathlib import Path
import sys
import tomllib


def load_json(path):
    with path.open(encoding="utf-8") as handle:
        value = json.load(handle)
    return require_object(value, str(path))


def require_object(value, label):
    if not isinstance(value, dict):
        raise ValueError(f"{label}: expected a JSON object")
    return value


def workspace_version(root):
    with (root / "Cargo.toml").open("rb") as handle:
        manifest = tomllib.load(handle)
    return manifest["workspace"]["package"]["version"]


def version_errors(root):
    root = Path(root)
    expected = workspace_version(root)
    errors = []

    codex = load_json(root / "plugins/codesage-tools/.codex-plugin/plugin.json")
    codex_version = str(codex.get("version", ""))
    if codex_version.split("+", 1)[0] != expected:
        errors.append(
            f"Codex plugin version {codex_version or '<missing>'}; expected {expected}"
        )

    claude = load_json(root / "plugins/codesage-tools/.claude-plugin/plugin.json")
    claude_version = str(claude.get("version", ""))
    if claude_version != expected:
        errors.append(
            f"Claude plugin version {claude_version or '<missing>'}; expected {expected}"
        )

    marketplace = load_json(root / ".claude-plugin/marketplace.json")
    metadata = require_object(marketplace.get("metadata", {}), "marketplace metadata")
    metadata_version = str(metadata.get("version", ""))
    if metadata_version != expected:
        errors.append(
            f"Claude marketplace metadata version {metadata_version or '<missing>'}; expected {expected}"
        )

    plugins = marketplace.get("plugins", [])
    if not isinstance(plugins, list):
        raise ValueError("marketplace plugins: expected a JSON array")
    entries = []
    for index, plugin in enumerate(plugins):
        plugin = require_object(plugin, f"marketplace plugins[{index}]")
        if plugin.get("name") == "codesage-tools":
            entries.append(plugin)
    if len(entries) != 1:
        errors.append(
            f"Claude marketplace has {len(entries)} codesage-tools entries; expected 1"
        )
    else:
        entry_version = str(entries[0].get("version", ""))
        if entry_version != expected:
            errors.append(
                f"Claude marketplace plugin version {entry_version or '<missing>'}; expected {expected}"
            )

    return errors


def main():
    parser = argparse.ArgumentParser(
        description="Check CodeSage workspace and plugin version alignment"
    )
    parser.add_argument("--root", default=".")
    args = parser.parse_args()
    try:
        errors = version_errors(Path(args.root).resolve())
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as exc:
        print(f"check-plugin-versions: {exc}", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"check-plugin-versions: {error}", file=sys.stderr)
        return 1
    print("Plugin versions match the workspace version.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
