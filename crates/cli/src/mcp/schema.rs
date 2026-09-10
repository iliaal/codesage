use std::sync::Arc;

/// Strict clients warn about schemars' Rust-specific numeric formats; advertise only standard formats.
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

/// Optional render-layer annotations shared by all output schemas.
fn meta_property_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Response envelope annotations, present only when the server \
            trimmed, capped, or flagged this response. `_meta.truncated` means the response \
            exceeded the per-call token budget and an array field was trimmed; it is \
            distinct from any same-named field inside a tool's own result (e.g. \
            impact_analysis's `truncated`, which reports that the tool's `limit` \
            parameter capped the result set). `_meta.clamps` lists numeric params \
            the caller over-asked (limit/offset/depth over the ceiling, min_jaccard \
            outside [0, 1]) with their requested-vs-applied values. \
            `_meta.test_override` marks a response served through the debug-only \
            test query-embedding override instead of the resident model; \
            production responses never carry it.",
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
            "clamps": { "type": "array", "items": { "type": "object", "properties": { "param": { "type": "string" }, "requested": {}, "applied": {} } }, "description": "numeric params adjusted from requested to applied (over-max limits capped, min_jaccard clamped to [0, 1])" },
            "stale_files": { "type": "array", "items": { "type": "string" }, "description": "referenced files that changed on disk since indexing" },
            "stale_warning": { "type": "string", "description": "human-readable staleness notice" },
            "ranking_recomputed": { "type": "boolean", "description": "cached overview or session ranking could not be reused and was recomputed for this request's read snapshot; does not imply analysis failed or that the index changed during computation" },
            "test_override": { "type": "boolean", "description": "response was served through the debug-only test query-embedding override (debug builds only); production responses never carry it" }
        }
    })
}

/// Merge optional envelope fields into schemars' open output objects.
fn merge_meta_property(schema: &mut serde_json::Map<String, serde_json::Value>) {
    let props = schema
        .entry("properties")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(props) = props {
        props.insert("_meta".to_string(), meta_property_schema());
        props.insert("next".to_string(), super::next::schema());
    }
}

/// Normalize schemas and advertise query tools as read-only for gated clients.
/// Session tools write snapshots and must remain marked as writers.
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
        assert!(props["limit"].get("format").is_none());
        assert_eq!(props["limit"]["minimum"], json!(0));
        assert!(props["min_jaccard"].get("format").is_none());
        assert!(props["min_jaccard"].get("minimum").is_none());
        let item = &props["results"]["items"]["properties"];
        assert!(item["line"].get("format").is_none());
        assert_eq!(item["line"]["minimum"], json!(0));
        assert!(item["delta"].get("format").is_none());
        assert!(item["delta"].get("minimum").is_none());
        assert_eq!(props["created_at"]["format"], json!("date-time"));
    }

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
            for field in [
                "truncated",
                "total_results",
                "returned",
                "also_truncated_fields",
                "dropped_files",
                "dropped_count",
                "clamps",
                "stale_files",
                "stale_warning",
                "ranking_recomputed",
                "test_override",
            ] {
                assert!(
                    meta["properties"].get(field).is_some(),
                    "tool `{}` _meta fragment lacks `{field}`",
                    tool.name
                );
            }
            assert_eq!(meta["properties"]["ranking_recomputed"]["type"], "boolean");
            assert!(
                !meta
                    .get("required")
                    .and_then(|value| value.as_array())
                    .is_some_and(|fields| fields.iter().any(|field| field == "ranking_recomputed")),
                "tool `{}` must not require `_meta.ranking_recomputed`",
                tool.name
            );
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

    #[test]
    fn every_tool_input_schema_closes_additional_properties() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        assert!(!tools.is_empty());
        let open: Vec<String> = tools
            .iter()
            .filter(|t| t.input_schema.get("additionalProperties") != Some(&json!(false)))
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            open,
            Vec::<String>::new(),
            "these tools advertise an open inputSchema while the server refuses unknown fields"
        );
    }

    #[test]
    fn offset_paged_kinds_match_tools_declaring_offset() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        assert!(!tools.is_empty());
        let mut declared: Vec<String> = tools
            .iter()
            .filter(|t| {
                t.input_schema
                    .get("properties")
                    .and_then(|p| p.get("offset"))
                    .is_some()
            })
            .map(|t| t.name.to_string())
            .collect();
        declared.sort_unstable();
        let mut expected: Vec<String> = crate::mcp::render::OFFSET_PAGED_KINDS
            .iter()
            .map(|k| (*k).to_string())
            .collect();
        expected.sort_unstable();
        assert_eq!(
            declared, expected,
            "OFFSET_PAGED_KINDS must equal the tools whose inputSchema declares `offset`"
        );
    }

    #[test]
    fn output_schemas_reflect_payload_trim() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        let schema = |name: &str| {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .and_then(|t| t.output_schema.clone())
                .unwrap_or_else(|| panic!("tool `{name}` must advertise an outputSchema"))
        };
        let input_schema = |name: &str| {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .map(|t| t.input_schema.clone())
                .unwrap_or_else(|| panic!("tool `{name}` missing"))
        };

        let risk = schema("assess_risk");
        let required = risk["required"].as_array().expect("required array");
        for key in [
            "churn_score",
            "churn_percentile",
            "fix_ratio",
            "total_commits",
            "fix_count",
            "dependent_files",
            "coupled_files",
            "test_gap",
            "in_cycle",
            "cycle_size",
            "top_coupled",
        ] {
            assert!(
                risk["properties"].get(key).is_some(),
                "assess_risk schema must still describe `{key}`"
            );
            assert!(
                !required.iter().any(|r| r == key),
                "`{key}` is verbose-only and must not be required"
            );
        }
        assert!(
            risk["properties"].get("verbose").is_none(),
            "the verbose switch is a request param, not a response field"
        );
        for tool in ["assess_risk", "assess_risk_batch", "assess_risk_diff"] {
            let input = input_schema(tool);
            assert_eq!(
                input["properties"]["verbose"]["type"],
                json!(["boolean", "null"]),
                "{tool} must accept an optional boolean `verbose`"
            );
        }

        let search = schema("search");
        for key in ["confidence", "margin_pct", "cliff_at"] {
            assert!(
                search["properties"].get(key).is_some(),
                "search schema must describe `{key}`"
            );
        }
        let search_required = search["required"].as_array().expect("required array");
        assert!(search_required.iter().any(|r| r == "results"));
        for key in ["confidence", "margin_pct", "cliff_at"] {
            assert!(
                !search_required.iter().any(|r| r == key),
                "`{key}` is optional on the wire"
            );
        }
        assert_eq!(
            input_schema("search")["properties"]["adaptive_limit"]["type"],
            json!(["boolean", "null"]),
            "search must accept an optional boolean `adaptive_limit`"
        );

        let symbols = serde_json::to_string(&*schema("find_symbol")).unwrap();
        assert!(!symbols.contains("col_start"), "{symbols}");
        assert!(!symbols.contains("col_end"), "{symbols}");
        let refs = serde_json::to_string(&*schema("find_references")).unwrap();
        assert!(!refs.contains("\"col\""), "{refs}");
    }

    /// Follow a `$ref` into `$defs` so nested per-file schemas can be checked.
    fn resolve<'a>(
        root: &'a serde_json::Value,
        schema: &'a serde_json::Value,
    ) -> &'a serde_json::Value {
        let Some(reference) = schema.get("$ref").and_then(|r| r.as_str()) else {
            return schema;
        };
        let name = reference
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("unexpected $ref form: {reference}"));
        root.get("$defs")
            .and_then(|defs| defs.get(name))
            .unwrap_or_else(|| panic!("`{name}` missing from $defs: {root}"))
    }

    /// Every key an actual response carries must be described by the
    /// advertised schema; otherwise an agent that reads `outputSchema` plans
    /// against a shape it will not receive.
    fn assert_response_keys_described(
        root: &serde_json::Value,
        schema: &serde_json::Value,
        response: &serde_json::Value,
        what: &str,
    ) {
        let schema = resolve(root, schema);
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("{what}: schema declares no properties: {schema}"));
        let object = response
            .as_object()
            .unwrap_or_else(|| panic!("{what}: response is not an object: {response}"));
        for key in object.keys() {
            assert!(
                props.contains_key(key),
                "{what}: response carries `{key}`, absent from the advertised schema \
                 (properties: {:?})",
                props.keys().collect::<Vec<_>>()
            );
        }
        // A schema that requires a field the response omits is equally wrong.
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            for field in required {
                let field = field.as_str().expect("required entry is a string");
                assert!(
                    object.contains_key(field),
                    "{what}: schema requires `{field}`, missing from the response: {response}"
                );
            }
        }
    }

    /// One PHP class, indexed structurally, with git history for
    /// `Repository.php` only: `New.php` exercises the unscored branch.
    fn risk_fixture() -> (tempfile::TempDir, codesage_storage::Database) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (name, class) in [("Repository.php", "Repository"), ("New.php", "New")] {
            std::fs::write(
                root.join(name),
                format!(
                    "<?php\nnamespace App;\nclass {class} {{\n  public function run() {{ return 1; }}\n}}\n"
                ),
            )
            .unwrap();
        }
        let db = codesage_storage::Database::open_in_memory().unwrap();
        codesage_graph::full_index(root, &db, &[], false).unwrap();
        db.upsert_git_file("Repository.php", 10.0, 2, 8, Some(1_700_000_000))
            .unwrap();
        (dir, db)
    }

    #[test]
    fn risk_output_schemas_match_scored_and_unscored_responses() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        let schema = |name: &str| {
            serde_json::Value::Object(
                (*tools
                    .iter()
                    .find(|t| t.name.as_ref() == name)
                    .and_then(|t| t.output_schema.clone())
                    .unwrap_or_else(|| panic!("tool `{name}` must advertise an outputSchema")))
                .clone(),
            )
        };

        let (_dir, db) = risk_fixture();
        let paths = ["Repository.php".to_string(), "New.php".to_string()];

        let single = schema("assess_risk");
        for (path, expect_unscored) in [("Repository.php", false), ("New.php", true)] {
            for verbose in [false, true] {
                let mut assessment = codesage_graph::assess_risk(&db, path).unwrap();
                assert_eq!(
                    assessment.unscored,
                    expect_unscored,
                    "{path}: fixture must exercise the {} branch",
                    if expect_unscored {
                        "unscored"
                    } else {
                        "scored"
                    }
                );
                assessment.set_verbose(verbose);
                let response = serde_json::to_value(&assessment).unwrap();
                assert_eq!(
                    response.get("unscored").is_some(),
                    expect_unscored,
                    "{path}: `unscored` is emitted only when true: {response}"
                );
                assert_response_keys_described(
                    &single,
                    &single,
                    &response,
                    &format!("assess_risk({path}, verbose={verbose})"),
                );
            }
        }

        let batch = schema("assess_risk_batch");
        let batch_response =
            serde_json::to_value(codesage_graph::assess_risk_batch(&db, &paths).unwrap()).unwrap();
        assert_response_keys_described(&batch, &batch, &batch_response, "assess_risk_batch");
        let item_schema = batch["properties"]["files"]["items"].clone();
        for file in batch_response["files"].as_array().expect("files array") {
            assert_response_keys_described(&batch, &item_schema, file, "assess_risk_batch.files[]");
        }

        let diff = schema("assess_risk_diff");
        let diff_result = codesage_graph::assess_risk_diff(&db, &paths).unwrap();
        assert_eq!(diff_result.unscored_files, vec!["New.php".to_string()]);
        assert_eq!(diff_result.scored_file_count, 1);
        let diff_response = serde_json::to_value(&diff_result).unwrap();
        for key in ["unscored_files", "scored_file_count"] {
            assert!(
                diff_response.get(key).is_some(),
                "the mixed fixture must exercise `{key}`: {diff_response}"
            );
        }
        assert_response_keys_described(&diff, &diff, &diff_response, "assess_risk_diff");
        let diff_item = diff["properties"]["files"]["items"].clone();
        for file in diff_response["files"].as_array().expect("files array") {
            assert_response_keys_described(&diff, &diff_item, file, "assess_risk_diff.files[]");
        }

        // Unmeasured state must not be verbose-gated away or made mandatory.
        let risk_props = resolve(&single, &single)["properties"].clone();
        assert!(
            risk_props.get("unscored").is_some(),
            "assess_risk schema must describe `unscored`"
        );
        let required = single["required"].as_array().expect("required array");
        assert!(
            !required.iter().any(|r| r == "unscored"),
            "`unscored` is emitted only when true and must not be required"
        );
    }

    #[test]
    fn graph_tools_declare_resolution_honesty_fields() {
        let server = CodeSageServer::new();
        let mut tools = server.tool_router.list_all();
        finalize_tools_for_listing(&mut tools);
        let props = |name: &str| {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .and_then(|t| t.output_schema.clone())
                .and_then(|s| s.get("properties").cloned())
                .unwrap_or_else(|| panic!("tool `{name}` must advertise outputSchema properties"))
        };
        for tool in ["find_references", "impact_analysis", "trace_call_path"] {
            assert!(
                props(tool).get("counts_floor").is_some(),
                "`{tool}` outputSchema must declare `counts_floor`"
            );
        }
        let refs = props("find_references");
        for key in ["definition_count", "ambiguous", "note"] {
            assert!(
                refs.get(key).is_some(),
                "find_references outputSchema must declare `{key}`"
            );
        }
    }

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
