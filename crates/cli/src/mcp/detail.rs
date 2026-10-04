use std::time::Duration;

use codesage_protocol::DescribeDetail as Detail;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

use super::error::{ErrorCode, McpError};

pub(super) const ROW_TOOLS: &[&str] = &[
    "describe",
    "project_overview",
    "review_rehearsal",
    "edit_check",
    "find_symbol",
    "find_references",
    "find_similar",
    "list_dependencies",
    "search",
    "trace_call_path",
    "from_trace",
    "impact_analysis",
    "export_context",
    "find_coupling",
    "assess_risk",
    "assess_risk_diff",
    "assess_risk_batch",
    "recommend_tests",
    "session_end",
    "list_features",
    "find_feature",
    "feature_bundle",
];

const MIN_BUDGET_TOKENS: usize = 256;

const COMPACT_OMISSIONS: &[&str] = &[
    "content",
    "snippet",
    "signature",
    "doc_comment",
    "churn_score",
    "churn_percentile",
    "fix_ratio",
    "total_commits",
    "fix_count",
    "dependent_files",
    "coupled_files",
    "top_coupled",
];

#[derive(Clone, Copy)]
pub(super) struct Options {
    pub detail: Detail,
    pub budget_tokens: Option<usize>,
}

pub(super) fn default_detail(tool: &str) -> Detail {
    match tool {
        "find_references" | "impact_analysis" | "list_dependencies" | "list_features"
        | "find_feature" | "describe" => Detail::Compact,
        _ => Detail::Standard,
    }
}

impl Options {
    pub(super) fn parse(tool: &str, arguments: &Map<String, Value>) -> Result<Self, McpError> {
        let invalid = |message: &str| McpError::new(ErrorCode::Param, format!("{tool}: {message}"));
        let explicit = match arguments.get("detail") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .and_then(Detail::parse)
                    .ok_or_else(|| invalid("`detail` must be compact, standard, or full"))?,
            ),
        };
        let alias = match tool {
            "assess_risk" | "assess_risk_diff" | "assess_risk_batch" => arguments
                .get("verbose")
                .filter(|value| !value.is_null())
                .map(|value| {
                    value
                        .as_bool()
                        .map(|enabled| {
                            if enabled {
                                Detail::Full
                            } else {
                                Detail::Standard
                            }
                        })
                        .ok_or_else(|| invalid("`verbose` must be boolean"))
                })
                .transpose()?,
            "impact_analysis" => arguments
                .get("summary_only")
                .filter(|value| !value.is_null())
                .map(|value| {
                    value
                        .as_bool()
                        .map(|enabled| {
                            if enabled {
                                Detail::Compact
                            } else {
                                Detail::Standard
                            }
                        })
                        .ok_or_else(|| invalid("`summary_only` must be boolean"))
                })
                .transpose()?,
            _ => None,
        };
        if let (Some(explicit), Some(alias)) = (explicit, alias)
            && explicit != alias
        {
            return Err(invalid(
                "`detail` conflicts with its deprecated verbosity alias; pass one spelling or matching values",
            ));
        }
        let budget_tokens = arguments
            .get("budget_tokens")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| *value >= MIN_BUDGET_TOKENS)
                    .ok_or_else(|| invalid("`budget_tokens` must be an integer of at least 256"))
            })
            .transpose()?;
        Ok(Self {
            detail: explicit.or(alias).unwrap_or_else(|| default_detail(tool)),
            budget_tokens,
        })
    }

    pub(super) fn budget_chars(self, cap: usize) -> usize {
        self.budget_tokens
            .map_or(cap, |tokens| tokens.saturating_mul(4).min(cap))
    }
}

pub(super) fn prepare(tool: &str, arguments: &mut Map<String, Value>) -> Result<(), McpError> {
    if !ROW_TOOLS.contains(&tool) {
        return Ok(());
    }
    let options = Options::parse(tool, arguments)?;
    arguments.remove("budget_tokens");
    if tool != "describe" {
        arguments.remove("detail");
    }
    match tool {
        "assess_risk" | "assess_risk_diff" | "assess_risk_batch" => {
            arguments.insert("verbose".into(), json!(options.detail == Detail::Full));
        }
        "impact_analysis" => {
            arguments.insert(
                "summary_only".into(),
                json!(options.detail == Detail::Compact),
            );
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn advertise(tool: &str, schema: &mut Value) {
    if !ROW_TOOLS.contains(&tool) {
        return;
    }
    schema["properties"]["detail"] = json!({
        "type": "string", "enum": ["compact", "standard", "full"],
        "default": default_detail(tool).as_str(),
        "description": "compact: handles and row facts without source content; standard: normal payload; full: available source snippets and risk decomposition. Deprecated verbose/summary_only aliases must agree when supplied with detail."
    });
    schema["properties"]["budget_tokens"] = json!({
        "type": "integer", "minimum": MIN_BUDGET_TOKENS,
        "description": "Requested response budget (4 serialized UTF-8 bytes per approximate token), capped by the server's tool budget. Evidence and at least one result are preserved; cost.budget_exceeded discloses an irreducible response."
    });
    for alias in ["verbose", "summary_only"] {
        if let Some(property) = schema["properties"].get_mut(alias) {
            property["deprecated"] = json!(true);
            property["description"] = json!(if alias == "verbose" {
                "Deprecated for one release: true aliases detail=full, false aliases detail=standard. Conflicting explicit detail is E_PARAM."
            } else {
                "Deprecated for one release: true aliases detail=compact, false aliases detail=standard. Conflicting explicit detail is E_PARAM."
            });
        }
    }
}

pub(super) fn project(value: &mut Value, detail: Detail) {
    if detail != Detail::Compact {
        return;
    }
    match value {
        Value::Object(map) => {
            // Navigation, uncertainty, counts, and executable remedies remain evidence.
            for key in COMPACT_OMISSIONS {
                map.remove(*key);
            }
            for (key, child) in map {
                if !matches!(
                    key.as_str(),
                    "arguments" | "recover" | "next" | "expand" | "target"
                ) {
                    project(child, detail);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| project(item, detail)),
        _ => {}
    }
}

pub(super) fn adapt_output_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(required)) = map.get_mut("required") {
                required.retain(|key| {
                    key.as_str()
                        .is_none_or(|key| !COMPACT_OMISSIONS.contains(&key))
                });
            }
            if let Some(Value::Object(properties)) = map.get_mut("properties")
                && ["file_path", "from_file", "path", "file"]
                    .iter()
                    .any(|key| properties.contains_key(*key))
            {
                properties.insert("snippet".into(), json!({"type": "string"}));
                properties.insert("snippet_source".into(), json!({"const": "working_tree"}));
            }
            for child in map.values_mut() {
                adapt_output_schema(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(adapt_output_schema),
        _ => {}
    }
}

pub(super) fn add_snippets(value: &mut Value, root: &std::path::Path) {
    use std::io::Read;
    fn visit(
        value: &mut Value,
        root: &std::path::Path,
        files: &mut std::collections::HashMap<String, Option<String>>,
    ) {
        match value {
            Value::Object(map) => {
                let path = ["file_path", "from_file", "path", "file"]
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_str))
                    .map(str::to_owned);
                let line = ["line_start", "line"]
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_u64))
                    .and_then(|line| usize::try_from(line).ok())
                    .filter(|line| *line > 0);
                if !map.contains_key("content")
                    && !map.contains_key("snippet")
                    && let (Some(path), Some(line)) = (path, line)
                    && (files.contains_key(&path) || files.len() < 20)
                {
                    let source = files.entry(path.clone()).or_insert_with(|| {
                        let file = super::render::confined_project_path(root, &path)?;
                        if !std::fs::metadata(&file).ok()?.is_file() {
                            return None;
                        }
                        let mut bytes = Vec::new();
                        std::fs::File::open(file)
                            .ok()?
                            .take(1_048_577)
                            .read_to_end(&mut bytes)
                            .ok()?;
                        (bytes.len() <= 1_048_576)
                            .then(|| String::from_utf8(bytes).ok())
                            .flatten()
                    });
                    if let Some(source) = source {
                        let snippet = source
                            .lines()
                            .skip(line - 1)
                            .take(3)
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !snippet.is_empty() {
                            map.insert("snippet".into(), json!(snippet));
                            map.insert("snippet_source".into(), json!("working_tree"));
                        }
                    }
                }
                for (key, child) in map {
                    if !matches!(
                        key.as_str(),
                        "arguments" | "recover" | "next" | "expand" | "target"
                    ) {
                        visit(child, root, files);
                    }
                }
            }
            Value::Array(rows) => rows.iter_mut().for_each(|row| visit(row, root, files)),
            _ => {}
        }
    }
    visit(value, root, &mut std::collections::HashMap::new());
}

pub(super) fn finish(
    mut result: CallToolResult,
    tool: &str,
    arguments: &Map<String, Value>,
    elapsed: Duration,
    cap: usize,
    envelope_enabled: bool,
) -> CallToolResult {
    let rendering = std::time::Instant::now();
    let options = Options::parse(tool, arguments).unwrap_or(Options {
        detail: default_detail(tool),
        budget_tokens: None,
    });
    if result.is_error == Some(true) {
        for content in &mut result.content {
            if let Some(text) = content.as_text()
                && let Ok(mut value) = serde_json::from_str::<Value>(&text.text)
                && value.pointer("/error/code").is_some()
            {
                stamp_cost(&mut value, options, elapsed, None);
                *content = ContentBlock::text(value.to_string());
            }
        }
        return result;
    }
    let Some(value) = result.structured_content.as_mut() else {
        return result;
    };
    if !ROW_TOOLS.contains(&tool) {
        stamp_cost(value, options, elapsed, None);
        super::render::rerender_json_text(&mut result);
        return result;
    }
    let cap = options.budget_chars(cap);
    let budget = if tool == "find_references" && options.detail == Detail::Compact {
        cap.min(1_848)
    } else {
        cap
    };
    let mut retry_arguments = arguments.clone();
    retry_arguments.remove("verbose");
    retry_arguments.remove("summary_only");
    retry_arguments.insert(
        "detail".into(),
        json!(if options.detail == Detail::Compact {
            "standard"
        } else {
            options.detail.as_str()
        }),
    );
    retry_arguments.insert(
        "budget_tokens".into(),
        json!(cap.max(super::render::MCP_BUDGET_CHARS) / 4),
    );
    let retry = (serde_json::to_vec(&retry_arguments).map_or(usize::MAX, |value| value.len())
        <= 1024)
        .then(|| json!({"tool":tool,"arguments":retry_arguments}));
    stamp_cost(value, options, elapsed, Some(budget));
    trim_to_budget(value, budget, retry.as_ref(), envelope_enabled);
    stamp_cost(value, options, elapsed + rendering.elapsed(), Some(budget));
    super::render::rerender_json_text(&mut result);
    result
}

fn stamp_cost(value: &mut Value, options: Options, elapsed: Duration, cap: Option<usize>) {
    if let Some(cost) = value.get_mut("cost").and_then(Value::as_object_mut) {
        cost.remove("budget_exceeded");
    }
    value["cost"]["ms"] = json!(elapsed.as_millis() as u64);
    value["cost"]["detail"] = json!(options.detail.as_str());
    if let Some(cap) = cap {
        value["cost"]["budget_tokens"] = json!(cap / 4);
    }
    for _ in 0..4 {
        let bytes = value.to_string().len();
        value["cost"]["bytes"] = json!(bytes);
        if let Some(cap) = cap
            && bytes > cap
        {
            value["cost"]["budget_exceeded"] = json!(true);
        }
        if value.to_string().len() == bytes {
            break;
        }
    }
}

fn trim_to_budget(value: &mut Value, budget: usize, retry: Option<&Value>, envelope_enabled: bool) {
    let mut removed = std::collections::BTreeMap::<String, usize>::new();
    let mut omitted_handles = std::collections::BTreeMap::<String, Vec<Value>>::new();
    let mut shortened = std::collections::BTreeSet::<String>::new();
    let prior = value.get("completeness").cloned();
    let legacy_meta = value
        .get("_meta")
        .cloned()
        .or_else(|| (!envelope_enabled).then(|| json!({"truncated":true})));
    let mut legacy_counts = legacy_meta
        .as_ref()
        .map(legacy_array_counts)
        .unwrap_or_default();
    while value.to_string().len() > budget {
        if let Some(path) = largest_source(value, "") {
            let text = value
                .pointer(&path)
                .and_then(Value::as_str)
                .expect("selected source")
                .to_owned();
            let mut end = text.len() / 2;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            *value.pointer_mut(&path).expect("selected source") =
                json!(format!("{}\n…[truncated by MCP budget]", &text[..end]));
            shortened.insert(path);
        } else {
            let Some(path) = largest_rows(value, "") else {
                break;
            };
            let rows = value
                .pointer_mut(&path)
                .and_then(Value::as_array_mut)
                .expect("selected array");
            if !envelope_enabled
                && !legacy_counts
                    .iter()
                    .any(|(_, existing, _)| *existing == path)
            {
                let label = path
                    .trim_start_matches('/')
                    .split('/')
                    .map(|part| part.replace("~1", "/").replace("~0", "~"))
                    .collect::<Vec<_>>()
                    .join(".");
                legacy_counts.push((label, path.clone(), rows.len()));
            }
            let dropped = rows.pop().expect("selected nonempty array");
            if protected_rows(&path) {
                let identity = ["handle", "file_path", "file", "path", "directory", "name"]
                    .iter()
                    .find_map(|key| dropped.get(*key).and_then(Value::as_str))
                    .or_else(|| dropped.as_str())
                    .map(|value| json!(value));
                if let Some(identity) = identity {
                    omitted_handles
                        .entry(path.clone())
                        .or_default()
                        .push(identity);
                }
            }
            *removed.entry(path).or_default() += 1;
        }
        if let Some(meta) = &legacy_meta {
            let mut meta = meta.clone();
            for (index, (label, path, total)) in legacy_counts.iter().enumerate() {
                let returned = value
                    .pointer(path)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                if index == 0 {
                    meta["returned"] = json!(returned);
                    if !envelope_enabled {
                        meta["field"] = json!(label);
                        meta["total_results"] = json!(total);
                    }
                } else {
                    if meta.get("also_truncated_fields").is_none() {
                        meta["also_truncated_fields"] = json!([]);
                    }
                    let fields = meta["also_truncated_fields"]
                        .as_array_mut()
                        .expect("truncated fields array");
                    if fields.len() < index {
                        fields.resize(index, Value::Null);
                    }
                    meta["also_truncated_fields"][index - 1] =
                        json!(format!("{label} ({returned}/{total})"));
                }
            }
            meta["approx_tokens_budget"] = json!(budget / 4);
            let new_dropped: usize = removed
                .iter()
                .filter(|(path, _)| protected_rows(path))
                .map(|(_, count)| count)
                .sum();
            let named: Vec<_> = omitted_handles.values().flatten().cloned().collect();
            if new_dropped > 0 {
                let original = meta
                    .get("dropped_files")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if meta.get("dropped_count").is_some() || named.len() < new_dropped {
                    let previous = meta
                        .get("dropped_count")
                        .and_then(Value::as_u64)
                        .unwrap_or(original.len() as u64);
                    meta["dropped_count"] = json!(previous + new_dropped as u64);
                }
                meta["dropped_files"] =
                    json!(original.into_iter().chain(named).collect::<Vec<_>>());
            }
            value["_meta"] = meta;
        }
        let fields: Vec<_> = removed
            .iter()
            .map(|(path, omitted)| {
                let returned = value
                    .pointer(path)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let omitted = legacy_counts
                    .iter()
                    .find(|(_, original_path, _)| original_path == path)
                    .map_or(*omitted, |(_, _, total)| total.saturating_sub(returned));
                let mut field = json!({"field": path, "returned": returned, "omitted": omitted});
                if let Some(handles) = omitted_handles.get(path) {
                    field["omitted_handles"] = json!(handles);
                }
                field
            })
            .collect();
        if !envelope_enabled {
            value["_meta"]["truncated"] = json!(true);
            value["_meta"]["trimmed"] = json!(fields);
            if !shortened.is_empty() {
                value["_meta"]["shortened"] = json!(shortened);
            }
            if let Some(retry) = retry {
                value["_meta"]["recover"] = retry.clone();
            }
            continue;
        }
        value["completeness"] =
            json!({"kind": "truncated", "recover": {"detail": "standard", "trimmed": fields}});
        if !shortened.is_empty() {
            value["completeness"]["recover"]["shortened"] = json!(shortened);
        }
        if let Some(retry) = retry {
            value["completeness"]["recover"]["tool"] = retry["tool"].clone();
            value["completeness"]["recover"]["arguments"] = retry["arguments"].clone();
        }
        if let Some(prior) = &prior {
            value["completeness"]["prior"] = prior.clone();
            if prior.pointer("/recover/returned").is_some()
                && let Some(returned) = value.pointer("/_meta/returned").cloned()
            {
                if let (Some(offset), Some(previous), Some(current)) = (
                    prior
                        .pointer("/recover/arguments/offset")
                        .and_then(Value::as_u64),
                    prior.pointer("/recover/returned").and_then(Value::as_u64),
                    returned.as_u64(),
                ) {
                    value["completeness"]["prior"]["recover"]["arguments"]["offset"] =
                        json!(offset.saturating_sub(previous).saturating_add(current));
                }
                value["completeness"]["prior"]["recover"]["returned"] = returned;
            }
            let mut kinds = vec![json!("truncated")];
            if let Some(kind) = prior.get("kind").filter(|kind| *kind != "truncated") {
                kinds.push(kind.clone());
            }
            if let Some(prior_kinds) = prior.get("kinds").and_then(Value::as_array) {
                for kind in prior_kinds {
                    if !kinds.contains(kind) {
                        kinds.push(kind.clone());
                    }
                }
            }
            value["completeness"]["kinds"] = json!(kinds);
        }
    }
}

fn protected_rows(path: &str) -> bool {
    path.ends_with("/files") || path.ends_with("/clustered_directories")
}

fn legacy_array_counts(meta: &Value) -> Vec<(String, String, usize)> {
    fn pointer(field: &str) -> String {
        field
            .split(['.', '[', ']'])
            .filter(|part| !part.is_empty())
            .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
            .collect()
    }
    let mut fields = Vec::new();
    if let Some(total) = meta.get("total_results").and_then(Value::as_u64) {
        let field = meta
            .get("field")
            .and_then(Value::as_str)
            .unwrap_or("results");
        fields.push((field.to_owned(), pointer(field), total as usize));
    }
    if let Some(additional) = meta.get("also_truncated_fields").and_then(Value::as_array) {
        for label in additional.iter().filter_map(Value::as_str) {
            if let Some((field, counts)) = label.rsplit_once(" (")
                && let Some((_, total)) = counts.trim_end_matches(')').split_once('/')
                && let Ok(total) = total.parse()
            {
                fields.push((field.to_owned(), pointer(field), total));
            }
        }
    }
    fields
}

fn largest_source(value: &Value, path: &str) -> Option<String> {
    fn candidates(value: &Value, path: &str, best: &mut Option<(usize, String)>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if matches!(
                        key.as_str(),
                        "next"
                            | "target"
                            | "_meta"
                            | "index"
                            | "cost"
                            | "completeness"
                            | "recover"
                            | "expand"
                            | "arguments"
                            | "evidence"
                    ) {
                        continue;
                    }
                    let pointer = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                    if matches!(key.as_str(), "content" | "snippet")
                        && let Some(text) = child.as_str().filter(|text| text.len() > 256)
                        && best.as_ref().is_none_or(|(size, _)| *size < text.len())
                    {
                        *best = Some((text.len(), pointer));
                    } else {
                        candidates(child, &pointer, best);
                    }
                }
            }
            Value::Array(rows) => {
                for (i, row) in rows.iter().enumerate() {
                    candidates(row, &format!("{path}/{i}"), best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    candidates(value, path, &mut best);
    best.map(|(_, path)| path)
}

fn largest_rows(value: &Value, path: &str) -> Option<String> {
    fn candidates(value: &Value, path: &str, best: &mut Option<(usize, String)>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if matches!(
                        key.as_str(),
                        "next"
                            | "target"
                            | "_meta"
                            | "index"
                            | "cost"
                            | "completeness"
                            | "recover"
                            | "expand"
                            | "arguments"
                            | "evidence"
                            | "notes"
                            | "warnings"
                            | "limitations"
                            | "kinds"
                            | "reasons"
                            | "cycle_files"
                            | "trace"
                            | "stages"
                    ) {
                        continue;
                    }
                    let escaped = key.replace('~', "~0").replace('/', "~1");
                    candidates(child, &format!("{path}/{escaped}"), best);
                }
            }
            Value::Array(rows)
                if rows.len() > 1
                    && (rows.iter().all(Value::is_object)
                        || rows.iter().all(Value::is_string)
                        || rows.iter().all(Value::is_array)) =>
            {
                let bytes = value.to_string().len();
                if best.as_ref().is_none_or(|(size, _)| *size < bytes) {
                    *best = Some((bytes, path.into()));
                }
            }
            Value::Array(rows) => {
                for (i, row) in rows.iter().enumerate() {
                    candidates(row, &format!("{path}/{i}"), best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    candidates(value, path, &mut best);
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_have_deterministic_matching_and_conflicts() {
        for (tool, alias, enabled, detail) in [
            ("assess_risk", "verbose", true, "full"),
            ("assess_risk_diff", "verbose", false, "standard"),
            ("assess_risk_batch", "verbose", true, "full"),
            ("impact_analysis", "summary_only", true, "compact"),
            ("impact_analysis", "summary_only", false, "standard"),
        ] {
            let mut arguments = json!({alias: enabled, "detail": detail})
                .as_object()
                .unwrap()
                .clone();
            assert_eq!(
                Options::parse(tool, &arguments).unwrap().detail.as_str(),
                detail
            );
            prepare(tool, &mut arguments).unwrap();
            assert!(arguments.get("detail").is_none());
            let conflict = if detail == "compact" {
                "full"
            } else {
                "compact"
            };
            arguments.insert("detail".into(), json!(conflict));
            assert!(Options::parse(tool, &arguments).is_err());
        }
    }

    #[test]
    fn every_advertised_row_tool_has_validated_common_controls() {
        for tool in super::super::CodeSageServer::new().advertised_tools() {
            if matches!(
                tool.name.as_ref(),
                "session_start" | "daemon_stats" | "help"
            ) {
                continue;
            }
            assert!(
                ROW_TOOLS.contains(&tool.name.as_ref()),
                "unclassified tool {}",
                tool.name
            );
            assert_eq!(
                tool.input_schema["properties"]["detail"]["enum"],
                json!(["compact", "standard", "full"])
            );
            assert_eq!(
                tool.input_schema["properties"]["budget_tokens"]["minimum"],
                256
            );
            for detail in ["compact", "standard", "full"] {
                let mut arguments = json!({"detail": detail, "budget_tokens": 512})
                    .as_object()
                    .unwrap()
                    .clone();
                prepare(&tool.name, &mut arguments).unwrap();
                assert!(!arguments.contains_key("budget_tokens"));
            }
            for invalid in [
                json!(0),
                json!(255),
                json!(-1),
                json!(1.5),
                json!("512"),
                Value::Null,
            ] {
                let arguments = json!({"budget_tokens": invalid})
                    .as_object()
                    .unwrap()
                    .clone();
                assert!(
                    Options::parse(&tool.name, &arguments).is_err(),
                    "{} accepted {invalid}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn budget_preserves_handles_trace_recovery_and_degradation() {
        let rows: Vec<_> = (0..30).map(|i| json!({
            "handle": format!("chunk:src/{i}.rs#1-3"),
            "content": "large source".repeat(100),
            "trace": {"stages": [{"stage":"semantic","score":0.4},{"stage":"final","score":0.6}]}
        })).collect();
        let next = json!([{"tool":"describe","arguments":{"project":"/tmp/project","target":"file:src/0.rs"}}]);
        let mut value = json!({"results": rows, "next":next,
            "lexical_fallback_reason":"lexical_lookup_failed",
            "completeness":{"kind":"partial","recover":{"reason":"lexical_lookup_failed"}}
        });
        project(&mut value, Detail::Compact);
        trim_to_budget(&mut value, 1500, None, true);
        assert!(value.to_string().len() <= 1500);
        assert_eq!(value["next"], next);
        assert_eq!(value["lexical_fallback_reason"], "lexical_lookup_failed");
        assert_eq!(value["completeness"]["kind"], "truncated");
        assert_eq!(
            value["completeness"]["kinds"],
            json!(["truncated", "partial"])
        );
        let retained = value["results"].as_array().unwrap();
        assert!(retained.len() > 1 && retained.len() < 30);
        for row in retained {
            assert!(row["handle"].as_str().unwrap().starts_with("chunk:"));
            assert_eq!(row["trace"]["stages"].as_array().unwrap().len(), 2);
            assert!(row.get("content").is_none());
        }
        assert_eq!(
            value["completeness"]["recover"]["trimmed"][0]["omitted"],
            30 - retained.len()
        );
    }

    #[test]
    fn requested_budgets_never_raise_the_server_cap() {
        for tokens in [256, 8_000, 12_000, usize::MAX] {
            let options = Options {
                detail: Detail::Full,
                budget_tokens: Some(tokens),
            };
            for cap in [16_000, 32_000, 48_000] {
                assert_eq!(options.budget_chars(cap), tokens.saturating_mul(4).min(cap));
            }
        }
    }

    #[test]
    fn internal_embedding_vectors_are_never_subject_to_row_budgets() {
        let vectors = vec![vec![0.5; 768]; 50];
        let result = finish(
            CallToolResult::structured(json!({"vectors": vectors})),
            "embed_texts",
            &Map::new(),
            Duration::from_millis(12),
            32_000,
            true,
        );
        let payload = result.structured_content.unwrap();
        assert_eq!(payload["vectors"], json!(vectors));
        assert!(payload["cost"]["bytes"].as_u64().unwrap() > 32_000);
        assert_eq!(payload["cost"]["bytes"], payload.to_string().len());
        assert!(payload.get("completeness").is_none());
    }

    #[test]
    fn final_trim_reconciles_prior_paging_and_protected_identities() {
        let rows: Vec<_> = (0..6)
            .map(|i| json!({"file":format!("src/{i}.rs"), "evidence":"long".repeat(120)}))
            .collect();
        let mut value = json!({
            "files":rows,
            "_meta":{"truncated":true,"field":"files","returned":6,"total_results":10,"dropped_files":["src/6.rs","src/7.rs","src/8.rs","src/9.rs"]},
            "completeness":{"kind":"truncated","recover":{"returned":6,"total":10,"arguments":{"offset":16}}}
        });
        trim_to_budget(&mut value, 1700, None, true);
        let returned = value["files"].as_array().unwrap().len();
        assert!(returned > 0 && returned < 6);
        assert_eq!(value["_meta"]["returned"], returned);
        assert_eq!(
            value["completeness"]["prior"]["recover"]["arguments"]["offset"],
            10 + returned
        );
        assert_eq!(
            value["completeness"]["recover"]["trimmed"][0]["omitted"],
            10 - returned
        );
        let mut omitted: Vec<_> = value["_meta"]["dropped_files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        omitted.sort();
        assert_eq!(
            omitted,
            (returned..10)
                .map(|i| format!("src/{i}.rs"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn scalar_measurements_and_coordinate_tuples_are_atomic_under_budgeting() {
        let measurements = json!({
            "coordinates":[1,3], "scores":[0.1,0.2], "flags":[true,false],
            "nullable":[1,null,3], "mixed":["symbol",12],
            "binding":["name",{"file":"module.py"}]
        });
        let mut value = json!({
            "measurements":measurements,
            "results":[{"handle":"sym:src/lib.rs#one"},{"handle":"sym:src/lib.rs#two"}]
        });
        trim_to_budget(&mut value, 1, None, true);
        assert_eq!(value["measurements"], measurements);
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["completeness"]["recover"]["trimmed"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn legacy_final_trim_accounts_for_nested_rows_source_and_prior_omissions() {
        let rows: Vec<_> = (0..6)
            .map(|i| json!({"file":format!("src/{i}.rs"), "evidence":"long".repeat(120)}))
            .collect();
        let mut value = json!({
            "files":rows,
            "card":{"sections":{"symbols":{"top":[{"handle":"sym:a"},{"handle":"sym:b"}]}}},
            "snippet":"source".repeat(300),
            "_meta":{"truncated":true,"field":"files","returned":6,"total_results":10,"dropped_files":["src/6.rs","src/7.rs","src/8.rs","src/9.rs"]}
        });
        let retry =
            json!({"tool":"describe","arguments":{"target":"file:src/lib.rs","detail":"standard"}});
        trim_to_budget(&mut value, 1, Some(&retry), false);
        assert!(value.get("completeness").is_none());
        assert_eq!(value["_meta"]["recover"], retry);
        assert_eq!(value["_meta"]["returned"], 1);
        assert_eq!(value["_meta"]["total_results"], 10);
        assert_eq!(value["_meta"]["dropped_files"].as_array().unwrap().len(), 9);
        assert_eq!(value["_meta"]["shortened"], json!(["/snippet"]));
        assert!(
            value["snippet"]
                .as_str()
                .unwrap()
                .contains("truncated by MCP budget")
        );
        for field in value["_meta"]["trimmed"].as_array().unwrap() {
            let path = field["field"].as_str().unwrap();
            assert_eq!(
                field["returned"],
                value.pointer(path).unwrap().as_array().unwrap().len()
            );
            assert_eq!(field["omitted"], if path == "/files" { 9 } else { 1 });
        }
        let counts = legacy_array_counts(&value["_meta"]);
        assert_eq!(
            counts[1],
            (
                "card.sections.symbols.top".into(),
                "/card/sections/symbols/top".into(),
                2
            )
        );
    }
}
