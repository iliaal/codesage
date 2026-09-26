//! The file-level import graph behind import-cycle detection: symbol-joined
//! edges from storage plus JavaScript/TypeScript module specifiers and C/C++
//! quoted includes resolved to indexed files by the same rules
//! `list_dependencies` applies.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use codesage_storage::Database;
use codesage_storage::db::ImportPairs;

use crate::bundle::path_import_candidates;

/// Every cross-file import pair, partitioned by whether any load-time
/// directive joins it. Pairs are sorted.
pub fn file_import_pairs(db: &Database) -> Result<ImportPairs> {
    let symbol_pairs = db.enumerate_file_import_pairs()?;
    let path_refs = db.path_import_refs(None)?;
    let mut pairs = PairSet::default();
    for (from, to) in symbol_pairs.eager {
        pairs.add(from, to, false);
    }
    for (from, to) in symbol_pairs.lazy_only {
        pairs.add(from, to, true);
    }
    if !path_refs.is_empty() {
        let files: HashSet<String> = db.all_file_paths()?.into_iter().collect();
        pairs.add_path_refs(path_refs, |path| files.contains(path))?;
    }
    Ok(pairs.into_import_pairs())
}

/// Load-time import edges with both endpoints in `files`, without a
/// whole-index sweep. Pairs joined only by lazy directives are excluded.
pub(crate) fn import_edges_within(db: &Database, files: &[&str]) -> Result<Vec<(String, String)>> {
    if files.len() < 2 {
        return Ok(Vec::new());
    }
    let mut pairs = PairSet::default();
    for (from, to) in db.import_edges_within(files)? {
        pairs.add(from, to, false);
    }
    let members: HashSet<&str> = files.iter().copied().collect();
    pairs.add_path_refs(db.path_import_refs(Some(files))?, |path| {
        members.contains(path)
    })?;
    Ok(pairs.into_import_pairs().eager)
}

/// `(from, to) -> lazy_only`; a load-time directive on any route clears it.
#[derive(Default)]
struct PairSet(BTreeMap<(String, String), bool>);

impl PairSet {
    fn add(&mut self, from: String, to: String, lazy_only: bool) {
        if from == to {
            return;
        }
        self.0
            .entry((from, to))
            .and_modify(|lazy| *lazy &= lazy_only)
            .or_insert(lazy_only);
    }

    fn add_path_refs(
        &mut self,
        refs: Vec<(String, String, bool)>,
        indexed: impl Fn(&str) -> bool,
    ) -> Result<()> {
        for (from, spec, lazy_only) in refs {
            codesage_protocol::work::checkpoint()?;
            let mut targets: Vec<String> = path_import_candidates(&spec, &from)
                .into_iter()
                .filter(|candidate| indexed(candidate))
                .collect();
            targets.sort_unstable();
            targets.dedup();
            for to in targets {
                self.add(from.clone(), to, lazy_only);
            }
        }
        Ok(())
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
