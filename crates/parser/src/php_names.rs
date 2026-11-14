//! PHP class-name resolution for qualified spellings.
//!
//! PHP resolves a qualified class name at compile time from the file alone:
//! `\A\B` is exactly `A\B`; `namespace\B` is the current namespace plus `B`;
//! any other `A\B` expands its first segment through a class `use` alias
//! when one names it, otherwise it is relative to the current namespace.
//! Bare names stay as written; their binding is the resolver's job.

use tree_sitter::Node;

use crate::parse::node_text_lossy;

/// The fully qualified class name a `qualified_name` or `relative_name` node
/// spells, without a leading `\`. A name that resolves into the global
/// namespace as a single segment keeps its leading `\` (`\Exception`), so it
/// stays distinguishable from a bare name that PHP resolves against the
/// current namespace. Returns `None` for any other node kind.
pub(crate) fn resolve_class_name(node: &Node, root: &Node, source: &[u8]) -> Option<String> {
    let text: String = node_text_lossy(node, source)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let resolved = match node.kind() {
        "relative_name" => {
            let rest = text
                .get(..10)
                .filter(|prefix| prefix.eq_ignore_ascii_case("namespace\\"))
                .map(|_| &text[10..])?;
            join(&namespace_at(node, root, source), rest)
        }
        "qualified_name" => {
            if let Some(rest) = text.strip_prefix('\\') {
                rest.to_string()
            } else {
                let (first, remainder) = text.split_once('\\')?;
                match alias_target(node, root, source, first) {
                    Some(target) => join(&target, remainder),
                    None => join(&namespace_at(node, root, source), &text),
                }
            }
        }
        _ => return None,
    };
    if resolved.is_empty() {
        return None;
    }
    Some(if resolved.contains('\\') {
        resolved
    } else {
        format!("\\{resolved}")
    })
}

fn join(prefix: &str, rest: &str) -> String {
    if prefix.is_empty() {
        rest.to_string()
    } else {
        format!("{prefix}\\{rest}")
    }
}

/// The namespace in effect at `node`: the enclosing braced `namespace { }`
/// block, otherwise the last file-level `namespace Name;` before it.
fn namespace_at(node: &Node, root: &Node, source: &[u8]) -> String {
    if let Some(definition) = enclosing_namespace(node) {
        return definition
            .child_by_field_name("name")
            .map(|name| node_text_lossy(&name, source))
            .unwrap_or_default();
    }
    let mut current = String::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.start_byte() > node.start_byte() {
            break;
        }
        if child.kind() == "namespace_definition"
            && let Some(name) = child.child_by_field_name("name")
        {
            current = node_text_lossy(&name, source);
        }
    }
    current
}

fn enclosing_namespace<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "namespace_definition" {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

/// The class a `use` alias named `alias` imports at `node`, matched
/// case-insensitively as PHP does. Only `use` declarations earlier in the
/// same namespace block count; `use function` and `use const` import other
/// symbol tables and never alias a class.
fn alias_target(node: &Node, root: &Node, source: &[u8], alias: &str) -> Option<String> {
    let container = enclosing_namespace(node)
        .and_then(|definition| definition.child_by_field_name("body"))
        .unwrap_or(*root);
    let mut found = None;
    let mut cursor = container.walk();
    for child in container.children(&mut cursor) {
        if child.start_byte() >= node.start_byte() {
            break;
        }
        match child.kind() {
            // A file-level `namespace Name;` starts a new block whose imports
            // begin empty.
            "namespace_definition" if child.child_by_field_name("body").is_none() => found = None,
            "namespace_use_declaration" => {
                for (name, target) in class_imports(&child, source) {
                    if name.eq_ignore_ascii_case(alias) {
                        found = Some(target);
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// `(alias, fully qualified class)` for each class import in one `use`
/// declaration, including group clauses.
fn class_imports(declaration: &Node, source: &[u8]) -> Vec<(String, String)> {
    if declaration.child_by_field_name("type").is_some() {
        return Vec::new();
    }
    let mut prefix = None;
    let mut clauses = Vec::new();
    let mut cursor = declaration.walk();
    for child in declaration.named_children(&mut cursor) {
        match child.kind() {
            "namespace_name" => prefix = Some(node_text_lossy(&child, source)),
            "namespace_use_clause" => clauses.push(child),
            "namespace_use_group" => {
                let mut group = child.walk();
                clauses.extend(
                    child
                        .named_children(&mut group)
                        .filter(|clause| clause.kind() == "namespace_use_clause"),
                );
            }
            _ => {}
        }
    }
    let mut imports = Vec::new();
    for clause in clauses {
        if clause.child_by_field_name("type").is_some() {
            continue;
        }
        let alias = clause.child_by_field_name("alias");
        let mut walker = clause.walk();
        let Some(target) = clause
            .named_children(&mut walker)
            .find(|n| matches!(n.kind(), "name" | "qualified_name") && Some(*n) != alias)
        else {
            continue;
        };
        let target: String = node_text_lossy(&target, source)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let target = target.trim_start_matches('\\');
        let target = match &prefix {
            Some(prefix) => format!("{}\\{target}", prefix.trim_start_matches('\\')),
            None => target.to_string(),
        };
        let alias = match alias {
            Some(alias) => node_text_lossy(&alias, source),
            None => target.rsplit('\\').next().unwrap_or(&target).to_string(),
        };
        imports.push((alias, target));
    }
    imports
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::Language;

    fn resolved(src: &str) -> Vec<String> {
        let tree = crate::parse::parse_file(src.as_bytes(), Language::Php).unwrap();
        let root = tree.root_node();
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "qualified_name" | "relative_name")
                && !node
                    .parent()
                    .is_some_and(|p| p.kind() == "namespace_use_clause")
                && let Some(name) = resolve_class_name(&node, &root, src.as_bytes())
            {
                out.push((node.start_byte(), name));
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        out.sort();
        out.into_iter().map(|(_, name)| name).collect()
    }

    #[test]
    fn leading_backslash_names_exactly_that_class() {
        assert_eq!(
            resolved(
                "<?php\nnamespace App;\nuse Other\\A;\nnew \\App\\A\\Foo(); new \\Exception();\n"
            ),
            ["App\\A\\Foo", "\\Exception"]
        );
    }

    #[test]
    fn qualified_name_is_relative_to_namespace_unless_aliased() {
        assert_eq!(
            resolved(
                "<?php\nnamespace App;\nnew A\\Foo();\nuse Lib\\Models;\nuse Vendor\\Pkg as P;\n\
                 models\\User::find(); P\\Thing::class; namespace\\Local\\Bar::x();\n"
            ),
            [
                "App\\A\\Foo",
                "Lib\\Models\\User",
                "Vendor\\Pkg\\Thing",
                "App\\Local\\Bar"
            ]
        );
    }

    #[test]
    fn function_and_const_imports_do_not_alias_classes() {
        assert_eq!(
            resolved(
                "<?php\nnamespace App;\nuse function Lib\\A;\nuse const Lib\\B;\n\
                 use Lib\\{C, function D};\nnew A\\X(); new B\\Y(); new C\\Z(); new D\\W();\n"
            ),
            ["App\\A\\X", "App\\B\\Y", "Lib\\C\\Z", "App\\D\\W"]
        );
    }

    #[test]
    fn each_namespace_block_has_its_own_imports() {
        assert_eq!(
            resolved(
                "<?php\nnamespace One;\nuse Lib\\M;\nnew M\\A();\nnamespace Two;\nnew M\\B();\n"
            ),
            ["Lib\\M\\A", "Two\\M\\B"]
        );
        assert_eq!(
            resolved(
                "<?php\nnamespace One { use Lib\\M; new M\\A(); }\nnamespace { new M\\B(); new \\G(); }\n"
            ),
            ["Lib\\M\\A", "M\\B", "\\G"]
        );
    }
}
