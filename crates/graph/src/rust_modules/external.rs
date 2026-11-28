use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_parser::{
    parse::parse_file_tolerant,
    references::{
        RustStdMacroShadows, is_rust_std_expression_macro, rust_macro_arguments_can_bind,
    },
    source::{IndexableSource, read_indexable_file},
};
use codesage_protocol::{Language, Reference, Symbol, SymbolKind, Visibility};
use codesage_storage::Database;
use tree_sitter::Node;

use super::RustImportEvidence;

const RESOLUTION_BUDGET: Duration = Duration::from_secs(2);
const RESOLUTION_READS: usize = 64;
const RESOLUTION_BYTES: usize = 8 * 1024 * 1024;

struct TypeSource {
    tree: tree_sitter::Tree,
    bytes: Vec<u8>,
    line_offsets: Vec<usize>,
    macro_shadows: OnceLock<RustStdMacroShadows>,
}

#[derive(Clone)]
enum TypeManifest {
    Parsed(Arc<toml::Value>),
    Missing,
    Unavailable,
}

enum TypeBinding<'tree> {
    Missing,
    Unknown,
    Nominal,
    Local,
    Extern(String),
    Import {
        path: String,
        declaration: Node<'tree>,
        glob: GlobCompetition<'tree>,
    },
}

enum GlobCompetition<'tree> {
    None,
    Unknown,
    Import {
        path: String,
        declaration: Node<'tree>,
    },
}

struct TypeCall<'tree> {
    site: Node<'tree>,
    macro_root: Option<String>,
}

struct TypeRoots<'a> {
    caller: &'a str,
    is_test: bool,
    sources: &'a [(&'a str, Arc<TypeSource>)],
}

struct ExternalTypePath {
    crate_name: String,
    tail: String,
    absolute: bool,
    explicit: bool,
}

impl ExternalTypePath {
    fn same_item_path(&self, other: &Self) -> bool {
        self.crate_name == other.crate_name && self.tail == other.tail
    }
}

#[derive(Default)]
struct Surface {
    locals: HashSet<String>,
    public_locals: HashSet<String>,
    definitions: HashMap<String, Vec<Symbol>>,
    modules: HashMap<String, bool>,
    uses: Vec<Use>,
    blocked: HashSet<String>,
    shadowed: HashSet<String>,
    uncertain_glob: bool,
    /// A top-level glob whose names are unknown; an explicit `use` of a name
    /// still overrides it.
    uncertain_module_glob: bool,
    /// Names a top-level glob brings in; an explicit `use` overrides them.
    glob_names: HashSet<String>,
    /// Source spans whose names may be rewritten, and whether the span is a
    /// macro invocation's scope (`true`) rather than an item under a
    /// rewriting attribute (`false`).
    uncertain_ranges: Vec<(tree_sitter::Point, tree_sitter::Point, bool)>,
    uncertain_names: HashSet<String>,
    proven_import_sites: HashSet<(u32, u32)>,
    import_bindings: HashMap<(u32, u32), String>,
    module_items: HashSet<(u32, u32)>,
    /// Top-level names declared only by `macro_rules!`: a macro occupies its
    /// own namespace, so it leaves a same-named `use` binding's function or
    /// type reachable.
    macro_only: HashSet<String>,
    /// Top-level `macro_rules!` marked `#[macro_export]`, which Rust places
    /// at the crate root whatever module defines them.
    exported_macros: HashSet<String>,
}

/// Visibility a path walk requires of what it reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Reach {
    /// From outside a dependency: modules, bindings, and definitions must be `pub`.
    Outside,
    /// Inside a dependency after a `use` redirect: definitions must be `pub`.
    Redirected,
    /// Inside the caller's own crate, where every item is reachable.
    Internal,
}

#[derive(Clone)]
struct Use {
    name: String,
    path: String,
    public: bool,
}

pub(super) struct ExternalCrates {
    root: Option<PathBuf>,
    contexts: HashMap<String, Vec<super::Module>>,
    library_callers: HashSet<String>,
    caller_roots: HashMap<String, Vec<String>>,
    manifests: HashMap<String, Option<Arc<toml::Value>>>,
    missing: HashSet<String>,
    sources: HashMap<(String, bool), Option<Arc<Surface>>>,
    std_macro_definitions: Option<Arc<Vec<(String, String)>>>,
    caller_std_macros: HashMap<String, Arc<HashSet<String>>>,
    spelling_masks: HashMap<String, Arc<HashMap<String, u16>>>,
    resolved: HashMap<(String, String), Option<Vec<Symbol>>>,
    type_sources: HashMap<String, Option<Arc<TypeSource>>>,
    type_manifests: HashMap<PathBuf, TypeManifest>,
    type_reads: usize,
    type_bytes: usize,
    type_elapsed: Duration,
    type_deadline: Option<Instant>,
    type_capped: bool,
    indexed_thread_local: Option<bool>,
    reads: usize,
    bytes: usize,
    elapsed: Duration,
    deadline: Option<Instant>,
    pub(super) capped: bool,
}

impl ExternalCrates {
    pub(super) fn import_evidence(
        &mut self,
        db: &Database,
        caller: &str,
    ) -> Result<RustImportEvidence> {
        if self.capped {
            return Ok(RustImportEvidence::default());
        }
        Ok(self
            .source(db, caller, true)?
            .map(|source| RustImportEvidence {
                bindings: source.import_bindings.clone(),
                module_items: Some(source.module_items.clone()),
            })
            .unwrap_or_default())
    }

    pub(super) fn package_import(
        &mut self,
        db: &Database,
        caller: &str,
        spelling: &str,
        is_test: bool,
        sites: &[&Reference],
    ) -> Result<Option<(String, bool)>> {
        if self.capped {
            return Ok(None);
        }
        let name = spelling.split("::").next().unwrap_or_default();
        if matches!(name, "crate" | "self" | "super") {
            return Ok(None);
        }
        let Some(source) = self.source(db, caller, true)? else {
            return Ok(None);
        };
        if !sites
            .iter()
            .any(|row| source.proven_import_sites.contains(&(row.line, row.col)))
        {
            return Ok(None);
        }
        let partial = sites
            .iter()
            .any(|row| !source.proven_import_sites.contains(&(row.line, row.col)));
        if matches!(name, "std" | "core" | "alloc") {
            return Ok(Some((name.to_string(), partial)));
        }
        let Some((package, manifest)) = self.package(caller) else {
            return Ok(None);
        };
        if !self.supported_caller(caller, &package, &manifest)
            || !self.modern_edition(&package, &manifest)
        {
            return Ok(None);
        }
        Ok(self
            .registry_dependency(&package, &manifest, name, is_test)
            .map(|package| (package, partial)))
    }

    fn registry_dependency(
        &mut self,
        package: &str,
        manifest: &toml::Value,
        name: &str,
        is_test: bool,
    ) -> Option<String> {
        let mut ancestor = Path::new(package);
        loop {
            for suffix in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
                let path = ancestor.join(suffix);
                let path = path.to_str()?;
                let config = self.manifest(path);
                if config.as_ref().is_some_and(|config| {
                    config.get("patch").is_some()
                        || config.get("replace").is_some()
                        || config.get("paths").is_some()
                }) || config.is_none() && !self.missing.contains(path)
                {
                    return None;
                }
            }
            let Some(parent) = ancestor.parent() else {
                break;
            };
            ancestor = parent;
        }
        let mut matches = Vec::new();
        for table in ["dependencies", "dev-dependencies"] {
            if table == "dev-dependencies" && !is_test {
                continue;
            }
            for (key, value) in manifest
                .get(table)
                .and_then(toml::Value::as_table)
                .into_iter()
                .flatten()
            {
                if key.replace('-', "_") != name {
                    continue;
                }
                let mut dependency = value.clone();
                if dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    let mut ancestor = Path::new(package);
                    loop {
                        let path = ancestor.join("Cargo.toml");
                        let path = path.to_str()?;
                        if let Some(manifest) = self.manifest(path)
                            && let Some(workspace) = manifest.get("workspace")
                        {
                            let inherited = workspace.get("dependencies")?.get(key)?;
                            dependency = inherited.clone();
                            break;
                        }
                        if self.manifests.get(path).is_some_and(Option::is_none)
                            && !self.missing.contains(path)
                        {
                            return None;
                        }
                        ancestor = ancestor.parent()?;
                    }
                }
                if dependency.get("path").is_some()
                    || dependency.get("optional").and_then(toml::Value::as_bool) == Some(true)
                    || value.get("optional").and_then(toml::Value::as_bool) == Some(true)
                    || !(dependency.is_str()
                        || dependency.get("version").is_some()
                        || dependency.get("git").is_some())
                {
                    return None;
                }
                matches.push(
                    dependency
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key)
                        .to_string(),
                );
            }
        }
        matches.sort();
        matches.dedup();
        match matches.as_slice() {
            [package] => Some(package.clone()),
            _ => None,
        }
    }

    pub(super) fn import_file(&mut self, db: &Database, row: &Reference) -> Result<Option<String>> {
        if self.capped {
            return Ok(None);
        }
        let (name, tail) = row.to_name.split_once("::").unwrap_or((&row.to_name, ""));
        if matches!(name, "crate" | "self" | "super") {
            return Ok(None);
        }
        let Some(source) = self.source(db, &row.from_file, true)? else {
            return Ok(None);
        };
        if !source.proven_import_sites.contains(&(row.line, row.col)) {
            return Ok(None);
        }
        let Some((package, manifest)) = self.package(&row.from_file) else {
            return Ok(None);
        };
        if !self.supported_caller(&row.from_file, &package, &manifest)
            || !self.modern_edition(&package, &manifest)
        {
            return Ok(None);
        }
        let Some(mut file) = self.library(&row.from_file, &package, &manifest, name) else {
            return Ok(None);
        };
        if db.file_id_for_path(&file)?.is_none() {
            return Ok(None);
        }
        // A dependency on a crate's export surface exists even when attributes
        // prevent resolving the imported item to a particular definition.
        for segment in tail.split("::") {
            let Some(surface) = self.source(db, &file, false)? else {
                return Ok(None);
            };
            if surface.modules.get(segment) != Some(&true) || surface.blocked.contains(segment) {
                return Ok((segment.is_empty()
                    || surface.public_locals.contains(segment)
                    || surface
                        .uses
                        .iter()
                        .any(|binding| binding.public && binding.name == segment))
                .then_some(file));
            }
            let directory = module_directory(&file);
            let mut children = Vec::new();
            for candidate in [
                format!("{directory}{segment}.rs"),
                format!("{directory}{segment}/mod.rs"),
            ] {
                if db.file_id_for_path(&candidate)?.is_some() {
                    children.push(candidate);
                }
            }
            let [child] = children.as_slice() else {
                return Ok(None);
            };
            file = child.clone();
        }
        Ok(Some(file))
    }

    pub(super) fn new(db: &Database, modules: &HashMap<String, Vec<super::Module>>) -> Self {
        let root = project_root(db);
        let library_callers = modules
            .iter()
            .filter(|(_, contexts)| {
                !contexts.is_empty()
                    && contexts
                        .iter()
                        .all(|context| !context.root_file.ends_with("/lib.rs"))
            })
            .map(|(file, _)| file.clone())
            .collect();
        let caller_roots = modules
            .iter()
            .filter(|(_, contexts)| !contexts.is_empty())
            .map(|(file, contexts)| {
                (
                    file.clone(),
                    contexts
                        .iter()
                        .map(|context| context.root_file.clone())
                        .collect(),
                )
            })
            .collect();
        let contexts = modules
            .iter()
            .filter(|(_, contexts)| !contexts.is_empty())
            .map(|(file, contexts)| (file.clone(), contexts.clone()))
            .collect();
        Self {
            root,
            contexts,
            library_callers,
            caller_roots,
            manifests: HashMap::new(),
            missing: HashSet::new(),
            sources: HashMap::new(),
            std_macro_definitions: None,
            caller_std_macros: HashMap::new(),
            spelling_masks: HashMap::new(),
            resolved: HashMap::new(),
            type_sources: HashMap::new(),
            type_manifests: HashMap::new(),
            type_reads: 0,
            type_bytes: 0,
            type_elapsed: Duration::ZERO,
            type_deadline: None,
            type_capped: false,
            indexed_thread_local: None,
            reads: 0,
            bytes: 0,
            elapsed: Duration::ZERO,
            deadline: None,
            capped: false,
        }
    }

    pub(super) fn capped(&self) -> bool {
        self.capped || self.type_capped
    }

    pub(super) fn unrelated_type_method(
        &mut self,
        db: &Database,
        row: &Reference,
        receiver: &str,
        recorded_receiver: bool,
        target: &Symbol,
    ) -> Result<bool> {
        // Import rows retain the original path. A same-file method already
        // needs this source, and a cached surface can prove other aliases.
        if !recorded_receiver
            && row.from_file != target.file_path
            && !self
                .sources
                .get(&(row.from_file.clone(), true))
                .and_then(Option::as_ref)
                .is_some_and(|surface| {
                    surface.uses.iter().any(|binding| binding.name == receiver)
                        || surface.shadowed.contains(receiver)
                })
        {
            return Ok(false);
        }
        let Some(roots) = self.caller_roots.get(&row.from_file).cloned() else {
            return Ok(false);
        };
        let start = Instant::now();
        self.type_deadline = Some(start + RESOLUTION_BUDGET.saturating_sub(self.type_elapsed));
        let result = self.unrelated_type_method_inner(db, row, receiver, target, &roots);
        self.type_elapsed += start.elapsed();
        self.type_deadline = None;
        self.type_capped |= self.type_elapsed > RESOLUTION_BUDGET;
        result
    }

    fn unrelated_type_method_inner(
        &mut self,
        db: &Database,
        row: &Reference,
        receiver: &str,
        target: &Symbol,
        roots: &[String],
    ) -> Result<bool> {
        if self.type_elapsed >= RESOLUTION_BUDGET {
            self.type_capped = true;
            return Ok(false);
        }
        let Some(caller) = self.type_source(db, &row.from_file)? else {
            return Ok(false);
        };
        let Some(offset) = source_offset(&caller, row.line, row.col) else {
            return Ok(false);
        };
        let Some(call) = caller
            .tree
            .root_node()
            .descendant_for_byte_range(offset, offset)
        else {
            return Ok(false);
        };
        if ancestors(call).any(|item| has_rewriting_attrs(item, &caller.bytes)) {
            return Ok(false);
        }
        let mut root_sources = Vec::new();
        for root in roots {
            let Some(source) = self.type_source(db, root)? else {
                return Ok(false);
            };
            root_sources.push((root.as_str(), source));
        }
        let roots = TypeRoots {
            caller: &row.from_file,
            is_test: row.is_test,
            sources: &root_sources,
        };
        let project_macros = self.caller_std_macros(db, &row.from_file)?;
        let root_macro_use = root_sources.iter().any(|(_, root)| {
            let tree = root.tree.root_node();
            tree.named_children(&mut tree.walk())
                .any(|item| attribute_has_name(item, "macro_use", &root.bytes))
        });
        let thread_local_shadowed = self.indexed_thread_local(db)? || root_macro_use;
        let mut scope = TypeScope {
            source: &caller,
            project_macros: &project_macros,
            thread_local_shadowed,
            qualified_macro: None,
        };
        let Some(call) = scope.call_site(call, offset, &row.to_name, root_macro_use) else {
            return Ok(false);
        };
        if let Some(macro_root) = call.macro_root {
            // A macro's path resolves before anything it expands.
            scope.qualified_macro = Some(call.site.id());
            if self
                .external_type_path(db, &scope, &roots, call.site, macro_root, true)?
                .is_none()
            {
                return Ok(false);
            }
        }
        let TypeBinding::Import {
            path,
            declaration,
            glob,
        } = scope.binding(call.site, receiver, 4)?
        else {
            return Ok(false);
        };
        // A use alias may only bind a value or macro. A block import in
        // either namespace can coexist with the outer type of the same name.
        let leaf = last_segment(&path);
        if leaf != receiver
            && !leaf
                .trim_start_matches("r#")
                .starts_with(char::is_uppercase)
            || !path.trim_start_matches("::").contains("::")
        {
            return Ok(false);
        }
        let Some(imported) =
            self.external_type_path(db, &scope, &roots, declaration, path, false)?
        else {
            return Ok(false);
        };
        let competing_glob = match glob {
            GlobCompetition::None => false,
            GlobCompetition::Unknown => true,
            GlobCompetition::Import { path, declaration } => self
                .external_type_path(db, &scope, &roots, declaration, path, false)?
                .is_none_or(|globbed| !imported.same_item_path(&globbed)),
        };
        let competing_block =
            if let Some(block) = declaration.parent().filter(|item| item.kind() == "block") {
                !matches!(scope.binding(block, receiver, 4)?, TypeBinding::Missing)
            } else {
                false
            };
        if competing_glob || competing_block {
            let mut witnessed = false;
            for annotation in qualified_type_annotations(call.site) {
                let path = rust_use_path(annotation, &caller.bytes);
                if self
                    .external_type_path(db, &scope, &roots, annotation, path.into_owned(), false)?
                    .is_some_and(|annotated| imported.same_item_path(&annotated))
                {
                    witnessed = true;
                    break;
                }
            }
            if !witnessed {
                return Ok(false);
            }
        }

        let Some(definition) = self.type_source(db, &target.file_path)? else {
            return Ok(false);
        };
        let Some(implementation) = source_at(&definition, target.line_start, target.col_start)
            .and_then(|item| ancestors(item).find(|item| item.kind() == "impl_item"))
        else {
            return Ok(false);
        };
        if ancestors(implementation).any(|item| has_rewriting_attrs(item, &definition.bytes)) {
            return Ok(false);
        }
        if implementation.child_by_field_name("trait").is_none() {
            return Ok(true);
        }
        if row.from_file != target.file_path
            && let Some(trait_name) =
                directly_declared_impl_trait(implementation, &definition.bytes)
            && scope.trait_is_out_of_scope(call.site, declaration, trait_name)?
            && !root_sources.iter().any(|(_, source)| {
                let tree = source.tree.root_node();
                tree.named_children(&mut tree.walk())
                    .any(|item| attribute_has_name(item, "prelude_import", &source.bytes))
            })
        {
            return Ok(true);
        }
        let project_macros = self.caller_std_macros(db, &target.file_path)?;
        let scope = TypeScope {
            source: &definition,
            project_macros: &project_macros,
            thread_local_shadowed: true,
            qualified_macro: None,
        };
        let Some(impl_type) = implementation.child_by_field_name("type") else {
            return Ok(false);
        };
        match scope.binding(impl_type, receiver, 4)? {
            TypeBinding::Nominal => Ok(true),
            TypeBinding::Import {
                path, declaration, ..
            } => self.local_crate_nominal(db, &target.file_path, &path, declaration),
            _ => Ok(false),
        }
    }

    fn external_type_path(
        &mut self,
        db: &Database,
        scope: &TypeScope<'_>,
        roots: &TypeRoots<'_>,
        site: Node<'_>,
        path: String,
        standard_only: bool,
    ) -> Result<Option<ExternalTypePath>> {
        let Some(path) = scope.import_root(site, path, roots, |name| {
            standard_crate(name)
                || self
                    .type_package(roots.caller)
                    .is_none_or(|(_, manifest)| package_names_extern_crate(&manifest, name))
        })?
        else {
            return Ok(None);
        };
        let name = path.crate_name.as_str();
        let builtin = standard_crate(name);
        if standard_only && !builtin {
            return Ok(None);
        }
        // Rust 2015 use paths start in the crate root instead of the extern prelude.
        if builtin && !self.type_modern_edition(roots.caller, name) {
            return Ok(None);
        }
        for (root, source) in roots.sources {
            let tree = source.tree.root_node();
            let project_macros = self.caller_std_macros(db, root)?;
            let root_scope = TypeScope {
                source,
                project_macros: &project_macros,
                thread_local_shadowed: scope.thread_local_shadowed,
                qualified_macro: scope.qualified_macro,
            };
            if builtin && name == "alloc" && !path.explicit {
                return Ok(None);
            }
            if tree.named_children(&mut tree.walk()).any(|item| {
                if root_scope.macro_may_bind(item) {
                    return true;
                }
                if path.explicit {
                    return false;
                }
                if (!path.absolute || item.kind() == "extern_crate_declaration")
                    && declared_type_binding(item)
                        .is_some_and(|alias| text(alias, &source.bytes) == name)
                {
                    return true;
                }
                if item.kind() == "inner_attribute_item" {
                    return name == "std" && attribute_has_name(item, "no_std", &source.bytes)
                        || attribute_has_name(item, "no_implicit_prelude", &source.bytes);
                }
                let Some(argument) = item
                    .child_by_field_name("argument")
                    .filter(|_| !path.absolute && item.kind() == "use_declaration")
                else {
                    return false;
                };
                let mut bindings = Vec::new();
                use_bindings(argument, &source.bytes, false, &mut bindings);
                bindings.iter().any(|binding| binding.name == name)
            }) {
                return Ok(None);
            }
        }
        if builtin {
            return Ok(Some(path));
        }
        let Some((package, manifest)) = self.package(roots.caller) else {
            return Ok(None);
        };
        let external = self.supported_caller(roots.caller, &package, &manifest)
            && self.modern_edition(&package, &manifest)
            && manifest
                .get("package")
                .and_then(|p| p.get("workspace"))
                .is_none()
            && self
                .registry_dependency(&package, &manifest, name, roots.is_test)
                .is_some();
        Ok(external.then_some(path))
    }

    fn local_crate_nominal(
        &mut self,
        db: &Database,
        file: &str,
        path: &str,
        declaration: Node<'_>,
    ) -> Result<bool> {
        let symbols = if let Some(path) = path.strip_prefix("crate::") {
            let Some(roots) = self.caller_roots.get(file).cloned() else {
                return Ok(false);
            };
            let [root] = roots.as_slice() else {
                return Ok(false);
            };
            self.export(db, root, root, path, Reach::Internal, &mut HashSet::new())?
        } else if (path.starts_with("self::") || path.starts_with("super::"))
            && matches!(self.contexts.get(file).map(Vec::as_slice), Some([_]))
            && module_scope(declaration).is_some_and(|scope| scope.kind() == "source_file")
        {
            self.crate_path(db, file, path, false)?.unwrap_or_default()
        } else {
            return Ok(false);
        };
        Ok(matches!(
            symbols.as_slice(),
            [nominal] if matches!(nominal.kind, SymbolKind::Struct | SymbolKind::Enum)
        ))
    }

    fn indexed_thread_local(&mut self, db: &Database) -> Result<bool> {
        if let Some(shadowed) = self.indexed_thread_local {
            return Ok(shadowed);
        }
        let shadowed = db
            .find_symbols("thread_local", Some(SymbolKind::Macro))?
            .iter()
            .any(|symbol| symbol.file_path.ends_with(".rs"));
        self.indexed_thread_local = Some(shadowed);
        Ok(shadowed)
    }

    fn type_source(&mut self, db: &Database, file: &str) -> Result<Option<Arc<TypeSource>>> {
        if let Some(source) = self.type_sources.get(file) {
            return Ok(source.clone());
        }
        let source = match db.overlay_source(file)? {
            Some(content) => content
                .filter(|content| self.reserve_type_read() && self.charge_type_bytes(content.len()))
                .map(overlay_source),
            None => self.read_type_evidence(Path::new(file)).ok().flatten(),
        };
        let source =
            match source {
                Some(source)
                    if db.get_file_hash(file)?.as_deref() == Some(source.content_hash.as_str()) =>
                {
                    let parsed = parse_file_tolerant(&source.bytes, Language::Rust)?;
                    (!parsed.degraded).then(|| {
                        let line_offsets =
                            std::iter::once(0)
                                .chain(source.bytes.iter().enumerate().filter_map(
                                    |(index, byte)| (*byte == b'\n').then_some(index + 1),
                                ))
                                .collect();
                        Arc::new(TypeSource {
                            tree: parsed.tree,
                            bytes: source.bytes,
                            line_offsets,
                            macro_shadows: OnceLock::new(),
                        })
                    })
                }
                _ => None,
            };
        self.type_sources.insert(file.to_string(), source.clone());
        Ok(source)
    }

    fn reserve_type_read(&mut self) -> bool {
        if self.type_reads >= RESOLUTION_READS
            || self.type_bytes >= RESOLUTION_BYTES
            || self.type_deadline.is_some_and(|end| Instant::now() >= end)
        {
            self.type_capped = true;
            return false;
        }
        self.type_reads += 1;
        true
    }

    fn charge_type_bytes(&mut self, bytes: usize) -> bool {
        self.type_bytes += bytes;
        if self.type_bytes > RESOLUTION_BYTES {
            self.type_capped = true;
            return false;
        }
        true
    }

    fn read_type_evidence(&mut self, path: &Path) -> Result<Option<IndexableSource>> {
        if !self.reserve_type_read() {
            return Ok(None);
        }
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let source = read_indexable_file(root, path)?;
        Ok(source.filter(|source| self.charge_type_bytes(source.bytes.len())))
    }

    fn type_manifest(&mut self, path: &Path) -> TypeManifest {
        if let Some(manifest) = self.type_manifests.get(path) {
            return manifest.clone();
        }
        // A failed read in the export walk may mean that walk ran out of
        // budget. Only reuse evidence known to be present or absent.
        let manifest = if path.to_str().is_some_and(|key| self.missing.contains(key)) {
            TypeManifest::Missing
        } else if let Some(Some(value)) = path.to_str().and_then(|key| self.manifests.get(key)) {
            TypeManifest::Parsed(value.clone())
        } else {
            match self.read_type_evidence(path) {
                Ok(Some(source)) => std::str::from_utf8(&source.bytes)
                    .ok()
                    .and_then(|text| toml::from_str(text).ok())
                    .map_or(TypeManifest::Unavailable, |value| {
                        TypeManifest::Parsed(Arc::new(value))
                    }),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    TypeManifest::Missing
                }
                _ => TypeManifest::Unavailable,
            }
        };
        self.type_manifests.insert(path.into(), manifest.clone());
        manifest
    }

    fn type_package<'a>(&mut self, caller: &'a str) -> Option<(&'a Path, Arc<toml::Value>)> {
        let mut package = Path::new(caller).parent()?;
        loop {
            match self.type_manifest(&package.join("Cargo.toml")) {
                TypeManifest::Parsed(manifest) => {
                    return manifest
                        .get("package")
                        .is_some()
                        .then_some((package, manifest));
                }
                TypeManifest::Unavailable => return None,
                TypeManifest::Missing => {}
            }
            package = package.parent()?;
        }
    }

    fn type_modern_edition(&mut self, caller: &str, builtin: &str) -> bool {
        let Some((package, manifest)) = self.type_package(caller) else {
            return false;
        };
        let Some(manifest_package) = manifest.get("package") else {
            return false;
        };
        if package_names_extern_crate(&manifest, builtin) {
            return false;
        }
        let edition = manifest_package.get("edition");
        if is_modern_edition(edition) {
            return true;
        }
        if edition
            .and_then(|edition| edition.get("workspace"))
            .and_then(toml::Value::as_bool)
            != Some(true)
            || manifest_package.get("workspace").is_some()
        {
            return false;
        }
        for ancestor in package.ancestors() {
            match self.type_manifest(&ancestor.join("Cargo.toml")) {
                TypeManifest::Parsed(manifest) => {
                    if let Some(workspace) = manifest.get("workspace") {
                        return is_modern_edition(
                            workspace
                                .get("package")
                                .and_then(|package| package.get("edition")),
                        );
                    }
                }
                TypeManifest::Unavailable => return false,
                TypeManifest::Missing => {}
            }
        }
        false
    }

    pub(super) fn resolve(
        &mut self,
        db: &Database,
        caller: &str,
        spelling: &str,
    ) -> Result<Option<Vec<Symbol>>> {
        if self.root.is_none() || !self.caller_roots.contains_key(caller) {
            return Ok(None);
        }
        let key = (caller.to_string(), spelling.to_string());
        if let Some(resolved) = self.resolved.get(&key) {
            return Ok(resolved.clone());
        }
        let start = Instant::now();
        self.deadline = Some(start + RESOLUTION_BUDGET.saturating_sub(self.elapsed));
        let result = self.resolve_inner(db, caller, spelling);
        self.elapsed += start.elapsed();
        if self.elapsed > RESOLUTION_BUDGET {
            self.capped = true;
        }
        let result = if self.capped && result.as_ref().is_ok_and(Option::is_some) {
            Ok(Some(Vec::new()))
        } else {
            result
        };
        if let Ok(resolved) = &result {
            self.resolved.insert(key, resolved.clone());
        }
        result
    }

    fn resolve_inner(
        &mut self,
        db: &Database,
        caller: &str,
        spelling: &str,
    ) -> Result<Option<Vec<Symbol>>> {
        let (head, tail) = spelling.split_once("::").unwrap_or((spelling, ""));
        let relative = matches!(head, "crate" | "self" | "super");
        // A spelled in-crate path already resolves through name lookup and
        // module qualification; only a name no definition in the caller's
        // crate carries (a re-export alias reached by path) needs the walk.
        if relative && !self.unnamed(db, caller, last_segment(spelling))? {
            return Ok(None);
        }
        let Some((package, manifest)) = self.package(caller) else {
            return Ok(None);
        };
        if !self.supported_caller(caller, &package, &manifest)
            || !self.modern_edition(&package, &manifest)
        {
            return Ok(None);
        }
        if relative {
            let Some(source) = self.source(db, caller, true)? else {
                return Ok(None);
            };
            if source.uncertain_names.contains(spelling) {
                return Ok(None);
            }
            return self.crate_path(db, caller, spelling, false);
        }
        let direct_library = if tail.is_empty() {
            None
        } else {
            self.library(caller, &package, &manifest, head)
        };
        let Some(source) = self.source(db, caller, true)? else {
            return Ok(None);
        };
        let macro_shadowed = source.macro_only.contains(head);
        let bindings: Vec<_> = source
            .uses
            .iter()
            .filter(|binding| binding.name == head)
            .collect();
        // An explicit `use` outranks what a module-level glob brings in.
        let globbed = bindings.is_empty()
            && (source.uncertain_module_glob || source.glob_names.contains(head));
        if source.locals.contains(head) && !macro_shadowed
            || source.uncertain_glob
            || globbed && source.uncertain_module_glob
        {
            return Ok(None);
        }
        let (library, path) = match bindings.as_slice() {
            [] => {
                let Some(library) = direct_library else {
                    return Ok(None);
                };
                (library, tail.to_string())
            }
            [binding] => {
                let mut path = binding.path.clone();
                if !tail.is_empty() {
                    path.push_str("::");
                    path.push_str(tail);
                }
                let Some((name, rest)) = path.split_once("::") else {
                    return Ok(None);
                };
                if matches!(name, "crate" | "self" | "super") {
                    let renamed = binding.name != last_segment(&binding.path);
                    if !renamed && !self.unnamed(db, caller, last_segment(&path))?
                        || source.shadowed.contains(head)
                        || source.blocked.contains(head)
                        || source.uncertain_names.contains(&binding.path)
                        || source.uncertain_names.contains(spelling)
                    {
                        return Ok(None);
                    }
                    return self.crate_path(db, caller, &path, macro_shadowed);
                }
                // The path's own head may come from a glob even when the
                // binding's name is explicit.
                if source.locals.contains(name)
                    || source.shadowed.contains(name)
                    || source.glob_names.contains(name)
                    || source.uncertain_module_glob
                    || source.blocked.contains(name)
                    || source.uncertain_names.contains(&binding.path)
                    || source
                        .uses
                        .iter()
                        .any(|binding| binding.name == name && binding.path != name)
                {
                    return Ok(None);
                }
                let Some(library) = self.library(caller, &package, &manifest, name) else {
                    return Ok(None);
                };
                (library, rest.to_string())
            }
            _ => return Ok(None),
        };
        if source.blocked.contains(head)
            || source.shadowed.contains(head)
            || globbed
            || source.uncertain_names.contains(spelling)
        {
            return Ok(Some(Vec::new()));
        }
        let mut resolved = self.export(
            db,
            &library,
            &library,
            &path,
            Reach::Outside,
            &mut HashSet::new(),
        )?;
        if macro_shadowed {
            // The same-file `macro_rules!` keeps the macro namespace; with no
            // other namespace's item imported, name resolution binds it.
            resolved.retain(|symbol| symbol.kind != SymbolKind::Macro);
            if resolved.is_empty() {
                return Ok(None);
            }
        }
        Ok(Some(unique(resolved)))
    }

    /// No indexed Rust definition named `name` belongs to a crate `caller`
    /// is part of. A definition whose crate is unknown counts as shared.
    fn unnamed(&self, db: &Database, caller: &str, name: &str) -> Result<bool> {
        let roots: Vec<&str> = self
            .contexts
            .get(caller)
            .into_iter()
            .flatten()
            .map(|context| context.root_file.as_str())
            .collect();
        Ok(!db.find_symbols(name, None)?.iter().any(|symbol| {
            symbol.file_path.ends_with(".rs")
                && self.contexts.get(&symbol.file_path).is_none_or(|contexts| {
                    roots.is_empty()
                        || contexts
                            .iter()
                            .any(|context| roots.contains(&context.root_file.as_str()))
                })
        }))
    }

    /// Directory holding the file modules `file` declares; a crate root's is
    /// the root's own directory (`src/` for `src/main.rs`, `src/bin/x/` for
    /// `src/bin/x/main.rs`).
    fn child_directory(&self, file: &str, root: &str) -> String {
        if file == root
            && let Some(context) = self
                .contexts
                .get(root)
                .and_then(|contexts| contexts.iter().find(|context| context.root_file == root))
        {
            return context.root_dir.clone();
        }
        module_directory(file)
    }

    /// Definitions an in-crate `crate::` / `self::` / `super::` path names
    /// from `caller`, walked through the caller's own crate the way a
    /// dependency's export surface is walked, without its `pub` requirement.
    /// An empty walk still answers: a definition outside the crate cannot be
    /// what the path names. `None` only when the caller's module context is
    /// unknown, or when a same-file `macro_rules!` keeps the spelling's macro
    /// namespace and the walk found nothing else.
    fn crate_path(
        &mut self,
        db: &Database,
        caller: &str,
        path: &str,
        macro_shadowed: bool,
    ) -> Result<Option<Vec<Symbol>>> {
        let Some(contexts) = self.contexts.get(caller).cloned() else {
            return Ok(None);
        };
        let segments: Vec<&str> = path.split("::").collect();
        let mut found: Vec<Symbol> = Vec::new();
        for context in contexts {
            let Some(absolute) = absolute_path(&context.path, &segments) else {
                return Ok(None);
            };
            let symbols = self.export(
                db,
                &context.root_file,
                &context.root_file,
                &absolute,
                Reach::Internal,
                &mut HashSet::new(),
            )?;
            extend_unique(&mut found, symbols);
        }
        if macro_shadowed {
            found.retain(|symbol| symbol.kind != SymbolKind::Macro);
            if found.is_empty() {
                return Ok(None);
            }
        }
        Ok(Some(unique(found)))
    }

    /// `#[macro_export]` macros named `name` defined anywhere in the crate
    /// rooted at `root`, which Rust places at that root.
    fn exported_macros(&mut self, db: &Database, root: &str, name: &str) -> Result<Vec<Symbol>> {
        let mut exported = Vec::new();
        for symbol in db.find_symbols(name, Some(SymbolKind::Macro))? {
            codesage_protocol::work::checkpoint()?;
            if !symbol.file_path.ends_with(".rs")
                || !self
                    .contexts
                    .get(&symbol.file_path)
                    .is_some_and(|contexts| {
                        contexts.iter().any(|context| context.root_file == root)
                    })
            {
                continue;
            }
            if self.exported_macro(db, &symbol)? == Some(true) {
                exported.push(symbol);
            }
        }
        Ok(exported)
    }

    /// Whether `symbol`, a macro, is `#[macro_export]`ed; `None` when its
    /// source cannot be read within the resolver's limits.
    pub(super) fn exported_macro(
        &mut self,
        db: &Database,
        symbol: &Symbol,
    ) -> Result<Option<bool>> {
        if symbol.kind != SymbolKind::Macro {
            return Ok(Some(false));
        }
        if self.capped {
            return Ok(None);
        }
        let Some(surface) = self.source(db, &symbol.file_path, false)? else {
            return Ok(None);
        };
        Ok(Some(
            surface.exported_macros.contains(&symbol.name)
                && surface
                    .definitions
                    .get(&symbol.name)
                    .is_some_and(|definitions| {
                        definitions
                            .iter()
                            .any(|definition| definition.handle() == symbol.handle())
                    }),
        ))
    }

    fn read(&mut self, path: &str) -> Option<codesage_parser::source::IndexableSource> {
        if self.capped
            || self.reads >= RESOLUTION_READS
            || self.bytes >= RESOLUTION_BYTES
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.capped = true;
            return None;
        }
        self.reads += 1;
        let source = match read_indexable_file(self.root.as_deref()?, Path::new(path)) {
            Ok(source) => source?,
            Err(error) => {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    self.missing.insert(path.to_string());
                }
                return None;
            }
        };
        self.bytes += source.bytes.len();
        if self.bytes > RESOLUTION_BYTES {
            self.capped = true;
            return None;
        }
        Some(source)
    }

    fn manifest(&mut self, path: &str) -> Option<Arc<toml::Value>> {
        if let Some(manifest) = self.manifests.get(path) {
            return manifest.clone();
        }
        let manifest = self
            .read(path)
            .and_then(|source| toml::from_str(std::str::from_utf8(&source.bytes).ok()?).ok())
            .map(Arc::new);
        self.manifests.insert(path.into(), manifest.clone());
        manifest
    }

    fn package(&mut self, file: &str) -> Option<(String, Arc<toml::Value>)> {
        let mut parent = Path::new(file).parent()?;
        loop {
            let path = parent.join("Cargo.toml").to_str()?.to_string();
            if let Some(manifest) = self.manifest(&path)
                && manifest.get("package").is_some()
            {
                return Some((parent.to_str()?.into(), manifest));
            }
            if !self.missing.contains(&path) {
                return None;
            }
            parent = parent.parent()?;
        }
    }

    fn library(
        &mut self,
        caller: &str,
        package: &str,
        manifest: &toml::Value,
        name: &str,
    ) -> Option<String> {
        let own_name = manifest
            .get("lib")
            .and_then(|lib| lib.get("name"))
            .and_then(toml::Value::as_str)
            .or_else(|| manifest.get("package")?.get("name")?.as_str())?
            .replace('-', "_");
        if name == own_name && self.library_callers.contains(caller) {
            return conventional_library(package, manifest);
        }
        let dependencies = manifest.get("dependencies")?.as_table()?;
        let mut matches = Vec::new();
        for (key, dependency) in dependencies {
            if let Some((extern_name, library)) = self.dependency_library(package, key, dependency)
                && extern_name == name
            {
                matches.push(library);
            }
        }
        match matches.as_slice() {
            [library] => Some(library.clone()),
            _ => None,
        }
    }

    fn supported_caller(&self, caller: &str, package: &str, manifest: &toml::Value) -> bool {
        let configured = manifest
            .get("package")
            .and_then(|package| package.get("build"));
        if configured.and_then(toml::Value::as_bool) == Some(false) {
            return true;
        }
        let path = configured
            .and_then(toml::Value::as_str)
            .unwrap_or("build.rs");
        let Some(build) = normalize(&Path::new(package).join(path)) else {
            return false;
        };
        caller != build
            && self
                .caller_roots
                .get(caller)
                .is_some_and(|roots| roots.iter().all(|root| root != &build))
    }

    fn modern_edition(&mut self, package: &str, manifest: &toml::Value) -> bool {
        let edition = manifest
            .get("package")
            .and_then(|package| package.get("edition"));
        if is_modern_edition(edition) {
            return true;
        }
        if edition
            .and_then(|edition| edition.get("workspace"))
            .and_then(toml::Value::as_bool)
            != Some(true)
        {
            return false;
        }
        let mut ancestor = Path::new(package);
        loop {
            let path = ancestor.join("Cargo.toml");
            let Some(path) = path.to_str() else {
                return false;
            };
            if let Some(manifest) = self.manifest(path)
                && let Some(workspace) = manifest.get("workspace")
            {
                return workspace
                    .get("package")
                    .and_then(|package| package.get("edition"))
                    .and_then(toml::Value::as_str)
                    .is_some_and(|edition| matches!(edition, "2018" | "2021" | "2024"));
            }
            if self.manifests.get(path).is_some_and(Option::is_none) && !self.missing.contains(path)
            {
                return false;
            }
            let Some(parent) = ancestor.parent() else {
                return false;
            };
            ancestor = parent;
        }
    }

    fn dependency_library(
        &mut self,
        package: &str,
        key: &str,
        dependency: &toml::Value,
    ) -> Option<(String, String)> {
        if dependency.get("optional").and_then(toml::Value::as_bool) == Some(true) {
            return None;
        }
        let mut dependency = dependency.clone();
        let mut base = package.to_string();
        if dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
            let mut ancestor = Path::new(package);
            loop {
                let path = ancestor.join("Cargo.toml").to_str()?.to_string();
                if let Some(manifest) = self.manifest(&path)
                    && let Some(workspace) = manifest.get("workspace")
                {
                    dependency = workspace.get("dependencies")?.get(key)?.clone();
                    base = ancestor.to_str()?.to_string();
                    break;
                }
                if self.manifests.get(&path).is_some_and(Option::is_none)
                    && !self.missing.contains(&path)
                {
                    return None;
                }
                ancestor = ancestor.parent()?;
            }
        }
        if dependency.get("optional").and_then(toml::Value::as_bool) == Some(true) {
            return None;
        }
        let dependency_path = dependency.get("path")?.as_str()?;
        let directory = normalize(&Path::new(&base).join(dependency_path))?;
        let target = self.manifest(Path::new(&directory).join("Cargo.toml").to_str()?)?;
        if !self.modern_edition(&directory, &target) {
            return None;
        }
        let expected = dependency
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(key);
        if target.get("package")?.get("name")?.as_str()? != expected {
            return None;
        }
        let extern_name = if dependency.get("package").is_some() {
            key.to_string()
        } else {
            target
                .get("lib")
                .and_then(|lib| lib.get("name"))
                .and_then(toml::Value::as_str)
                .unwrap_or(key)
                .to_string()
        }
        .replace('-', "_");
        Some((extern_name, conventional_library(&directory, &target)?))
    }

    /// Allowlisted std macro names an indexed Rust `macro_rules!` carries,
    /// with each definition's file.
    fn std_macro_definitions(&mut self, db: &Database) -> Result<Arc<Vec<(String, String)>>> {
        if let Some(definitions) = &self.std_macro_definitions {
            return Ok(definitions.clone());
        }
        let mut definitions = Vec::new();
        for name in codesage_parser::references::rust_std_expression_macro_names() {
            for symbol in db.find_symbols(name, Some(SymbolKind::Macro))? {
                if symbol.file_path.ends_with(".rs") {
                    definitions.push((symbol.file_path, (*name).to_string()));
                }
            }
        }
        let definitions = Arc::new(definitions);
        self.std_macro_definitions = Some(definitions.clone());
        Ok(definitions)
    }

    /// Std expression macro names a `macro_rules!` in `caller`'s own crate can
    /// rebind there through `#[macro_use]` modules or textual scope, which
    /// the per-file scan cannot follow. A definition in a file CodeSage cannot
    /// place in a crate (a stray fixture, say) is ignored, so one such file
    /// cannot unbind rows across the project.
    pub(super) fn caller_std_macros(
        &mut self,
        db: &Database,
        caller: &str,
    ) -> Result<Arc<HashSet<String>>> {
        if let Some(names) = self.caller_std_macros.get(caller) {
            return Ok(names.clone());
        }
        let names: HashSet<String> = self
            .std_macro_definitions(db)?
            .iter()
            .filter(|(file, _)| super::same_crate_in(&self.contexts, file, caller) == Some(true))
            .map(|(_, name)| name.clone())
            .collect();
        let names = Arc::new(names);
        self.caller_std_macros
            .insert(caller.to_string(), names.clone());
        Ok(names)
    }

    /// The file as the index holds it: an overlay's content, or the working
    /// file when its hash still matches the indexed one.
    fn indexed_source(
        &mut self,
        db: &Database,
        file: &str,
    ) -> Result<Option<codesage_parser::source::IndexableSource>> {
        // An overlaid file reads as the overlay holds it, so a request-local
        // view of other content still matches its own indexed hash.
        let source = match db.overlay_source(file)? {
            Some(content) => content.map(overlay_source),
            None => self.read(file),
        };
        Ok(match source {
            Some(source)
                if db.get_file_hash(file)?.as_deref() == Some(source.content_hash.as_str()) =>
            {
                Some(source)
            }
            _ => None,
        })
    }

    pub(super) fn shadowed_reference_files(&mut self, db: &Database) -> Result<Vec<(String, u16)>> {
        let bit = codesage_parser::references::rust_std_macro_bit;
        let all = self
            .std_macro_definitions(db)?
            .iter()
            .fold(0, |mask, (_, name)| mask | bit(name));
        if all == 0 {
            return Ok(Vec::new());
        }
        let mut shadowed = Vec::new();
        for file in db.files_with_macro_mask(all)? {
            codesage_protocol::work::checkpoint()?;
            let mask = self
                .caller_std_macros(db, &file)?
                .iter()
                .fold(0, |mask, name| mask | bit(name));
            if mask != 0 {
                shadowed.push((file, mask));
            }
        }
        Ok(shadowed)
    }

    /// Whether `spelling` in `caller` must stay unbound: a row of that
    /// spelling was read from the arguments of a bare std macro name
    /// (`Reference::macro_mask`) that a `macro_rules!` in the caller's crate
    /// can rebind, so the indexed macro may never evaluate it.
    pub(super) fn macro_shadowed(
        &mut self,
        db: &Database,
        caller: &str,
        spelling: &str,
    ) -> Result<bool> {
        let names = self.caller_std_macros(db, caller)?;
        let mask = names.iter().fold(0, |mask, name| {
            mask | codesage_parser::references::rust_std_macro_bit(name)
        });
        if mask == 0 {
            return Ok(false);
        }
        let masks = match self.spelling_masks.get(caller) {
            Some(masks) => masks.clone(),
            None => {
                let mut masks: HashMap<String, u16> = HashMap::new();
                for row in db.references_in_file_range(caller, 1, u32::MAX)? {
                    if row.macro_mask != 0 {
                        *masks.entry(row.to_name).or_default() |= row.macro_mask;
                    }
                }
                let masks = Arc::new(masks);
                self.spelling_masks
                    .insert(caller.to_string(), masks.clone());
                masks
            }
        };
        Ok(masks.get(spelling).is_some_and(|row| row & mask != 0))
    }

    fn source(&mut self, db: &Database, file: &str, caller: bool) -> Result<Option<Arc<Surface>>> {
        let key = (file.to_string(), caller);
        if let Some(surface) = self.sources.get(&key) {
            return Ok(surface.clone());
        }
        let source = self.indexed_source(db, file)?;
        let project_macros = self.caller_std_macros(db, file)?;
        let surface = if let Some(source) = source {
            let parsed = parse_file_tolerant(&source.bytes, Language::Rust)?;
            (!parsed.degraded)
                .then(|| {
                    let mut result = surface(
                        parsed.tree.root_node(),
                        &source.bytes,
                        db.symbols_for_file(file),
                        caller,
                        &project_macros,
                    )?;
                    result.module_items = module_item_positions(parsed.tree.root_node());
                    if caller {
                        let rows = db.references_in_file_range(file, 1, u32::MAX)?;
                        for row in &rows {
                            codesage_protocol::work::checkpoint()?;
                            let position = tree_sitter::Point {
                                row: row.line.saturating_sub(1) as usize,
                                column: row.col as usize,
                            };
                            if row.kind == codesage_protocol::ReferenceKind::Import
                                && !result
                                    .uncertain_ranges
                                    .iter()
                                    .any(|(start, end, _)| *start <= position && position < *end)
                            {
                                if let Some(binding) = module_import_binding(
                                    parsed.tree.root_node(),
                                    &source.bytes,
                                    row,
                                    &result.module_items,
                                ) {
                                    result.import_bindings.insert((row.line, row.col), binding);
                                }
                                if import_namespace_proven(
                                    parsed.tree.root_node(),
                                    &source.bytes,
                                    row,
                                )? {
                                    result.proven_import_sites.insert((row.line, row.col));
                                }
                            }
                        }
                        // Only a caller walk records uncertain ranges; this pass
                        // reads the rows fetched above instead of querying again.
                        if !result.uncertain_ranges.is_empty() {
                            for reference in rows {
                                let position = tree_sitter::Point {
                                    row: reference.line.saturating_sub(1) as usize,
                                    column: reference.col as usize,
                                };
                                // An invocation's macro path resolves before its own expansion,
                                // but a rewriting attribute can still replace it.
                                let invoked =
                                    reference.kind == codesage_protocol::ReferenceKind::MacroCall;
                                if result.uncertain_ranges.iter().any(|(start, end, scope)| {
                                    *start <= position && position < *end && !(invoked && *scope)
                                }) {
                                    result.uncertain_names.insert(reference.to_name);
                                }
                            }
                            result.uncertain_ranges.clear();
                        }
                    }
                    Ok::<_, anyhow::Error>(result)
                })
                .transpose()?
        } else {
            None
        };
        let surface = surface.map(Arc::new);
        self.sources.insert(key, surface.clone());
        Ok(surface)
    }

    /// Definitions `path` names inside `file`, a module of the crate rooted
    /// at `root`, under the visibility `reach` requires.
    fn export(
        &mut self,
        db: &Database,
        root: &str,
        file: &str,
        path: &str,
        reach: Reach,
        visited: &mut HashSet<(String, String, Reach)>,
    ) -> Result<Vec<Symbol>> {
        codesage_protocol::work::checkpoint()?;
        if visited.len() >= 32
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.capped = true;
            return Ok(Vec::new());
        }
        if !visited.insert((file.into(), path.into(), reach)) {
            return Ok(Vec::new());
        }
        let Some(source) = self.source(db, file, false)? else {
            return Ok(Vec::new());
        };
        let (name, tail) = path.split_once("::").unwrap_or((path, ""));
        let mut found = if tail.is_empty() && file == root {
            self.exported_macros(db, root, name)?
        } else {
            Vec::new()
        };
        if source.blocked.contains(name) {
            return Ok(found);
        }
        let module = source.modules.get(name).copied();
        if let Some(is_public) = module
            && !tail.is_empty()
        {
            if reach == Reach::Outside && !is_public {
                return Ok(found);
            }
            let directory = self.child_directory(file, root);
            let children = [
                format!("{directory}{name}.rs"),
                format!("{directory}{name}/mod.rs"),
            ];
            let mut present = Vec::new();
            for child in children {
                if db.file_id_for_path(&child)?.is_some() {
                    present.push(child);
                }
            }
            if let [child] = present.as_slice() {
                let child = child.clone();
                let reach = match reach {
                    Reach::Internal => Reach::Internal,
                    _ => Reach::Outside,
                };
                let symbols = self.export(db, root, &child, tail, reach, visited)?;
                extend_unique(&mut found, symbols);
            }
            return Ok(found);
        }
        // A `macro_rules!` is addressable at its module's path only through a
        // `use` of itself there (`pub(crate) use dual;`); an exported one is
        // reached at the crate root above. A function or type beside a
        // same-named module (`pub mod dual; pub fn dual()`) is reachable.
        if tail.is_empty()
            && let Some(definitions) = source.definitions.get(name)
        {
            let reexported = self_reexport(&source, name, reach);
            let symbols = definitions
                .iter()
                .filter(|symbol| {
                    if symbol.kind == SymbolKind::Macro {
                        return reexported;
                    }
                    reach == Reach::Internal || symbol.visibility == Some(Visibility::Public)
                })
                .cloned()
                .collect();
            extend_unique(&mut found, symbols);
            return Ok(found);
        }
        if module.is_some() {
            return Ok(found);
        }
        let bindings: Vec<_> = source
            .uses
            .iter()
            .filter(|binding| binding.name == name && (reach != Reach::Outside || binding.public))
            .collect();
        if bindings.len() != 1 {
            return Ok(found);
        }
        let mut redirect = bindings[0].path.clone();
        if !tail.is_empty() {
            redirect.push_str("::");
            redirect.push_str(tail);
        }
        let (start_file, redirect) = if let Some(path) = redirect.strip_prefix("crate::") {
            (root.to_string(), path.to_string())
        } else if let Some(path) = redirect.strip_prefix("self::") {
            (file.to_string(), path.to_string())
        } else if reach == Reach::Internal && redirect.starts_with("super::") {
            let mut contexts = self
                .contexts
                .get(file)
                .into_iter()
                .flatten()
                .filter(|context| context.root_file == root);
            let (Some(context), None) = (contexts.next(), contexts.next()) else {
                return Ok(found);
            };
            let segments: Vec<&str> = redirect.split("::").collect();
            let Some(absolute) = absolute_path(&context.path, &segments) else {
                return Ok(found);
            };
            (root.to_string(), absolute)
        } else if redirect.starts_with("super::") || redirect.starts_with("::") {
            return Ok(found);
        } else {
            (file.to_string(), redirect)
        };
        let reach = match reach {
            Reach::Internal => Reach::Internal,
            _ => Reach::Redirected,
        };
        let symbols = self.export(db, root, &start_file, &redirect, reach, visited)?;
        extend_unique(&mut found, symbols);
        Ok(found)
    }
}

/// One definition per namespace: macros resolve apart from functions and
/// types, so a path naming both (`fn dual` beside an exported `dual!`) keeps
/// each, while two candidates in one namespace stay unresolved.
fn unique(symbols: Vec<Symbol>) -> Vec<Symbol> {
    let (macros, items): (Vec<_>, Vec<_>) = symbols
        .into_iter()
        .partition(|symbol| symbol.kind == SymbolKind::Macro);
    [items, macros]
        .into_iter()
        .filter(|group| group.len() == 1)
        .flatten()
        .collect()
}

/// `segments` (led by `crate`, `self`, or one or more `super`) as a path
/// from the crate root, for a module at `module` below that root.
fn absolute_path(module: &[String], segments: &[&str]) -> Option<String> {
    let mut parts: Vec<&str> = module.iter().map(String::as_str).collect();
    let mut rest = segments;
    match rest.first() {
        Some(&"crate") => {
            parts.clear();
            rest = &rest[1..];
        }
        Some(&"self") => rest = &rest[1..],
        Some(&"super") => {
            while rest.first() == Some(&"super") {
                parts.pop()?;
                rest = &rest[1..];
            }
        }
        _ => return None,
    }
    if rest.is_empty() {
        return None;
    }
    parts.extend(rest);
    Some(parts.join("::"))
}

/// `surface` re-exports its own `name` (`use name;`, `pub(crate) use
/// self::name;`) visibly enough for `reach`.
fn self_reexport(surface: &Surface, name: &str, reach: Reach) -> bool {
    surface.uses.iter().any(|binding| {
        binding.name == name
            && (binding.path == name || binding.path == format!("self::{name}"))
            && (reach != Reach::Outside || binding.public)
    })
}

fn extend_unique(found: &mut Vec<Symbol>, symbols: Vec<Symbol>) {
    for symbol in symbols {
        if !found.iter().any(|known| known.handle() == symbol.handle()) {
            found.push(symbol);
        }
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(Some(node), Node::parent)
}

fn source_at(source: &TypeSource, line: u32, column: u32) -> Option<Node<'_>> {
    let offset = source_offset(source, line, column)?;
    source
        .tree
        .root_node()
        .descendant_for_byte_range(offset, offset)
}

fn source_offset(source: &TypeSource, line: u32, column: u32) -> Option<usize> {
    let row = line.checked_sub(1)? as usize;
    let start = *source.line_offsets.get(row)?;
    let end = source
        .line_offsets
        .get(row + 1)
        .map_or(source.bytes.len(), |offset| offset - 1);
    let line = source.bytes.get(start..end)?;
    let column = if line.is_ascii() {
        column as usize
    } else {
        std::str::from_utf8(line)
            .ok()?
            .char_indices()
            .nth(column as usize)?
            .0
    };
    if column > line.len() {
        return None;
    }
    Some(start + column)
}

fn overlay_source(content: String) -> IndexableSource {
    IndexableSource {
        content_hash: codesage_parser::discover::content_hash(content.as_bytes()),
        bytes: content.into_bytes(),
    }
}

fn is_modern_edition(edition: Option<&toml::Value>) -> bool {
    edition
        .and_then(toml::Value::as_str)
        .is_some_and(|edition| matches!(edition, "2018" | "2021" | "2024"))
}

fn has_dependency_name(manifest: &toml::Value, name: &str) -> bool {
    ["dependencies", "dev-dependencies", "build-dependencies"]
        .iter()
        .filter_map(|key| manifest.get(key).and_then(toml::Value::as_table))
        .any(|dependencies| {
            dependencies
                .keys()
                .any(|key| key == name || key.contains('-') && key.replace('-', "_") == name)
        })
}

fn package_names_extern_crate(manifest: &toml::Value, name: &str) -> bool {
    let library_name = manifest
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .or_else(|| manifest.get("package")?.get("name"))
        .and_then(toml::Value::as_str);
    library_name.is_some_and(|library| library == name || library.replace('-', "_") == name)
        || has_dependency_name(manifest, name)
        || manifest
            .get("target")
            .and_then(toml::Value::as_table)
            .is_some_and(|targets| {
                targets
                    .values()
                    .any(|target| has_dependency_name(target, name))
            })
}

fn standard_crate(name: &str) -> bool {
    matches!(name, "std" | "core" | "alloc")
}

fn extern_name<'a>(item: Node<'_>, name: &str, bytes: &'a [u8]) -> Option<&'a str> {
    if item.kind() != "extern_crate_declaration"
        || !declared_binding(item).is_some_and(|visible| text(visible, bytes) == name)
        || item_attributes(item).any(|attr| conditional_or_rewriting(attr, bytes))
    {
        return None;
    }
    item.child_by_field_name("name")
        .map(|original| text(original, bytes))
}

fn qualified_type_annotations(site: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    ancestors(site)
        .flat_map(|node| {
            [
                node.child_by_field_name("type"),
                node.child_by_field_name("return_type"),
            ]
            .into_iter()
            .flatten()
        })
        .filter(|node| node.kind() == "scoped_type_identifier")
        .take(8)
}

fn directly_declared_impl_trait<'a>(implementation: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    let name = implementation
        .child_by_field_name("trait")
        .filter(|node| node.kind() == "type_identifier")
        .map(|node| text(node, bytes))?;
    let module = implementation
        .parent()
        .filter(|node| is_module_scope(*node))?;
    let mut cursor = module.walk();
    let mut found = module.named_children(&mut cursor).filter(|node| {
        node.kind() == "trait_item"
            && node
                .child_by_field_name("name")
                .is_some_and(|node| text(node, bytes) == name)
    });
    let declaration = found.next()?;
    (found.next().is_none()
        && !item_attributes(declaration).any(|attr| conditional_or_rewriting(attr, bytes)))
    .then_some(name)
}

struct TypeScope<'a> {
    source: &'a TypeSource,
    project_macros: &'a HashSet<String>,
    thread_local_shadowed: bool,
    qualified_macro: Option<usize>,
}

impl TypeScope<'_> {
    fn shadows(&self) -> &RustStdMacroShadows {
        self.source.macro_shadows.get_or_init(|| {
            let mut shadows =
                RustStdMacroShadows::scan(self.source.tree.root_node(), &self.source.bytes);
            shadows.extend(self.project_macros.iter().cloned());
            shadows
        })
    }

    fn call_site<'tree>(
        &self,
        node: Node<'tree>,
        offset: usize,
        spelling: &str,
        root_macro_use: bool,
    ) -> Option<TypeCall<'tree>> {
        let bytes = &self.source.bytes;
        for parent in ancestors(node) {
            if parent.kind() == "call_expression" {
                let function = parent.child_by_field_name("function")?;
                return (rust_use_path(function, bytes) == spelling).then_some(TypeCall {
                    site: function,
                    macro_root: None,
                });
            }
            if parent.kind() != "macro_invocation" {
                continue;
            }
            let name = parent.child_by_field_name("macro")?;
            let shadows = self.shadows();
            if !is_rust_std_expression_macro(parent, bytes, shadows)
                || rust_macro_arguments_can_bind(parent, bytes, shadows)
            {
                return None;
            }
            let macro_root = match name.kind() {
                "identifier" if !root_macro_use => None,
                "scoped_identifier" => {
                    let path = rust_use_path(name, bytes);
                    let (root, _) = path.rsplit_once("::")?;
                    Some(root.to_owned())
                }
                _ => return None,
            };
            // Extraction reparses transparent expressions inside token trees.
            // The source hash and the indexed byte position identify this call.
            let suffix = std::str::from_utf8(bytes.get(offset..parent.end_byte())?).ok()?;
            return suffix
                .strip_prefix(spelling)
                .is_some_and(|suffix| suffix.trim_start().starts_with('('))
                .then_some(TypeCall {
                    site: parent,
                    macro_root,
                });
        }
        None
    }

    fn import_root(
        &self,
        mut site: Node<'_>,
        mut path: String,
        roots: &TypeRoots<'_>,
        mut prelude_crate: impl FnMut(&str) -> bool,
    ) -> Result<Option<ExternalTypePath>> {
        let mut requires_crate_alias = false;
        for _ in 0..=4 {
            let absolute = path.starts_with("::");
            let unrooted = path.trim_start_matches("::");
            let (name, tail) = unrooted.split_once("::").unwrap_or((unrooted, ""));
            if matches!(name, "" | "self" | "super" | "crate") {
                return Ok(None);
            }
            if !absolute {
                match self.binding(site, name, 4)? {
                    TypeBinding::Import {
                        path: import,
                        declaration,
                        glob,
                    } => {
                        // A value-only alias can coexist with a module in
                        // an enclosing scope or a crate in the extern prelude.
                        if let Some(block) =
                            declaration.parent().filter(|item| item.kind() == "block")
                            && !matches!(self.binding(block, name, 4)?, TypeBinding::Missing)
                        {
                            requires_crate_alias = true;
                        }
                        requires_crate_alias |= !matches!(glob, GlobCompetition::None)
                            || prelude_crate(name)
                            || roots.sources.iter().any(|(_, source)| {
                                let tree = source.tree.root_node();
                                tree.named_children(&mut tree.walk()).any(|item| {
                                    item.kind() == "extern_crate_declaration"
                                        && declared_binding(item)
                                            .is_some_and(|ident| text(ident, &source.bytes) == name)
                                })
                            });
                        if requires_crate_alias && import.trim_start_matches("::").contains("::") {
                            return Ok(None);
                        }
                        path = if tail.is_empty() {
                            import
                        } else {
                            format!("{import}::{tail}")
                        };
                        site = declaration;
                        continue;
                    }
                    TypeBinding::Extern(original) => {
                        return Ok((original != "self").then(|| ExternalTypePath {
                            crate_name: original,
                            tail: tail.to_owned(),
                            absolute,
                            explicit: true,
                        }));
                    }
                    TypeBinding::Missing => {}
                    _ => return Ok(None),
                }
            }
            let mut explicit = roots.sources.iter().map(|(_, source)| {
                let tree = source.tree.root_node();
                tree.named_children(&mut tree.walk())
                    .find_map(|item| extern_name(item, name, &source.bytes))
            });
            if let Some(Some(original)) = explicit.next()
                && explicit.all(|other| other == Some(original))
            {
                return Ok((original != "self").then(|| ExternalTypePath {
                    crate_name: original.to_owned(),
                    tail: tail.to_owned(),
                    absolute,
                    explicit: true,
                }));
            }
            return Ok(Some(ExternalTypePath {
                crate_name: name.to_owned(),
                tail: tail.to_owned(),
                absolute,
                explicit: false,
            }));
        }
        Ok(None)
    }

    fn trait_is_out_of_scope(
        &self,
        site: Node<'_>,
        receiver_import: Node<'_>,
        trait_name: &str,
    ) -> Result<bool> {
        if !matches!(self.binding(site, trait_name, 4)?, TypeBinding::Missing) {
            return Ok(false);
        }
        let Some(argument) = receiver_import.child_by_field_name("argument") else {
            return Ok(false);
        };
        let mut bindings = Vec::new();
        use_bindings(argument, &self.source.bytes, false, &mut bindings);
        if bindings.len() != 1 || contains_use_wildcard(argument) {
            return Ok(false);
        }
        // A renamed, anonymous, or re-exported trait may be brought into
        // scope without a binding for the spelling used in its declaration.
        for scope in ancestors(site).skip(1) {
            let module = is_module_scope(scope);
            if (module || scope.kind() == "block")
                && scope.named_children(&mut scope.walk()).any(|item| {
                    item.kind() == "use_declaration" && item.id() != receiver_import.id()
                })
            {
                return Ok(false);
            }
            if module {
                break;
            }
        }
        Ok(true)
    }

    fn macro_may_bind(&self, item: Node<'_>) -> bool {
        let invocation = match item.kind() {
            "macro_invocation" => item,
            "expression_statement" => match item.named_child(0) {
                Some(expression) if expression.kind() == "macro_invocation" => expression,
                _ => return false,
            },
            _ => return false,
        };
        if self.qualified_macro == Some(invocation.id()) {
            return false;
        }
        let bytes = &self.source.bytes;
        let Some(path) = invocation.child_by_field_name("macro") else {
            return true;
        };
        if path.kind() != "identifier" {
            return true;
        }
        if text(path, bytes) == "thread_local" {
            // The prelude macro emits value-namespace statics. Attributes
            // forwarded to those statics or a shadowing macro can emit items.
            return self.thread_local_shadowed
                || thread_local_may_be_shadowed(invocation, bytes)
                || invocation
                    .named_children(&mut invocation.walk())
                    .any(|node| {
                        node.kind() == "token_tree"
                            && node
                                .children(&mut node.walk())
                                .any(|token| token.kind() == "#")
                    });
        }
        if !codesage_parser::references::rust_std_expression_macro_names()
            .contains(&text(path, bytes))
        {
            return true;
        }
        !is_rust_std_expression_macro(invocation, bytes, self.shadows())
    }

    fn binding<'tree>(
        &self,
        node: Node<'tree>,
        name: &str,
        remaining: usize,
    ) -> Result<TypeBinding<'tree>> {
        let bytes = &self.source.bytes;
        for scope in ancestors(node).skip(1) {
            codesage_protocol::work::checkpoint()?;
            if scope
                .child_by_field_name("type_parameters")
                .is_some_and(|params| {
                    params.named_children(&mut params.walk()).any(|param| {
                        declared_type_binding(param).is_some_and(|ident| text(ident, bytes) == name)
                    })
                })
            {
                return Ok(TypeBinding::Local);
            }
            let module = is_module_scope(scope);
            if !module && scope.kind() != "block" {
                continue;
            }
            let mut matching = None;
            let mut glob = false;
            let mut parent_glob = false;
            for item in scope.named_children(&mut scope.walk()) {
                if self.macro_may_bind(item)
                    || item.kind() == "attribute_item"
                        && attribute_can_rewrite(text(item, bytes))
                        && !is_lint_attribute(text(item, bytes))
                {
                    return Ok(TypeBinding::Unknown);
                }
                if item.kind() != "use_declaration" {
                    if declared_type_binding(item).is_some_and(|ident| text(ident, bytes) == name) {
                        return Ok(
                            if item_attributes(item)
                                .any(|attr| conditional_or_rewriting(attr, bytes))
                            {
                                TypeBinding::Unknown
                            } else if matches!(
                                item.kind(),
                                "struct_item" | "enum_item" | "union_item"
                            ) {
                                TypeBinding::Nominal
                            } else if let Some(original) = extern_name(item, name, bytes) {
                                TypeBinding::Extern(original.to_owned())
                            } else {
                                TypeBinding::Local
                            },
                        );
                    }
                    continue;
                }
                let Some(argument) = item.child_by_field_name("argument") else {
                    return Ok(TypeBinding::Unknown);
                };
                if argument.kind() == "use_wildcard"
                    && rust_use_path(argument, bytes) == "super::*"
                    && !item_attributes(item).any(|attr| conditional_or_rewriting(attr, bytes))
                {
                    parent_glob = true;
                } else {
                    glob |= contains_use_wildcard(argument);
                }
                let mut bindings = Vec::new();
                use_bindings(argument, bytes, false, &mut bindings);
                for binding in bindings.into_iter().filter(|binding| binding.name == name) {
                    if matching.is_some()
                        || item_attributes(item).any(|attr| conditional_or_rewriting(attr, bytes))
                    {
                        return Ok(TypeBinding::Unknown);
                    }
                    matching = Some((binding.path, item));
                }
            }
            let glob_binding = if glob {
                TypeBinding::Unknown
            } else if parent_glob {
                match module_scope(scope).and_then(|scope| scope.parent()) {
                    Some(enclosing) if remaining > 0 => {
                        self.binding(enclosing, name, remaining - 1)?
                    }
                    _ => TypeBinding::Unknown,
                }
            } else {
                TypeBinding::Missing
            };
            if let Some((path, declaration)) = matching {
                // An explicit use only overrides a glob in the namespaces
                // that the imported item actually binds.
                let glob = match glob_binding {
                    TypeBinding::Missing => GlobCompetition::None,
                    TypeBinding::Import {
                        path,
                        declaration,
                        glob: GlobCompetition::None,
                    } => GlobCompetition::Import { path, declaration },
                    _ => GlobCompetition::Unknown,
                };
                return Ok(TypeBinding::Import {
                    path,
                    declaration,
                    glob,
                });
            }
            if !matches!(glob_binding, TypeBinding::Missing) {
                return Ok(glob_binding);
            }
            if module {
                break;
            }
        }
        Ok(TypeBinding::Missing)
    }
}

fn thread_local_may_be_shadowed(invocation: Node<'_>, bytes: &[u8]) -> bool {
    ancestors(invocation)
        .filter(|scope| matches!(scope.kind(), "block" | "source_file" | "declaration_list"))
        .any(|scope| {
            scope.named_children(&mut scope.walk()).any(|item| {
                if attribute_has_name(item, "macro_use", bytes)
                    || item.kind() == "macro_definition"
                        && declared_binding(item)
                            .is_some_and(|name| text(name, bytes) == "thread_local")
                {
                    return true;
                }
                let Some(argument) = item
                    .child_by_field_name("argument")
                    .filter(|_| item.kind() == "use_declaration")
                else {
                    return false;
                };
                let path = text(argument, bytes);
                if path.contains('*') && path != "super::*" {
                    return true;
                }
                let mut bindings = Vec::new();
                use_bindings(argument, bytes, false, &mut bindings);
                bindings
                    .iter()
                    .any(|binding| binding.name == "thread_local")
            })
        })
}

fn item_attributes(item: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(item.prev_named_sibling(), Node::prev_named_sibling)
        .take_while(|item| item.kind() == "attribute_item" || item.kind().ends_with("comment"))
        .filter(|item| item.kind() == "attribute_item")
}

fn attribute_name<'a>(item: Node<'_>, bytes: &'a [u8]) -> &'a str {
    item.named_child(0)
        .filter(|attribute| attribute.kind() == "attribute")
        .and_then(|attribute| attribute.named_child(0))
        .map_or("", |name| text(name, bytes))
}

fn conditional_or_rewriting(attribute: Node<'_>, bytes: &[u8]) -> bool {
    matches!(attribute_name(attribute, bytes), "cfg" | "cfg_attr")
        || attribute_can_rewrite(text(attribute, bytes))
}

fn has_rewriting_attrs(item: Node<'_>, bytes: &[u8]) -> bool {
    item_attributes(item).any(|attr| {
        attribute_can_rewrite(text(attr, bytes)) && !is_lint_attribute(text(attr, bytes))
    })
}

fn attribute_has_name(item: Node<'_>, name: &str, bytes: &[u8]) -> bool {
    if !matches!(item.kind(), "attribute_item" | "inner_attribute_item") {
        return false;
    }
    if attribute_name(item, bytes) == name {
        return true;
    }
    if attribute_name(item, bytes) != "cfg_attr" {
        return false;
    }
    let Some(arguments) = item
        .named_child(0)
        .and_then(|attribute| attribute.child_by_field_name("arguments"))
    else {
        return false;
    };
    text(arguments, bytes)
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|token| token == name)
}

pub(super) fn project_root(db: &Database) -> Option<PathBuf> {
    crate::constructor_imports::project_root(db).map(Path::to_path_buf)
}

fn conventional_library(package: &str, manifest: &toml::Value) -> Option<String> {
    if manifest.get("lib").is_none()
        && manifest
            .get("package")
            .and_then(|package| package.get("autolib"))
            .and_then(toml::Value::as_bool)
            == Some(false)
    {
        return None;
    }
    if manifest
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(toml::Value::as_str)
        .is_some_and(|path| path != "src/lib.rs")
    {
        return None;
    }
    Some(Path::new(package).join("src/lib.rs").to_str()?.into())
}

fn normalize(path: &Path) -> Option<String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir if normalized.pop() => {}
            _ => return None,
        }
    }
    Some(normalized.to_str()?.into())
}

fn module_directory(file: &str) -> String {
    if file.ends_with("/lib.rs") || file.ends_with("/mod.rs") {
        file.rsplit_once('/')
            .map_or(String::new(), |(parent, _)| format!("{parent}/"))
    } else {
        format!("{}/", file.strip_suffix(".rs").unwrap_or(file))
    }
}

fn surface(
    node: Node<'_>,
    bytes: &[u8],
    symbols: Result<Vec<Symbol>>,
    caller: bool,
    project_macros: &HashSet<String>,
) -> Result<Surface> {
    let symbols = symbols?;
    let mut surface = Surface::default();
    let mut attributed = false;
    let mut exported = false;
    let mut linted = false;
    let mut macro_names = HashSet::new();
    let mut item_names = HashSet::new();
    for item in node.named_children(&mut node.walk()) {
        if item.kind() == "attribute_item" {
            let attribute = text(item, bytes);
            if is_macro_export(attribute) {
                exported = true;
            } else if is_lint_attribute(attribute) {
                linted = true;
            } else {
                attributed |= !matches!(attribute.strip_prefix("#[doc"), Some(tail) if tail.starts_with('(') || tail.starts_with(" ="));
            }
            continue;
        }
        if item.kind().ends_with("comment") {
            continue;
        }
        let public = item
            .named_children(&mut item.walk())
            .any(|child| child.kind() == "visibility_modifier" && text(child, bytes) == "pub");
        if item.kind() == "use_declaration" {
            if let Some(argument) = item.child_by_field_name("argument") {
                let mut uses = Vec::new();
                use_bindings(argument, bytes, public, &mut uses);
                if attributed || exported {
                    surface
                        .blocked
                        .extend(uses.into_iter().map(|binding| binding.name));
                } else {
                    surface.uses.extend(uses);
                }
            }
        } else if let Some(name) = declared_binding(item) {
            let name = text(name, bytes).to_string();
            surface.locals.insert(name.clone());
            if public {
                surface.public_locals.insert(name.clone());
            }
            let is_macro = item.kind() == "macro_definition";
            if is_macro {
                macro_names.insert(name.clone());
            } else {
                item_names.insert(name.clone());
            }
            // `#[macro_export]` relocates a macro and a lint level only
            // changes diagnostics; neither rewrites the macro.
            let attributed = attributed || (exported || linted) && !is_macro;
            if is_macro && exported && !attributed {
                surface.exported_macros.insert(name.clone());
            }
            if attributed || item.kind() == "mod_item" && item.child_by_field_name("body").is_some()
            {
                surface.blocked.insert(name);
            } else if item.kind() == "mod_item" {
                surface.modules.insert(name, public);
            } else {
                let definitions: Vec<_> = symbols
                    .iter()
                    .filter(|symbol| {
                        symbol.line_start == item.start_position().row as u32 + 1
                            && symbol.col_start == item.start_position().column as u32
                    })
                    .cloned()
                    .collect();
                surface
                    .definitions
                    .entry(name)
                    .or_default()
                    .extend(definitions);
            }
        }
        attributed = false;
        exported = false;
        linted = false;
    }
    surface.macro_only = macro_names
        .into_iter()
        .filter(|name| !item_names.contains(name))
        .collect();
    if !caller {
        return Ok(surface);
    }
    // Each entry carries whether its parent is a nested node rather than
    // `node`; `Node::parent` re-walks the tree from the root on every call.
    // `Node::kind` measures and validates the name on every call, and
    // `child_by_field_name` scans the grammar's field names, so the walk
    // reads each node's kind once and resolves the field id once.
    let pattern_field = node.language().field_id_for_name("pattern");
    let mut invocations = Vec::new();
    let mut pending: Vec<_> = node
        .named_children(&mut node.walk())
        .map(|item| (item, false))
        .collect();
    while let Some((item, nested)) = pending.pop() {
        let kind = item.kind();
        if nested
            && kind == "use_declaration"
            && let Some(argument) = item.child_by_field_name("argument")
        {
            let mut bindings = Vec::new();
            use_bindings(argument, bytes, false, &mut bindings);
            surface
                .shadowed
                .extend(bindings.into_iter().map(|binding| binding.name));
        }
        if nested && let Some(name) = declared_binding_of(item, kind) {
            surface.shadowed.insert(text(name, bytes).into());
        }
        if kind == "use_wildcard" {
            let mut declaration = item.parent();
            while declaration.is_some_and(|parent| parent.kind() != "use_declaration") {
                declaration = declaration.and_then(|parent| parent.parent());
            }
            // Only a glob among the file's own items yields to an explicit
            // `use`; one inside a function or block shadows it there.
            let top_level = declaration
                .and_then(|declaration| declaration.parent())
                .is_some_and(|parent| parent.id() == node.id());
            match (relative_glob_bindings(item, node, bytes), top_level) {
                (Some(bindings), true) => surface.glob_names.extend(bindings),
                (Some(bindings), false) => surface.shadowed.extend(bindings),
                (None, true) => surface.uncertain_module_glob = true,
                (None, false) => surface.uncertain_glob = true,
            }
        }
        if kind == "macro_invocation" {
            invocations.push(item);
        }
        if kind == "attribute_item" && attribute_can_rewrite(text(item, bytes)) {
            let mut next = item.next_named_sibling();
            while next.is_some_and(|node| {
                node.kind() == "attribute_item" || node.kind().ends_with("comment")
            }) {
                next = next.and_then(|node| node.next_named_sibling());
            }
            if let Some(next) = next {
                surface
                    .uncertain_ranges
                    .push((next.start_position(), next.end_position(), false));
            }
        }
        if let Some(pattern) = pattern_field.and_then(|field| item.child_by_field_id(field.get())) {
            pattern_bindings(pattern, bytes, &mut surface.shadowed);
        }
        if kind == "closure_parameters" {
            for pattern in item
                .named_children(&mut item.walk())
                .filter(|child| child.kind() != "parameter")
            {
                pattern_bindings(pattern, bytes, &mut surface.shadowed);
            }
        }
        pending.extend(
            item.named_children(&mut item.walk())
                .map(|child| (child, true)),
        );
    }
    // A std expression macro expands to an expression and binds nothing; the
    // calls in its arguments are extracted as ordinary rows, unless the
    // arguments themselves can bind a name the walk cannot see.
    let mut shadows = codesage_parser::references::RustStdMacroShadows::scan(node, bytes);
    shadows.extend(project_macros.iter().cloned());
    for invocation in invocations {
        let transparent =
            codesage_parser::references::is_rust_std_expression_macro(invocation, bytes, &shadows)
                && !codesage_parser::references::rust_macro_arguments_can_bind(
                    invocation, bytes, &shadows,
                );
        if !transparent && let Some(scope) = lexical_scope(invocation) {
            surface
                .uncertain_ranges
                .push((scope.start_position(), scope.end_position(), true));
        }
    }
    Ok(surface)
}

fn is_macro_export(attribute: &str) -> bool {
    let inner = attribute
        .strip_prefix("#[")
        .and_then(|rest| rest.strip_suffix(']'))
        .map(str::trim)
        .unwrap_or_default();
    inner == "macro_export"
        || inner
            .strip_prefix("macro_export")
            .is_some_and(|rest| rest.trim_start().starts_with('('))
}

/// `#[allow(..)]`, `#[expect(..)]`, `#[warn(..)]`, `#[deny(..)]`,
/// `#[forbid(..)]`, or a `#[cfg_attr(predicate, ..)]` whose attributes are
/// all of those.
fn is_lint_attribute(attribute: &str) -> bool {
    let Some(inner) = attribute
        .strip_prefix("#[")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let lint = |meta: &str| {
        let meta = meta.trim();
        ["allow", "expect", "warn", "deny", "forbid"]
            .iter()
            .any(|level| {
                meta.strip_prefix(level)
                    .is_some_and(|rest| rest.trim_start().starts_with('('))
            })
    };
    let inner = inner.trim();
    if let Some(arguments) = inner
        .strip_prefix("cfg_attr")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts = top_level_arguments(arguments);
        return parts.len() >= 2 && parts[1..].iter().all(|part| lint(part));
    }
    lint(inner)
}

/// `arguments` split at commas outside parentheses and brackets.
fn top_level_arguments(arguments: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, c) in arguments.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&arguments[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    let last = &arguments[start..];
    if !last.trim().is_empty() {
        parts.push(last);
    }
    parts
}

fn text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or_default()
}

fn declared_binding(item: Node<'_>) -> Option<Node<'_>> {
    declared_binding_of(item, item.kind())
}

fn declared_type_binding(item: Node<'_>) -> Option<Node<'_>> {
    let kind = item.kind();
    matches!(
        kind,
        "associated_type"
            | "enum_item"
            | "extern_crate_declaration"
            | "mod_item"
            | "struct_item"
            | "trait_item"
            | "type_item"
            | "type_parameter"
            | "union_item"
    )
    .then(|| declared_binding_of(item, kind))
    .flatten()
}

/// [`declared_binding`] for a caller that already holds `item.kind()`.
fn declared_binding_of<'tree>(item: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    if kind == "extern_crate_declaration" {
        return item
            .child_by_field_name("alias")
            .or_else(|| item.child_by_field_name("name"));
    }
    matches!(
        kind,
        "associated_type"
            | "const_item"
            | "const_parameter"
            | "enum_item"
            | "function_item"
            | "function_signature_item"
            | "macro_definition"
            | "mod_item"
            | "static_item"
            | "struct_item"
            | "trait_item"
            | "type_item"
            | "type_parameter"
            | "union_item"
    )
    .then(|| item.child_by_field_name("name"))
    .flatten()
}

fn lexical_scope(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if matches!(node.kind(), "block" | "source_file" | "declaration_list") {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn attribute_can_rewrite(attribute: &str) -> bool {
    let name = attribute
        .trim_start_matches("#[")
        .trim()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
        .next()
        .unwrap_or_default();
    // Derive-generated sibling items remain outside the indexed source model.
    !matches!(
        name,
        "derive"
            | "test"
            | "cfg"
            | "doc"
            | "allow"
            | "deny"
            | "warn"
            | "forbid"
            | "expect"
            | "inline"
            | "cold"
            | "track_caller"
            | "must_use"
            | "deprecated"
            | "no_mangle"
            | "export_name"
            | "link"
            | "link_name"
            | "link_section"
            | "repr"
            | "non_exhaustive"
    )
}

fn relative_glob_bindings(glob: Node<'_>, root: Node<'_>, bytes: &[u8]) -> Option<HashSet<String>> {
    let path = text(glob, bytes).strip_suffix("::*")?;
    let mut scope = module_scope(glob)?;
    for (index, segment) in path.split("::").enumerate() {
        scope = match segment {
            "crate" if index == 0 => root,
            "self" if index == 0 => scope,
            "super" => module_scope(scope.parent()?)?,
            _ => return None,
        };
    }
    let mut bindings = HashSet::new();
    for item in scope.named_children(&mut scope.walk()) {
        if let Some(name) = declared_binding(item) {
            bindings.insert(text(name, bytes).into());
        }
        if item.kind() == "macro_invocation" {
            return None;
        }
        if item.kind() == "use_declaration" {
            let argument = item.child_by_field_name("argument")?;
            let mut pending = vec![argument];
            while let Some(node) = pending.pop() {
                if node.kind() == "use_wildcard" {
                    return None;
                }
                pending.extend(node.named_children(&mut node.walk()));
            }
            let mut uses = Vec::new();
            use_bindings(argument, bytes, false, &mut uses);
            bindings.extend(uses.into_iter().map(|binding| binding.name));
        }
    }
    Some(bindings)
}

fn module_scope(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if is_module_scope(node) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn is_module_scope(node: Node<'_>) -> bool {
    node.kind() == "source_file"
        || node.kind() == "declaration_list"
            && node
                .parent()
                .is_some_and(|parent| parent.kind() == "mod_item")
}

fn import_declaration<'a>(root: Node<'a>, row: &Reference) -> Option<Node<'a>> {
    let position = tree_sitter::Point {
        row: row.line.saturating_sub(1) as usize,
        column: row.col as usize,
    };
    let mut node = root.descendant_for_point_range(position, position)?;
    while node.kind() != "use_declaration" {
        node = node.parent()?;
    }
    Some(node)
}

fn module_item_positions(root: Node<'_>) -> HashSet<(u32, u32)> {
    let mut positions = HashSet::new();
    let mut modules = vec![root];
    while let Some(module) = modules.pop() {
        for item in module.named_children(&mut module.walk()) {
            let start = item.start_position();
            positions.insert((start.row as u32 + 1, start.column as u32));
            if item.kind() == "mod_item"
                && let Some(body) = item.child_by_field_name("body")
            {
                modules.push(body);
            }
        }
    }
    positions
}

fn module_import_binding(
    root: Node<'_>,
    bytes: &[u8],
    row: &Reference,
    module_items: &HashSet<(u32, u32)>,
) -> Option<String> {
    let declaration = import_declaration(root, row)?;
    let start = declaration.start_position();
    if !module_items.contains(&(start.row as u32 + 1, start.column as u32)) {
        return None;
    }
    let mut bindings = Vec::new();
    use_bindings(
        declaration.child_by_field_name("argument")?,
        bytes,
        false,
        &mut bindings,
    );
    let mut matches = bindings
        .into_iter()
        .filter(|binding| binding.path == row.to_name);
    let binding = matches.next()?;
    matches.next().is_none().then_some(binding.name)
}

fn import_namespace_proven(root: Node<'_>, bytes: &[u8], row: &Reference) -> Result<bool> {
    let head = row.to_name.split("::").next().unwrap_or_default();
    let Some(mut node) = import_declaration(root, row) else {
        return Ok(false);
    };
    let declaration = node.id();
    while let Some(scope) = node.parent() {
        if matches!(scope.kind(), "block" | "declaration_list" | "source_file") {
            for item in scope.named_children(&mut scope.walk()) {
                codesage_protocol::work::checkpoint()?;
                if matches!(
                    item.kind(),
                    "mod_item"
                        | "struct_item"
                        | "enum_item"
                        | "trait_item"
                        | "type_item"
                        | "union_item"
                        | "extern_crate_declaration"
                ) && declared_binding(item).is_some_and(|name| text(name, bytes) == head)
                {
                    return Ok(false);
                }
                if item.kind() == "use_declaration"
                    && let Some(argument) = item.child_by_field_name("argument")
                {
                    let mut bindings = Vec::new();
                    use_bindings(argument, bytes, false, &mut bindings);
                    if bindings.iter().any(|binding| {
                        binding.name == head && (item.id() != declaration || binding.path != head)
                    }) {
                        return Ok(false);
                    }
                    let mut pending = vec![argument];
                    while let Some(part) = pending.pop() {
                        codesage_protocol::work::checkpoint()?;
                        let start = part.start_position();
                        if part.kind() == "use_wildcard"
                            && (item.id() != declaration
                                || (start.row as u32 + 1, start.column as u32)
                                    != (row.line, row.col))
                            && relative_glob_bindings(part, root, bytes)
                                .is_none_or(|bindings| bindings.contains(head))
                        {
                            return Ok(false);
                        }
                        pending.extend(part.named_children(&mut part.walk()));
                    }
                }
            }
        }
        if let Some(parameters) = scope.child_by_field_name("type_parameters")
            && parameters
                .named_children(&mut parameters.walk())
                .any(|parameter| {
                    declared_binding(parameter).is_some_and(|name| text(name, bytes) == head)
                })
        {
            return Ok(false);
        }
        node = scope;
    }
    Ok(true)
}

fn pattern_bindings(pattern: Node<'_>, bytes: &[u8], bindings: &mut HashSet<String>) {
    let mut pending = vec![pattern];
    while let Some(pattern) = pending.pop() {
        if matches!(
            pattern.kind(),
            "scoped_identifier"
                | "scoped_type_identifier"
                | "type_identifier"
                | "generic_type"
                | "type_arguments"
        ) {
            continue;
        }
        if matches!(pattern.kind(), "identifier" | "shorthand_field_identifier") {
            bindings.insert(text(pattern, bytes).into());
        }
        pending.extend(pattern.named_children(&mut pattern.walk()));
    }
}

fn use_bindings(node: Node<'_>, bytes: &[u8], public: bool, out: &mut Vec<Use>) {
    let mut pending = vec![(node, String::new())];
    while let Some((node, prefix)) = pending.pop() {
        match node.kind() {
            "use_list" => {
                for child in node.named_children(&mut node.walk()) {
                    pending.push((child, prefix.clone()));
                }
            }
            "scoped_use_list" => {
                if let (Some(path), Some(list)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("list"),
                ) {
                    pending.push((list, format!("{prefix}{}::", rust_use_path(path, bytes))));
                }
            }
            "use_as_clause" => {
                if let (Some(path), Some(alias)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("alias"),
                ) {
                    let path = format!("{prefix}{}", rust_use_path(path, bytes));
                    out.push(Use {
                        name: text(alias, bytes).into(),
                        path: path.strip_suffix("::self").unwrap_or(&path).to_string(),
                        public,
                    });
                }
            }
            "identifier" | "scoped_identifier" | "self" => {
                let path = format!("{prefix}{}", rust_use_path(node, bytes));
                let path = path.strip_suffix("::self").unwrap_or(&path).to_string();
                out.push(Use {
                    name: path.rsplit("::").next().unwrap_or_default().into(),
                    path,
                    public,
                });
            }
            _ => {}
        }
    }
}

fn rust_use_path<'a>(node: Node<'_>, bytes: &'a [u8]) -> Cow<'a, str> {
    let raw = text(node, bytes);
    if !raw.bytes().any(|b| b.is_ascii_whitespace() || b == b'/') {
        return Cow::Borrowed(raw);
    }
    let mut path = String::new();
    let mut stack = vec![node];
    while let Some(segment) = stack.pop() {
        match segment.kind() {
            "scoped_identifier" | "scoped_type_identifier" | "use_wildcard" => {
                let children: Vec<_> = segment.children(&mut segment.walk()).collect();
                stack.extend(children.into_iter().rev());
            }
            "identifier" | "type_identifier" | "self" | "super" | "crate" | "::" | "*" => {
                path.push_str(text(segment, bytes));
            }
            "line_comment" | "block_comment" => {}
            _ => return Cow::Borrowed(raw),
        }
    }
    Cow::Owned(path)
}

fn contains_use_wildcard(argument: Node<'_>) -> bool {
    let mut pending = vec![argument];
    while let Some(node) = pending.pop() {
        if node.kind() == "use_wildcard" {
            return true;
        }
        if matches!(node.kind(), "use_list" | "scoped_use_list") {
            pending.extend(node.named_children(&mut node.walk()));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The caller walk resolves the `pattern` field id once; it must find
    /// the child `child_by_field_name("pattern")` finds, on every node of
    /// this crate's own sources.
    #[test]
    fn pattern_field_id_finds_the_field_name_child() {
        let mut files = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        let (mut nodes, mut patterns) = (0, 0);
        while let Some(path) = files.pop() {
            if path.is_dir() {
                files.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let parsed = parse_file_tolerant(&bytes, Language::Rust).unwrap();
            let root = parsed.tree.root_node();
            let field = root.language().field_id_for_name("pattern").unwrap();
            let mut pending = vec![root];
            while let Some(node) = pending.pop() {
                nodes += 1;
                let by_name = node.child_by_field_name("pattern");
                assert_eq!(node.child_by_field_id(field.get()), by_name, "{path:?}");
                patterns += usize::from(by_name.is_some());
                pending.extend(node.named_children(&mut node.walk()));
            }
        }
        assert!(
            nodes > 100_000 && patterns > 1_000,
            "{nodes} nodes, {patterns} patterns"
        );
    }
}
