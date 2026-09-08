#[cfg(unix)]
use std::io::Read;
use std::path::{Component, Path};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use codesage_parser::{detect::detect_language, parse::parse_file};
use codesage_protocol::Language;
use schemars::JsonSchema;
use serde::Serialize;
use tree_sitter::Node;

const MAX_SOURCE_BYTES: usize = 1_048_576;
const MAX_CALLERS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Arity {
    pub minimum: usize,
    pub maximum: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Signature {
    pub declaration: String,
    pub arity: Option<Arity>,
    pub visibility: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IncompatibleCaller {
    pub file: String,
    pub line: usize,
    pub expression: String,
    pub reason: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EditCheckReport {
    pub head: String,
    pub file: String,
    pub symbol: String,
    pub line: usize,
    pub worktree_matches_head: bool,
    pub before: Signature,
    pub after: Signature,
    pub arity_changed: bool,
    pub visibility_changed: bool,
    pub overloads_before: Vec<Signature>,
    pub overloads_after: Vec<Signature>,
    pub overloads_changed: bool,
    pub incompatible_callers: Vec<IncompatibleCaller>,
    pub callers_truncated: bool,
    pub unknown: Vec<String>,
    pub counts_floor: bool,
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .context("read Git HEAD")?;
    ensure!(
        output.status.success(),
        "Git HEAD read failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

pub fn edit_check(
    project: &Path,
    file: &str,
    symbol: &str,
    line: Option<usize>,
    replacement: &str,
) -> Result<EditCheckReport> {
    ensure!(project.is_absolute(), "project must be absolute");
    ensure!(
        !file.is_empty()
            && Path::new(file)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "file must be a repository-relative path without traversal"
    );
    ensure!(
        replacement.len() <= MAX_SOURCE_BYTES,
        "replacement exceeds 1 MiB"
    );
    let root = project.canonicalize()?;
    let git_root = git(&root, &["rev-parse", "--show-toplevel"])?;
    ensure!(
        Path::new(std::str::from_utf8(&git_root)?.trim_end()).canonicalize()? == root,
        "project must be the Git worktree root"
    );
    let head = String::from_utf8(git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?)?
        .trim()
        .to_owned();
    let object = format!("{head}:{file}");
    let size: usize = String::from_utf8(git(&root, &["cat-file", "-s", &object])?)?
        .trim()
        .parse()?;
    ensure!(size <= MAX_SOURCE_BYTES, "HEAD file exceeds 1 MiB");
    let entry = git(&root, &["ls-tree", &head, "--", file])?;
    ensure!(
        entry.starts_with(b"100644 ") || entry.starts_with(b"100755 "),
        "HEAD path must be a regular source file"
    );
    let source = String::from_utf8(git(&root, &["cat-file", "blob", &object])?)?;
    let language = detect_language(Path::new(file)).context("unsupported source language")?;
    ensure!(
        Path::new(file).extension().is_none_or(|ext| ext != "h"),
        "ambiguous .h dialect: use an unambiguous source file"
    );
    let mut report = check_source(&source, language, file, symbol, line, replacement)?;
    report.head = head;
    report.worktree_matches_head = worktree_matches(&root.join(file), source.as_bytes());
    if !report.worktree_matches_head {
        report.unknown.push("The working file differs from HEAD or cannot be read as a regular file without following a symlink; findings describe HEAD callers only.".into());
    }
    Ok(report)
}

#[cfg(unix)]
fn worktree_matches(path: &Path, source: &[u8]) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .is_ok_and(|file| {
            if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
                return false;
            }
            let mut bytes = Vec::new();
            file.take((MAX_SOURCE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .is_ok()
                && bytes == source
        })
}

#[cfg(not(unix))]
fn worktree_matches(_path: &Path, _source: &[u8]) -> bool {
    false
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk()).collect()
}

fn walk<'tree>(node: Node<'tree>, nodes: &mut Vec<Node<'tree>>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        nodes.push(node);
        let mut next = children(node);
        next.reverse();
        pending.extend(next);
    }
}

fn function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_declaration"
            | "method_definition"
            | "function_signature"
            | "method_signature"
    ) || (matches!(node.kind(), "declaration" | "field_declaration")
        && declarator(node).kind() == "function_declarator"
        && declarator(node)
            .child_by_field_name("declarator")
            .is_some_and(|n| {
                matches!(
                    n.kind(),
                    "identifier"
                        | "field_identifier"
                        | "qualified_identifier"
                        | "scoped_identifier"
                        | "operator_name"
                        | "destructor_name"
                )
            }))
}

fn declarator(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    while let Some(next) = current.child_by_field_name("declarator").or_else(|| {
        (current.kind() == "reference_declarator")
            .then(|| current.named_child(0))
            .flatten()
    }) {
        current = next;
        if current.kind() == "function_declarator" {
            break;
        }
    }
    current
}

fn name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    node.child_by_field_name("name")
        .or_else(|| declarator(node).child_by_field_name("declarator"))
        .map(|n| text(n, source))
}

fn scope(node: Node<'_>) -> Vec<usize> {
    let mut result = Vec::new();
    let mut parent = node.parent();
    while let Some(node) = parent {
        result.push(node.start_byte());
        parent = node.parent();
    }
    result
}

fn signature(node: Node<'_>, source: &str, language: Language) -> Signature {
    let parameters = node
        .child_by_field_name("parameters")
        .or_else(|| declarator(node).child_by_field_name("parameters"));
    let arity = parameters.and_then(|params| parameter_arity(params, source, language));
    let body = node.child_by_field_name("body");
    let declaration = source[node.start_byte()..body.map_or(node.end_byte(), |b| b.start_byte())]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .trim_end()
        .to_owned();
    let modifier = children(node)
        .into_iter()
        .find(|n| matches!(n.kind(), "visibility_modifier" | "modifiers"));
    let visibility = match language {
        Language::Rust => Some(modifier.map_or("private", |n| text(n, source)).to_owned()),
        Language::Java | Language::Php => Some(
            modifier
                .and_then(|n| {
                    text(n, source)
                        .split_whitespace()
                        .find(|word| matches!(*word, "public" | "private" | "protected"))
                })
                .unwrap_or("default")
                .to_owned(),
        ),
        Language::Go => name(node, source).map(|name| {
            if name.starts_with(char::is_uppercase) {
                "exported"
            } else {
                "unexported"
            }
            .into()
        }),
        Language::C => Some(
            if children(node)
                .iter()
                .any(|n| n.kind() == "storage_class_specifier" && text(*n, source) == "static")
            {
                "internal"
            } else {
                "external"
            }
            .into(),
        ),
        _ => None,
    };
    Signature {
        declaration,
        arity,
        visibility,
    }
}

fn parameter_arity(params: Node<'_>, source: &str, language: Language) -> Option<Arity> {
    let mut minimum = 0;
    let mut maximum = Some(0);
    let has_ellipsis = params
        .children(&mut params.walk())
        .any(|node| node.kind() == "...");
    for parameter in children(params) {
        let kind = parameter.kind();
        if matches!(kind, "comment" | "line_comment" | "block_comment") {
            continue;
        }
        if kind.contains("attribute") {
            return None;
        }
        if kind == "self_parameter"
            || (language == Language::TypeScript
                && parameter
                    .child_by_field_name("pattern")
                    .is_some_and(|n| text(n, source) == "this"))
        {
            return None;
        }
        if matches!(
            kind,
            "positional_separator"
                | "keyword_separator"
                | "dictionary_splat_pattern"
                | "list_splat_pattern"
        ) {
            return None;
        }
        if text(parameter, source) == "void" && matches!(language, Language::C | Language::Cpp) {
            continue;
        }
        if kind.contains("variadic")
            || matches!(kind, "rest_pattern" | "spread_parameter")
            || children(parameter)
                .iter()
                .any(|n| n.kind() == "rest_pattern")
        {
            maximum = None;
            continue;
        }
        let optional = matches!(
            kind,
            "default_parameter"
                | "typed_default_parameter"
                | "optional_parameter"
                | "assignment_pattern"
                | "optional_parameter_declaration"
        ) || parameter.child_by_field_name("default_value").is_some()
            || parameter.child_by_field_name("value").is_some();
        let count = if language == Language::Go {
            let mut cursor = parameter.walk();
            parameter
                .children(&mut cursor)
                .enumerate()
                .filter(|(index, _)| parameter.field_name_for_child(*index as u32) == Some("name"))
                .count()
                .max(1)
        } else {
            1
        };
        maximum = maximum.map(|n| n + count);
        if !optional {
            minimum = maximum?;
        }
    }
    if language == Language::C && children(params).is_empty() {
        return None;
    }
    // JavaScript permits omitted and surplus arguments independently of declared parameters.
    if language == Language::JavaScript {
        return None;
    }
    if language == Language::Php {
        maximum = None;
    }
    if has_ellipsis {
        maximum = None;
    }
    Some(Arity { minimum, maximum })
}

fn deduplicate_signatures(signatures: &mut Vec<Signature>) {
    let mut seen = Vec::new();
    signatures.retain(|signature| {
        if seen.contains(signature) {
            false
        } else {
            seen.push(signature.clone());
            true
        }
    });
}

fn check_source(
    source: &str,
    language: Language,
    file: &str,
    symbol: &str,
    line: Option<usize>,
    replacement: &str,
) -> Result<EditCheckReport> {
    let tree = parse_file(source.as_bytes(), language)?;
    ensure!(
        !tree.root_node().has_error(),
        "HEAD source has syntax errors; compatibility is unknown"
    );
    let mut nodes = Vec::new();
    walk(tree.root_node(), &mut nodes);
    let matches: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|n| {
            function(*n)
                && name(*n, source) == Some(symbol)
                && line.is_none_or(|line| n.start_position().row + 1 == line)
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "expected one named declaration at HEAD; found {} (supply its start line to disambiguate)",
        matches.len()
    );
    let old = matches[0];
    let mut proposed = source.to_owned();
    proposed.replace_range(old.byte_range(), replacement);
    ensure!(
        proposed.len() <= MAX_SOURCE_BYTES,
        "proposed file exceeds 1 MiB"
    );
    let new_tree = parse_file(proposed.as_bytes(), language)?;
    ensure!(
        !new_tree.root_node().has_error(),
        "replacement produces syntax errors"
    );
    let mut new_nodes = Vec::new();
    walk(new_tree.root_node(), &mut new_nodes);
    let start = old.start_byte();
    let end = start + replacement.len();
    let candidates: Vec<_> = new_nodes
        .iter()
        .copied()
        .filter(|n| {
            function(*n)
                && n.start_byte() >= start
                && n.end_byte() <= end
                && scope(*n) == scope(old)
        })
        .collect();
    ensure!(
        candidates.len() == 1,
        "replacement must contain exactly one complete declaration in the original scope"
    );
    let new = candidates[0];
    ensure!(
        name(new, &proposed) == Some(symbol),
        "replacement must retain the selected symbol name"
    );
    ensure!(
        proposed[start..new.start_byte()].trim().is_empty()
            && proposed[new.end_byte()..end].trim().is_empty(),
        "replacement must contain only the complete declaration"
    );
    let before = signature(old, source, language);
    let after = signature(new, &proposed, language);
    let mut overloads_before: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|n| function(*n) && name(*n, source) == Some(symbol) && scope(*n) == scope(old))
        .map(|n| signature(n, source, language))
        .collect();
    let mut overloads_after: Vec<_> = new_nodes
        .iter()
        .copied()
        .filter(|n| function(*n) && name(*n, &proposed) == Some(symbol) && scope(*n) == scope(new))
        .map(|n| signature(n, &proposed, language))
        .collect();
    deduplicate_signatures(&mut overloads_before);
    deduplicate_signatures(&mut overloads_after);
    let mut unknown = vec!["This is syntax evidence, not a compilation or safety verdict. Only callers in the selected HEAD file are examined; external, dynamic, indirect, and generated callers remain unknown.".into()];
    if matches!(language, Language::C | Language::Cpp) {
        unknown.push("C/C++ snapshots include same-scope declarations and definitions, deduplicating identical normalized signatures. Different parameter spellings, inherited defaults, multi-declarator statements, templates, and declarations in other files require semantic resolution; this is not a complete compiler overload set.".into());
    }
    if before.arity.is_none() || after.arity.is_none() {
        unknown.push(
            "Accepted argument counts are unknown for this parameter syntax or language.".into(),
        );
    }
    if before.visibility.is_none() || after.visibility.is_none() {
        unknown.push("Visibility semantics are not modelled for this language; inspect the declaration diff.".into());
    }
    let (incompatible_callers, callers_truncated) = rust_callers(
        &nodes,
        source,
        language,
        old,
        file,
        symbol,
        &before,
        &after,
        &mut unknown,
    );
    Ok(EditCheckReport {
        head: String::new(),
        file: file.into(),
        symbol: symbol.into(),
        line: old.start_position().row + 1,
        worktree_matches_head: false,
        arity_changed: before.arity != after.arity,
        visibility_changed: before.visibility != after.visibility,
        overloads_changed: overloads_before != overloads_after,
        before,
        after,
        overloads_before,
        overloads_after,
        incompatible_callers,
        callers_truncated,
        unknown,
        counts_floor: true,
    })
}

fn rust_modules(node: Node<'_>, source: &str) -> Option<Vec<String>> {
    let mut modules = Vec::new();
    let mut parent = node.parent();
    while let Some(node) = parent {
        if matches!(
            node.kind(),
            "impl_item" | "trait_item" | "function_item" | "block"
        ) {
            return None;
        }
        if node.kind() == "mod_item" {
            modules.push(text(node.child_by_field_name("name")?, source).to_owned());
        }
        parent = node.parent();
    }
    modules.reverse();
    Some(modules)
}

#[expect(
    clippy::too_many_arguments,
    reason = "bounded private analysis over one parsed file"
)]
fn rust_callers(
    nodes: &[Node<'_>],
    source: &str,
    language: Language,
    target: Node<'_>,
    file: &str,
    symbol: &str,
    before: &Signature,
    after: &Signature,
    unknown: &mut Vec<String>,
) -> (Vec<IncompatibleCaller>, bool) {
    let mut result = Vec::new();
    if language != Language::Rust {
        unknown.push("Provable caller checks currently cover explicit self::/super:: paths to Rust free functions only; other languages receive declaration/overload diffs.".into());
        return (result, false);
    }
    if nodes.iter().any(|n| {
        matches!(
            n.kind(),
            "attribute_item"
                | "inner_attribute_item"
                | "macro_invocation"
                | "macro_definition"
                | "use_declaration"
        )
    }) {
        unknown.push("Rust attributes, macros, or imports can alter name resolution; caller compatibility is unknown.".into());
        return (result, false);
    }
    let Some(target_modules) = rust_modules(target, source) else {
        unknown.push("Rust methods and nested functions need semantic resolution; caller compatibility is unknown.".into());
        return (result, false);
    };
    'calls: for call in nodes
        .iter()
        .copied()
        .filter(|n| n.kind() == "call_expression")
    {
        if target.byte_range().contains(&call.start_byte()) {
            continue;
        }
        let Some(callee) = call.child_by_field_name("function") else {
            continue;
        };
        let path = text(callee, source);
        let mut enclosing = call.parent();
        while enclosing.is_some_and(|n| n.kind() != "function_item") {
            enclosing = enclosing.and_then(|n| n.parent());
        }
        let Some(caller_modules) = enclosing.and_then(|n| rust_modules(n, source)) else {
            continue;
        };
        let mut resolved = caller_modules.clone();
        let mut parts = path.split("::").peekable();
        match parts.next() {
            Some("self") => {}
            Some("super") => {
                if resolved.pop().is_none() {
                    continue;
                }
            }
            _ => continue,
        }
        while parts.peek() == Some(&"super") {
            parts.next();
            if resolved.pop().is_none() {
                continue 'calls;
            }
        }
        resolved.extend(parts.map(str::to_owned));
        let mut expected = target_modules.clone();
        expected.push(symbol.to_owned());
        if resolved != expected {
            continue;
        }
        let Some(args) = call.child_by_field_name("arguments") else {
            continue;
        };
        let count = children(args)
            .iter()
            .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
            .count();
        let accepts =
            |arity: &Arity| count >= arity.minimum && arity.maximum.is_none_or(|max| count <= max);
        let mut reasons = Vec::new();
        if let (Some(old), Some(new)) = (&before.arity, &after.arity)
            && accepts(old)
            && !accepts(new)
        {
            reasons.push(format!(
                "{count} arguments accepted at HEAD are outside the proposed argument count"
            ));
        }
        if before.visibility.as_deref() == Some("pub")
            && after.visibility.as_deref() == Some("private")
            && !caller_modules.starts_with(&target_modules)
        {
            reasons.push(
                "the caller is outside the newly private function's module and its descendants"
                    .into(),
            );
        }
        if !reasons.is_empty() {
            if result.len() == MAX_CALLERS {
                return (result, true);
            }
            result.push(IncompatibleCaller {
                file: file.into(),
                line: call.start_position().row + 1,
                expression: path.into(),
                reason: reasons.join("; "),
            });
        }
    }
    (result, false)
}
