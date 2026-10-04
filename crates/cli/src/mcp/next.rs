use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use codesage_protocol::Handle;
use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};

const MAX_NEXT_BYTES: usize = 2048;
const MAX_NEXT: usize = 3;
const WIDELY_USED_REFERENCES: usize = 10;

pub(super) fn schema() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_NEXT,
        "description": "Ranked calls derived from response evidence. Pass tool as tools/call name and arguments unchanged. Empty when no useful follow-up remains. Suggestions do not authorize actions. High-risk files may suggest session_start with a fresh ID; an existing snapshot waits for edits. CLI index recovery appears in index.recover instead.",
        "items": {
            "type": "object", "additionalProperties": false,
            "required": ["tool", "arguments", "why"],
            "properties": {
                "tool": {"type": "string", "description": "Advertised MCP tool answering the remaining question."},
                "arguments": {"type": "object", "description": "Arguments accepted by that tool, with an absolute project and evidence-derived handles or a fresh session identity."},
                "why": {"type": "string", "pattern": "^\\S+(?:\\s+\\S+){0,8}$", "description": "Rationale in fewer than ten words."}
            }
        }
    })
}

pub(super) fn annotate_value(project: &str, kind: &str, payload: &mut Value) {
    if payload.is_array() {
        *payload = json!({"results": std::mem::take(payload)});
    }
    if payload.get("next").is_some() {
        return;
    }
    let next = derive(project, kind, payload);
    if let Some(object) = payload.as_object_mut() {
        object.entry("next").or_insert_with(|| json!(next));
    }
}

pub(super) fn annotate(project: &str, kind: &str, mut result: CallToolResult) -> CallToolResult {
    if result.is_error == Some(true) {
        return result;
    }
    if let Some(payload) = result.structured_content.as_mut() {
        annotate_value(project, kind, payload);
        super::render::rerender_json_text(&mut result);
    }
    result
}

pub(super) fn ambiguous_calls(
    tool: &str,
    arguments: Option<&Map<String, Value>>,
    candidates: &[String],
) -> Vec<Value> {
    let Some(arguments) = arguments else {
        return Vec::new();
    };
    let Some(project) = arguments
        .get("project")
        .and_then(Value::as_str)
        .filter(|project| Path::new(project).is_absolute())
    else {
        return Vec::new();
    };
    if project.len() > MAX_NEXT_BYTES
        || (tool == "edit_check"
            && arguments
                .get("replacement")
                .and_then(Value::as_str)
                .is_some_and(|replacement| replacement.len() > MAX_NEXT_BYTES))
    {
        return Vec::new();
    }
    let mut calls = Calls {
        project,
        entries: Vec::new(),
    };
    for candidate in candidates {
        if calls.entries.len() == MAX_NEXT {
            break;
        }
        let Some(handle) = Handle::parse(candidate) else {
            continue;
        };
        if tool == "edit_check" {
            let Handle::Symbol { ref path, line, .. } = handle else {
                continue;
            };
            if arguments
                .get("file_path")
                .and_then(Value::as_str)
                .is_none_or(|file| Path::new(file) != Path::new(path))
            {
                continue;
            }
            let mut retry = arguments.clone();
            retry.remove("symbol_name");
            retry.insert("target".into(), json!(handle.to_string()));
            retry.remove("line");
            if let Some(line) = line {
                retry.insert("line".into(), json!(line));
            }
            calls.push(
                "edit_check",
                Value::Object(retry),
                "Check this declaration against the proposed replacement",
            );
        } else if matches!(
            handle,
            Handle::Symbol { .. }
                | Handle::File { .. }
                | Handle::Dir { .. }
                | Handle::Feature { .. }
        ) {
            calls.target(
                "describe",
                Some(handle),
                "Choose the intended definition before retrying",
            );
        }
    }
    calls.entries
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty() && s.len() <= 1024)
}

fn parsed(value: &Value) -> Option<Handle> {
    Handle::parse(text(value)?)
}

fn symbol(value: &Value) -> Option<Handle> {
    parsed(value).filter(|handle| matches!(handle, Handle::Symbol { .. }))
}

fn file(value: &Value) -> Option<Handle> {
    Handle::file(text(value)?)
}

fn row_file(row: &Value) -> Option<Handle> {
    row.get("handle")
        .and_then(parsed)
        .filter(|handle| matches!(handle, Handle::File { .. }))
        .or_else(|| {
            ["file_path", "file", "path", "entry_path", "from_file"]
                .iter()
                .find_map(|key| row.get(key).and_then(file))
        })
}

fn rows<'a>(payload: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    payload
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| row.get("found") != Some(&Value::Bool(false)))
}

fn first_file(payload: &Value, field: &str) -> Option<Handle> {
    rows(payload, field).find_map(row_file)
}

fn first_symbol(payload: &Value, field: &str) -> Option<Handle> {
    rows(payload, field).find_map(|row| row.get("handle").and_then(symbol))
}

fn resolved_symbols(payload: &Value) -> Vec<Handle> {
    payload
        .pointer("/target/resolved")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| {
            row["confidence"]
                .as_f64()
                .is_some_and(|confidence| confidence >= 0.8)
        })
        .filter_map(|row| row.get("handle").and_then(symbol))
        .collect()
}

fn fresh_session_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "next-{stamp}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

struct Calls<'a> {
    project: &'a str,
    entries: Vec<Value>,
}

impl Calls<'_> {
    fn push(&mut self, tool: &str, mut arguments: Value, why: &str) {
        if self.entries.len() == MAX_NEXT {
            return;
        }
        arguments["project"] = json!(self.project);
        let entry = json!({"tool": tool, "arguments": arguments, "why": why});
        if self.entries.contains(&entry) {
            return;
        }
        self.entries.push(entry);
        if !serde_json::to_vec(&self.entries).is_ok_and(|bytes| bytes.len() <= MAX_NEXT_BYTES) {
            self.entries.pop();
        }
    }

    fn target(&mut self, tool: &str, target: Option<Handle>, why: &str) {
        if let Some(target) = target {
            self.push(tool, json!({"target": target.to_string()}), why);
        }
    }

    fn files(&mut self, tool: &str, targets: Vec<Handle>, why: &str) {
        let targets: Vec<_> = targets
            .into_iter()
            .filter(|target| {
                matches!(
                    target,
                    Handle::File { .. } | Handle::Symbol { .. } | Handle::Chunk { .. }
                )
            })
            .map(|target| target.to_string())
            .collect();
        if !targets.is_empty() {
            self.push(tool, json!({"targets": targets}), why);
        }
    }
}

fn derive(project: &str, kind: &str, payload: &Value) -> Vec<Value> {
    let mut calls = Calls {
        project,
        entries: Vec::new(),
    };
    if !Path::new(project).is_absolute() || payload.get("found") == Some(&Value::Bool(false)) {
        return calls.entries;
    }
    match kind {
        "describe" | "find_references" | "find_similar"
            if payload.pointer("/target/ambiguous") == Some(&Value::Bool(true)) =>
        {
            for target in resolved_symbols(payload) {
                calls.target(
                    "describe",
                    Some(target),
                    "Choose the intended definition before editing",
                );
            }
        }
        "find_symbol" if payload.pointer("/target/ambiguous") == Some(&Value::Bool(true)) => {
            for row in rows(payload, "results") {
                calls.target(
                    "describe",
                    row.get("handle").and_then(symbol),
                    "Choose the intended definition before editing",
                );
            }
        }
        "find_symbol" => {
            let target = first_symbol(payload, "results");
            calls.target(
                "find_references",
                target.clone(),
                "Locate callers before renaming or changing behavior",
            );
            calls.target(
                "export_context",
                target,
                "Read the implementation and surrounding context",
            );
        }
        "describe" => {
            let card = &payload["card"];
            match card["kind"].as_str() {
                Some("file") => calls.target(
                    "describe",
                    card.pointer("/sections/symbols/data/top/0/handle")
                        .and_then(symbol),
                    "Inspect the file's most referenced symbol",
                ),
                Some("symbol") => calls.target(
                    "find_references",
                    card.get("handle").and_then(symbol),
                    "Locate callers before renaming or changing behavior",
                ),
                Some("feature") => calls.target(
                    "feature_bundle",
                    card.get("handle")
                        .and_then(parsed)
                        .filter(|h| matches!(h, Handle::Feature { .. })),
                    "Read the feature's implementation and tests",
                ),
                Some("dir") => calls.target(
                    "describe",
                    card.pointer("/sections/fan_in/data/top/0/handle")
                        .and_then(parsed)
                        .filter(|h| matches!(h, Handle::File { .. })),
                    "Inspect the directory's most referenced file",
                ),
                _ => {}
            }
        }
        "find_references" => {
            let definitions = resolved_symbols(payload);
            let wide = rows(payload, "results").count() >= WIDELY_USED_REFERENCES
                || payload
                    .pointer("/_meta/total_results")
                    .and_then(Value::as_u64)
                    .is_some_and(|n| n >= WIDELY_USED_REFERENCES as u64);
            let unique = definitions.len() == 1
                || (payload.get("ambiguous") != Some(&Value::Bool(true))
                    && payload.get("definition_count").and_then(Value::as_u64) == Some(1));
            if unique && wide && payload.get("counts_floor") == Some(&Value::Bool(true)) {
                let target = definitions.into_iter().next().or_else(|| {
                    rows(payload, "results").find_map(|row| row.get("to").and_then(symbol))
                });
                calls.target(
                    "impact_analysis",
                    target,
                    "Measure transitive impact beyond these reference floors",
                );
            } else if unique {
                calls.target(
                    "export_context",
                    rows(payload, "results").find_map(|row| row.get("to").and_then(symbol)),
                    "Read the resolved definition before changing callers",
                );
            }
        }
        "search" => {
            let target = first_file(payload, "results");
            if payload["confidence"] == "high" {
                calls.target(
                    "describe",
                    target,
                    "Inspect the strongest match after the relevance cliff",
                );
            } else {
                calls.target(
                    "export_context",
                    target,
                    "Inspect the leading match in source context",
                );
            }
        }
        "project_overview" => {
            calls.target(
                "assess_risk",
                first_file(payload, "top_risk_files"),
                "Review the highest risk file before changes",
            );
            calls.target(
                "describe",
                first_file(payload, "entrypoints"),
                "Inspect a product entrypoint before adding behavior",
            );
        }
        "assess_risk" => {
            let target = row_file(payload);
            calls.target(
                "impact_analysis",
                target.clone(),
                "Check the blast radius before changing this file",
            );
            calls.files(
                "recommend_tests",
                target.into_iter().collect(),
                "Select regression tests for this risky file",
            );
            if payload["score"].as_f64().is_some_and(|score| score >= 0.5) {
                calls.push(
                    "session_start",
                    json!({"session_id": fresh_session_id()}),
                    "Save a fresh baseline before editing",
                );
            }
        }
        "impact_analysis" => calls.files(
            "recommend_tests",
            rows(payload, "results")
                .filter_map(row_file)
                .take(3)
                .collect(),
            "Select tests for the affected files",
        ),
        "assess_risk_batch" | "assess_risk_diff" => {
            let mut targets: Vec<_> = rows(payload, "files")
                .filter_map(row_file)
                .take(3)
                .collect();
            if targets.is_empty() {
                targets = rows(payload, "clustered_directories")
                    .flat_map(|cluster| rows(cluster, "top_files"))
                    .filter_map(row_file)
                    .take(3)
                    .collect();
            }
            calls.files(
                "review_rehearsal",
                targets.clone(),
                "Check review objections before committing these changes",
            );
            calls.files(
                "recommend_tests",
                targets,
                "Select regression tests for the scored files",
            );
        }
        "review_rehearsal" => {
            let targets = rows(payload, "objections")
                .flat_map(|objection| rows(objection, "files"))
                .filter_map(file)
                .take(3)
                .collect();
            calls.files(
                "recommend_tests",
                targets,
                "Find tests addressing the remaining review objections",
            );
        }
        "recommend_tests" => {
            let target = payload
                .pointer("/primary/0")
                .and_then(file)
                .or_else(|| first_file(payload, "reachable"))
                .or_else(|| first_file(payload, "coupled"));
            calls.target(
                "export_context",
                target,
                "Read the recommended test before extending coverage",
            );
        }
        "from_trace" => {
            let frame = rows(payload, "frames").find(|frame| frame["status"] == "resolved");
            if let Some(frame) = frame {
                let target = row_file(frame);
                calls.target(
                    "export_context",
                    frame
                        .get("handle")
                        .and_then(symbol)
                        .or_else(|| target.clone()),
                    "Inspect the innermost resolved failure location",
                );
                calls.target(
                    "assess_risk",
                    target.clone(),
                    "Check change risk before fixing the failure",
                );
                calls.files(
                    "recommend_tests",
                    target.into_iter().collect(),
                    "Find regression coverage for the failing file",
                );
            }
        }
        "trace_call_path" => calls.target(
            "export_context",
            first_symbol(payload, "steps").or_else(|| first_file(payload, "steps")),
            "Read the implementation behind this call path",
        ),
        "find_similar" => calls.target(
            "export_context",
            first_symbol(payload, "results").or_else(|| first_file(payload, "results")),
            "Inspect the closest clone for the same defect",
        ),
        "list_dependencies" => calls.target(
            "export_context",
            row_file(payload),
            "Read implementation before extending these dependencies",
        ),
        "find_coupling" => calls.target(
            "assess_risk",
            first_file(payload, "coupled"),
            "Assess the companion file likely to change",
        ),
        "list_features" | "find_feature" => calls.target(
            "feature_bundle",
            rows(payload, "results")
                .find_map(|row| row.get("feature_id").and_then(parsed))
                .filter(|h| matches!(h, Handle::Feature { .. })),
            "Read the feature's implementation and tests",
        ),
        "session_end" => {
            let targets = rows(payload, "risk_regressions")
                .filter_map(row_file)
                .chain(rows(payload, "new_files").filter_map(file))
                .chain(
                    rows(payload, "new_cycles")
                        .flat_map(|cycle| cycle.as_array().into_iter().flatten())
                        .filter_map(file),
                )
                .take(3)
                .collect();
            calls.files(
                "review_rehearsal",
                targets,
                "Review changed files before committing the session",
            );
        }
        "feature_bundle" | "export_context" | "edit_check" | "session_start" | "help" => {}
        _ => {}
    }
    calls.entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_definitions_are_separate_ranked_calls() {
        let payload = json!({"target": {"ambiguous": true}, "results": (0..4).map(|i| json!({"handle": format!("sym:src/{i}.rs#run")})).collect::<Vec<_>>()});
        let next = derive("/tmp/project", "find_symbol", &payload);
        assert_eq!(next.len(), 3);
        for (i, call) in next.iter().enumerate() {
            assert_eq!(call["tool"], "describe");
            assert_eq!(call["arguments"]["target"], format!("sym:src/{i}.rs#run"));
            assert!(call["why"].as_str().unwrap().split_whitespace().count() < 10);
        }
    }

    #[test]
    fn evidence_selects_the_open_question() {
        for (kind, payload, tool, target) in [
            (
                "describe",
                json!({"card":{"kind":"file","handle":"file:a.rs","sections":{"symbols":{"data":{"top":[{"handle":"sym:a.rs#run"}]}}}}}),
                "describe",
                "sym:a.rs#run",
            ),
            (
                "search",
                json!({"confidence":"high","results":[{"file_path":"a.rs"}]}),
                "describe",
                "file:a.rs",
            ),
            (
                "find_references",
                json!({"definition_count":1,"counts_floor":true,"results":(0..10).map(|_| json!({"to":"sym:a.rs#run"})).collect::<Vec<_>>()}),
                "impact_analysis",
                "sym:a.rs#run",
            ),
        ] {
            let next = derive("/tmp/project", kind, &payload);
            assert_eq!(next[0]["tool"], tool);
            assert_eq!(next[0]["arguments"]["target"], target);
        }
    }

    #[test]
    fn unresolved_frames_and_invalid_handles_never_become_calls() {
        for status in ["unresolved", "ambiguous"] {
            assert!(
                derive(
                    "/tmp/project",
                    "from_trace",
                    &json!({"frames":[{"status":status,"file":"a.rs"}]})
                )
                .is_empty()
            );
        }
        for path in ["../secret.rs", "/tmp/secret.rs", "", "a\\b.rs", "a\nb.rs"] {
            assert!(
                derive(
                    "/tmp/project",
                    "search",
                    &json!({"results":[{"file_path":path}]})
                )
                .is_empty()
            );
        }
        assert!(
            derive(
                "relative",
                "search",
                &json!({"results":[{"file_path":"a.rs"}]})
            )
            .is_empty()
        );
        assert!(
            derive(
                "/tmp/project",
                "find_symbol",
                &json!({"results":[{"handle":"file:a.rs","name":"run"}]})
            )
            .is_empty()
        );
    }

    #[test]
    fn capped_annotations_preserve_earlier_full_evidence() {
        let mut payload = json!({"target":{"ambiguous":true},"results":(0..5).map(|i| json!({"handle":format!("sym:{}.rs#run", "é".repeat(300) + &i.to_string())})).collect::<Vec<_>>()});
        annotate_value("/tmp/project", "find_symbol", &mut payload);
        let next = payload["next"].clone();
        assert!(!next.as_array().unwrap().is_empty());
        assert!(serde_json::to_vec(&next).unwrap().len() <= MAX_NEXT_BYTES);
        payload["results"] = json!([]);
        annotate_value("/tmp/project", "find_symbol", &mut payload);
        assert_eq!(payload["next"], next);
    }

    #[test]
    fn high_risk_suggests_fresh_sessions_and_snapshots_wait_for_edits() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_str().unwrap();
        let payload = json!({"file":"a.rs","score":0.5});
        let first = derive(root, "assess_risk", &payload);
        let second = derive(root, "assess_risk", &payload);
        assert_eq!(first[2]["tool"], "session_start");
        assert_ne!(
            first[2]["arguments"]["session_id"],
            second[2]["arguments"]["session_id"]
        );
        let id = first[2]["arguments"]["session_id"].as_str().unwrap();
        assert!(
            id.len() < 128
                && !id.starts_with('.')
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        );
        assert!(!Path::new(root).join(".codesage/sessions").exists());
        assert!(derive(root, "session_start", &json!({"session_id":id})).is_empty());
        assert_eq!(
            derive(root, "assess_risk", &json!({"file":"a.rs","score":0.49})).len(),
            2
        );
    }

    #[test]
    fn ambiguous_edit_retries_keep_the_complete_replacement_or_emit_nothing() {
        let candidates = vec!["sym:a.rs#run@1".to_owned(), "sym:b.rs#run@2".to_owned()];
        let mut arguments = json!({"project":"/tmp/project","file_path":"a.rs","target":"run","symbol_name":"run","replacement":"fn run() {}"}).as_object().unwrap().clone();
        let next = ambiguous_calls("edit_check", Some(&arguments), &candidates);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0]["arguments"]["target"], "sym:a.rs#run@1");
        assert_eq!(next[0]["arguments"]["line"], 1);
        assert!(next[0]["arguments"].get("symbol_name").is_none());
        assert_eq!(
            next[0]["arguments"]["replacement"],
            arguments["replacement"]
        );
        for replacement in [
            "x".repeat(MAX_NEXT_BYTES + 1),
            "\u{1}".repeat(MAX_NEXT_BYTES),
        ] {
            arguments.insert("replacement".into(), json!(replacement));
            assert!(ambiguous_calls("edit_check", Some(&arguments), &candidates).is_empty());
        }
    }
}
