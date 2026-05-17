//! Extract decision-shape comments attached to a symbol's definition.
//!
//! "Rationale" here means a comment whose first non-whitespace token is one of
//! the recognized markers (`WHY`, `NOTE`, `IMPORTANT`, `FIXME`, `HACK`, `XXX`,
//! `TODO`) followed by a colon. The comment must be **immediately adjacent**
//! to the symbol's definition node (one of its previous siblings, with only
//! whitespace between).
//!
//! Two design choices worth keeping in mind:
//!
//! 1. **Marker-based, not all-doc-comments.** Filtering keeps the rationale
//!    table small and high-signal. Plain API descriptions ("Returns the
//!    parent of the path") are not stored — the agent recovers those from
//!    the symbol name + signature anyway. See `notes/20260509-...md` §1.2
//!    for the full reasoning.
//! 2. **Language-specific by design.** This module currently covers Rust and
//!    Python. Each language needs a deliberate attachment rule because comment
//!    placement varies (Rust `///` precedes the item, Python docstrings
//!    live inside the body as the first `(string)` statement, PHP `/** */`
//!    blocks immediately precede, etc.).

use codesage_protocol::{RationaleEntry, RationaleKind};
use tree_sitter::Node;

/// Walk previous siblings of `def_node`, collecting rationale-shape comments
/// until a non-comment / non-attribute node breaks the contiguous block.
///
/// Stops at the first non-comment, non-attribute node so a rationale comment
/// belonging to a *different* item earlier in the file doesn't bleed into
/// this symbol's rationale list. Rust attribute macros (`#[derive(...)]`,
/// `#[cfg(...)]`) are skipped over rather than treated as breakers because
/// they routinely sit between the doc-comment and the item.
pub fn extract_rust_rationale(def_node: &Node, source: &[u8]) -> Vec<RationaleEntry> {
    let mut entries = Vec::new();
    let mut sib = def_node.prev_sibling();
    while let Some(node) = sib {
        match node.kind() {
            "line_comment" | "block_comment" => {
                if let Ok(text) = node.utf8_text(source) {
                    let stripped = strip_rust_comment_markers(text);
                    if let Some(parsed) = parse_marker_line(&stripped, &node) {
                        entries.push(parsed);
                    }
                }
            }
            "attribute_item" | "inner_attribute_item" => {
                // Skip attributes — they sit between docs and the item.
            }
            _ => break,
        }
        sib = node.prev_sibling();
    }
    // We walked previous siblings, so entries is in reverse source order.
    // Restore source order so consumers reading the list line up with the file.
    entries.reverse();
    entries
}

/// Collect Python rationale from line comments immediately above a definition
/// and from a leading triple-quoted docstring inside the definition body.
///
/// Two tree-sitter shapes complicate the walk:
///   1. **Decorators.** `@foo\ndef bar()` wraps as
///         decorated_definition
///           decorator (@foo)
///           function_definition (bar)
///      so the rationale comment is a sibling of the wrapper, not of the
///      inner def. Anchor the walk at the wrapper so `# TODO:` above
///      `@app.route(...)` / `@property` / `@dataclass` / `@lru_cache` etc.
///      still attaches — these patterns dominate real Python codebases.
///   2. **First statement in a class body.** Tree-sitter parks the leading
///      comment inside a class as a child of `class_definition`, NOT inside
///      the body `block`. The first method's prev_sibling chain therefore
///      runs out before reaching the comment. When the anchor is the first
///      child of a class block, climb to the block and continue the walk
///      from its prev_siblings inside the class header.
pub fn extract_python_rationale(def_node: &Node, source: &[u8]) -> Vec<RationaleEntry> {
    let mut entries = Vec::new();
    let initial_anchor = python_traversal_anchor(def_node);
    let mut next_start_row = initial_anchor.start_position().row;

    let exhausted = walk_python_prev_comments(
        &initial_anchor,
        &mut next_start_row,
        &mut entries,
        source,
    );

    // Climb once into the enclosing class header if we ran out of siblings
    // inside the block without hitting any non-comment node — that's the
    // "first method in a class" shape where the comment lives at
    // class_definition level. Restricted to class_definition: climbing
    // out of a function body would attribute the comment above the outer
    // function to the inner nested def, which is wrong.
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

/// Parse a comment body's first line for a `MARKER: rest` pattern. Multi-line
/// block comments only have their first line examined for the marker; the
/// full body is preserved as `text`. This matches author intent: the marker
/// labels the whole comment, so all of it counts as the rationale.
fn parse_marker_line(body: &str, node: &Node) -> Option<RationaleEntry> {
    let mut iter = body.splitn(2, |c: char| c == ':' || c.is_whitespace());
    let maybe_marker = iter.next()?.trim();
    if maybe_marker.is_empty() {
        return None;
    }
    let kind = RationaleKind::from_marker(maybe_marker)?;

    // Re-split on `:` specifically to capture only the after-colon body.
    // If the first token was a marker but no colon followed (e.g. "TODO ..."
    // without a colon), we still record it — TODO/FIXME tools often write
    // the marker without a colon.
    let after_marker = match body.find(':') {
        Some(idx) if idx == maybe_marker.len() => body[idx + 1..].trim().to_string(),
        Some(_) | None => body[maybe_marker.len()..]
            .trim_start_matches(|c: char| c.is_whitespace() || c == ':')
            .to_string(),
    };

    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(RationaleEntry {
        kind,
        text: after_marker,
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

    // Marker parsing — node-independent paths

    fn parse_for_test(body: &str) -> Option<(RationaleKind, String)> {
        // Build a tiny tree with one comment line so we can call parse_marker_line.
        // Since RationaleEntry needs a Node, easier to test the logic end-to-end
        // via the public extract_rust_rationale function in an integration test.
        // Here we just exercise the matcher.
        let mut iter = body.splitn(2, |c: char| c == ':' || c.is_whitespace());
        let marker = iter.next()?.trim();
        let kind = RationaleKind::from_marker(marker)?;
        let after = match body.find(':') {
            Some(idx) if idx == marker.len() => body[idx + 1..].trim().to_string(),
            _ => body[marker.len()..]
                .trim_start_matches(|c: char| c.is_whitespace() || c == ':')
                .to_string(),
        };
        Some((kind, after))
    }

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

    // End-to-end: parse a Rust source snippet, verify rationale appears
    // attached to the right symbol.

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
}
