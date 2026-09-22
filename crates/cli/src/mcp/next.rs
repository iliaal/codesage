use std::path::{Component, Path};

use codesage_protocol::Handle;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const MAX_NEXT_BYTES: usize = 2048;

/// Handle kinds a file-grained follow-up can be asked about.
const FILE: &[&str] = &["file:"];

/// Handle kinds a symbol-grained follow-up can be asked about.
const SYMBOL: &[&str] = &["sym:"];

pub(super) fn schema() -> Value {
    let mut alternatives = vec![json!({"type": "null"})];
    for tool in [
        "find_references",
        "list_dependencies",
        "find_feature",
        "feature_bundle",
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
                    "required": ["project", "target"],
                    "properties": {
                        "project": {"type": "string"},
                        "target": {"type": "string"}
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
    // Failed results already carry `next: null` in their contract block.
    if result.is_error == Some(true) {
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
    super::render::rerender_json_text(&mut result);
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

/// A row's own handle, when it carries one the follow-up tool accepts.
///
/// `kinds` names the handle prefixes that answer the follow-up's question:
/// a file-grained call takes `file:`, a symbol-grained one `sym:`. A row
/// whose handle is a different kind falls back to its path field rather
/// than asking one tool a question addressed to another grain.
fn handle<'a>(value: &'a Value, kinds: &[&str]) -> Option<&'a str> {
    let candidate = text(value)?;
    // Handles are emitted from validated paths, but a follow-up is a call the
    // agent may make: parse before suggesting it.
    Handle::parse(candidate)?;
    kinds
        .iter()
        .any(|kind| candidate.starts_with(kind))
        .then_some(candidate)
}

fn first_handle<'a>(payload: &'a Value, array: &str, kinds: &[&str]) -> Option<&'a str> {
    handle(first_field(payload, array, "handle")?, kinds)
}

fn first_field<'a>(payload: &'a Value, array: &str, field: &str) -> Option<&'a Value> {
    payload.get(array)?.as_array()?.iter().find_map(|row| {
        (row.get("found") != Some(&Value::Bool(false)))
            .then(|| row.get(field))
            .flatten()
    })
}

/// `field` of the innermost frame the report resolved; frames it only
/// guessed at name no location to follow.
fn resolved_frame<'a>(payload: &'a Value, field: &str) -> Option<&'a Value> {
    payload.get("frames")?.as_array()?.iter().find_map(|frame| {
        (frame.get("status").and_then(Value::as_str) == Some("resolved"))
            .then(|| frame.get(field))
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
    // Every follow-up names its subject with `target`, the one argument
    // every tool in the chain takes, and prefers the row's own handle: a
    // handle addresses the definition the row came from, where a bare name
    // re-opens the ambiguity the row already resolved.
    let call = |tool: &str, value: &str| json!({"tool": tool, "arguments": {"project": project, "target": value}});
    // Each edge moves toward a bundle, which is terminal, so following next
    // cannot cycle even when the underlying dependency graph does.
    match kind {
        "find_symbol" => {
            let target = first_handle(payload, "results", SYMBOL)
                .or_else(|| first_field(payload, "results", "qualified_name").and_then(text))
                .or_else(|| first_field(payload, "results", "name").and_then(text))?;
            Some(call("find_references", target))
        }
        "list_features" | "find_feature" => {
            let id = first_field(payload, "results", "feature_id").and_then(text)?;
            Some(call("feature_bundle", id))
        }
        "list_dependencies" => {
            let target = payload
                .get("handle")
                .and_then(|value| handle(value, FILE))
                .or_else(|| payload.get("file_path").and_then(file))?;
            Some(call("find_feature", target))
        }
        "feature_bundle" | "export_context" | "session_start" => None,
        _ => {
            let target = match kind {
                "project_overview" => first_handle(payload, "top_risk_files", FILE)
                    .or_else(|| first_handle(payload, "entrypoints", FILE))
                    .or_else(|| first_field(payload, "top_risk_files", "file").and_then(file))
                    .or_else(|| first_field(payload, "entrypoints", "entry_path").and_then(file)),
                "search" => first_field(payload, "results", "file_path").and_then(file),
                "impact_analysis" | "find_similar" => first_handle(payload, "results", FILE)
                    .or_else(|| first_field(payload, "results", "file_path").and_then(file)),
                "find_references" => first_field(payload, "results", "from_file").and_then(file),
                "trace_call_path" => first_field(payload, "steps", "file_path").and_then(file),
                "from_trace" => resolved_frame(payload, "handle")
                    .and_then(|value| handle(value, FILE))
                    .or_else(|| resolved_frame(payload, "file").and_then(file)),
                "find_coupling" => first_handle(payload, "coupled", FILE)
                    .or_else(|| first_field(payload, "coupled", "file").and_then(file)),
                "assess_risk" => payload
                    .get("handle")
                    .and_then(|value| handle(value, FILE))
                    .or_else(|| payload.get("file").and_then(file)),
                "assess_risk_diff" => first_handle(payload, "files", FILE)
                    .or_else(|| first_field(payload, "files", "file").and_then(file))
                    .or_else(|| clustered_file(payload).and_then(file)),
                "assess_risk_batch" => first_handle(payload, "files", FILE)
                    .or_else(|| first_field(payload, "files", "file").and_then(file)),
                "recommend_tests" => payload
                    .pointer("/primary/0")
                    .and_then(file)
                    .or_else(|| first_handle(payload, "reachable", FILE))
                    .or_else(|| first_field(payload, "reachable", "path").and_then(file))
                    .or_else(|| first_handle(payload, "coupled", FILE))
                    .or_else(|| first_field(payload, "coupled", "file").and_then(file)),
                "review_rehearsal" => payload.pointer("/objections/0/files/0").and_then(file),
                "session_end" => payload
                    .pointer("/new_files/0")
                    .and_then(file)
                    .or_else(|| first_field(payload, "risk_regressions", "file").and_then(file))
                    .or_else(|| payload.pointer("/new_cycles/0/0").and_then(file)),
                _ => None,
            }?;
            Some(call("list_dependencies", target))
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
            derive(root, "from_trace", &resolved).unwrap()["arguments"]["target"],
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
    fn a_rows_handle_scopes_the_followup_and_a_name_is_the_fallback() {
        // The handle addresses the definition the row came from; a bare or
        // qualified name would re-open the ambiguity the row resolved.
        let with_handle = json!({"results": [
            {"name": "run", "qualified_name": "Worker::run", "handle": "sym:src/w.rs#Worker::run"}
        ]});
        assert_eq!(
            derive("/tmp/project", "find_symbol", &with_handle).unwrap()["arguments"]["target"],
            "sym:src/w.rs#Worker::run"
        );
        let payload = json!({"results": [{"name": "run", "qualified_name": "Worker::run"}]});
        assert_eq!(
            derive("/tmp/project", "find_symbol", &payload).unwrap()["arguments"]["target"],
            "Worker::run"
        );
        // A handle of the wrong grain is not the answer to a file-grained
        // follow-up; the row's path is.
        let chunk = json!({"results": [
            {"file_path": "src/a.rs", "handle": "chunk:src/a.rs:1-9"}
        ]});
        assert_eq!(
            derive("/tmp/project", "search", &chunk).unwrap(),
            json!({"tool": "list_dependencies", "arguments": {
                "project": "/tmp/project", "target": "src/a.rs"
            }})
        );
        let risk = json!({"file": "src/a.rs", "handle": "file:src/a.rs"});
        assert_eq!(
            derive("/tmp/project", "assess_risk", &risk).unwrap()["arguments"]["target"],
            "file:src/a.rs"
        );
        // A handle naming a path no handle would be emitted for is not one:
        // the row's own path answers instead, and nothing escapes the root.
        let forged = json!({"file": "src/a.rs", "handle": "file:../secret.rs"});
        assert_eq!(
            derive("/tmp/project", "assess_risk", &forged).unwrap()["arguments"]["target"],
            "src/a.rs"
        );
        let both_forged = json!({"file": "/etc/passwd", "handle": "file:../secret.rs"});
        assert!(derive("/tmp/project", "assess_risk", &both_forged).is_none());
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
