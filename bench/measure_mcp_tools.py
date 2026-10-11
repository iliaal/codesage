#!/usr/bin/env python3
"""Measure advertised descriptions and input schemas from a real MCP process."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
from datetime import datetime, timezone

import tiktoken


def capture(binary):
    process = subprocess.Popen(
        [str(binary), "mcp", "--direct"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env={**os.environ, "CODESAGE_WATCH": "0"},
    )
    replies = queue.Queue()

    def read():
        for line in process.stdout:
            replies.put(line)
        replies.put(None)

    threading.Thread(target=read, daemon=True).start()

    def send(method, params=None, request_id=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        if request_id is not None:
            message["id"] = request_id
        process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()
        if request_id is None:
            return None
        while True:
            line = replies.get(timeout=30)
            if line is None:
                raise RuntimeError("MCP process exited before replying")
            response = json.loads(line)
            if response.get("id") == request_id:
                if "error" in response:
                    raise RuntimeError(response["error"])
                return response["result"]

    try:
        send("initialize", {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "tools-list-measurement", "version": "1"},
        }, 1)
        send("notifications/initialized")
        return send("tools/list", {}, 2)
    finally:
        process.kill()
        process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--listing", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    listing = capture(binary)
    encoding = tiktoken.get_encoding("cl100k_base")
    canonical = lambda value: json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    records = []
    for tool in sorted(listing["tools"], key=lambda tool: tool["name"]):
        description = tool.get("description", "")
        schema = canonical(tool["inputSchema"])
        records.append({
            "name": tool["name"],
            "description_tokens": len(encoding.encode(description)),
            "input_schema_tokens": len(encoding.encode(schema)),
            "input_schema_sha256": hashlib.sha256(schema.encode()).hexdigest(),
        })
    result = {
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "tools_list_sha256": hashlib.sha256(canonical(listing).encode()).hexdigest(),
        "tokenizer": "cl100k_base",
        "tiktoken_version": tiktoken.__version__,
        "measurement": "Sum of separately encoded descriptions and canonical compact inputSchema JSON for every advertised tool. Excludes names, output schemas, annotations, transport, and host-specific serialization. This is a reproducible tokenizer measurement, not a host billing count.",
        "tool_count": len(records),
        "description_tokens": sum(row["description_tokens"] for row in records),
        "input_schema_tokens": sum(row["input_schema_tokens"] for row in records),
        "tools": records,
    }
    result["total_tokens"] = result["description_tokens"] + result["input_schema_tokens"]
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    if args.listing:
        args.listing.write_text(json.dumps(listing, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key != "tools"}, indent=2))


if __name__ == "__main__":
    main()
