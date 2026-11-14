use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_parser::{parse::parse_file_tolerant, source::read_indexable_file};
use codesage_protocol::{Language, Reference, Symbol, SymbolKind, Visibility};
use codesage_storage::Database;
use tree_sitter::Node;

use super::RustImportEvidence;

const RESOLUTION_BUDGET: Duration = Duration::from_secs(2);

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
    resolved: HashMap<(String, String), Option<Vec<Symbol>>>,
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
        let mut ancestor = Path::new(&package);
        loop {
            for suffix in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
                let path = ancestor.join(suffix);
                let Some(path) = path.to_str() else {
                    return Ok(None);
                };
                let config = self.manifest(path);
                if config.as_ref().is_some_and(|config| {
                    config.get("patch").is_some()
                        || config.get("replace").is_some()
                        || config.get("paths").is_some()
                }) || config.is_none() && !self.missing.contains(path)
                {
                    return Ok(None);
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
                    let mut ancestor = Path::new(&package);
                    loop {
                        let Some(path) = ancestor.join("Cargo.toml").to_str().map(str::to_string)
                        else {
                            return Ok(None);
                        };
                        if let Some(manifest) = self.manifest(&path)
                            && let Some(workspace) = manifest.get("workspace")
                        {
                            let Some(inherited) =
                                workspace.get("dependencies").and_then(|deps| deps.get(key))
                            else {
                                return Ok(None);
                            };
                            dependency = inherited.clone();
                            break;
                        }
                        if self.manifests.get(&path).is_some_and(Option::is_none)
                            && !self.missing.contains(&path)
                        {
                            return Ok(None);
                        }
                        let Some(parent) = ancestor.parent() else {
                            return Ok(None);
                        };
                        ancestor = parent;
                    }
                }
                if dependency.get("path").is_some()
                    || dependency.get("optional").and_then(toml::Value::as_bool) == Some(true)
                    || value.get("optional").and_then(toml::Value::as_bool) == Some(true)
                    || !(dependency.is_str()
                        || dependency.get("version").is_some()
                        || dependency.get("git").is_some())
                {
                    return Ok(None);
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
        Ok(match matches.as_slice() {
            [package] => Some((package.clone(), partial)),
            _ => None,
        })
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

    fn source(&mut self, db: &Database, file: &str, caller: bool) -> Result<Option<Arc<Surface>>> {
        let key = (file.to_string(), caller);
        if let Some(surface) = self.sources.get(&key) {
            return Ok(surface.clone());
        }
        // An overlaid file reads as the overlay holds it, so a request-local
        // view of other content still matches its own indexed hash.
        let source = match db.overlay_source(file)? {
            Some(content) => content.map(|content| codesage_parser::source::IndexableSource {
                content_hash: codesage_parser::discover::content_hash(content.as_bytes()),
                bytes: content.into_bytes(),
            }),
            None => self.read(file),
        };
        let mut surface = if let Some(source) = source
            && db.get_file_hash(file)?.as_deref() == Some(source.content_hash.as_str())
        {
            let parsed = parse_file_tolerant(&source.bytes, Language::Rust)?;
            (!parsed.degraded)
                .then(|| {
                    let mut result = surface(
                        parsed.tree.root_node(),
                        &source.bytes,
                        db.symbols_for_file(file),
                        caller,
                    )?;
                    result.module_items = module_item_positions(parsed.tree.root_node());
                    if caller {
                        for row in db.references_in_file_range(file, 1, u32::MAX)? {
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
                                    &row,
                                    &result.module_items,
                                ) {
                                    result.import_bindings.insert((row.line, row.col), binding);
                                }
                                if import_namespace_proven(
                                    parsed.tree.root_node(),
                                    &source.bytes,
                                    &row,
                                )? {
                                    result.proven_import_sites.insert((row.line, row.col));
                                }
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(result)
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
                // An invocation's macro path resolves before its own expansion,
                // but a rewriting attribute can still replace it.
                let invoked = reference.kind == codesage_protocol::ReferenceKind::MacroCall;
                if surface.uncertain_ranges.iter().any(|(start, end, scope)| {
                    *start <= position && position < *end && !(invoked && *scope)
                }) {
                    surface.uncertain_names.insert(reference.to_name);
                }
            }
            surface.uncertain_ranges.clear();
        }
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

pub(super) fn project_root(db: &Database) -> Option<PathBuf> {
    db.path().and_then(|path| {
        let dir = path.parent()?;
        (path.file_name()? == "index.db" && dir.file_name()? == ".codesage")
            .then(|| dir.parent().map(Path::to_path_buf))
            .flatten()
    })
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
        if item.kind() == "macro_invocation"
            && let Some(scope) = lexical_scope(item)
        {
            surface
                .uncertain_ranges
                .push((scope.start_position(), scope.end_position(), true));
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
                    .push((next.start_position(), next.end_position(), false));
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
                    pending.push((list, format!("{prefix}{}::", text(path, bytes))));
                }
            }
            "use_as_clause" => {
                if let (Some(path), Some(alias)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("alias"),
                ) {
                    let path = format!("{prefix}{}", text(path, bytes));
                    out.push(Use {
                        name: text(alias, bytes).into(),
                        path: path.strip_suffix("::self").unwrap_or(&path).to_string(),
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
