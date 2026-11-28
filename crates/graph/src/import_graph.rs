//! The file-level import graph behind import-cycle detection: symbol-joined
//! edges from storage plus JavaScript/TypeScript module specifiers and C/C++
//! quoted includes, each resolved to the one indexed file its resolver loads.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use codesage_storage::Database;
use codesage_storage::db::ImportPairs;

use codesage_protocol::ReferenceKind;

use crate::bundle::{JsAliases, resolved_path_import_candidates};
use crate::go_modules::GoFileHeaders;

/// Every cross-file import pair, partitioned by whether any load-time
/// directive joins it. Pairs are sorted.
pub fn file_import_pairs(db: &Database) -> Result<ImportPairs> {
    let symbol_pairs = db.enumerate_file_import_pairs()?;
    let path_refs = db.path_import_refs()?;
    let mut pairs = PairSet::default();
    for (from, to) in symbol_pairs.eager {
        pairs.add(from, to, false);
    }
    for (from, to) in symbol_pairs.lazy_only {
        pairs.add(from, to, true);
    }
    if !path_refs.is_empty() {
        let files: HashSet<String> = db.all_file_paths()?.into_iter().collect();
        let aliases = JsAliases::load(db)?;
        let go_modules = if path_refs.iter().any(|(from, _, _)| from.ends_with(".go")) {
            crate::go_modules::GoModules::load(db, files.iter().map(String::as_str))?
        } else {
            crate::go_modules::GoModules::default()
        };
        let mut go_headers = GoFileHeaders::default();
        for (from, spec, lazy_only) in path_refs {
            codesage_protocol::work::checkpoint()?;
            if from.ends_with(".go") {
                let targets = go_modules.targets(&from, &spec);
                if !targets.is_empty() && !go_headers.excludes(db, &from)? {
                    for to in targets {
                        codesage_protocol::work::checkpoint()?;
                        if !go_headers.excludes(db, to)? {
                            pairs.add(from.clone(), to.clone(), lazy_only);
                        }
                    }
                }
                continue;
            }
            let spec = aliases.effective(ReferenceKind::Import, &spec, &from)?;
            if let Some(to) = loaded_target(&spec, &from, &files) {
                pairs.add(from, to, lazy_only);
            }
        }
    }
    Ok(pairs.into_import_pairs())
}

/// The one file a directive loads: the first indexed candidate in resolver
/// order for JavaScript/TypeScript, or an unambiguous C/C++ include target.
/// A declaration file (`.d.ts`) loads nothing at runtime, so it is neither an
/// importer nor a target; the resolver's next candidate stands instead.
fn loaded_target(spec: &str, from: &str, indexed: &HashSet<String>) -> Option<String> {
    if is_declaration_file(from) {
        return None;
    }
    resolved_path_import_candidates(spec, from, indexed)
        .into_iter()
        .find(|candidate| !is_declaration_file(candidate) && indexed.contains(candidate))
}

fn is_declaration_file(path: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
}

/// `(from, to) -> lazy_only`; a load-time directive on any route clears it.
/// Self pairs and declaration-file endpoints are never edges.
#[derive(Default)]
struct PairSet(BTreeMap<(String, String), bool>);

impl PairSet {
    fn add(&mut self, from: String, to: String, lazy_only: bool) {
        if from == to || is_declaration_file(&from) || is_declaration_file(&to) {
            return;
        }
        self.0
            .entry((from, to))
            .and_modify(|lazy| *lazy &= lazy_only)
            .or_insert(lazy_only);
    }

    fn into_import_pairs(self) -> ImportPairs {
        let mut out = ImportPairs::default();
        for (pair, lazy_only) in self.0 {
            if lazy_only {
                out.lazy_only.push(pair);
            } else {
                out.eager.push(pair);
            }
        }
        out
    }
}
