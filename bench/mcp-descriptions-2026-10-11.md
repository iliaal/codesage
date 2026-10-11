# MCP description measurement

Measured the actual `codesage mcp --direct` `tools/list` response before and after
the description change. Both binaries were built with `cargo build -p codesage`
(the after binary was also rebuilt by the integration tests). The baseline source
was `cb0ff7a93e0585d833f5ec0ee7c79e61aaaea8ce`.

| cl100k_base tokens | Before | After |
|---|---:|---:|
| Descriptions | 5,770 | 1,213 |
| Input schemas | 9,146 | 9,146 |
| Combined | 14,916 | 10,359 |

There are 24 advertised tools; two further registrations are internal and hidden.
The combined reduction is 30.6%; descriptions alone shrink 79.0%.
Every input-schema hash is unchanged. Counts exclude output schemas, names,
annotations, transport, and host serialization; they are tokenizer measurements,
not billed tokens or an estimate of the entire host prompt.

The 1,500-token combined target is not achieved. Removing all schema `description`,
`title`, and `examples` annotations in memory still leaves 2,857 schema tokens,
before any tool description. Reaching the target requires a separate API/schema
design change. This change keeps field guidance and validation intact.

Reproduce with Python's `tiktoken` package installed:

```bash
python3 bench/measure_mcp_tools.py target/debug/codesage /tmp/tools-metrics.json --listing /tmp/tools-list.json
```

[The measurement record](mcp-descriptions-2026-10-11.json) pins binary and response
hashes, tokenizer version, timestamps, and per-tool counts. Full field semantics
and limitations remain in the help catalog and are checked through the running
binary by `cargo test -p codesage --test mcp_help`.
