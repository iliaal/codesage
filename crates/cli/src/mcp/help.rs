use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use rmcp::schemars;
use serde_json::{Map, Value, json};

use super::CodeSageServer;
use super::error::{ErrorCode, McpError};
use super::params::HelpParams;

static CATALOG: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../../plugins/codesage-tools/skills/codesage-retrieval/references/help.json"
    ))
    .expect("embedded help catalog is valid JSON")
});

pub(super) fn field_description(path: &str) -> Option<&'static str> {
    CATALOG["fields"]
        .get(path)
        .or_else(|| {
            let mut suffix = path.strip_prefix("completeness.")?;
            while let Some(rest) = suffix.strip_prefix("prior.") {
                suffix = rest;
            }
            CATALOG["fields"].get(format!("completeness.{suffix}"))
        })
        .and_then(Value::as_str)
}

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Review,
    FixBug,
    Rename,
    AddFeature,
    BeforeCommit,
    DebugFailure,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(super) struct HelpResult {
    /// Available selectors when no selector was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog: Option<Value>,
    /// Tool semantics, its input schema, and available output fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<Value>,
    /// Selected field semantics and schema, including scope and parent semantics.
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<Value>,
    /// Error semantics and recovery guidance.
    #[serde(skip_serializing_if = "Option::is_none")]
    error_help: Option<Value>,
    /// Recovery ladder for empty, incomplete, or stale evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<Value>,
    /// Ordered plan with argument templates, binding sources, rationale, and stopping conditions.
    #[serde(skip_serializing_if = "Option::is_none")]
    recipe: Option<Value>,
    /// Measured daemon-lifetime wall-time histograms, or explicit unknowns. Not a latency prediction.
    price_list: Value,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    McpError::new(ErrorCode::Param, message).into()
}

pub(super) fn project_root(path: &Path) -> anyhow::Result<PathBuf> {
    let invalid = |message: &str| McpError::new(ErrorCode::ProjectPath, message);
    if !path.is_absolute() {
        return Err(invalid("project must be an absolute path").into());
    }
    let path = path
        .canonicalize()
        .map_err(|error| invalid("project directory is unavailable").source(error.into()))?;
    if !path.is_dir() {
        return Err(invalid("project must be a directory").into());
    }
    Ok(path)
}

pub(super) fn answer(server: &CodeSageServer, params: &HelpParams) -> anyhow::Result<HelpResult> {
    let selectors = usize::from(params.tool.is_some() || params.field.is_some())
        + usize::from(params.code.is_some())
        + usize::from(params.intent.is_some());
    if selectors > 1 {
        return Err(invalid(
            "help: select tool/field, code, or intent; only tool and field combine",
        ));
    }
    let mut tools = server.tool_router.list_all();
    super::schema::finalize_tools_for_listing(&mut tools);
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut result = HelpResult {
        catalog: None,
        subject: None,
        field: None,
        error_help: None,
        recovery: None,
        recipe: None,
        price_list: price_list(&tools, &server.state.diagnostics.snapshot(0)),
    };
    if let Some(code) = &params.code {
        result.error_help = Some(CATALOG["errors"].get(code).cloned().ok_or_else(|| {
            invalid(format!(
                "help: unknown error code {code:?}; omit selectors to list codes"
            ))
        })?);
        result.error_help.as_mut().unwrap()["code"] = json!(code);
    } else if let Some(intent) = &params.intent {
        let key = serde_json::to_value(intent)?;
        let mut recipe = CATALOG["intents"][key.as_str().unwrap()].clone();
        recipe["intent"] = key;
        recipe["execution"] = CATALOG["recipe_execution"].clone();
        bind_project(&mut recipe, &params.project);
        result.recipe = Some(recipe);
        result.recovery = Some(CATALOG["recovery"].clone());
    } else if params.tool.as_deref() == Some("daemon") {
        if params.field.is_some() {
            return Err(invalid(
                "help: daemon is an operator catalog; select daemon_stats to inspect its fields",
            ));
        }
        result.subject = Some(CATALOG["daemon"].clone());
    } else if let Some(name) = &params.tool {
        if let Some(tool) = tools.iter().find(|tool| tool.name == *name) {
            let input = Value::Object((*tool.input_schema).clone());
            let output = tool
                .output_schema
                .as_ref()
                .map(|schema| Value::Object((**schema).clone()));
            if let Some(field) = &params.field {
                result.field = Some(tool_field(name, &input, output.as_ref(), field)?);
            } else {
                result.subject = Some(json!({
                    "name": name,
                    "description": tool.description,
                    "advertised": !super::HIDDEN_TOOLS.contains(&name.as_str()),
                    "input_schema": input,
                    "output_fields": output.as_ref().and_then(|schema| schema["properties"].as_object()).map(|fields| fields.keys().collect::<Vec<_>>()).unwrap_or_default(),
                    "field_query": {"tool": name, "field": "output.<field>"},
                    "annotations": tool.annotations,
                }));
            }
        } else if name == "daemon_stats" {
            let operator = &CATALOG["daemon"]["tools"]["daemon_stats"];
            if let Some(field) = &params.field {
                result.field = Some(tool_field(
                    name,
                    &operator["input_schema"],
                    Some(&operator["output_schema"]),
                    field,
                )?);
            } else {
                result.subject = Some(operator.clone());
            }
        } else {
            return Err(invalid(format!(
                "help: unknown tool {name:?}; omit selectors to list tools"
            )));
        }
        result.recovery = Some(CATALOG["recovery"].clone());
    } else if let Some(field) = &params.field {
        let mut properties: Map<String, Value> = super::envelope::schema_properties()
            .into_iter()
            .map(|(name, schema)| (name.to_string(), schema))
            .collect();
        properties.insert("next".into(), super::next::schema());
        properties.insert("_meta".into(), super::schema::meta_property_schema());
        let root = json!({"type": "object", "properties": properties});
        result.field = Some(field_help(&root, field, "envelope").ok_or_else(|| {
            invalid(format!(
                "help: unknown envelope field {field:?}; select tool for a tool-specific field"
            ))
        })?);
        result.recovery = Some(CATALOG["recovery"].clone());
    } else {
        result.catalog = Some(json!({
            "tools": tools.iter().filter(|tool| !super::HIDDEN_TOOLS.contains(&tool.name.as_ref())).map(|tool| tool.name.as_ref()).collect::<Vec<_>>(),
            "hidden_tools": super::HIDDEN_TOOLS,
            "operator_tools": ["daemon_stats"],
            "fields": super::envelope::schema_properties().iter().map(|(name, _)| *name).chain(["next", "_meta", "error", "status", "phase", "work_continuing", "request_id", "persistence_committed", "complete"]).collect::<Vec<_>>(),
            "codes": CATALOG["errors"].as_object().unwrap().keys().collect::<Vec<_>>(),
            "intents": CATALOG["intents"].as_object().unwrap().keys().collect::<Vec<_>>(),
            "usage": "Select tool, tool plus field (input.query or output.results[].score), envelope field, error code, or intent. Templates in recipes require bindings before execution.",
        }));
        result.recovery = Some(CATALOG["recovery"].clone());
    }
    Ok(result)
}

fn bind_project(value: &mut Value, project: &str) {
    match value {
        Value::String(text) if text == "$project" => *text = project.to_owned(),
        Value::Object(map) => map
            .values_mut()
            .for_each(|value| bind_project(value, project)),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|value| bind_project(value, project)),
        _ => {}
    }
}

fn tool_field(
    name: &str,
    input: &Value,
    output: Option<&Value>,
    field: &str,
) -> anyhow::Result<Value> {
    let found = if let Some(path) = field.strip_prefix("input.") {
        field_help(input, path, "input")
    } else if let Some(path) = field.strip_prefix("output.") {
        output.and_then(|root| field_help(root, path, "output"))
    } else {
        output
            .and_then(|root| field_help(root, field, "output"))
            .or_else(|| field_help(input, field, "input"))
    };
    let mut found = found.ok_or_else(|| {
        invalid(format!(
            "help: unknown field {field:?}; use input.<field> or output.<field>"
        ))
    })?;
    let key = format!(
        "{}.{}",
        found["scope"].as_str().unwrap(),
        found["path"].as_str().unwrap().replace("[]", "")
    );
    if let Some(description) = CATALOG["tool_fields"][name].get(key) {
        found["description"] = description.clone();
    }
    Ok(found)
}

fn field_help(root: &Value, path: &str, scope: &str) -> Option<Value> {
    if path.is_empty() || path.len() > 512 {
        return None;
    }
    let parts: Vec<_> = path
        .split('.')
        .flat_map(|part| match part.strip_suffix("[]") {
            Some(name) => vec![name, "[]"],
            None => vec![part],
        })
        .collect();
    let field_key = path.replace("[]", "");
    let Some((schema, parent)) = lookup(root, root, &parts, None, 0) else {
        return field_description(&field_key)
            .filter(|_| scope != "input")
            .map(|description| json!({"path": path, "scope": scope, "description": description}));
    };
    let description = field_description(&field_key)
        .or_else(|| schema["description"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            if parts.last() != Some(&"[]") {
                return None;
            }
            let (array, _) = lookup(root, root, &parts[..parts.len() - 1], None, 0)?;
            array["description"]
                .as_str()
                .map(|description| format!("Element of: {description}"))
        });
    Some(json!({
        "path": path,
        "scope": scope,
        "description": description,
        "schema": schema,
        "parent_semantics": parent,
        "definitions": root.get("$defs"),
    }))
}

fn lookup<'a>(
    root: &'a Value,
    schema: &'a Value,
    parts: &[&str],
    parent: Option<&'a str>,
    depth: usize,
) -> Option<(Value, Option<&'a str>)> {
    if depth > 32 {
        return None;
    }
    let parent = schema["description"].as_str().or(parent);
    if let Some(reference) = schema["$ref"]
        .as_str()
        .and_then(|value| value.strip_prefix('#'))
    {
        let (mut found, context) =
            lookup(root, root.pointer(reference)?, parts, parent, depth + 1)?;
        if parts.is_empty() {
            let siblings: Map<_, _> = schema
                .as_object()?
                .iter()
                .filter(|(key, _)| key.as_str() != "$ref")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let conflict = siblings.iter().any(|(key, value)| {
                !matches!(
                    key.as_str(),
                    "description"
                        | "title"
                        | "default"
                        | "examples"
                        | "deprecated"
                        | "readOnly"
                        | "writeOnly"
                ) && found.get(key).is_some_and(|existing| existing != value)
            });
            if !siblings.is_empty() {
                if conflict || !found.is_object() {
                    found = json!({"allOf": [found, siblings]});
                } else {
                    found.as_object_mut()?.extend(siblings);
                }
            }
        }
        return Some((found, context));
    }
    if parts.is_empty() {
        return Some((schema.clone(), parent));
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(alternatives) = schema[keyword].as_array() {
            let mut found: Vec<_> = alternatives
                .iter()
                .filter_map(|alternative| lookup(root, alternative, parts, parent, depth + 1))
                .collect();
            if found.len() == 1 {
                return found.pop();
            }
            if !found.is_empty() {
                let keyword = if keyword == "allOf" { "allOf" } else { "anyOf" };
                let schemas: Vec<_> = found.into_iter().map(|(schema, _)| schema).collect();
                return Some((json!({keyword: schemas}), parent));
            }
        }
    }
    if let Some(items) = schema.get("items") {
        let parts = if parts[0] == "[]" { &parts[1..] } else { parts };
        return lookup(root, items, parts, parent, depth + 1);
    }
    let child = schema.get("properties")?.get(parts[0])?;
    lookup(root, child, &parts[1..], parent, depth + 1)
}

fn price_list(tools: &[rmcp::model::Tool], snapshot: &Value) -> Value {
    let mut rows = BTreeMap::new();
    for name in tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .chain(["daemon_stats"])
    {
        let measured = snapshot["tools"][name]
            .get("request_wall_ms")
            .filter(|counts| {
                counts.as_array().is_some_and(|counts| {
                    counts.iter().any(|count| count.as_u64().unwrap_or(0) > 0)
                })
            });
        rows.insert(name, match measured {
            Some(counts) => json!({
                "status": "measured",
                "completed_requests": counts.as_array().unwrap().iter().filter_map(Value::as_u64).sum::<u64>(),
                "request_wall_ms": counts,
                "execution_wall_ms": snapshot["tools"][name]["execution_wall_ms"],
                "queue_ms": snapshot["tools"][name]["queue_ms"],
            }),
            None => json!({"status": "unknown", "reason": if snapshot["enabled"] == false { "diagnostics_disabled" } else { "no_completed_requests" }}),
        });
    }
    json!({
        "scope": "This daemon lifetime across all projects and outcomes; histograms are observations, not predictions. Request wall time includes admission and queue wait. The current help call is not completed yet.",
        "histogram_semantics": "Non-cumulative bucket counts; each upper bound is inclusive, the last bucket is unbounded. Execution and queue histograms measure physical work, which may be shared by requests.",
        "histogram_upper_bounds_ms": snapshot.get("histogram_upper_bounds_ms"),
        "tools": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn params(value: Value) -> HelpParams {
        let mut value = value;
        value["project"] = json!("/project");
        serde_json::from_value(value).unwrap()
    }

    fn value(server: &CodeSageServer, params: HelpParams) -> Value {
        serde_json::to_value(answer(server, &params).unwrap()).unwrap()
    }

    #[test]
    fn every_registered_tool_is_queryable_without_a_parallel_tool_catalog() {
        let server = CodeSageServer::new();
        for tool in server.tool_router.list_all() {
            let result = value(&server, params(json!({"tool": tool.name})));
            assert_eq!(result["subject"]["description"], json!(tool.description));
            assert_eq!(
                result["subject"]["advertised"],
                !super::super::HIDDEN_TOOLS.contains(&tool.name.as_ref())
            );
            assert!(result["subject"]["input_schema"]["properties"]["project"].is_object());
            assert!(
                result["price_list"]["tools"]
                    .get(tool.name.as_ref())
                    .is_some()
            );
        }
        let result = value(&server, params(json!({"tool": "daemon"})));
        assert_eq!(
            result["subject"]["tools"]["daemon_stats"]["call"],
            json!({"tool": "daemon_stats", "arguments": {"recent": 20}})
        );
    }

    #[test]
    fn all_envelope_fields_and_completeness_kinds_have_semantics() {
        fn paths(schema: &Value, prefix: &str, out: &mut Vec<String>) {
            if let Some(items) = schema.get("items") {
                paths(items, &format!("{prefix}[]"), out);
            }
            if let Some(properties) = schema["properties"].as_object() {
                for (name, child) in properties {
                    let path = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    out.push(path.clone());
                    paths(child, &path, out);
                }
            }
        }
        let server = CodeSageServer::new();
        let mut fields = Vec::new();
        for (name, schema) in super::super::envelope::schema_properties() {
            fields.push(name.to_string());
            paths(&schema, name, &mut fields);
            if name == "completeness" {
                for kind in schema["properties"]["kind"]["enum"].as_array().unwrap() {
                    assert!(
                        CATALOG["recovery"][kind.as_str().unwrap()]["meaning"]
                            .as_str()
                            .is_some()
                    );
                    assert!(
                        CATALOG["recovery"][kind.as_str().unwrap()]["steps"]
                            .as_array()
                            .unwrap()
                            .len()
                            >= 2
                    );
                }
            }
        }
        paths(
            &super::super::schema::meta_property_schema(),
            "_meta",
            &mut fields,
        );
        fields.extend(CATALOG["fields"].as_object().unwrap().keys().cloned());
        fields.extend(["next".to_string(), "_meta".to_string()]);
        for field in fields {
            let result = value(&server, params(json!({"field": field})));
            assert!(
                result["field"]["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "{field}: {result}"
            );
        }
    }

    #[test]
    fn budget_recovery_schema_and_help_use_the_same_catalog_descriptions() {
        let mut properties: Map<_, _> = super::super::envelope::schema_properties()
            .into_iter()
            .map(|(name, schema)| (name.to_owned(), schema))
            .collect();
        properties.insert("_meta".into(), super::super::schema::meta_property_schema());
        let schema = json!({"properties": properties});
        for field in CATALOG["fields"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|field| {
                field.starts_with("completeness.recover.")
                    || field.as_str() == "completeness.prior"
                    || field.starts_with("_meta.recover")
                    || field.starts_with("_meta.trimmed")
                    || field.starts_with("_meta.shortened")
            })
        {
            let parts: Vec<_> = field.split('.').collect();
            let (declared, _) = lookup(&schema, &schema, &parts, None, 0)
                .unwrap_or_else(|| panic!("catalog field {field} is absent from output schema"));
            assert_eq!(declared["description"], CATALOG["fields"][field], "{field}");
        }
    }

    #[test]
    fn every_emitted_budget_recovery_field_has_queryable_schema_and_semantics() {
        fn inspect(schema: &Value, value: &Value, path: &str) {
            let field = field_help(schema, path, "envelope")
                .unwrap_or_else(|| panic!("undocumented emitted field {path}"));
            assert!(
                field["schema"].is_object(),
                "{path} only has a prose fallback"
            );
            assert!(
                field["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "{path} has no semantics"
            );
            if matches!(
                path.rsplit('.').next(),
                Some("arguments" | "requested" | "applied")
            ) {
                return;
            }
            match value {
                Value::Object(fields) => {
                    for (name, value) in fields {
                        inspect(schema, value, &format!("{path}.{name}"));
                    }
                }
                Value::Array(items) => {
                    for value in items {
                        inspect(schema, value, &format!("{path}[]"));
                    }
                }
                _ => {}
            }
        }
        let mut properties: Map<_, _> = super::super::envelope::schema_properties()
            .into_iter()
            .map(|(name, schema)| (name.to_owned(), schema))
            .collect();
        properties.insert("_meta".into(), super::super::schema::meta_property_schema());
        let schema = json!({"properties": properties});
        for envelope_enabled in [true, false] {
            let mut payload = json!({"files": (0..20).map(|i| json!({"handle": format!("file:src/{i}.rs"), "file": format!("src/{i}.rs"), "content": "source ".repeat(500)})).collect::<Vec<_>>()});
            if envelope_enabled {
                payload["completeness"] = json!({"kind": "partial", "kinds": ["partial"], "recover": {"reason": "lexical_lookup_failed", "command": "codesage index --full"}});
            }
            let arguments = json!({"project": "/project", "targets": ["src/0.rs"], "detail": "standard", "budget_tokens": 1024}).as_object().unwrap().clone();
            let result = super::super::detail::finish(
                rmcp::model::CallToolResult::structured(payload),
                "assess_risk_diff",
                &arguments,
                std::time::Duration::ZERO,
                4096,
                envelope_enabled,
            );
            let payload = result.structured_content.unwrap();
            let root = if envelope_enabled {
                "completeness"
            } else {
                "_meta"
            };
            let recovery = if envelope_enabled {
                &payload[root]["recover"]
            } else {
                &payload[root]
            };
            assert!(!recovery["trimmed"].as_array().unwrap().is_empty());
            assert!(!recovery["shortened"].as_array().unwrap().is_empty());
            assert!(
                !recovery["trimmed"][0]["omitted_handles"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            inspect(&schema, &payload[root], root);
        }
    }

    #[test]
    fn every_emitted_error_code_has_a_shared_catalog_entry() {
        let source = include_str!("error.rs");
        let mapping = source
            .split("pub(crate) fn as_str")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn status")
            .next()
            .unwrap();
        let emitted: BTreeSet<_> = mapping
            .split('"')
            .filter(|part| part.starts_with("E_"))
            .collect();
        let documented: BTreeSet<_> = CATALOG["errors"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(emitted, documented);
        let server = CodeSageServer::new();
        for code in emitted {
            let result = value(&server, params(json!({"code": code})));
            assert_eq!(result["error_help"]["code"], code);
            assert!(result["error_help"]["meaning"].is_string());
            assert!(result["error_help"]["recovery"].is_string());
        }
    }

    #[test]
    fn recipes_bind_project_and_validate_argument_templates_against_live_schemas() {
        let server = CodeSageServer::new();
        let tools = server.advertised_tools();
        let intent_schema = rmcp::handler::server::tool::schema_for_type::<Intent>();
        let intents: BTreeSet<_> = intent_schema["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        let catalog_intents: BTreeSet<_> = CATALOG["intents"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(intents, catalog_intents);
        for intent in intents {
            let result = value(&server, params(json!({"intent": intent})));
            let recipe = &result["recipe"];
            assert_eq!(recipe["execution"]["mode"], "plan_only");
            let steps = recipe["steps"].as_array().unwrap();
            assert!(steps.len() >= 3);
            for step in steps {
                let name = step["tool"].as_str().unwrap();
                assert!(step["why"].as_str().is_some_and(|text| !text.is_empty()));
                assert!(
                    step["stop_when"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                );
                let arguments = step["arguments"].as_object().unwrap();
                let schema = if name == "daemon_stats" {
                    CATALOG["daemon"]["tools"][name]["input_schema"].clone()
                } else {
                    Value::Object(
                        (*tools
                            .iter()
                            .find(|tool| tool.name == name)
                            .expect("recipe tool is registered")
                            .input_schema)
                            .clone(),
                    )
                };
                for (key, argument) in arguments {
                    assert!(
                        schema["properties"].get(key).is_some(),
                        "{intent}: {name}.{key} is not an accepted argument"
                    );
                    if let Some(binding) = argument.as_str().and_then(|text| text.strip_prefix('$'))
                    {
                        assert!(
                            recipe["bindings"].get(binding).is_some(),
                            "{intent}: undocumented binding {binding}"
                        );
                    }
                }
                if let Some(required) = schema["required"].as_array() {
                    for field in required {
                        assert!(
                            arguments.contains_key(field.as_str().unwrap()),
                            "{intent}: {name} lacks {field}"
                        );
                    }
                }
                if name != "daemon_stats" {
                    assert_eq!(arguments["project"], "/project");
                }
            }
        }
        assert!(
            include_str!("../../../../plugins/codesage-tools/skills/codesage-retrieval/SKILL.md")
                .contains("(references/help.json)")
        );
    }

    #[test]
    fn field_lookup_resolves_refs_arrays_and_differentiates_input_output() {
        let server = CodeSageServer::new();
        let query = value(
            &server,
            params(json!({"tool": "search", "field": "input.query"})),
        );
        assert_eq!(query["field"]["scope"], "input");
        assert_eq!(query["field"]["schema"]["type"], "string");
        let score = value(
            &server,
            params(json!({"tool": "search", "field": "output.results[].score"})),
        );
        assert_eq!(score["field"]["scope"], "output");
        assert_eq!(score["field"]["schema"]["type"], "number");
        assert!(
            score["field"]["description"]
                .as_str()
                .unwrap()
                .contains("Final ranking score")
        );
        assert!(
            !score["field"]["description"]
                .as_str()
                .unwrap()
                .contains("Serialize")
        );
        let recent = value(
            &server,
            params(json!({"tool": "daemon_stats", "field": "input.recent"})),
        );
        assert_eq!(recent["field"]["schema"]["maximum"], 256);
        let recovery = value(
            &server,
            params(json!({"tool": "search", "field": "output.completeness.recover.reason"})),
        );
        assert!(
            recovery["field"]["description"]
                .as_str()
                .unwrap()
                .contains("lexical_lookup_failed")
        );
    }

    #[test]
    fn field_lookup_preserves_all_union_variants_and_nullable_array_paths() {
        let schema = json!({"properties": {"next": {"oneOf": [
            {"type": "null"},
            {"properties": {"tool": {"const": "first"}}},
            {"properties": {"tool": {"const": "second"}}}
        ]}, "rows": {"type": ["array", "null"], "items": {"$ref": "#/$defs/Row"}}},
        "$defs": {"Row": {"properties": {"stage": {"enum": ["alpha", "beta"]}}}}});
        let next = field_help(&schema, "next.tool", "output").unwrap();
        assert_eq!(
            next["schema"],
            json!({"anyOf": [{"const": "first"}, {"const": "second"}]})
        );
        let stage = field_help(&schema, "rows[].stage", "output").unwrap();
        assert_eq!(stage["schema"]["enum"], json!(["alpha", "beta"]));
        let row = field_help(&schema, "rows[]", "output").unwrap();
        assert!(row["schema"]["properties"].get("stage").is_some());
        assert!(field_help(&schema, "rows[].missing", "output").is_none());
    }

    #[test]
    fn undeclared_field_semantics_do_not_borrow_ancestor_implementation_prose() {
        let root = json!({"description": "Internal serialization implementation.", "properties": {"bare": {"type": "string"}, "names": {"type": "array", "description": "Names of omitted files.", "items": {"type": "string"}}, "named": {"$ref": "#/$defs/Name", "description": "This field's specific contract.", "default": "compact"}, "narrowed": {"$ref": "#/$defs/Name", "type": "number"}}, "$defs": {"Name": {"type": "string", "description": "Shared type documentation."}}});
        let result = field_help(&root, "bare", "output").unwrap();
        assert!(result["description"].is_null());
        assert_eq!(
            result["parent_semantics"],
            "Internal serialization implementation."
        );
        let named = field_help(&root, "named", "output").unwrap();
        assert_eq!(named["description"], "This field's specific contract.");
        assert_eq!(named["schema"]["default"], "compact");
        let narrowed = field_help(&root, "narrowed", "output").unwrap();
        assert_eq!(narrowed["schema"]["allOf"][0]["type"], "string");
        assert_eq!(narrowed["schema"]["allOf"][1]["type"], "number");
        let item = field_help(&root, "names[]", "output").unwrap();
        assert_eq!(item["description"], "Element of: Names of omitted files.");
    }

    #[test]
    fn price_list_never_invents_measurements() {
        let server = CodeSageServer::new();
        let tools = server.advertised_tools();
        let disabled = price_list(&tools, &json!({"enabled": false}));
        assert_eq!(disabled["tools"]["search"]["status"], "unknown");
        assert_eq!(
            disabled["tools"]["search"]["reason"],
            "diagnostics_disabled"
        );
        let diagnostics = super::super::diagnostics::Diagnostics::default();
        diagnostics
            .begin_request("search", Some("/project"))
            .finish("success");
        let snapshot = diagnostics.snapshot(0);
        let price = price_list(&tools, &snapshot);
        assert_eq!(price["tools"]["search"]["status"], "measured");
        assert_eq!(price["tools"]["search"]["completed_requests"], 1);
        assert_eq!(
            price["tools"]["search"]["request_wall_ms"],
            snapshot["tools"]["search"]["request_wall_ms"]
        );
        assert_eq!(price["tools"]["describe"]["status"], "unknown");
        assert_eq!(
            price["tools"]["describe"]["reason"],
            "no_completed_requests"
        );
    }

    #[test]
    fn invalid_selectors_refuse_instead_of_answering_a_different_question() {
        let server = CodeSageServer::new();
        for selector in [
            json!({"tool": "missing"}),
            json!({"field": "missing"}),
            json!({"tool": "search", "field": "input.missing"}),
            json!({"code": "E_MISSING"}),
            json!({"tool": "search", "code": "E_MODEL"}),
            json!({"intent": "review", "field": "cost.ms"}),
            json!({"tool": "daemon", "field": "recent"}),
        ] {
            let error = match answer(&server, &params(selector)) {
                Ok(_) => panic!("invalid selector accepted"),
                Err(error) => error,
            };
            assert_eq!(super::super::error::classify(&error).code, ErrorCode::Param);
        }
    }
}
