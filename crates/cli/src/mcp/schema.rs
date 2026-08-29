use std::sync::Arc;

/// JSON Schema standard `format` values (Draft 2020-12 + the common format
/// vocabulary). `schemars` emits Rust-specific formats (`uint32`, `uint`,
/// `int64`, `float`, `double`, …) for numeric fields that are NOT in this set;
/// strict MCP clients (e.g. opencode) log "unknown format uint32" warnings on
/// them. We strip the non-standard ones from advertised tool schemas.
fn is_standard_json_schema_format(fmt: &str) -> bool {
    matches!(
        fmt,
        "date-time"
            | "date"
            | "time"
            | "duration"
            | "email"
            | "idn-email"
            | "hostname"
            | "idn-hostname"
            | "ipv4"
            | "ipv6"
            | "uri"
            | "uri-reference"
            | "iri"
            | "iri-reference"
            | "uri-template"
            | "uuid"
            | "json-pointer"
            | "relative-json-pointer"
            | "regex"
    )
}

/// Recursively remove non-standard `format` annotations from a JSON Schema.
/// Unsigned-int formats (`uint*`) are replaced with `minimum: 0` so the
/// non-negativity constraint they encoded survives the strip.
fn strip_nonstandard_schema_formats(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let fmt = map
                .get("format")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            if let Some(fmt) = fmt
                && !is_standard_json_schema_format(&fmt)
            {
                map.remove("format");
                if fmt.starts_with("uint") {
                    map.entry("minimum".to_string())
                        .or_insert_with(|| serde_json::Value::from(0));
                }
            }
            for child in map.values_mut() {
                strip_nonstandard_schema_formats(child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items.iter_mut() {
                strip_nonstandard_schema_formats(child);
            }
        }
        _ => {}
    }
}

/// Tools that write inside the project tree (`.codesage/sessions/<id>.json`)
/// and therefore must not advertise `readOnlyHint: true`.
const NON_READONLY_TOOLS: &[&str] = &["session_start", "session_end"];

/// Schema for the `_meta` object the render layer may inject at the top level
/// of ANY tool response (budget truncation, protected-array drops, stale-file
/// annotations — see `render.rs`). Merged into every tool's outputSchema as an
/// optional property so schema-consulting agents aren't surprised by it.
fn meta_property_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Response envelope annotations, present only when the server \
            trimmed or flagged this response. `_meta.truncated` means the response \
            exceeded the per-call token budget and an array field was trimmed; it is \
            distinct from any same-named field inside a tool's own result (e.g. \
            impact_analysis's `truncated`, which reports that the tool's `limit` \
            parameter capped the result set).",
        "properties": {
            "truncated": { "type": "boolean", "description": "response was trimmed to fit the token budget" },
            "kind": { "type": "string", "description": "tool that produced the truncated response" },
            "field": { "type": "string", "description": "name of the trimmed array field" },
            "also_truncated_fields": { "type": "array", "items": { "type": "string" }, "description": "further array fields trimmed to fit the budget, each as `name (kept/total)`; `field`/`total_results`/`returned` describe only the first" },
            "total_results": { "type": "integer", "minimum": 0, "description": "element count before trimming" },
            "returned": { "type": "integer", "minimum": 0, "description": "element count kept" },
            "approx_tokens_budget": { "type": "integer", "minimum": 0, "description": "approximate token budget applied" },
            "hint": { "type": "string", "description": "suggested next step (refine query, narrow scope, paginate via offset)" },
            "dropped_files": { "type": "array", "items": { "type": "string" }, "description": "identifiers of elements trimmed from a protected array (e.g. assess_risk_diff `files`)" },
            "dropped_count": { "type": "integer", "minimum": 0, "description": "trimmed protected-array elements that had no identifier" },
            "stale_files": { "type": "array", "items": { "type": "string" }, "description": "referenced files that changed on disk since indexing" },
            "stale_warning": { "type": "string", "description": "human-readable staleness notice" }
        }
    })
}

/// Add the shared `_meta` fragment to an output schema's `properties` without
/// marking it required. Output schemas are plain object schemas from schemars
/// (no `additionalProperties: false`), so the merge never conflicts.
fn merge_meta_property(schema: &mut serde_json::Map<String, serde_json::Value>) {
    let props = schema
        .entry("properties")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(props) = props {
        props.insert("_meta".to_string(), meta_property_schema());
    }
}

/// Finalize the router's tool list for the MCP `tools/list` response: strip
/// schemars' non-standard numeric `format` keys from each schema and stamp the
/// read-only / closed-world annotations. The server never mutates project
/// source and reads a local index rather than the open internet, so
/// `readOnlyHint`/`openWorldHint` hold for the surface — except the session
/// tools, which write snapshots under `.codesage/` and are stamped
/// `readOnlyHint: false`. Read-only-gated clients (e.g. Cursor's Ask mode)
/// refuse to call a tool that doesn't advertise `readOnlyHint`, so the
/// annotation is a reachability fix.
pub(super) fn finalize_tools_for_listing(tools: &mut [rmcp::model::Tool]) {
    for tool in tools.iter_mut() {
        let mut input = serde_json::Value::Object((*tool.input_schema).clone());
        strip_nonstandard_schema_formats(&mut input);
        if let serde_json::Value::Object(map) = input {
            tool.input_schema = Arc::new(map);
        }
        if let Some(output) = tool.output_schema.take() {
            let mut out = serde_json::Value::Object((*output).clone());
            strip_nonstandard_schema_formats(&mut out);
            if let serde_json::Value::Object(mut map) = out {
                merge_meta_property(&mut map);
                tool.output_schema = Some(Arc::new(map));
            }
        }
        let read_only = !NON_READONLY_TOOLS.contains(&tool.name.as_ref());
        tool.annotations = Some(
            rmcp::model::ToolAnnotations::new()
                .read_only(read_only)
                .open_world(false),
        );
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mcp::CodeSageServer;

    #[test]
    fn strips_nonstandard_numeric_formats() {
        // Shape mirrors what schemars emits for `Option<usize>` / `Option<f32>`
        // params and `u32` line numbers in nested output schemas.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "format": "uint" },
                "min_jaccard": { "type": "number", "format": "float" },
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "line": { "type": "integer", "format": "uint32" },
                            "delta": { "type": "integer", "format": "int64" }
                        }
                    }
                },
                "created_at": { "type": "string", "format": "date-time" }
            }
        });
        strip_nonstandard_schema_formats(&mut schema);

        let props = &schema["properties"];
        // uint* formats dropped, minimum:0 added to preserve non-negativity.
        assert!(props["limit"].get("format").is_none());
        assert_eq!(props["limit"]["minimum"], json!(0));
        // float dropped, no minimum injected.
        assert!(props["min_jaccard"].get("format").is_none());
        assert!(props["min_jaccard"].get("minimum").is_none());
        // Recurses into array items.
        let item = &props["results"]["items"]["properties"];
        assert!(item["line"].get("format").is_none());
        assert_eq!(item["line"]["minimum"], json!(0));
        assert!(item["delta"].get("format").is_none());
        assert!(item["delta"].get("minimum").is_none());
        // Standard formats are left untouched.
        assert_eq!(props["created_at"]["format"], json!("date-time"));
    }

    /// Every tool's outputSchema must declare the render-injected `_meta`
    /// envelope as an optional property: agents that consult outputSchema
    /// before calling would otherwise meet undeclared top-level fields on
    /// truncated or stale responses.
    #[test]
    fn every_tool_output_schema_declares_optional_meta() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        assert!(!tools.is_empty());
        for tool in &tools {
            let out = tool
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` is missing outputSchema", tool.name));
            let meta = out
                .get("properties")
                .and_then(|p| p.get("_meta"))
                .unwrap_or_else(|| {
                    panic!(
                        "tool `{}` outputSchema lacks the `_meta` property",
                        tool.name
                    )
                });
            assert_eq!(meta["type"], json!("object"), "tool `{}`", tool.name);
            for field in [
                "truncated",
                "total_results",
                "returned",
                "also_truncated_fields",
                "dropped_files",
                "dropped_count",
                "stale_files",
                "stale_warning",
            ] {
                assert!(
                    meta["properties"].get(field).is_some(),
                    "tool `{}` _meta fragment lacks `{field}`",
                    tool.name
                );
            }
            // Optional: injected only on trimmed/flagged responses.
            if let Some(required) = out.get("required").and_then(|r| r.as_array()) {
                assert!(
                    !required.iter().any(|v| v == "_meta"),
                    "tool `{}` must not require `_meta`",
                    tool.name
                );
            }
            assert_ne!(
                out.get("additionalProperties"),
                Some(&json!(false)),
                "tool `{}`: additionalProperties: false would reject render-injected fields",
                tool.name
            );
        }
    }

    /// Every tool must advertise annotations through the `tools/list`
    /// finalization path: `readOnlyHint: true` + `openWorldHint: false` for
    /// the query surface, `readOnlyHint: false` for the session tools (they
    /// write `.codesage/sessions/<id>.json` inside the project — advertising
    /// them read-only would be a lie to read-only-gated clients). Exercises
    /// the same `finalize_tools_for_listing` the `list_tools` override uses.
    #[test]
    fn every_tool_advertises_correct_readonly_annotation() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        assert!(!tools.is_empty(), "router should expose at least one tool");
        let mut read_only_count = 0;
        let mut writer_count = 0;
        for tool in &tools {
            let ann = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name));
            let expect_read_only = !matches!(tool.name.as_ref(), "session_start" | "session_end");
            assert_eq!(
                ann.read_only_hint,
                Some(expect_read_only),
                "tool `{}` must advertise readOnlyHint: {expect_read_only}",
                tool.name
            );
            assert_eq!(
                ann.open_world_hint,
                Some(false),
                "tool `{}` must advertise openWorldHint: false",
                tool.name
            );
            if expect_read_only {
                read_only_count += 1;
            } else {
                writer_count += 1;
            }
        }
        assert_eq!(writer_count, 2, "session_start + session_end are writers");
        assert_eq!(
            read_only_count,
            tools.len() - 2,
            "everything else stays read-only"
        );
    }
}
