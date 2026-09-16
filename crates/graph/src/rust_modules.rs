use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use anyhow::Result;
use codesage_protocol::{Symbol, SymbolKind};
use codesage_storage::Database;

use crate::bundle::rust_crate_layout;

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
pub(crate) struct RustModules {
    modules: HashMap<String, Vec<Module>>,
    declared: HashSet<String>,
    capped: bool,
    scopes: Mutex<HashMap<String, Vec<Symbol>>>,
}

impl RustModules {
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    pub(crate) fn qualified_target(
        &self,
        db: &Database,
        caller: &str,
        spelling: &str,
        target: &Symbol,
    ) -> Result<bool> {
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
        Ok(self.candidates(caller, &module).is_some_and(|candidates| {
            candidates
                .iter()
                .any(|candidate| candidate == &target.file_path)
        }))
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
        if self.capped {
            return Some(Vec::new());
        }
        let Some(contexts) = self.modules.get(file) else {
            return self.declared.contains(file).then(Vec::new);
        };
        let mut common: Option<Vec<String>> = None;
        for context in contexts {
            let mut parts = context.path.clone();
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
            parts.extend(segments.map(str::to_string));
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
        if self.capped {
            return Some(
                "Rust module-context limit reached; context-dependent dependencies are omitted.",
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
