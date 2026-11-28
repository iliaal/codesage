//! Request-scoped answers for Python binding resolution.
//!
//! A reverse-dependency walk resolves every Python import row once per
//! request and re-resolves the same bindings, import statements, and file
//! interpretations from many references. Inside a [`PythonMemoScope`] those
//! reads are answered from memory; outside one every read goes to SQLite.

use std::cell::RefMut;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use codesage_protocol::Symbol;
use codesage_protocol::python::{
    PythonBindingCapture, PythonBindingTarget, PythonImportSite, PythonModule,
};

use super::Database;

/// `(file, kind, to_name, line, col)`, the `python_bindings` primary key with
/// the file named by path. Import statements use the same key with the
/// reference kind.
pub(super) type BindingKey = (String, String, String, u32, u32);

#[derive(Debug, Default)]
pub(crate) struct PythonMemo {
    depth: usize,
    /// `total_changes` when the answers were read; a write through this
    /// handle discards them.
    changes: u64,
    table: Option<bool>,
    interpretations: HashMap<(String, String), bool>,
    indexed: Option<Arc<HashSet<String>>>,
    packages: HashMap<String, bool>,
    bindings: HashMap<BindingKey, Option<PythonBindingTarget>>,
    imports: HashMap<BindingKey, Option<PythonModule>>,
    symbols: HashMap<String, Arc<Vec<Symbol>>>,
    /// Estimated bytes of `bindings`, whose decoded targets dominate.
    binding_bytes: usize,
    /// Estimated bytes of every other answer.
    bytes: usize,
}

impl PythonMemo {
    /// Budgets for estimated retained bytes. Decoded binding targets average
    /// about 2 KB, so they get their own budget and cannot crowd out the
    /// smaller answers. An answer that does not fit is read from SQLite again
    /// on each lookup.
    const MAX_BINDING_BYTES: usize = 32 * 1024 * 1024;
    const MAX_BYTES: usize = 16 * 1024 * 1024;

    fn clear(&mut self) {
        let depth = self.depth;
        *self = Self {
            depth,
            ..Self::default()
        };
    }

    fn admit(&mut self, bytes: usize) -> bool {
        admit(&mut self.bytes, Self::MAX_BYTES, bytes)
    }

    pub(super) fn table(&self) -> Option<bool> {
        self.table
    }

    pub(super) fn set_table(&mut self, exists: bool) {
        self.table = Some(exists);
    }

    pub(super) fn interpretation(&self, path: &str, interpretation: &str) -> Option<bool> {
        self.interpretations
            .get(&(path.to_string(), interpretation.to_string()))
            .copied()
    }

    pub(super) fn set_interpretation(&mut self, path: &str, interpretation: &str, matches: bool) {
        if self.admit(path.len() + interpretation.len() + std::mem::size_of::<(String, String)>()) {
            self.interpretations
                .insert((path.to_string(), interpretation.to_string()), matches);
        }
    }

    pub(super) fn indexed(&self) -> Option<Arc<HashSet<String>>> {
        self.indexed.clone()
    }

    pub(super) fn set_indexed(&mut self, indexed: &Arc<HashSet<String>>) {
        let bytes = indexed.iter().fold(
            indexed.capacity() * std::mem::size_of::<String>(),
            |bytes, path| bytes.saturating_add(path.capacity()),
        );
        if self.admit(bytes) {
            self.indexed = Some(Arc::clone(indexed));
        }
    }

    pub(super) fn package(&self, directory: &str) -> Option<bool> {
        self.packages.get(directory).copied()
    }

    pub(super) fn set_package(&mut self, directory: &str, imported: bool) {
        if self.admit(directory.len() + std::mem::size_of::<String>()) {
            self.packages.insert(directory.to_string(), imported);
        }
    }

    pub(super) fn binding(&self, key: &BindingKey) -> Option<Option<PythonBindingTarget>> {
        self.bindings.get(key).cloned()
    }

    pub(super) fn set_binding(&mut self, key: BindingKey, target: &Option<PythonBindingTarget>) {
        let bytes = key_bytes(&key).saturating_add(target.as_ref().map_or(0, decoded_bytes));
        if admit(&mut self.binding_bytes, Self::MAX_BINDING_BYTES, bytes) {
            self.bindings.insert(key, target.clone());
        }
    }

    pub(super) fn import(&self, key: &BindingKey) -> Option<Option<PythonModule>> {
        self.imports.get(key).cloned()
    }

    pub(super) fn set_import(&mut self, key: BindingKey, loaded: &Option<PythonModule>) {
        let bytes = loaded.as_ref().map_or(0, |loaded| {
            loaded
                .loaded
                .iter()
                .chain(&loaded.module_keys)
                .chain(&loaded.file)
                .fold(
                    (loaded.loaded.capacity() + loaded.module_keys.capacity())
                        * std::mem::size_of::<String>(),
                    |bytes, value| bytes.saturating_add(value.capacity()),
                )
        });
        if self.admit(key_bytes(&key).saturating_add(bytes)) {
            self.imports.insert(key, loaded.clone());
        }
    }
}

impl PythonMemo {
    fn symbols(&self, path: &str) -> Option<Arc<Vec<Symbol>>> {
        self.symbols.get(path).cloned()
    }

    fn set_symbols(&mut self, path: &str, symbols: &Arc<Vec<Symbol>>) {
        let bytes = codesage_protocol::symbols_heap_bytes(symbols)
            .saturating_add(path.len() + std::mem::size_of::<String>());
        if self.admit(bytes) {
            self.symbols.insert(path.to_string(), Arc::clone(symbols));
        }
    }
}

fn admit(used: &mut usize, max: usize, bytes: usize) -> bool {
    let fits = bytes <= max.saturating_sub(*used);
    if fits {
        *used += bytes;
    }
    fits
}

/// Estimated heap and inline bytes of a decoded target, counted from its
/// fields rather than from the stored JSON, whose length depends on the
/// encoding (an import site stored as an array is shorter than as an object).
fn decoded_bytes(target: &PythonBindingTarget) -> usize {
    use std::mem::size_of;
    let strings = |names: &[String]| {
        names
            .iter()
            .map(|name| size_of::<String>() + name.len())
            .sum::<usize>()
    };
    let sites = |sites: &[PythonImportSite]| {
        sites
            .iter()
            .map(|site| size_of::<PythonImportSite>() + site.file.len() + site.to_name.len())
            .sum::<usize>()
    };
    size_of::<PythonBindingTarget>()
        + target.module.as_ref().map_or(0, String::len)
        + target.name.len()
        + target.fallback.as_deref().map_or(0, decoded_bytes)
        + target.export_names.as_deref().map_or(0, strings)
        + sites(&target.import_sites)
        + target
            .captures
            .iter()
            .map(|capture| size_of::<PythonBindingCapture>() + sites(&capture.import_sites))
            .sum::<usize>()
        + target.namespace.as_deref().map_or(0, |members| {
            members
                .iter()
                .map(|(name, member)| size_of::<String>() + name.len() + decoded_bytes(member))
                .sum()
        })
}

fn key_bytes(key: &BindingKey) -> usize {
    std::mem::size_of::<BindingKey>() + key.0.len() + key.1.len() + key.2.len()
}

/// Keeps Python binding answers on its [`Database`] until the outermost scope
/// drops. Open one per read-only request; nested scopes share the answers.
pub struct PythonMemoScope<'a> {
    db: &'a Database,
}

impl Drop for PythonMemoScope<'_> {
    fn drop(&mut self) {
        let mut memo = self.db.python_memo.borrow_mut();
        memo.depth -= 1;
        if memo.depth == 0 {
            memo.clear();
        }
    }
}

impl Database {
    /// Answer repeated Python binding reads from memory until the returned
    /// scope, and every scope opened inside it, drops. Writes through this
    /// handle discard the answers. Commits by other connections are not
    /// observed inside the scope unless the caller also holds a read snapshot;
    /// a repeated read then returns what the first read saw.
    pub fn python_memo_scope(&self) -> PythonMemoScope<'_> {
        let mut memo = self.python_memo.borrow_mut();
        if memo.depth == 0 {
            memo.clear();
            memo.changes = self.conn.total_changes();
        }
        memo.depth += 1;
        PythonMemoScope { db: self }
    }

    /// The live answers, or `None` outside a scope. Callers drop the borrow
    /// before the next read; a read that finds it still held goes to SQLite.
    pub(super) fn python_memo(&self) -> Option<RefMut<'_, PythonMemo>> {
        let mut memo = self.python_memo.try_borrow_mut().ok()?;
        if memo.depth == 0 {
            return None;
        }
        let changes = self.conn.total_changes();
        if memo.changes != changes {
            memo.clear();
            memo.changes = changes;
        }
        Some(memo)
    }

    /// [`Database::symbols_for_file`], read once per file in a scope.
    pub fn python_file_symbols(&self, path: &str) -> Result<Arc<Vec<Symbol>>> {
        if let Some(symbols) = self.python_memo().and_then(|memo| memo.symbols(path)) {
            return Ok(symbols);
        }
        let symbols = Arc::new(self.symbols_for_file(path)?);
        if let Some(mut memo) = self.python_memo() {
            memo.set_symbols(path, &symbols);
        }
        Ok(symbols)
    }

    /// Indexed file paths, shared by every Python module load in a scope.
    /// Import loads against this set are memoized; loads against any other
    /// set, even an equal one, are not.
    pub fn python_indexed_files(&self) -> Result<Arc<HashSet<String>>> {
        if let Some(indexed) = self.python_memo().and_then(|memo| memo.indexed()) {
            return Ok(indexed);
        }
        let indexed = Arc::new(self.all_file_paths()?.into_iter().collect::<HashSet<_>>());
        if let Some(mut memo) = self.python_memo() {
            memo.set_indexed(&indexed);
        }
        Ok(indexed)
    }
}

#[cfg(test)]
mod tests {
    use codesage_protocol::python::{PythonBindingTarget, PythonBindings};
    use codesage_protocol::{FileInfo, Language};

    use super::Database;

    fn export(module: &str) -> PythonBindings {
        PythonBindings {
            exports: vec![(
                "patch".into(),
                PythonBindingTarget {
                    module: Some(module.into()),
                    name: "patch".into(),
                    ..Default::default()
                },
            )],
            ..Default::default()
        }
    }

    fn module(db: &Database) -> Option<String> {
        db.python_export_binding("pkg.py", "patch")
            .unwrap()
            .and_then(|target| target.module)
    }

    #[test]
    fn scoped_answers_last_until_the_outer_scope_and_drop_on_local_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let reader = Database::open(&path).unwrap();
        let writer = Database::open(&path).unwrap();
        let file = FileInfo {
            path: "pkg.py".into(),
            language: Language::Python,
            content_hash: "hash".into(),
            is_test: false,
        };
        let rewrite = |db: &Database, name: &str| {
            let id = db.upsert_file(&file).unwrap();
            db.insert_python_bindings(id, &export(name)).unwrap();
        };
        rewrite(&writer, "first");

        let outer = reader.python_memo_scope();
        assert_eq!(module(&reader).as_deref(), Some("first"));
        rewrite(&writer, "second");
        // Another connection's commit is not observed inside the scope, and an
        // inner scope shares the outer scope's answers.
        drop(reader.python_memo_scope());
        assert_eq!(module(&reader).as_deref(), Some("first"));
        // A write through the scoped handle discards every answer.
        rewrite(&reader, "third");
        assert_eq!(module(&reader).as_deref(), Some("third"));
        rewrite(&writer, "fourth");
        assert_eq!(module(&reader).as_deref(), Some("third"));
        drop(outer);
        assert_eq!(module(&reader).as_deref(), Some("fourth"));
    }

    #[test]
    fn binding_budget_counts_every_decoded_field() {
        use codesage_protocol::python::{PythonBindingCapture, PythonImportSite};

        use super::{PythonMemo, decoded_bytes};
        use std::mem::size_of;

        const LONG: usize = 4096;
        let long = || "x".repeat(LONG);
        let site = |file: String, to_name: String| PythonImportSite {
            file,
            to_name,
            line: 1,
            col: 0,
        };
        let empty = PythonBindingTarget::default();
        let grown: [(&str, PythonBindingTarget); 9] = [
            (
                "module",
                PythonBindingTarget {
                    module: Some(long()),
                    ..Default::default()
                },
            ),
            (
                "name",
                PythonBindingTarget {
                    name: long(),
                    ..Default::default()
                },
            ),
            (
                "fallback",
                PythonBindingTarget {
                    fallback: Some(Box::new(PythonBindingTarget {
                        name: long(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ),
            (
                "export_names",
                PythonBindingTarget {
                    export_names: Some(vec![long()]),
                    ..Default::default()
                },
            ),
            (
                "import_sites.file",
                PythonBindingTarget {
                    import_sites: vec![site(long(), String::new())],
                    ..Default::default()
                },
            ),
            (
                "import_sites.to_name",
                PythonBindingTarget {
                    import_sites: vec![site(String::new(), long())],
                    ..Default::default()
                },
            ),
            (
                "captures",
                PythonBindingTarget {
                    captures: vec![PythonBindingCapture {
                        attributes: 0,
                        import_sites: vec![site(long(), String::new())],
                    }],
                    ..Default::default()
                },
            ),
            (
                "namespace.name",
                PythonBindingTarget {
                    namespace: Some(vec![(long(), PythonBindingTarget::default())]),
                    ..Default::default()
                },
            ),
            (
                "namespace.member",
                PythonBindingTarget {
                    namespace: Some(vec![(
                        String::new(),
                        PythonBindingTarget {
                            name: long(),
                            ..Default::default()
                        },
                    )]),
                    ..Default::default()
                },
            ),
        ];
        for (field, target) in &grown {
            assert!(
                decoded_bytes(target) >= decoded_bytes(&empty) + LONG,
                "{field} is not counted"
            );
        }
        // Each element also costs its inline size, so many empty elements count.
        const MANY: usize = 1000;
        let empty_site = || site(String::new(), String::new());
        let inline: [(&str, PythonBindingTarget, usize); 4] = [
            (
                "export_names",
                PythonBindingTarget {
                    export_names: Some(vec![String::new(); MANY]),
                    ..Default::default()
                },
                size_of::<String>(),
            ),
            (
                "import_sites",
                PythonBindingTarget {
                    import_sites: (0..MANY).map(|_| empty_site()).collect(),
                    ..Default::default()
                },
                size_of::<PythonImportSite>(),
            ),
            (
                "captures",
                PythonBindingTarget {
                    captures: vec![
                        PythonBindingCapture {
                            attributes: 0,
                            import_sites: Vec::new(),
                        };
                        MANY
                    ],
                    ..Default::default()
                },
                size_of::<PythonBindingCapture>(),
            ),
            (
                "namespace",
                PythonBindingTarget {
                    namespace: Some(vec![(String::new(), PythonBindingTarget::default()); MANY]),
                    ..Default::default()
                },
                size_of::<String>() + size_of::<PythonBindingTarget>(),
            ),
        ];
        for (field, target, each) in &inline {
            assert!(
                decoded_bytes(target) >= decoded_bytes(&empty) + MANY * each,
                "{field} elements are not counted"
            );
        }
        assert!(decoded_bytes(&empty) >= size_of::<PythonBindingTarget>());

        // Admission charges the decoded estimate, so a target larger than the
        // whole budget is not retained while a small one is.
        let mut memo = PythonMemo::default();
        let key = |name: &str| {
            (
                "pkg.py".to_string(),
                "export".to_string(),
                name.to_string(),
                0,
                0,
            )
        };
        let huge = PythonBindingTarget {
            name: "x".repeat(PythonMemo::MAX_BINDING_BYTES),
            ..Default::default()
        };
        memo.set_binding(key("huge"), &Some(huge));
        assert!(memo.binding(&key("huge")).is_none());
        memo.set_binding(key("small"), &Some(empty.clone()));
        assert_eq!(memo.binding(&key("small")), Some(Some(empty)));
    }
}
