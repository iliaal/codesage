use std::path::{Component, Path};

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

const MAX_NEXT_BYTES: usize = 2048;

pub(super) fn schema() -> Value {
    let mut alternatives = vec![json!({"type": "null"})];
    for (tool, argument) in [
        ("find_references", "name"),
        ("list_dependencies", "file_path"),
        ("find_feature", "file_path"),
        ("feature_bundle", "feature_id"),
    ] {
        alternatives.push(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tool", "arguments"],
            "properties": {
                "tool": {"const": tool},
                "arguments": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["project", argument],
                    "properties": {
                        "project": {"type": "string"},
                        (argument): {"type": "string"}
                    }
                }
            }
        }));
    }
    json!({
        "description": "Optional follow-up derived from retained response evidence. Pass tool as tools/call name and arguments unchanged. Null means no supported follow-up: empty/error evidence, a delivered context bundle, or a session snapshot awaiting edits. Suggestions are read-only and terminate; they never authorize an action.",
        "oneOf": alternatives
    })
}

pub(super) fn annotate(project: &str, kind: &str, mut result: CallToolResult) -> CallToolResult {
    if result.is_error == Some(true) {
        result.content.push(ContentBlock::text("{\"next\":null}"));
        return result;
    }
    let Some(payload) = result.structured_content.as_mut() else {
        return result;
    };
    let next = derive(project, kind, payload)
        .filter(|next| serde_json::to_vec(next).is_ok_and(|bytes| bytes.len() <= MAX_NEXT_BYTES))
        .unwrap_or(Value::Null);
    if let Some(object) = payload.as_object_mut() {
        object.insert("next".to_owned(), next);
    }
    // Keep annotation banners while replacing the JSON block clients paste.
    for content in &mut result.content {
        if let Some(text) = content.as_text()
            && serde_json::from_str::<Value>(&text.text).is_ok()
        {
            *content =
                ContentBlock::text(serde_json::to_string_pretty(payload).unwrap_or_default());
        }
    }
    result
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty() && s.len() <= 1024)
}

fn file(value: &Value) -> Option<&str> {
    text(value).filter(|s| {
        Path::new(s)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    })
}

fn first_field<'a>(payload: &'a Value, array: &str, field: &str) -> Option<&'a Value> {
    payload.get(array)?.as_array()?.iter().find_map(|row| {
        (row.get("found") != Some(&Value::Bool(false)))
            .then(|| row.get(field))
            .flatten()
    })
}

fn clustered_file(payload: &Value) -> Option<&Value> {
    payload
        .get("clustered_directories")?
        .as_array()?
        .iter()
        .find_map(|cluster| {
            cluster
                .get("top_files")?
                .as_array()?
                .iter()
                .filter(|row| row.get("found") == Some(&Value::Bool(true)))
                .find_map(|row| row.get("file").filter(|value| file(value).is_some()))
        })
}

fn derive(project: &str, kind: &str, payload: &Value) -> Option<Value> {
    if !Path::new(project).is_absolute() || payload.get("found") == Some(&Value::Bool(false)) {
        return None;
    }
    let call = |tool: &str, argument: &str, value: &str| json!({"tool": tool, "arguments": {"project": project, (argument): value}});
    // Each edge moves toward a bundle, which is terminal, so following next
    // cannot cycle even when the underlying dependency graph does.
    match kind {
        "find_symbol" => {
            let name = first_field(payload, "results", "qualified_name")
                .and_then(text)
                .or_else(|| first_field(payload, "results", "name").and_then(text))?;
            Some(call("find_references", "name", name))
        }
        "list_features" | "find_feature" => {
            let id = first_field(payload, "results", "feature_id").and_then(text)?;
            Some(call("feature_bundle", "feature_id", id))
        }
        "list_dependencies" => {
            let path = file(payload.get("file_path")?)?;
            Some(call("find_feature", "file_path", path))
        }
        "feature_bundle" | "export_context" | "session_start" => None,
        _ => {
            let path = match kind {
                "project_overview" => first_field(payload, "top_risk_files", "file")
                    .or_else(|| first_field(payload, "entrypoints", "entry_path")),
                "search" | "impact_analysis" | "find_similar" => {
                    first_field(payload, "results", "file_path")
                }
                "find_references" => first_field(payload, "results", "from_file"),
                "trace_call_path" => first_field(payload, "steps", "file_path"),
                "from_trace" => payload.get("frames")?.as_array()?.iter().find_map(|frame| {
                    (frame.get("status").and_then(Value::as_str) == Some("resolved"))
                        .then(|| frame.get("file"))
                        .flatten()
                }),
                "find_coupling" => first_field(payload, "coupled", "file"),
                "assess_risk" => payload.get("file"),
                "assess_risk_diff" => {
                    first_field(payload, "files", "file").or_else(|| clustered_file(payload))
                }
                "assess_risk_batch" => first_field(payload, "files", "file"),
                "recommend_tests" => payload
                    .pointer("/primary/0")
                    .or_else(|| first_field(payload, "reachable", "path"))
                    .or_else(|| first_field(payload, "coupled", "file")),
                "review_rehearsal" => payload.pointer("/objections/0/files/0"),
                "session_end" => payload
                    .pointer("/new_files/0")
                    .or_else(|| first_field(payload, "risk_regressions", "file"))
                    .or_else(|| payload.pointer("/new_cycles/0/0")),
                _ => None,
            }
            .and_then(file)?;
            Some(call("list_dependencies", "file_path", path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_frames_and_non_relative_paths_never_become_calls() {
        let root = "/tmp/project";
        let resolved = json!({"frames": [{"status": "resolved", "file": "src/app.rs"}]});
        assert_eq!(
            derive(root, "from_trace", &resolved).unwrap()["arguments"]["file_path"],
            "src/app.rs"
        );
        for status in ["unresolved", "ambiguous"] {
            let frame = json!({"frames": [{"status": status, "file": "src/app.rs"}]});
            assert!(derive(root, "from_trace", &frame).is_none());
        }
        for path in ["../secret.rs", "/tmp/secret.rs", ""] {
            let payload = json!({"results": [{"file_path": path}]});
            assert!(derive(root, "search", &payload).is_none());
        }
        assert!(derive("relative-root", "from_trace", &resolved).is_none());
    }

    #[test]
    fn returned_qualified_symbol_scopes_the_reference_followup() {
        let payload = json!({"results": [{"name": "run", "qualified_name": "Worker::run"}]});
        assert_eq!(
            derive("/tmp/project", "find_symbol", &payload).unwrap()["arguments"]["name"],
            "Worker::run"
        );
    }

    #[test]
    fn annotation_overhead_is_bounded_even_with_escaped_evidence() {
        for name in ["ordinary".to_owned(), "\u{1}".repeat(1024)] {
            let input = json!({"results": [{"name": name}]});
            let before = serde_json::to_vec(&input).unwrap().len();
            let output = annotate(
                "/tmp/project",
                "find_symbol",
                CallToolResult::structured(input),
            );
            let payload = output.structured_content.unwrap();
            assert!(serde_json::to_vec(&payload).unwrap().len() <= before + MAX_NEXT_BYTES + 8);
            assert_eq!(payload["next"].is_null(), name.len() == 1024);
        }
    }
}
