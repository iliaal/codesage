use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_parser::{parse::parse_file_tolerant, source::read_indexable_file};
use codesage_protocol::{Language, Symbol, Visibility};
use codesage_storage::Database;
use tree_sitter::Node;

const RESOLUTION_BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
struct Surface {
    locals: HashSet<String>,
    definitions: HashMap<String, Vec<Symbol>>,
    modules: HashMap<String, bool>,
    uses: Vec<Use>,
    blocked: HashSet<String>,
    shadowed: HashSet<String>,
    uncertain_glob: bool,
    uncertain_ranges: Vec<(tree_sitter::Point, tree_sitter::Point)>,
    uncertain_names: HashSet<String>,
}

#[derive(Clone)]
struct Use {
    name: String,
    path: String,
    public: bool,
}

pub(super) struct ExternalCrates {
    root: Option<PathBuf>,
    library_callers: HashSet<String>,
    caller_roots: HashMap<String, Vec<String>>,
    manifests: HashMap<String, Option<toml::Value>>,
    missing: HashSet<String>,
    sources: HashMap<(String, bool), Option<Surface>>,
    resolved: HashMap<(String, String), Option<Vec<Symbol>>>,
    reads: usize,
    bytes: usize,
    elapsed: Duration,
    deadline: Option<Instant>,
    pub(super) capped: bool,
}

impl ExternalCrates {
    pub(super) fn new(db: &Database, modules: &HashMap<String, Vec<super::Module>>) -> Self {
        let root = db.path().and_then(|path| {
            let dir = path.parent()?;
            (path.file_name()? == "index.db" && dir.file_name()? == ".codesage")
                .then(|| dir.parent().map(Path::to_path_buf))
                .flatten()
        });
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
        Self {
            root,
            library_callers,
            caller_roots,
            manifests: HashMap::new(),
            missing: HashSet::new(),
            sources: HashMap::new(),
            resolved: HashMap::new(),
            reads: 0,
            bytes: 0,
            elapsed: Duration::ZERO,
            deadline: None,
            capped: false,
        }
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
        if matches!(head, "crate" | "self" | "super") {
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
        let direct_library = if tail.is_empty() {
            None
        } else {
            self.library(caller, &package, &manifest, head)
        };
        let Some(source) = self.source(db, caller, true)? else {
            return Ok(None);
        };
        if source.locals.contains(head) || source.uncertain_glob {
            return Ok(None);
        }
        let bindings: Vec<_> = source
            .uses
            .iter()
            .filter(|binding| binding.name == head)
            .collect();
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
                if source.locals.contains(name)
                    || source.shadowed.contains(name)
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
            || source.uncertain_names.contains(spelling)
        {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(unique(self.export(
            db,
            &library,
            &library,
            &path,
            true,
            &mut HashSet::new(),
        )?)))
    }

    fn read(&mut self, path: &str) -> Option<codesage_parser::source::IndexableSource> {
        if self.capped
            || self.reads >= 64
            || self.bytes >= 8 * 1024 * 1024
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
        if self.bytes > 8 * 1024 * 1024 {
            self.capped = true;
            return None;
        }
        Some(source)
    }

    fn manifest(&mut self, path: &str) -> Option<toml::Value> {
        if let Some(manifest) = self.manifests.get(path) {
            return manifest.clone();
        }
        let manifest = self
            .read(path)
            .and_then(|source| toml::from_str(std::str::from_utf8(&source.bytes).ok()?).ok());
        self.manifests.insert(path.into(), manifest.clone());
        manifest
    }

    fn package(&mut self, file: &str) -> Option<(String, toml::Value)> {
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
        if edition
            .and_then(toml::Value::as_str)
            .is_some_and(|edition| matches!(edition, "2018" | "2021" | "2024"))
        {
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
            if let Some(workspace) = self
                .manifest(path)
                .and_then(|value| value.get("workspace").cloned())
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
                if let Some(workspace) = self
                    .manifest(&path)
                    .and_then(|value| value.get("workspace").cloned())
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

    fn source(&mut self, db: &Database, file: &str, caller: bool) -> Result<Option<Surface>> {
        let key = (file.to_string(), caller);
        if let Some(surface) = self.sources.get(&key) {
            return Ok(surface.clone());
        }
        let source = self.read(file);
        let mut surface = if let Some(source) = source
            && db.get_file_hash(file)?.as_deref() == Some(source.content_hash.as_str())
        {
            let parsed = parse_file_tolerant(&source.bytes, Language::Rust)?;
            (!parsed.degraded)
                .then(|| {
                    surface(
                        parsed.tree.root_node(),
                        &source.bytes,
                        db.symbols_for_file(file),
                        caller,
                    )
                })
                .transpose()?
        } else {
            None
        };
        if let Some(surface) = &mut surface
            && !surface.uncertain_ranges.is_empty()
        {
            for reference in db.references_in_file_range(file, 1, u32::MAX)? {
                let position = tree_sitter::Point {
                    row: reference.line.saturating_sub(1) as usize,
                    column: reference.col as usize,
                };
                if surface
                    .uncertain_ranges
                    .iter()
                    .any(|(start, end)| *start <= position && position < *end)
                {
                    surface.uncertain_names.insert(reference.to_name);
                }
            }
            surface.uncertain_ranges.clear();
        }
        self.sources.insert(key, surface.clone());
        Ok(surface)
    }

    fn export(
        &mut self,
        db: &Database,
        root: &str,
        file: &str,
        path: &str,
        public: bool,
        visited: &mut HashSet<(String, String, bool)>,
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
        if !visited.insert((file.into(), path.into(), public)) {
            return Ok(Vec::new());
        }
        let Some(source) = self.source(db, file, false)? else {
            return Ok(Vec::new());
        };
        let (name, tail) = path.split_once("::").unwrap_or((path, ""));
        if source.blocked.contains(name) {
            return Ok(Vec::new());
        }
        if let Some(is_public) = source.modules.get(name) {
            if public && !is_public {
                return Ok(Vec::new());
            }
            let directory = module_directory(file);
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
                return self.export(db, root, child, tail, true, visited);
            }
            return Ok(Vec::new());
        }
        if tail.is_empty()
            && let Some(definitions) = source.definitions.get(name)
        {
            return Ok(definitions
                .iter()
                .filter(|symbol| symbol.visibility == Some(Visibility::Public))
                .cloned()
                .collect());
        }
        let bindings: Vec<_> = source
            .uses
            .iter()
            .filter(|binding| binding.name == name && (!public || binding.public))
            .collect();
        if bindings.len() != 1 {
            return Ok(Vec::new());
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
        } else if redirect.starts_with("super::") || redirect.starts_with("::") {
            return Ok(Vec::new());
        } else {
            (file.to_string(), redirect)
        };
        self.export(db, root, &start_file, &redirect, false, visited)
    }
}

fn unique(mut symbols: Vec<Symbol>) -> Vec<Symbol> {
    if symbols.len() != 1 {
        symbols.clear();
    }
    symbols
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
) -> Result<Surface> {
    let symbols = symbols?;
    let mut surface = Surface::default();
    let mut attributed = false;
    for item in node.named_children(&mut node.walk()) {
        if item.kind() == "attribute_item" {
            attributed |= !matches!(text(item, bytes).strip_prefix("#[doc"), Some(tail) if tail.starts_with('(') || tail.starts_with(" ="));
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
                if attributed {
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
    }
    if !caller {
        return Ok(surface);
    }
    let mut pending: Vec<_> = node.named_children(&mut node.walk()).collect();
    while let Some(item) = pending.pop() {
        let nested = item.parent().is_some_and(|parent| parent.id() != node.id());
        if nested
            && item.kind() == "use_declaration"
            && let Some(argument) = item.child_by_field_name("argument")
        {
            let mut bindings = Vec::new();
            use_bindings(argument, bytes, false, &mut bindings);
            surface
                .shadowed
                .extend(bindings.into_iter().map(|binding| binding.name));
        }
        if nested && let Some(name) = declared_binding(item) {
            surface.shadowed.insert(text(name, bytes).into());
        }
        if item.kind() == "use_wildcard" {
            if let Some(bindings) = relative_glob_bindings(item, node, bytes) {
                surface.shadowed.extend(bindings);
            } else {
                surface.uncertain_glob = true;
            }
        }
        if item.kind() == "macro_invocation"
            && let Some(scope) = lexical_scope(item)
        {
            surface
                .uncertain_ranges
                .push((scope.start_position(), scope.end_position()));
        }
        if item.kind() == "attribute_item" && attribute_can_rewrite(text(item, bytes)) {
            let mut next = item.next_named_sibling();
            while next.is_some_and(|node| {
                node.kind() == "attribute_item" || node.kind().ends_with("comment")
            }) {
                next = next.and_then(|node| node.next_named_sibling());
            }
            if let Some(next) = next {
                surface
                    .uncertain_ranges
                    .push((next.start_position(), next.end_position()));
            }
        }
        if let Some(pattern) = item.child_by_field_name("pattern") {
            pattern_bindings(pattern, bytes, &mut surface.shadowed);
        }
        if item.kind() == "closure_parameters" {
            for pattern in item
                .named_children(&mut item.walk())
                .filter(|child| child.kind() != "parameter")
            {
                pattern_bindings(pattern, bytes, &mut surface.shadowed);
            }
        }
        pending.extend(item.named_children(&mut item.walk()));
    }
    Ok(surface)
}

fn text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or_default()
}

fn declared_binding(item: Node<'_>) -> Option<Node<'_>> {
    if item.kind() == "extern_crate_declaration" {
        return item
            .child_by_field_name("alias")
            .or_else(|| item.child_by_field_name("name"));
    }
    matches!(
        item.kind(),
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
        if node.kind() == "source_file"
            || node.kind() == "declaration_list"
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "mod_item")
        {
            return Some(node);
        }
        node = node.parent()?;
    }
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
                    pending.push((list, format!("{prefix}{}::", text(path, bytes))));
                }
            }
            "use_as_clause" => {
                if let (Some(path), Some(alias)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("alias"),
                ) {
                    out.push(Use {
                        name: text(alias, bytes).into(),
                        path: format!("{prefix}{}", text(path, bytes)),
                        public,
                    });
                }
            }
            "identifier" | "scoped_identifier" | "self" => {
                let path = format!("{prefix}{}", text(node, bytes));
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
