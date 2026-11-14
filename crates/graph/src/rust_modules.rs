use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use codesage_protocol::{Reference, ReferenceKind, Symbol, SymbolKind};
use codesage_storage::Database;

use crate::bundle::rust_crate_layout;

mod external;

#[cfg(test)]
thread_local! {
    static CONSTRUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Module {
    root_file: String,
    root_dir: String,
    path: Vec<String>,
}

#[derive(Default)]
struct CandidateCache {
    rows: HashMap<String, HashMap<String, Vec<String>>>,
    entries: usize,
    bytes: usize,
}

const CANDIDATE_CACHE_ENTRIES: usize = 16_384;
const CANDIDATE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// `use` re-export hops followed from a renamed binding to the files that
/// import the new name.
const MAX_ALIAS_DEPTH: usize = 3;

/// Distinct alias names one lookup follows across files.
const MAX_ALIAS_NAMES: usize = 32;

#[derive(Default)]
pub(crate) struct RustModules {
    modules: HashMap<String, Vec<Module>>,
    declared: HashSet<String>,
    capped: bool,
    scopes: Mutex<HashMap<String, Vec<Symbol>>>,
    external: Mutex<Option<external::ExternalCrates>>,
    candidates: Mutex<CandidateCache>,
    reexports: Mutex<HashMap<String, Arc<HashSet<String>>>>,
}

#[derive(Default)]
pub(crate) struct RustImportEvidence {
    pub bindings: HashMap<(u32, u32), String>,
    pub module_items: Option<HashSet<(u32, u32)>>,
}

impl RustModules {
    pub(crate) fn import_evidence(
        &self,
        db: &Database,
        caller: &str,
    ) -> Result<RustImportEvidence> {
        let mut external = self
            .external
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        external
            .get_or_insert_with(|| external::ExternalCrates::new(db, &self.modules))
            .import_evidence(db, caller)
    }

    pub(crate) fn external_package(
        &self,
        db: &Database,
        caller: &str,
        spelling: &str,
        is_test: bool,
        sites: &[&Reference],
    ) -> Result<Option<(String, bool)>> {
        let mut external = self
            .external
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        external
            .get_or_insert_with(|| external::ExternalCrates::new(db, &self.modules))
            .package_import(db, caller, spelling, is_test, sites)
    }

    pub(crate) fn external_import_file(
        &self,
        db: &Database,
        row: &Reference,
    ) -> Result<Option<String>> {
        let mut external = self
            .external
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        external
            .get_or_insert_with(|| external::ExternalCrates::new(db, &self.modules))
            .import_file(db, row)
    }

    pub(crate) fn capped(&self) -> bool {
        self.capped
            || self
                .external
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|external| external.capped)
    }

    pub(crate) fn external_definitions(
        &self,
        db: &Database,
        caller: &str,
        spelling: &str,
    ) -> Result<Option<Vec<Symbol>>> {
        if !caller.ends_with(".rs") {
            return Ok(None);
        }
        let mut external = self
            .external
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        external
            .get_or_insert_with(|| external::ExternalCrates::new(db, &self.modules))
            .resolve(db, caller, spelling)
    }

    pub(crate) fn qualified_target(
        &self,
        db: &Database,
        caller: &str,
        spelling: &str,
        target: &Symbol,
    ) -> Result<bool> {
        if let Some(resolved) = self.external_definitions(db, caller, spelling)? {
            return Ok(resolved
                .iter()
                .any(|symbol| symbol.handle() == target.handle()));
        }
        if self.same_crate(caller, &target.file_path) != Some(true) {
            return Ok(false);
        }
        let mut scopes = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scopes = match scopes.entry(target.file_path.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mut symbols = db.symbols_for_file(&target.file_path)?;
                symbols.retain(|symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Module | SymbolKind::Function | SymbolKind::Method
                    )
                });
                entry.insert(symbols)
            }
        };
        let mut owners: Vec<_> = scopes
            .iter()
            .filter(|scope| {
                (scope.line_start, scope.col_start) < (target.line_start, target.col_start)
                    && (target.line_end, target.col_end) <= (scope.line_end, scope.col_end)
            })
            .collect();
        if owners.iter().any(|owner| owner.kind != SymbolKind::Module) {
            return Ok(false);
        }
        owners.sort_by_key(|owner| (owner.line_start, owner.col_start));
        let qualified = owners
            .iter()
            .map(|owner| owner.name.as_str())
            .chain(std::iter::once(target.qualified_name.as_str()))
            .collect::<Vec<_>>()
            .join("::");
        let Some(module) = spelling
            .strip_suffix(&qualified)
            .and_then(|prefix| prefix.strip_suffix("::"))
        else {
            return Ok(false);
        };
        let first = module.split("::").next().unwrap_or_default();
        let module = if matches!(first, "crate" | "self" | "super") {
            module.to_string()
        } else {
            format!("self::{module}")
        };
        let Some(candidates) = self.candidates(caller, &module) else {
            return Ok(false);
        };
        if target.kind == SymbolKind::Macro {
            // A `macro_rules!` has a module path only where a `use` of itself
            // re-exports it; `#[macro_export]` places it at its crate root,
            // which `crate::`, or `super::` from a direct child of the root,
            // names.
            let in_module = candidates
                .iter()
                .any(|candidate| candidate == &target.file_path);
            if owners.is_empty() && in_module && self.self_reexports(db, target)? {
                return Ok(true);
            }
            let roots: Vec<&str> = self
                .modules
                .get(caller)
                .into_iter()
                .flatten()
                .map(|context| context.root_file.as_str())
                .collect();
            if !owners.is_empty()
                || !candidates
                    .iter()
                    .any(|candidate| roots.contains(&candidate.as_str()))
            {
                return Ok(false);
            }
            let mut external = self
                .external
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Past the resolver's read limits, keep the module-path rule.
            return Ok(external
                .get_or_insert_with(|| external::ExternalCrates::new(db, &self.modules))
                .exported_macro(db, target)?
                .unwrap_or(in_module));
        }
        Ok(candidates
            .iter()
            .any(|candidate| candidate == &target.file_path))
    }

    /// A module-level `use` of itself (`pub(crate) use name;`) in `target`'s
    /// file re-exports the macro at that module's path. Read from the stored
    /// import rows, once per file.
    fn self_reexports(&self, db: &Database, target: &Symbol) -> Result<bool> {
        let cached = self
            .reexports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&target.file_path)
            .cloned();
        let names = match cached {
            Some(names) => names,
            None => {
                let names: HashSet<String> = db
                    .references_in_file_range(&target.file_path, 1, u32::MAX)?
                    .into_iter()
                    .filter(|row| row.kind == ReferenceKind::Import && row.from_symbol.is_none())
                    .map(|row| {
                        row.to_name
                            .strip_prefix("self::")
                            .unwrap_or(&row.to_name)
                            .to_string()
                    })
                    .filter(|name| !name.contains("::"))
                    .collect();
                let names = Arc::new(names);
                self.reexports
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(target.file_path.clone(), Arc::clone(&names));
                names
            }
        };
        Ok(names.contains(&target.name))
    }

    pub(crate) fn load(db: &Database) -> Result<Self> {
        Self::from_imports(db, &db.import_include_refs_all()?)
    }

    pub(crate) fn from_imports(db: &Database, imports: &[(String, String)]) -> Result<Self> {
        #[cfg(test)]
        CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
        let files: HashSet<String> = db
            .indexed_files_with_prefix("")?
            .into_iter()
            .filter(|file| file.ends_with(".rs"))
            .collect();
        let mut declarations: HashMap<&str, Vec<&str>> = HashMap::new();
        for (file, name) in imports {
            if files.contains(file)
                && let Some(name) = name.strip_prefix("./")
            {
                declarations.entry(file).or_default().push(name);
            }
        }
        let roots: BTreeMap<_, _> = files
            .iter()
            .filter_map(|file| {
                rust_crate_layout(file)
                    .filter(|(_, root)| *root)
                    .map(|(directory, _)| (file.as_str(), directory))
            })
            .collect();
        let mut parents: HashMap<&str, usize> = roots.keys().map(|file| (*file, 0)).collect();
        let mut children: HashMap<&str, HashSet<&str>> = HashMap::new();
        for (file, directory) in &roots {
            for declaration in declarations.get(file).into_iter().flatten() {
                let candidates: Vec<_> = module_files(&format!("{directory}{declaration}"))
                    .into_iter()
                    .filter(|candidate| files.contains(candidate))
                    .collect();
                if let [candidate] = candidates.as_slice()
                    && let Some((&candidate, _)) = roots.get_key_value(candidate.as_str())
                    && children.entry(file).or_default().insert(candidate)
                {
                    *parents.get_mut(candidate).expect("candidate root") += 1;
                }
            }
        }
        let mut root_queue: VecDeque<_> = roots
            .keys()
            .copied()
            .filter(|file| parents[file] == 0)
            .collect();
        let mut result = Self::default();
        let mut total = 0;
        while let Some(root) = root_queue.pop_front() {
            if !result.declared.contains(root) {
                let mut pending = VecDeque::from([(
                    root.to_string(),
                    Module {
                        root_file: root.to_string(),
                        root_dir: roots[root].to_string(),
                        path: Vec::new(),
                    },
                )]);
                while let Some((file, context)) = pending.pop_front() {
                    codesage_protocol::work::checkpoint()?;
                    let contexts = result.modules.entry(file.clone()).or_default();
                    if contexts.contains(&context) {
                        continue;
                    }
                    total += 1;
                    if total > 16_384 {
                        result.capped = true;
                        return Ok(result);
                    }
                    contexts.push(context.clone());
                    for declaration in declarations.get(file.as_str()).into_iter().flatten() {
                        let mut path = context.path.clone();
                        path.extend(declaration.split('/').map(str::to_string));
                        let candidates: Vec<_> =
                            module_files(&format!("{}{}", context.root_dir, path.join("/")))
                                .into_iter()
                                .filter(|candidate| files.contains(candidate))
                                .collect();
                        if let [candidate] = candidates.as_slice() {
                            result.declared.insert(candidate.clone());
                            pending.push_back((
                                candidate.clone(),
                                Module {
                                    path,
                                    ..context.clone()
                                },
                            ));
                        }
                    }
                }
            }
            for child in children.get(root).into_iter().flatten() {
                let count = parents.get_mut(child).expect("candidate root");
                *count -= 1;
                if *count == 0 {
                    root_queue.push_back(child);
                }
            }
        }
        for root in roots.keys() {
            result.modules.entry((*root).to_string()).or_default();
        }
        Ok(result)
    }

    pub(crate) fn candidates(&self, file: &str, module: &str) -> Option<Vec<String>> {
        if let Some(candidates) = self
            .candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rows
            .get(file)
            .and_then(|rows| rows.get(module))
        {
            return Some(candidates.clone());
        }
        let result = self.uncached_candidates(file, module);
        if let Some(candidates) = &result {
            let mut cache = self
                .candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache
                .rows
                .get(file)
                .is_some_and(|rows| rows.contains_key(module))
            {
                return result;
            }
            let file_bytes = if cache.rows.contains_key(file) {
                0
            } else {
                file.len()
            };
            let bytes = candidates
                .iter()
                .fold(
                    candidates
                        .len()
                        .saturating_mul(std::mem::size_of::<String>()),
                    |bytes, candidate| bytes.saturating_add(candidate.len()),
                )
                .saturating_add(module.len())
                .saturating_add(file_bytes);
            // Retention limits affect reuse only; resolution always returns its full result.
            if cache.entries < CANDIDATE_CACHE_ENTRIES
                && bytes <= CANDIDATE_CACHE_BYTES.saturating_sub(cache.bytes)
            {
                cache
                    .rows
                    .entry(file.to_string())
                    .or_default()
                    .insert(module.to_string(), candidates.clone());
                cache.entries += 1;
                cache.bytes += bytes;
            }
        }
        result
    }

    fn uncached_candidates(&self, file: &str, module: &str) -> Option<Vec<String>> {
        if self.capped {
            return Some(Vec::new());
        }
        let Some(contexts) = self.modules.get(file) else {
            return self.declared.contains(file).then(Vec::new);
        };
        let mut common: Option<Vec<String>> = None;
        for context in contexts {
            let mut parts: Vec<_> = context.path.iter().map(String::as_str).collect();
            let mut segments = module.split("::").peekable();
            match segments.peek().copied() {
                Some("crate") => {
                    parts.clear();
                    segments.next();
                }
                Some("self") => {
                    segments.next();
                }
                Some("super") => {
                    while segments.peek() == Some(&"super") {
                        if parts.pop().is_none() {
                            return Some(Vec::new());
                        }
                        segments.next();
                    }
                }
                _ => parts.clear(),
            }
            parts.extend(segments);
            let candidates = if parts.is_empty() {
                vec![context.root_file.clone()]
            } else {
                module_files(&format!("{}{}", context.root_dir, parts.join("/")))
            };
            if let Some(common) = common.as_mut() {
                common.retain(|candidate| candidates.contains(candidate));
            } else {
                common = Some(candidates);
            }
        }
        Some(common.unwrap_or_default())
    }

    pub(crate) fn same_crate(&self, left: &str, right: &str) -> Option<bool> {
        if self.capped {
            return None;
        }
        let left = self.modules.get(left)?;
        let right = self.modules.get(right)?;
        if left.is_empty() || right.is_empty() {
            return None;
        }
        Some(
            left.iter()
                .any(|a| right.iter().any(|b| a.root_file == b.root_file)),
        )
    }

    pub(crate) fn module_contains(&self, definition: &str, caller: &str) -> Option<bool> {
        if self.capped {
            return None;
        }
        let definitions = self.modules.get(definition)?;
        let callers = self.modules.get(caller)?;
        if definitions.is_empty() || callers.is_empty() {
            return None;
        }
        Some(definitions.iter().any(|definition| {
            callers.iter().any(|caller| {
                caller.root_file == definition.root_file
                    && caller.path.starts_with(&definition.path)
            })
        }))
    }

    pub(crate) fn note(&self, file: &str, imported_by: &[String]) -> Option<&'static str> {
        if self.capped() {
            return Some(
                "Rust module-context or external-resolution limit reached; unresolved dependencies remain unverified.",
            );
        }
        if self.modules.get(file).is_some_and(Vec::is_empty) {
            return Some(
                "Rust target/module roles cannot be distinguished from indexed declarations; context-dependent dependencies are omitted.",
            );
        }
        let has_multiple_roles = |file: &str| {
            self.declared.contains(file) && rust_crate_layout(file).is_some_and(|(_, root)| root)
        };
        if has_multiple_roles(file) || imported_by.iter().any(|file| has_multiple_roles(file)) {
            Some(
                "Rust dependencies follow indexed mod declarations; included files may also be standalone Cargo targets, whose additional dependencies are unverified.",
            )
        } else {
            None
        }
    }

    /// Reference rows that may reach the definition named `name` through a
    /// renamed Rust `use` (`use crate::f as g; g()`), given `rows`, the rows
    /// spelled `name`. Each import row spelled `…::name` whose source text
    /// renames it contributes its file's rows spelled with the alias; a
    /// `pub use … as alias` re-export also contributes the rows spelled
    /// `alias` elsewhere. Candidates only: callers keep a row when forward
    /// resolution binds it to the definition.
    pub(crate) fn alias_references(
        &self,
        db: &Database,
        name: &str,
        rows: &[Reference],
    ) -> Result<Vec<Reference>> {
        let Some(root) = external::project_root(db) else {
            return Ok(Vec::new());
        };
        let key = |row: &Reference| {
            (
                row.from_file.clone(),
                row.line,
                row.col,
                row.to_name.clone(),
            )
        };
        let mut seen: HashSet<_> = rows.iter().map(key).collect();
        let mut names = HashSet::from([name.to_string()]);
        let mut aliases = HashSet::new();
        let mut sources: HashMap<String, Option<Vec<u8>>> = HashMap::new();
        let mut found = Vec::new();
        let mut level = vec![(name.to_string(), rows.to_vec())];
        for _ in 0..MAX_ALIAS_DEPTH {
            let mut next = Vec::new();
            for (current, rows) in &level {
                for row in rows.iter().filter(|row| {
                    row.kind == ReferenceKind::Import
                        && row.from_file.ends_with(".rs")
                        && row.to_name.rsplit("::").next() == Some(current.as_str())
                }) {
                    codesage_protocol::work::checkpoint()?;
                    let source = sources.entry(row.from_file.clone()).or_insert_with(|| {
                        codesage_parser::source::read_indexable_file(
                            &root,
                            std::path::Path::new(&row.from_file),
                        )
                        .ok()
                        .flatten()
                        .map(|source| source.bytes)
                    });
                    let Some((alias, public)) = source
                        .as_deref()
                        .and_then(|bytes| use_alias_at(bytes, row.line, row.col, &row.to_name))
                    else {
                        continue;
                    };
                    if alias == *current || !aliases.insert((row.from_file.clone(), alias.clone()))
                    {
                        continue;
                    }
                    let spelled = |to_name: &str| {
                        to_name == alias
                            || to_name
                                .strip_prefix(alias.as_str())
                                .is_some_and(|rest| rest.starts_with("::"))
                    };
                    for reference in db.references_in_file_range(&row.from_file, 1, u32::MAX)? {
                        if reference.kind != ReferenceKind::Import
                            && spelled(&reference.to_name)
                            && seen.insert(key(&reference))
                        {
                            found.push(reference);
                        }
                    }
                    if public && names.len() < MAX_ALIAS_NAMES && names.insert(alias.clone()) {
                        let chained = db.find_references(&alias, None)?;
                        for reference in &chained {
                            if seen.insert(key(reference)) {
                                found.push(reference.clone());
                            }
                        }
                        next.push((alias, chained));
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            level = next;
        }
        Ok(found)
    }
}

/// The alias a `use` path at (`line`, `col`) is renamed to (`x as y` yields
/// `y`), and whether its declaration is `pub` in any form. `to_name` is the
/// row's full path, whose tail the source text there must spell. A text
/// scan, not a parse: what it reports is only a candidate for resolution.
fn use_alias_at(bytes: &[u8], line: u32, col: u32, to_name: &str) -> Option<(String, bool)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let line_start = if line <= 1 {
        0
    } else {
        text.match_indices('\n')
            .nth(line as usize - 2)
            .map(|(index, _)| index + 1)?
    };
    let start = line_start
        + text[line_start..]
            .char_indices()
            .nth(col as usize)
            .map(|(index, _)| index)?;
    let rest = &text[start..];
    let path_len = rest
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '#')))
        .unwrap_or(rest.len());
    let path = &rest[..path_len];
    let head = to_name.strip_suffix(path)?;
    if path.is_empty() || !(head.is_empty() || head.ends_with("::")) {
        return None;
    }
    let after = rest[path_len..].trim_start();
    let after = after.strip_prefix("as")?;
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let after = after.trim_start();
    let after = after.strip_prefix("r#").unwrap_or(after);
    let alias_len = after
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(after.len());
    let alias = &after[..alias_len];
    if alias.is_empty() || alias == "_" {
        return None;
    }
    Some((alias.to_string(), use_is_public(&text[..start])))
}

/// The indexed source of `file` spells `as alias` somewhere, the shape of a
/// `use` rename; a cheap screen before resolution, not evidence of one.
pub(crate) fn source_renames_to(db: &Database, file: &str, alias: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    if alias.is_empty() || !alias.chars().all(ident) {
        return false;
    }
    let Some(root) = external::project_root(db) else {
        return false;
    };
    let Ok(Some(source)) =
        codesage_parser::source::read_indexable_file(&root, std::path::Path::new(file))
    else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&source.bytes) else {
        return false;
    };
    text.match_indices(alias).any(|(index, _)| {
        let after = text[index + alias.len()..].chars().next();
        let before = text[..index].trim_end_matches(char::is_whitespace);
        after.is_none_or(|c| !ident(c))
            && before.len() < index
            && before
                .strip_suffix("as")
                .is_some_and(|head| head.chars().next_back().is_none_or(|c| !ident(c)))
    })
}

/// Whether the `use` declaration that `before` ends inside is `pub`: the
/// last `use` keyword at an item boundary carries a `pub` visibility.
fn use_is_public(before: &str) -> bool {
    let mut search = before;
    while let Some(index) = search.rfind("use") {
        let preceding = &search[..index];
        let following = search[index + 3..].chars().next();
        let boundary_before = preceding
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if boundary_before && following.is_some_and(char::is_whitespace) {
            let visibility = preceding.trim_end();
            let visibility = match visibility.strip_suffix(')') {
                Some(scoped) => scoped.rfind('(').map_or(visibility, |open| &scoped[..open]),
                None => visibility,
            };
            let visibility = visibility.trim_end();
            return visibility.ends_with("pub")
                && visibility[..visibility.len() - 3]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        }
        search = preceding;
    }
    false
}

fn module_files(path: &str) -> Vec<String> {
    vec![format!("{path}.rs"), format!("{path}/mod.rs")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{
        CallPathRequest, ExportRequest, ImpactOptions, ImpactRequest, ImpactTarget,
    };

    fn candidate_context() -> RustModules {
        RustModules {
            modules: HashMap::from([(
                "src/nested/child.rs".to_string(),
                vec![Module {
                    root_file: "src/lib.rs".to_string(),
                    root_dir: "src/".to_string(),
                    path: vec!["nested".to_string(), "child".to_string()],
                }],
            )]),
            ..RustModules::default()
        }
    }

    #[test]
    fn candidate_memo_preserves_paths_order_and_unknown_states() {
        let modules = candidate_context();
        let file = "src/nested/child.rs";
        for (module, expected) in [
            ("crate", vec!["src/lib.rs"]),
            ("crate::api", vec!["src/api.rs", "src/api/mod.rs"]),
            (
                "self",
                vec!["src/nested/child.rs", "src/nested/child/mod.rs"],
            ),
            ("super", vec!["src/nested.rs", "src/nested/mod.rs"]),
            ("super::super", vec!["src/lib.rs"]),
            ("super::super::super", vec![]),
            ("other", vec!["src/other.rs", "src/other/mod.rs"]),
        ] {
            let expected = Some(expected.into_iter().map(str::to_string).collect());
            assert_eq!(modules.candidates(file, module), expected);
            assert_eq!(modules.candidates(file, module), expected);
        }
        assert_eq!(modules.candidates("unknown.rs", "crate"), None);
        assert_eq!(modules.candidates("unknown.rs", "crate"), None);
        assert_eq!(modules.candidates.lock().unwrap().entries, 7);

        let mut ambiguous = candidate_context();
        ambiguous.modules.get_mut(file).unwrap().push(Module {
            root_file: "alternate/lib.rs".to_string(),
            root_dir: "alternate/".to_string(),
            path: vec![],
        });
        assert_eq!(ambiguous.candidates(file, "crate::api"), Some(vec![]));
        assert_eq!(ambiguous.candidates(file, "crate::api"), Some(vec![]));
        let capped = RustModules {
            capped: true,
            ..RustModules::default()
        };
        assert_eq!(capped.candidates(file, "crate"), Some(vec![]));
        assert_eq!(capped.candidates(file, "crate"), Some(vec![]));
    }

    #[test]
    fn candidate_memo_limits_only_retention_not_resolution() {
        let file = "src/nested/child.rs";
        for prefix in [String::new(), "x".repeat(512)] {
            let modules = candidate_context();
            let mut retained = 0;
            let mut rejected = None;
            for index in 0..=CANDIDATE_CACHE_ENTRIES {
                let module = format!("{prefix}entry{index}");
                let expected = Some(vec![
                    format!("src/{module}.rs"),
                    format!("src/{module}/mod.rs"),
                ]);
                assert_eq!(modules.candidates(file, &module), expected);
                let cache = modules.candidates.lock().unwrap();
                assert!(cache.entries <= CANDIDATE_CACHE_ENTRIES);
                assert!(cache.bytes <= CANDIDATE_CACHE_BYTES);
                if cache.entries == retained {
                    if prefix.is_empty() {
                        assert_eq!(cache.entries, CANDIDATE_CACHE_ENTRIES);
                    } else {
                        assert!(cache.entries < CANDIDATE_CACHE_ENTRIES);
                    }
                    rejected = Some((module, expected));
                    break;
                }
                retained = cache.entries;
            }
            let (module, expected) = rejected.expect("one retention limit must be reached");
            assert_eq!(modules.candidates(file, &module), expected);
            assert_eq!(modules.candidates(file, &module), expected);
            assert_eq!(modules.candidates.lock().unwrap().entries, retained);
            assert!(!modules.capped());
        }
    }

    #[test]
    fn public_rust_resolution_requests_build_context_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let calls: String = (0..100).map(|n| format!("f{n}();")).collect();
        let functions: String = (0..100).map(|n| format!("pub fn f{n}() {{}}\n")).collect();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            format!("mod api; use api::*; pub fn run() {{ {calls} }}"),
        )
        .unwrap();
        std::fs::write(dir.path().join("src/api.rs"), functions).unwrap();
        let db = Database::open_in_memory().unwrap();
        crate::full_index(dir.path(), &db, &[], false).unwrap();
        CONSTRUCTIONS.with(|count| count.set(0));
        assert!(
            crate::trace_call_path(
                &db,
                &CallPathRequest {
                    from: "run".into(),
                    to: "f99".into(),
                    max_depth: 3
                }
            )
            .unwrap()
            .found
        );
        let trace = CONSTRUCTIONS.with(|count| count.replace(0));
        crate::export_context_for_symbol(
            &db,
            "run",
            &ExportRequest {
                query: None,
                symbol: Some("run".into()),
                include_callers: false,
                include_callees: true,
                limit: 100,
            },
        )
        .unwrap();
        let bundle = CONSTRUCTIONS.with(|count| count.replace(0));
        crate::impact_analysis_report(
            &db,
            &ImpactRequest {
                target: ImpactTarget::File {
                    path: "src/api.rs".into(),
                },
                depth: 1,
                source_only: false,
            },
            &ImpactOptions::default(),
        )
        .unwrap();
        let impact = CONSTRUCTIONS.with(|count| count.replace(0));
        assert_eq!((trace, bundle, impact), (1, 1, 1));
    }

    #[test]
    fn public_recommendation_shares_context_across_input_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/tests")).unwrap();
        let declarations: String = (0..10).map(|n| format!("pub mod api{n};")).collect();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            format!("{declarations} mod tests;"),
        )
        .unwrap();
        let calls: String = (0..10).map(|n| format!("crate::api{n}::f{n}();")).collect();
        std::fs::write(
            dir.path().join("src/tests/mod.rs"),
            format!("#[test] fn check() {{ {calls} }}"),
        )
        .unwrap();
        let inputs: Vec<_> = (0..10).map(|n| format!("src/api{n}.rs")).collect();
        for (n, path) in inputs.iter().enumerate() {
            std::fs::write(dir.path().join(path), format!("pub fn f{n}() {{}} ")).unwrap();
        }
        let db = Database::open_in_memory().unwrap();
        crate::full_index(dir.path(), &db, &[], false).unwrap();
        CONSTRUCTIONS.with(|count| count.set(0));
        let report = crate::recommend_tests_with_reachability(
            &db,
            &inputs,
            &crate::ReachabilityOptions::default(),
        )
        .unwrap();
        assert!(!report.reach_walk_capped);
        assert_eq!(report.reachable_total, 1);
        assert_eq!(CONSTRUCTIONS.with(|count| count.get()), 1);
    }
}
