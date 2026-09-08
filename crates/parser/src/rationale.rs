//! Extract marked rationale from adjacent comments and leading Python docstrings.
//! `WHY`, `NOTE`, and `IMPORTANT` require a colon; `FIXME`, `HACK`, `XXX`, and
//! `TODO` also accept whitespace. Ordinary API descriptions are not rationale.

use codesage_protocol::{RationaleEntry, RationaleKind};
use tree_sitter::Node;

/// Collect adjacent marked comments across Rust attributes, in source order.
pub fn extract_rust_rationale(def_node: &Node, source: &[u8]) -> Vec<RationaleEntry> {
    let mut entries = Vec::new();
    let mut next_start_row = def_node.start_position().row;
    let mut sib = def_node.prev_sibling();
    while let Some(node) = sib {
        match node.kind() {
            "line_comment" | "block_comment" => {
                // Rust line comments include the newline; its next row is not content.
                let end = node.end_position();
                let last_content_row = if end.column == 0 && end.row > node.start_position().row {
                    end.row - 1
                } else {
                    end.row
                };
                if last_content_row + 1 != next_start_row {
                    break;
                }
                if let Ok(text) = node.utf8_text(source) {
                    let stripped = strip_rust_comment_markers(text);
                    if let Some(parsed) = parse_marker_line(&stripped, &node) {
                        entries.push(parsed);
                    }
                }
                next_start_row = node.start_position().row;
            }
            "attribute_item" | "inner_attribute_item" => {
                // Attributes bridge the comment-to-definition adjacency chain.
                next_start_row = node.start_position().row;
            }
            _ => break,
        }
        sib = node.prev_sibling();
    }
    entries.reverse();
    entries
}

/// Collect Python rationale from line comments immediately above a definition
/// and from a leading triple-quoted docstring inside the definition body.
///
/// Decorated definitions anchor at their wrapper. A class's leading comment
/// belongs to `class_definition`, outside its body block, so the walk may climb once.
pub fn extract_python_rationale(def_node: &Node, source: &[u8]) -> Vec<RationaleEntry> {
    let mut entries = Vec::new();
    let initial_anchor = python_traversal_anchor(def_node);
    let mut next_start_row = initial_anchor.start_position().row;

    let exhausted =
        walk_python_prev_comments(&initial_anchor, &mut next_start_row, &mut entries, source);

    // Climb only from classes; climbing a function would steal its rationale
    // for a nested definition.
    if exhausted
        && let Some(block) = initial_anchor.parent()
        && block.kind() == "block"
        && block
            .parent()
            .is_some_and(|p| p.kind() == "class_definition")
    {
        let _ = walk_python_prev_comments(&block, &mut next_start_row, &mut entries, source);
    }

    entries.reverse();

    if let Some(parsed) = extract_python_docstring(def_node, source) {
        entries.push(parsed);
    }

    entries
}

/// If `def_node` is a function_definition or class_definition wrapped by
/// `decorated_definition`, return the wrapper so prev-sibling traversal sees
/// the comment above the decorators. Otherwise return `def_node`.
fn python_traversal_anchor<'a>(def_node: &Node<'a>) -> Node<'a> {
    if let Some(parent) = def_node.parent()
        && parent.kind() == "decorated_definition"
    {
        return parent;
    }
    *def_node
}

/// Walk prev_siblings of `anchor`, attaching adjacent `# MARKER: text`
/// comments. Skips unnamed tokens (the `:`, `def`, etc.) so they don't
/// look like break-points. Returns `true` when the sibling chain runs
/// out cleanly (caller may want to climb); `false` when a non-comment
/// named node stops the walk (comments past it are definitely not
/// attached to this def).
fn walk_python_prev_comments(
    anchor: &Node,
    next_start_row: &mut usize,
    entries: &mut Vec<RationaleEntry>,
    source: &[u8],
) -> bool {
    let mut sib = anchor.prev_sibling();
    while let Some(node) = sib {
        if !node.is_named() {
            sib = node.prev_sibling();
            continue;
        }
        if node.kind() == "comment" && node.end_position().row + 1 == *next_start_row {
            if let Some(parsed) = parse_python_comment(&node, source) {
                entries.push(parsed);
            }
            *next_start_row = node.start_position().row;
            sib = node.prev_sibling();
        } else {
            return false;
        }
    }
    true
}

/// Collect adjacent C/C++/Go/PHP rationale across PHP attributes, in source order.
/// Scan block-comment lines for the first marker, including interior docblock lines.
pub fn extract_clike_rationale(def_node: &Node, source: &[u8]) -> Vec<RationaleEntry> {
    let mut entries = Vec::new();
    let mut next_start_row = def_node.start_position().row;
    let mut sib = def_node.prev_sibling();
    while let Some(node) = sib {
        match node.kind() {
            "comment" => {
                // Some grammars include the newline; its next row is not content.
                let end = node.end_position();
                let last_content_row = if end.column == 0 && end.row > node.start_position().row {
                    end.row - 1
                } else {
                    end.row
                };
                if last_content_row + 1 != next_start_row {
                    break;
                }
                if let Ok(text) = node.utf8_text(source)
                    && let Some(parsed) = parse_clike_comment(text, &node)
                {
                    entries.push(parsed);
                }
                next_start_row = node.start_position().row;
            }
            // PHP attributes bridge the comment-to-definition adjacency chain.
            "attribute_list" => {
                next_start_row = node.start_position().row;
            }
            _ => break,
        }
        sib = node.prev_sibling();
    }
    entries.reverse();
    entries
}

/// Parse one C-family / PHP comment node into at most one rationale entry. The
/// delimiters are stripped, then each interior line (with a leading docblock
/// `*` removed) is scanned for the first `MARKER: text` — so both a single-line
/// `// WHY: ...` and a multi-line `/** ... * WHY: ... */` docblock resolve.
fn parse_clike_comment(raw: &str, node: &Node) -> Option<RationaleEntry> {
    let body = strip_clike_comment_markers(raw);
    for line in body.lines() {
        let line = line.trim_start();
        let line = line.strip_prefix('*').map(str::trim_start).unwrap_or(line);
        if let Some((kind, text)) = parse_marker(line) {
            return Some(RationaleEntry {
                kind,
                text,
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
            });
        }
    }
    None
}

/// Strip C-family / PHP comment delimiters: PHP `#` line comments, plus
/// everything [`strip_rust_comment_markers`] handles (`//`, `///`, `//!`,
/// `/* */`, `/** */`, `/*! */`). A block comment's interior newlines are kept so
/// the caller can scan each docblock line for a marker.
fn strip_clike_comment_markers(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('#') {
        // PHP `#` line comment. `#[...]` attributes never reach here — the
        // grammar emits them as `attribute_list`, not `comment`.
        return rest.trim_start().to_string();
    }
    strip_rust_comment_markers(trimmed)
}

/// Strip Rust comment delimiters and any leading `!` (inner doc) so the
/// returned text contains only the comment body. `///`, `//!`, `//`, and
/// `/* ... */` / `/** ... */` are all reduced to their inner content.
fn strip_rust_comment_markers(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("///") {
        return rest.trim_start().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("//!") {
        return rest.trim_start().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("//") {
        return rest.trim_start().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("/**") {
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        return rest.trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("/*!") {
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        return rest.trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("/*") {
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        return rest.trim().to_string();
    }
    trimmed.to_string()
}

fn parse_python_comment(node: &Node, source: &[u8]) -> Option<RationaleEntry> {
    let raw = node.utf8_text(source).ok()?;
    let body = raw.trim_start().strip_prefix('#')?.trim_start();
    parse_marker_line(body, node)
}

fn extract_python_docstring(def_node: &Node, source: &[u8]) -> Option<RationaleEntry> {
    let body = def_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first_stmt = body.named_children(&mut cursor).next()?;
    if first_stmt.kind() != "expression_statement" {
        return None;
    }

    let mut cursor = first_stmt.walk();
    for child in first_stmt.named_children(&mut cursor) {
        if child.kind() == "string" {
            return parse_python_docstring(&child, source);
        }
    }
    None
}

fn parse_python_docstring(node: &Node, source: &[u8]) -> Option<RationaleEntry> {
    let raw = node.utf8_text(source).ok()?.trim();
    let body = raw
        .strip_prefix("\"\"\"")
        .and_then(|s| s.strip_suffix("\"\"\""))
        .or_else(|| raw.strip_prefix("'''").and_then(|s| s.strip_suffix("'''")))?;
    parse_marker_line(body.trim(), node)
}

/// Parse the first token as a marker, preserving the remaining body as text.
fn parse_marker(body: &str) -> Option<(RationaleKind, String)> {
    let mut iter = body.splitn(2, |c: char| c == ':' || c.is_whitespace());
    let maybe_marker = iter.next()?.trim();
    if maybe_marker.is_empty() {
        return None;
    }
    let kind = RationaleKind::from_marker(maybe_marker)?;

    // Require colons on prose markers so "Note that ..." is not rationale.
    let has_colon = body.as_bytes().get(maybe_marker.len()) == Some(&b':');
    if !has_colon
        && !matches!(
            kind,
            RationaleKind::Todo | RationaleKind::Fixme | RationaleKind::Xxx | RationaleKind::Hack
        )
    {
        return None;
    }

    let after_marker = match body.find(':') {
        Some(idx) if idx == maybe_marker.len() => body[idx + 1..].trim().to_string(),
        Some(_) | None => body[maybe_marker.len()..]
            .trim_start_matches(|c: char| c.is_whitespace() || c == ':')
            .to_string(),
    };

    Some((kind, after_marker))
}

fn parse_marker_line(body: &str, node: &Node) -> Option<RationaleEntry> {
    let (kind, text) = parse_marker(body)?;
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;
    Some(RationaleEntry {
        kind,
        text,
        line_start,
        line_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_line_doc_comment() {
        assert_eq!(strip_rust_comment_markers("/// hello"), "hello");
        assert_eq!(strip_rust_comment_markers("    /// hello"), "hello");
        assert_eq!(strip_rust_comment_markers("//! crate-level"), "crate-level");
        assert_eq!(
            strip_rust_comment_markers("// just a comment"),
            "just a comment"
        );
    }

    #[test]
    fn strip_block_doc_comment() {
        assert_eq!(strip_rust_comment_markers("/** doc body */"), "doc body");
        assert_eq!(strip_rust_comment_markers("/*! inner */"), "inner");
        assert_eq!(strip_rust_comment_markers("/* note */"), "note");
    }

    #[test]
    fn strip_block_without_terminator_does_not_panic() {
        // tree-sitter's `block_comment` kind always has both delimiters in
        // the byte-range. Defensive coverage in case it ever doesn't.
        let s = strip_rust_comment_markers("/** unterminated");
        assert_eq!(s, "unterminated");
    }

    use super::parse_marker as parse_for_test;

    #[test]
    fn parses_why_marker() {
        let r = parse_for_test("WHY: caches the result").unwrap();
        assert_eq!(r.0, RationaleKind::Why);
        assert_eq!(r.1, "caches the result");
    }

    #[test]
    fn parses_note_marker_case_insensitive() {
        let r = parse_for_test("note: case folds").unwrap();
        assert_eq!(r.0, RationaleKind::Note);
        assert_eq!(r.1, "case folds");
    }

    #[test]
    fn parses_todo_without_colon() {
        let r = parse_for_test("TODO write tests").unwrap();
        assert_eq!(r.0, RationaleKind::Todo);
        assert_eq!(r.1, "write tests");
    }

    #[test]
    fn rejects_unrecognized_marker() {
        assert!(parse_for_test("STYLE: tabs not spaces").is_none());
        assert!(parse_for_test("Just a regular comment.").is_none());
    }

    #[test]
    fn rejects_empty_body() {
        assert!(parse_for_test("").is_none());
    }

    fn extract_for_source(src: &str) -> Vec<codesage_protocol::Symbol> {
        let tree = crate::parse::parse_file(src.as_bytes(), codesage_protocol::Language::Rust)
            .expect("rust parse");
        crate::extract::extract_symbols(
            &tree,
            src.as_bytes(),
            codesage_protocol::Language::Rust,
            "src.rs",
        )
        .expect("rust extraction")
    }

    #[test]
    fn extracts_why_comment_above_function() {
        let src = "\
/// WHY: tested on lib.rs corpus, see notes/.
fn parent_or_dot(p: &str) -> &str { p }
";
        let symbols = extract_for_source(src);
        let parent = symbols.iter().find(|s| s.name == "parent_or_dot").unwrap();
        assert_eq!(parent.rationale.len(), 1);
        assert_eq!(parent.rationale[0].kind, RationaleKind::Why);
        assert!(parent.rationale[0].text.contains("lib.rs corpus"));
    }

    #[test]
    fn extracts_multiple_markers_in_source_order() {
        let src = "\
// NOTE: holds a per-process lock.
// IMPORTANT: do not pickle this struct.
fn worker() {}
";
        let symbols = extract_for_source(src);
        let worker = symbols.iter().find(|s| s.name == "worker").unwrap();
        assert_eq!(worker.rationale.len(), 2);
        assert_eq!(worker.rationale[0].kind, RationaleKind::Note);
        assert_eq!(worker.rationale[1].kind, RationaleKind::Important);
    }

    #[test]
    fn skips_attributes_between_doc_and_item() {
        let src = "\
/// WHY: derive is required for serde round-trip.
#[derive(Debug)]
struct Foo;
";
        let symbols = extract_for_source(src);
        let foo = symbols.iter().find(|s| s.name == "Foo").unwrap();
        assert_eq!(foo.rationale.len(), 1);
        assert_eq!(foo.rationale[0].kind, RationaleKind::Why);
    }

    #[test]
    fn ignores_unrelated_doc_comment() {
        let src = "\
/// Returns the input unchanged. Just describes behavior.
fn id<T>(x: T) -> T { x }
";
        let symbols = extract_for_source(src);
        let id = symbols.iter().find(|s| s.name == "id").unwrap();
        assert!(id.rationale.is_empty());
    }

    #[test]
    fn blank_line_gapped_comment_does_not_attach() {
        let src = "\
// NOTE: general file-level note, not about beta.

fn beta() {}
";
        let symbols = extract_for_source(src);
        let beta = symbols.iter().find(|s| s.name == "beta").unwrap();
        assert!(
            beta.rationale.is_empty(),
            "blank-line-gapped comment must not attach"
        );
    }

    #[test]
    fn adjacent_comment_still_attaches_after_guard() {
        let src = "\
// FIXME: needs bounds check.
fn gamma() {}
";
        let symbols = extract_for_source(src);
        let gamma = symbols.iter().find(|s| s.name == "gamma").unwrap();
        assert_eq!(gamma.rationale.len(), 1);
        assert_eq!(gamma.rationale[0].kind, RationaleKind::Fixme);
    }

    #[test]
    fn note_without_colon_is_rejected() {
        assert!(parse_for_test("Note that this is prose").is_none());
        assert!(parse_for_test("Why not just inline it").is_none());
        assert!(parse_for_test("Important consideration here").is_none());
        assert!(parse_for_test("FIXME broken").is_some());
        assert!(parse_for_test("HACK workaround").is_some());
        assert!(parse_for_test("XXX revisit").is_some());
        assert_eq!(
            parse_for_test("NOTE: real note").unwrap().0,
            RationaleKind::Note
        );
    }

    #[test]
    fn does_not_borrow_rationale_from_earlier_item() {
        let src = "\
/// WHY: belongs to alpha.
fn alpha() {}

fn beta() {}
";
        let symbols = extract_for_source(src);
        let alpha = symbols.iter().find(|s| s.name == "alpha").unwrap();
        let beta = symbols.iter().find(|s| s.name == "beta").unwrap();
        assert_eq!(alpha.rationale.len(), 1);
        assert!(beta.rationale.is_empty());
    }

    fn extract_lang(
        src: &str,
        lang: codesage_protocol::Language,
        path: &str,
    ) -> Vec<codesage_protocol::Symbol> {
        let tree = crate::parse::parse_file(src.as_bytes(), lang).expect("parse");
        crate::extract::extract_symbols(&tree, src.as_bytes(), lang, path).expect("extract")
    }

    #[test]
    fn strip_clike_markers() {
        assert_eq!(strip_clike_comment_markers("# WHY: x"), "WHY: x");
        assert_eq!(strip_clike_comment_markers("// note"), "note");
        assert_eq!(strip_clike_comment_markers("/** WHY: y */"), "WHY: y");
    }

    #[test]
    fn extracts_c_line_comment_rationale() {
        let src = "// WHY: guards against a null deref.\nint foo(int x) { return x; }\n";
        let syms = extract_lang(src, codesage_protocol::Language::C, "s.c");
        let foo = syms.iter().find(|s| s.name == "foo").expect("foo symbol");
        assert_eq!(foo.rationale.len(), 1);
        assert_eq!(foo.rationale[0].kind, RationaleKind::Why);
        assert!(foo.rationale[0].text.contains("null deref"));
    }

    #[test]
    fn extracts_cpp_line_comment_rationale() {
        let src = "// IMPORTANT: not thread-safe.\nvoid run() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Cpp, "s.cpp");
        let run = syms.iter().find(|s| s.name == "run").expect("run symbol");
        assert_eq!(run.rationale.len(), 1);
        assert_eq!(run.rationale[0].kind, RationaleKind::Important);
    }

    #[test]
    fn extracts_go_line_comment_rationale() {
        let src = "package main\n// FIXME: retries are not idempotent yet.\nfunc Foo() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Go, "s.go");
        let foo = syms.iter().find(|s| s.name == "Foo").expect("Foo symbol");
        assert_eq!(foo.rationale.len(), 1);
        assert_eq!(foo.rationale[0].kind, RationaleKind::Fixme);
    }

    #[test]
    fn extracts_php_docblock_rationale() {
        let src = "<?php\n/**\n * WHY: cached because the upstream API is rate-limited.\n */\nfunction load() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Php, "s.php");
        let load = syms.iter().find(|s| s.name == "load").expect("load symbol");
        assert_eq!(load.rationale.len(), 1);
        assert_eq!(load.rationale[0].kind, RationaleKind::Why);
        assert!(load.rationale[0].text.contains("rate-limited"));
    }

    #[test]
    fn extracts_php_hash_comment_rationale() {
        let src = "<?php\n# HACK: works around a driver bug.\nfunction q() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Php, "s.php");
        let q = syms.iter().find(|s| s.name == "q").expect("q symbol");
        assert_eq!(q.rationale.len(), 1);
        assert_eq!(q.rationale[0].kind, RationaleKind::Hack);
    }

    #[test]
    fn php_rationale_survives_attribute_between_docblock_and_fn() {
        let src = "<?php\n/** WHY: the sole registration entrypoint. */\n#[Route(\"/x\")]\nfunction handler() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Php, "s.php");
        let handler = syms
            .iter()
            .find(|s| s.name == "handler")
            .expect("handler symbol");
        assert_eq!(handler.rationale.len(), 1);
        assert_eq!(handler.rationale[0].kind, RationaleKind::Why);
    }

    #[test]
    fn clike_plain_docblock_without_marker_does_not_attach() {
        let src = "<?php\n/**\n * Returns the parsed config. Just describes behaviour.\n */\nfunction cfg() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Php, "s.php");
        let cfg = syms.iter().find(|s| s.name == "cfg").expect("cfg symbol");
        assert!(cfg.rationale.is_empty());
    }

    #[test]
    fn clike_blank_line_gapped_comment_does_not_attach() {
        let src = "package main\n// NOTE: file-level note, not about Bar.\n\nfunc Bar() {}\n";
        let syms = extract_lang(src, codesage_protocol::Language::Go, "s.go");
        let bar = syms.iter().find(|s| s.name == "Bar").expect("Bar symbol");
        assert!(bar.rationale.is_empty());
    }
}
