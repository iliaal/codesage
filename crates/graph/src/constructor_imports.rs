use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use codesage_parser::{
    detect::detect_language, parse::parse_file_tolerant, source::read_indexable_file,
};
use codesage_protocol::{Language, Reference, ReferenceKind, Symbol, SymbolKind, work::checkpoint};
use codesage_storage::Database;
use tree_sitter::Node;

const MAX_FILES: usize = 64;
const MAX_MODULES: usize = 256;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 100_000;

#[derive(Default)]
pub(crate) struct ConstructorImports {
    sources: HashMap<String, Option<Arc<Surface>>>,
    modules: HashMap<(String, Arc<str>), Option<String>>,
    bytes: usize,
    capped: bool,
}

#[derive(Default)]
struct Surface {
    constructions: HashMap<(u32, u32), Arc<NamedImport>>,
    exports: Vec<ExportedClass>,
}

struct NamedImport {
    name: String,
    module: Arc<str>,
}

struct ExportedClass {
    name: String,
    line_start: u32,
    line_end: u32,
}

impl ConstructorImports {
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    pub(crate) fn select(
        &mut self,
        db: &Database,
        row: &Reference,
        candidates: &[Symbol],
    ) -> Result<Option<Symbol>> {
        if row.kind != ReferenceKind::Instantiation || js_language(&row.from_file).is_none() {
            return Ok(None);
        }
        let Some(caller) = self.source(db, &row.from_file)? else {
            return Ok(None);
        };
        let Some(import) = caller.constructions.get(&(row.line, row.col)) else {
            return Ok(None);
        };
        if import.name != row.to_name {
            return Ok(None);
        }
        let Some(file) = self.module(db, &row.from_file, &import.module)? else {
            return Ok(None);
        };
        let Some(target) = self.source(db, &file)? else {
            return Ok(None);
        };
        let mut matching = candidates.iter().filter(|symbol| {
            symbol.kind == SymbolKind::Class
                && symbol.file_path == file
                && symbol.qualified_name == import.name
                && target.exports.iter().any(|export| {
                    export.name == import.name
                        && export.line_start == symbol.line_start
                        && export.line_end == symbol.line_end
                })
        });
        Ok(match (matching.next(), matching.next()) {
            (Some(only), None) => Some(only.clone()),
            _ => None,
        })
    }

    fn module(&mut self, db: &Database, caller: &str, module: &Arc<str>) -> Result<Option<String>> {
        let key = (caller.to_string(), Arc::clone(module));
        if let Some(file) = self.modules.get(&key) {
            return Ok(file.clone());
        }
        if self.modules.len() >= MAX_MODULES {
            self.capped = true;
            return Ok(None);
        }
        let mut resolved = None;
        for file in crate::bundle::path_import_candidates(module, caller) {
            checkpoint()?;
            if db.file_id_for_path(&file)?.is_some() {
                let basename = file.rsplit('/').next().unwrap_or(&file);
                let explicit = module.rsplit('/').next().unwrap_or(module);
                // A package manifest can override a directory's index entrypoint.
                if !basename.starts_with("index.")
                    || explicit == "index"
                    || explicit.starts_with("index.")
                {
                    resolved = Some(file);
                }
                break;
            }
        }
        self.modules.insert(key, resolved.clone());
        Ok(resolved)
    }

    fn source(&mut self, db: &Database, file: &str) -> Result<Option<Arc<Surface>>> {
        if let Some(source) = self.sources.get(file) {
            return Ok(source.clone());
        }
        if self.sources.len() >= MAX_FILES || self.bytes >= MAX_BYTES {
            self.capped = true;
            return Ok(None);
        }
        let source = self.read_surface(db, file)?;
        self.sources.insert(file.to_string(), source.clone());
        Ok(source)
    }

    fn read_surface(&mut self, db: &Database, file: &str) -> Result<Option<Arc<Surface>>> {
        let Some(language) = js_language(file) else {
            return Ok(None);
        };
        let Some(root) = project_root(db) else {
            return Ok(None);
        };
        checkpoint()?;
        let Ok(Some(source)) = read_indexable_file(root, Path::new(file)) else {
            checkpoint()?;
            return Ok(None);
        };
        self.bytes += source.bytes.len();
        if self.bytes > MAX_BYTES || source.bytes.len() > MAX_FILE_BYTES {
            self.capped = true;
            return Ok(None);
        }
        if db.get_file_hash(file)?.as_deref() != Some(source.content_hash.as_str()) {
            return Ok(None);
        }
        let parsed = parse_file_tolerant(&source.bytes, language)?;
        if parsed.degraded {
            return Ok(None);
        }
        let Some(surface) = surface(parsed.tree.root_node(), &source.bytes)? else {
            self.capped = true;
            return Ok(None);
        };
        Ok(Some(Arc::new(surface)))
    }
}

fn js_language(file: &str) -> Option<Language> {
    detect_language(Path::new(file))
        .filter(|language| matches!(language, Language::JavaScript | Language::TypeScript))
}

fn project_root(db: &Database) -> Option<&Path> {
    let path = db.path()?;
    let directory = path.parent()?;
    (path.file_name()? == "index.db" && directory.file_name()? == ".codesage")
        .then(|| directory.parent())
        .flatten()
}

fn surface(root: Node<'_>, bytes: &[u8]) -> Result<Option<Surface>> {
    let mut nodes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if nodes.len() == MAX_NODES {
            return Ok(None);
        }
        if nodes.len() % 256 == 0 {
            checkpoint()?;
        }
        nodes.push(node);
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    if nodes.iter().any(|node| {
        node.kind() == "with_statement"
            || node.kind() == "call_expression"
                && node
                    .child_by_field_name("function")
                    .is_some_and(|function| text(function, bytes) == Some("eval"))
    }) {
        return Ok(Some(Surface::default()));
    }
    let mut occurrences: HashMap<&str, Vec<Node<'_>>> = HashMap::new();
    for node in &nodes {
        if matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) && let Some(name) = text(*node, bytes)
        {
            occurrences.entry(name).or_default().push(*node);
        }
    }
    let mut result = Surface::default();
    let mut modules: HashMap<&str, Arc<str>> = HashMap::new();
    for node in nodes {
        checkpoint()?;
        if let Some((binding, module)) = named_import(node, bytes)
            && let Some(name) = text(binding, bytes)
            && let Some(sites) = occurrences.get(name)
            && sites
                .iter()
                .all(|site| *site == binding || is_constructor(*site))
        {
            let import = Arc::new(NamedImport {
                name: name.to_string(),
                module: Arc::clone(modules.entry(module).or_insert_with(|| Arc::from(module))),
            });
            for site in sites.iter().filter(|site| is_constructor(**site)) {
                let (row, col) = codesage_parser::position::node_start_utf8(site, bytes);
                result
                    .constructions
                    .insert((row + 1, col), Arc::clone(&import));
            }
        }
        if let Some((class, name)) = exported_class(node, bytes)
            && occurrences
                .get(name)
                .is_some_and(|sites| sites.iter().all(|site| !is_written(*site)))
        {
            result.exports.push(ExportedClass {
                name: name.to_string(),
                line_start: (class.start_position().row + 1) as u32,
                line_end: (class.end_position().row + 1) as u32,
            });
        }
    }
    Ok(Some(result))
}

fn named_import<'tree, 'source>(
    node: Node<'tree>,
    bytes: &'source [u8],
) -> Option<(Node<'tree>, &'source str)> {
    if node.kind() != "import_specifier"
        || node.child_by_field_name("alias").is_some()
        || has_token(node, "type")
    {
        return None;
    }
    let statement = node.parent()?.parent()?.parent()?;
    if statement.kind() != "import_statement"
        || statement.parent()?.kind() != "program"
        || has_token(statement, "type")
    {
        return None;
    }
    let module = statement.child_by_field_name("source")?;
    if module.kind() != "string" {
        return None;
    }
    let literal = text(module, bytes)?;
    let module = literal.get(1..literal.len().checked_sub(1)?)?;
    if module.contains(['\\', '\n', '\r'])
        || !module.starts_with("./") && !module.starts_with("../")
    {
        return None;
    }
    Some((node.child_by_field_name("name")?, module))
}

fn is_constructor(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "new_expression" && parent.child_by_field_name("constructor") == Some(node)
    })
}

fn exported_class<'tree, 'source>(
    node: Node<'tree>,
    bytes: &'source [u8],
) -> Option<(Node<'tree>, &'source str)> {
    if node.kind() != "export_statement"
        || node.parent()?.kind() != "program"
        || has_token(node, "default")
    {
        return None;
    }
    let class = node.child_by_field_name("declaration")?;
    if class.kind() != "class_declaration" {
        return None;
    }
    Some((class, text(class.child_by_field_name("name")?, bytes)?))
}

fn is_written(mut node: Node<'_>) -> bool {
    for _ in 0..128 {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "parenthesized_expression"
            | "array_pattern"
            | "object_pattern"
            | "pair_pattern"
            | "assignment_pattern"
            | "rest_pattern" => node = parent,
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                return parent.child_by_field_name("left") == Some(node);
            }
            "variable_declarator" => return parent.child_by_field_name("name") == Some(node),
            "update_expression" => return true,
            _ => return false,
        }
    }
    true
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

fn text<'source>(node: Node<'_>, bytes: &'source [u8]) -> Option<&'source str> {
    node.utf8_text(bytes).ok()
}
