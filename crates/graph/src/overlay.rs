use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use codesage_features::trust_boundary::JavaTypeContext;
use codesage_parser::detect::{detect_language_with_dialect, is_unambiguous_cpp_extension};
use codesage_parser::discover::{hash_indexable_file, is_test_like_path};
use codesage_parser::source::read_indexable_file;
use codesage_protocol::Language;
use codesage_protocol::stat_cache::CachedFileHash;
use codesage_protocol::work::{StopReason, WorkControl};
use codesage_storage::Database;
use codesage_storage::db::StructuralOverlayFile;
use serde::Serialize;

use crate::index::{ParsedFile, STRUCTURAL_INTERPRETATION, fingerprint_inputs, parse_source};

const MAX_DIRTY_FILES: usize = 50;
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const PREPARATION_BUDGET: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayState {
    Clean,
    Overlay,
    Dirty,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverlayReport {
    pub state: OverlayState,
    pub dirty_paths: Vec<String>,
    pub reason: Option<String>,
    pub checked_all: bool,
    pub elapsed_ms: u64,
}

struct CachedParse {
    parsed: ParsedFile,
    source: String,
    source_bytes: usize,
    observation: Option<CachedFileHash>,
}

#[derive(Default)]
pub struct WorkingTreeOverlayCache {
    root: PathBuf,
    generation: Option<u64>,
    entries: HashMap<String, Arc<CachedParse>>,
}

impl WorkingTreeOverlayCache {
    /// Install request-local structural rows for edits to already indexed files.
    /// Unknown generations deliberately disable reuse between requests.
    /// Removing the last indexed C++ dialect activator falls back when a bounded
    /// walk finds another candidate, including ignored or oversized sources.
    pub fn apply(&mut self, root: &Path, db: &Database, generation: Option<u64>) -> OverlayReport {
        self.apply_with_budget(root, db, generation, PREPARATION_BUDGET)
    }

    fn apply_with_budget(
        &mut self,
        root: &Path,
        db: &Database,
        generation: Option<u64>,
        budget: Duration,
    ) -> OverlayReport {
        let started = Instant::now();
        let parent = codesage_protocol::work::current();
        let deadline = parent
            .as_ref()
            .and_then(WorkControl::deadline)
            .map_or(started + budget, |outer| outer.min(started + budget));
        let control = WorkControl::new(Some(deadline));
        let _propagation = parent.as_ref().map(|outer| {
            let child = control.clone();
            let outer_copy = outer.clone();
            outer.on_cancel(Arc::new(move || {
                child.cancel(outer_copy.reason().unwrap_or(StopReason::ClientCancelled));
            }))
        });
        if self.root != root || self.generation != generation || generation.is_none() {
            self.entries.clear();
            self.root = root.to_path_buf();
            self.generation = generation;
        }
        let mut report = OverlayReport {
            state: OverlayState::Dirty,
            dirty_paths: Vec::new(),
            reason: None,
            checked_all: false,
            elapsed_ms: 0,
        };
        let result = {
            let _scope = control.enter();
            self.prepare(root, db, &mut report)
        };
        if let Err(error) = result {
            report.reason = Some(format!("{error:#}"));
        }
        report.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        report
    }

    fn prepare(&mut self, root: &Path, db: &Database, report: &mut OverlayReport) -> Result<()> {
        codesage_protocol::work::checkpoint()?;
        let files = db.overlay_file_infos()?;
        let interpretations = db.all_file_interpretations()?;
        let stale = files
            .iter()
            .filter(|file| {
                interpretations.get(&file.path).and_then(Option::as_deref)
                    != Some(STRUCTURAL_INTERPRETATION)
            })
            .count();
        ensure!(
            stale == 0,
            "structural interpretation is stale for {stale} indexed files; run codesage index"
        );
        let indexed_cache = db.file_hash_cache()?;
        let mut changed = Vec::new();
        let mut observations = HashMap::new();
        let mut unavailable = None;
        let mut java_changed = false;
        let mut saw_cpp = false;
        let indexed_saw_cpp = files.iter().any(|file| {
            Path::new(&file.path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(is_unambiguous_cpp_extension)
        });
        let mut deleted = HashSet::new();
        for file in &files {
            codesage_protocol::work::checkpoint()?;
            let cached = self
                .entries
                .get(&file.path)
                .and_then(|entry| entry.observation.as_ref())
                .or_else(|| indexed_cache.get(&file.path));
            match hash_indexable_file(root, Path::new(&file.path), cached) {
                Ok(Some((hash, observation, _, _))) => {
                    saw_cpp |= Path::new(&file.path)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(is_unambiguous_cpp_extension);
                    if hash == file.content_hash {
                        continue;
                    }
                    let mut current = file.clone();
                    current.content_hash = hash;
                    observations.insert(file.path.clone(), observation);
                    changed.push(current);
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    deleted.insert(file.path.clone());
                }
                error => {
                    unavailable.get_or_insert_with(|| match error {
                        Err(error) => format!("cannot read {}: {error:#}", file.path),
                        _ => format!("{} exceeds the indexable source size limit", file.path),
                    });
                }
            }
            java_changed |= file.language == Language::Java;
            report.dirty_paths.push(file.path.clone());
            ensure!(
                report.dirty_paths.len() <= MAX_DIRTY_FILES,
                "overlay exceeds {MAX_DIRTY_FILES} changed files"
            );
        }
        codesage_protocol::work::checkpoint()?;
        report.checked_all = true;
        if let Some(reason) = unavailable {
            bail!(reason);
        }
        let mut affected_paths = report.dirty_paths.clone();
        if indexed_saw_cpp && !saw_cpp && files.iter().any(|file| file.path.ends_with(".h")) {
            verify_no_cpp_dialect_candidates(root)?;
        }
        for file in files.iter().filter(|file| {
            indexed_saw_cpp != saw_cpp && file.path.ends_with(".h") && !deleted.contains(&file.path)
        }) {
            let Some(language) = detect_language_with_dialect(Path::new(&file.path), saw_cpp)
            else {
                continue;
            };
            if file.language == language {
                continue;
            }
            if let Some(current) = changed.iter_mut().find(|current| current.path == file.path) {
                current.language = language;
            } else {
                let mut current = file.clone();
                current.language = language;
                changed.push(current);
                affected_paths.push(file.path.clone());
                ensure!(
                    affected_paths.len() <= MAX_DIRTY_FILES,
                    "overlay header dialect changes exceed {MAX_DIRTY_FILES} affected files"
                );
            }
        }
        if affected_paths.is_empty() {
            self.entries.clear();
            report.state = OverlayState::Clean;
            return Ok(());
        }
        // Java annotation roles depend on declarations in otherwise unchanged files.
        if java_changed {
            for file in files.iter().filter(|file| file.language == Language::Java) {
                if !affected_paths.contains(&file.path) {
                    affected_paths.push(file.path.clone());
                    ensure!(
                        affected_paths.len() <= MAX_DIRTY_FILES,
                        "overlay Java declaration context exceeds {MAX_DIRTY_FILES} files"
                    );
                    changed.push(file.clone());
                }
            }
        }
        let mut sources = Vec::new();
        let mut total_bytes = 0;
        for mut file in changed {
            codesage_protocol::work::checkpoint()?;
            if file.language != Language::Java
                && let Some(entry) = self.entries.get(&file.path)
                && entry.parsed.info.content_hash == file.content_hash
                && entry.parsed.info.language == file.language
            {
                total_bytes += entry.source_bytes;
                ensure!(
                    total_bytes <= MAX_SOURCE_BYTES,
                    "overlay exceeds {MAX_SOURCE_BYTES} source bytes"
                );
                sources.push((file, None, Some(entry.clone())));
                continue;
            }
            let snapshot =
                read_indexable_file(root, Path::new(&file.path))?.with_context(|| {
                    format!("{} exceeds the indexable source size limit", file.path)
                })?;
            ensure!(
                snapshot.content_hash == file.content_hash,
                "{} changed during overlay preparation",
                file.path
            );
            total_bytes += snapshot.bytes.len();
            ensure!(
                total_bytes <= MAX_SOURCE_BYTES,
                "overlay exceeds {MAX_SOURCE_BYTES} source bytes"
            );
            file.is_test = is_test_like_path(&file.path);
            sources.push((file, Some(snapshot.bytes), None));
        }
        let java_sources: Vec<_> = sources
            .iter()
            .filter(|(file, _, _)| file.language == Language::Java)
            .map(|(file, source, _)| {
                Ok((
                    file.path.as_str(),
                    source
                        .as_deref()
                        .context("Java overlay source is missing")?,
                ))
            })
            .collect::<Result<_>>()?;
        let java_context = JavaTypeContext::from_sources(&java_sources)?;
        let mut entries = HashMap::new();
        for (file, source, cached) in sources {
            codesage_protocol::work::checkpoint()?;
            let entry = match cached {
                Some(entry) => entry,
                None => {
                    let source = source.context("overlay source snapshot is missing")?;
                    Arc::new(CachedParse {
                        parsed: parse_source(&file, &source, &java_context)?,
                        source_bytes: source.len(),
                        source: String::from_utf8_lossy(&source).into_owned(),
                        observation: observations.remove(&file.path).flatten(),
                    })
                }
            };
            entries.insert(file.path.clone(), entry);
        }
        codesage_protocol::work::checkpoint()?;
        let mut sorted: Vec<_> = entries.values().collect();
        sorted.sort_by(|left, right| left.parsed.info.path.cmp(&right.parsed.info.path));
        let fingerprints: Vec<_> = sorted
            .iter()
            .map(|entry| fingerprint_inputs(&entry.parsed))
            .collect();
        let replacements: Vec<_> = sorted
            .iter()
            .zip(&fingerprints)
            .map(|(entry, fingerprints)| {
                let parsed = &entry.parsed;
                StructuralOverlayFile {
                    file: &parsed.info,
                    symbols: &parsed.symbols,
                    references: &parsed.refs,
                    python_bindings: &parsed.python_bindings,
                    fingerprints,
                    trust_boundaries: &parsed.boundaries,
                    interpretation: STRUCTURAL_INTERPRETATION,
                    source: &entry.source,
                }
            })
            .collect();
        db.install_structural_overlay(&affected_paths, &replacements)?;
        self.entries = entries;
        report.state = OverlayState::Overlay;
        Ok(())
    }
}

fn verify_no_cpp_dialect_candidates(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        codesage_protocol::work::checkpoint()?;
        for entry in std::fs::read_dir(&directory)? {
            codesage_protocol::work::checkpoint()?;
            visited += 1;
            ensure!(
                visited <= 16_384,
                "header dialect context exceeds 16384 directory entries; run codesage index"
            );
            let entry = entry?;
            if entry.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(is_unambiguous_cpp_extension)
            {
                bail!(
                    "header dialect requires codesage index: remaining C++ candidate {}",
                    entry.path().strip_prefix(root)?.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (path, source) in files {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        let path = root.path().join("index.sqlite");
        let db = Database::open(&path).unwrap();
        let stats = crate::full_index(root.path(), &db, &[], false).unwrap();
        assert_eq!(stats.files_failed, 0);
        assert_eq!(stats.files_indexed, files.len());
        drop(db);
        (root, path)
    }

    #[test]
    fn edits_replace_definitions_references_and_hash_without_writing_main() {
        let (root, path) = indexed(&[(
            "src/lib.rs",
            "pub fn before() {}\npub fn caller() { before(); }\n",
        )]);
        let durable = Database::open_read_only(&path).unwrap();
        let baseline_hash = durable.get_file_hash("src/lib.rs").unwrap();
        let source = "pub fn after() {}\npub fn caller() { after(); }\n";
        std::fs::write(root.path().join("src/lib.rs"), source).unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let _snapshot = db.read_snapshot().unwrap();
        let report = WorkingTreeOverlayCache::default().apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert!(report.checked_all);
        assert_eq!(db.find_symbols("after", None).unwrap().len(), 1);
        assert!(db.find_symbols("before", None).unwrap().is_empty());
        assert!(db.find_references("before", None).unwrap().is_empty());
        assert_eq!(db.find_references("after", None).unwrap().len(), 1);
        assert_eq!(
            db.get_file_hash("src/lib.rs").unwrap(),
            Some(codesage_parser::discover::content_hash(source.as_bytes()))
        );
        assert_eq!(durable.find_symbols("before", None).unwrap().len(), 1);
        assert!(durable.find_symbols("after", None).unwrap().is_empty());
        assert_eq!(durable.get_file_hash("src/lib.rs").unwrap(), baseline_hash);
    }

    #[test]
    fn deletions_remove_files_and_restore_on_next_request() {
        let (root, path) = indexed(&[("gone.rs", "pub fn before() {}")]);
        std::fs::remove_file(root.path().join("gone.rs")).unwrap();
        let mut cache = WorkingTreeOverlayCache::default();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert!(db.find_symbols("before", None).unwrap().is_empty());
        assert_eq!(db.get_file_hash("gone.rs").unwrap(), None);
        std::fs::write(root.path().join("gone.rs"), "pub fn before() {}").unwrap();
        let restored = Database::open_read_only(&path).unwrap();
        assert_eq!(
            cache.apply(root.path(), &restored, Some(1)).state,
            OverlayState::Clean
        );
        assert_eq!(restored.find_symbols("before", None).unwrap().len(), 1);
    }

    #[test]
    fn cache_rechecks_source_and_invalidates_on_real_index_generation() {
        let (root, path) = indexed(&[("subject.rs", "pub fn before() {}")]);
        let mut cache = WorkingTreeOverlayCache::default();
        for name in ["first", "later"] {
            std::fs::write(
                root.path().join("subject.rs"),
                format!("pub fn {name}() {{}}"),
            )
            .unwrap();
            let db = Database::open_read_only(&path).unwrap();
            let report = cache.apply(root.path(), &db, Some(1));
            assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
            assert_eq!(db.find_symbols(name, None).unwrap().len(), 1);
            assert!(db.find_symbols("before", None).unwrap().is_empty());
            if name == "later" {
                assert!(db.find_symbols("first", None).unwrap().is_empty());
            }
        }
        let writer = Database::open(&path).unwrap();
        crate::incremental_index(root.path(), &writer, &[], false).unwrap();
        drop(writer);
        let db = Database::open_read_only(&path).unwrap();
        assert_eq!(
            cache.apply(root.path(), &db, Some(2)).state,
            OverlayState::Clean
        );
        assert!(cache.entries.is_empty());
        assert_eq!(db.find_symbols("later", None).unwrap().len(), 1);
    }

    #[test]
    fn more_than_fifty_changes_falls_back_without_any_partial_overlay() {
        let names: Vec<_> = (0..51).map(|n| format!("file{n}.rs")).collect();
        let files: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), "pub fn before() {}"))
            .collect();
        let (root, path) = indexed(&files);
        for name in names {
            std::fs::write(root.path().join(name), "pub fn after() {}").unwrap();
        }
        let db = Database::open_read_only(&path).unwrap();
        let report = WorkingTreeOverlayCache::default().apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Dirty);
        assert_eq!(report.dirty_paths.len(), 51);
        assert!(!report.checked_all);
        assert!(report.reason.unwrap().contains("50 changed files"));
        assert!(db.find_symbols("after", None).unwrap().is_empty());
        assert_eq!(db.find_symbols("before", None).unwrap().len(), 51);
    }

    #[test]
    fn byte_budget_and_expired_deadline_keep_durable_rows() {
        let (root, path) = indexed(&[("subject.rs", "pub fn before() {}")]);
        std::fs::write(
            root.path().join("subject.rs"),
            format!("// {}", "x".repeat(MAX_SOURCE_BYTES)),
        )
        .unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let mut cache = WorkingTreeOverlayCache::default();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Dirty, "{report:?}");
        assert!(report.checked_all);
        assert!(report.reason.unwrap().contains("source bytes"));
        assert_eq!(db.find_symbols("before", None).unwrap().len(), 1);
        let report = cache.apply_with_budget(root.path(), &db, Some(1), Duration::ZERO);
        assert_eq!(report.state, OverlayState::Dirty);
        assert!(!report.checked_all);
        assert!(report.reason.unwrap().contains("DeadlineExceeded"));
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn parent_cancellation_is_preserved() {
        let (root, path) = indexed(&[("subject.rs", "pub fn before() {}")]);
        let db = Database::open_read_only(&path).unwrap();
        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let _scope = control.enter();
        let report = WorkingTreeOverlayCache::default().apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Dirty);
        assert!(report.reason.unwrap().contains("ClientCancelled"));
        assert_eq!(
            codesage_protocol::work::checkpoint().unwrap_err().reason,
            StopReason::ClientCancelled
        );
    }

    #[test]
    fn java_declaration_edit_updates_unchanged_dependents() {
        let (root, path) = indexed(&[
            (
                "Web.java",
                "package app; import org.springframework.web.bind.annotation.*; @RestController class Web {}",
            ),
            ("Shadow.java", "package app; class Other {}"),
        ]);
        let durable = Database::open_read_only(&path).unwrap();
        assert!(
            durable
                .trust_boundaries_for_file_path("Web.java")
                .unwrap()
                .contains(&codesage_protocol::TrustBoundary::Network)
        );
        std::fs::write(
            root.path().join("Shadow.java"),
            "package app; @interface RestController {}",
        )
        .unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let report = WorkingTreeOverlayCache::default().apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert_eq!(report.dirty_paths, ["Shadow.java"]);
        assert_eq!(db.structural_overlay_paths().unwrap().len(), 2);
        assert!(
            db.trust_boundaries_for_file_path("Web.java")
                .unwrap()
                .is_empty()
        );
        assert!(
            durable
                .trust_boundaries_for_file_path("Web.java")
                .unwrap()
                .contains(&codesage_protocol::TrustBoundary::Network)
        );
    }

    #[test]
    fn deleting_last_cpp_source_reinterprets_headers_and_invalidates_cached_dialect() {
        let cpp = "int marker() { return 0; }";
        let (root, path) = indexed(&[
            ("marker.cpp", cpp),
            (
                "subject.h",
                "class original { public: int answer() { return 1; } };",
            ),
        ]);
        let durable = Database::open_read_only(&path).unwrap();
        assert_eq!(
            durable
                .find_symbols("original", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );
        let mut cache = WorkingTreeOverlayCache::default();
        std::fs::remove_file(root.path().join("marker.cpp")).unwrap();
        std::fs::write(root.path().join(".hidden.cpp"), "void hidden() {}").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.cpp"), "void outside() {}").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert_eq!(report.dirty_paths, ["marker.cpp"]);
        assert_eq!(
            db.structural_overlay_paths().unwrap(),
            ["marker.cpp", "subject.h"]
        );
        assert_eq!(
            db.all_files_with_id_and_language().unwrap()[0].2,
            Language::C
        );
        assert!(
            db.find_symbols("original", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .is_empty()
        );

        std::fs::write(
            root.path().join("subject.h"),
            "class current { public: int answer() { return 2; } };",
        )
        .unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert!(
            db.find_symbols("current", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .is_empty()
        );

        std::fs::write(root.path().join("marker.cpp"), cpp).unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert_eq!(report.dirty_paths, ["subject.h"]);
        assert_eq!(
            db.find_symbols("current", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.all_files_with_id_and_language().unwrap()[1].2,
            Language::Cpp
        );
        assert_eq!(
            durable
                .find_symbols("original", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn stale_interpretations_fall_back_for_clean_and_edited_requests() {
        let (root, path) = indexed(&[
            ("a.rs", "pub fn old_interpretation() {}"),
            ("b.rs", "pub fn before() {}"),
        ]);
        let writer = Database::open(&path).unwrap();
        let id = writer.all_files_with_id_and_language().unwrap()[0].0;
        writer.record_file_interpretation(id, "obsolete").unwrap();
        drop(writer);
        let mut cache = WorkingTreeOverlayCache::default();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Dirty, "{report:?}");
        assert!(!report.checked_all);
        assert!(report.dirty_paths.is_empty());
        assert!(report.reason.unwrap().contains("stale for 1 indexed files"));
        assert!(!db.has_structural_overlay().unwrap());

        std::fs::write(root.path().join("b.rs"), "pub fn after() {}").unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Dirty, "{report:?}");
        assert!(!report.checked_all);
        assert!(report.dirty_paths.is_empty());
        assert!(
            report
                .reason
                .unwrap()
                .contains("structural interpretation is stale")
        );
        assert!(!db.has_structural_overlay().unwrap());
        assert!(db.find_symbols("after", None).unwrap().is_empty());
        assert_eq!(db.find_symbols("before", None).unwrap().len(), 1);
    }

    #[test]
    fn skipped_cpp_activator_does_not_change_stored_header_dialect() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("subject.h"), "class original {};").unwrap();
        std::fs::File::create(root.path().join("oversized.cpp"))
            .unwrap()
            .set_len(codesage_parser::discover::MAX_INDEXABLE_FILE_BYTES + 1)
            .unwrap();
        let path = root.path().join("index.sqlite");
        let writer = Database::open(&path).unwrap();
        let stats = crate::full_index(root.path(), &writer, &[], false).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_failed, 0);
        drop(writer);

        let mut cache = WorkingTreeOverlayCache::default();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Clean, "{report:?}");
        assert_eq!(
            db.all_files_with_id_and_language().unwrap()[0].2,
            Language::Cpp
        );
        assert_eq!(
            db.find_symbols("original", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );

        std::fs::write(root.path().join("subject.h"), "class current {};").unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let report = cache.apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert_eq!(
            db.find_symbols("current", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.all_files_with_id_and_language().unwrap()[0].2,
            Language::Cpp
        );
    }

    #[test]
    fn remaining_oversized_cpp_candidate_prevents_false_header_downgrade() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("subject.h"), "class Widget {};").unwrap();
        std::fs::write(root.path().join("marker.cpp"), "void marker() {}").unwrap();
        std::fs::File::create(root.path().join("oversized.cpp"))
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        let path = root.path().join("index.sqlite");
        let writer = Database::open(&path).unwrap();
        let stats = crate::full_index(root.path(), &writer, &[], false).unwrap();
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(stats.files_failed, 0);
        drop(writer);
        std::fs::remove_file(root.path().join("marker.cpp")).unwrap();

        let db = Database::open_read_only(&path).unwrap();
        let report = WorkingTreeOverlayCache::default().apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Dirty, "{report:?}");
        assert_eq!(report.dirty_paths, ["marker.cpp"]);
        assert!(report.checked_all);
        assert!(
            report
                .reason
                .unwrap()
                .contains("remaining C++ candidate oversized.cpp")
        );
        assert!(!db.has_structural_overlay().unwrap());
        assert_eq!(
            db.find_symbols("Widget", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );

        let writer = Database::open(&path).unwrap();
        crate::full_index(root.path(), &writer, &[], false).unwrap();
        assert_eq!(
            writer
                .find_symbols("Widget", Some(codesage_protocol::SymbolKind::Class))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn deeply_nested_java_falls_back_on_a_small_stack_and_normal_edits_still_overlay() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let (root, path) = indexed(&[("Subject.java", "class Subject {}")]);
                let depth = 1000;
                std::fs::write(
                    root.path().join("Subject.java"),
                    format!("{}{}", "class A { ".repeat(depth), "}".repeat(depth)),
                )
                .unwrap();
                let mut cache = WorkingTreeOverlayCache::default();
                let db = Database::open_read_only(&path).unwrap();
                let report = cache.apply(root.path(), &db, Some(1));
                assert_eq!(report.state, OverlayState::Dirty, "{report:?}");
                assert!(
                    report
                        .reason
                        .unwrap()
                        .contains("Java syntax nesting exceeds 128 levels")
                );
                assert!(!db.has_structural_overlay().unwrap());
                assert_eq!(db.find_symbols("Subject", None).unwrap().len(), 1);

                std::fs::write(
                    root.path().join("Subject.java"),
                    "class Subject { class Inner {} }",
                )
                .unwrap();
                let db = Database::open_read_only(&path).unwrap();
                let report = cache.apply(root.path(), &db, Some(1));
                assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
                assert_eq!(
                    db.find_symbols("Inner", Some(codesage_protocol::SymbolKind::Class))
                        .unwrap()
                        .len(),
                    1
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
