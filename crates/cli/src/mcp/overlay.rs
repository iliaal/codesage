use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use codesage_graph::overlay::{OverlayReport, OverlayState, WorkingTreeOverlayCache};
use codesage_storage::Database;
use parking_lot::Mutex;
use rmcp::model::CallToolResult;
use serde_json::json;

use super::CodeSageServer;

type CacheSlot = Arc<Mutex<WorkingTreeOverlayCache>>;

#[derive(Default)]
pub(super) struct OverlayCache {
    projects: Mutex<VecDeque<(PathBuf, CacheSlot)>>,
}

impl OverlayCache {
    fn project(&self, root: &Path) -> CacheSlot {
        let mut projects = self.projects.lock();
        if let Some(position) = projects.iter().position(|(path, _)| path == root) {
            let entry = projects.remove(position).expect("position exists");
            let slot = entry.1.clone();
            projects.push_back(entry);
            return slot;
        }
        let slot = Arc::new(Mutex::new(WorkingTreeOverlayCache::default()));
        if projects.len() == 16 {
            projects.pop_front();
        }
        projects.push_back((root.to_path_buf(), slot.clone()));
        slot
    }
}

thread_local! {
    static REPORT: RefCell<Option<OverlayReport>> = const { RefCell::new(None) };
}

pub(super) struct ReportScope;

impl ReportScope {
    pub(super) fn enter() -> Self {
        REPORT.with(|report| report.replace(None));
        Self
    }
}

impl Drop for ReportScope {
    fn drop(&mut self) {
        REPORT.with(|report| report.replace(None));
    }
}

pub(super) fn current() -> Option<OverlayReport> {
    REPORT.with(|report| report.borrow().clone())
}

pub(super) fn clear() {
    REPORT.with(|report| report.replace(None));
}

pub(super) fn annotate(
    mut result: CallToolResult,
    report: Option<OverlayReport>,
) -> CallToolResult {
    let Some(report) = report.filter(|report| report.state != OverlayState::Clean) else {
        return result;
    };
    if let Some(payload) = result.structured_content.as_mut() {
        payload["_meta"]["working_tree_overlay"] = json!(report);
        super::render::rerender_json_text(&mut result);
    }
    result
}

impl CodeSageServer {
    pub(super) fn prepare_overlay(&self, root: &Path, db: &Database) -> anyhow::Result<bool> {
        let generation = self.index_generation(root, &crate::db_path(root));
        let excludes = match crate::load_project_config(root) {
            Ok(config) => Ok(crate::get_exclude_patterns(&config)),
            Err(error) if db.all_file_paths()?.iter().any(|path| path.ends_with(".h")) => {
                Err(error)
            }
            Err(_) => Ok(Vec::new()),
        };
        let report = match excludes {
            Ok(excludes) => {
                let slot = self.state.overlay_cache.project(root);
                match slot.try_lock() {
                    Some(mut cache) => cache.apply_with_excludes(root, db, generation, &excludes),
                    None => WorkingTreeOverlayCache::default()
                        .apply_with_excludes(root, db, generation, &excludes),
                }
            }
            Err(error) => OverlayReport {
                state: OverlayState::Dirty,
                dirty_paths: Vec::new(),
                reason: Some(format!(
                    "working-tree overlay unavailable: project configuration could not be read: {error:#}"
                )),
                checked_all: false,
                elapsed_ms: 0,
            },
        };
        let active = report.state == OverlayState::Overlay;
        codesage_protocol::work::checkpoint()?;
        anyhow::ensure!(
            db.read_snapshot_active(),
            "working-tree overlay lost its pinned read snapshot: {}",
            report
                .reason
                .as_deref()
                .unwrap_or("snapshot is no longer active")
        );
        anyhow::ensure!(
            active || !db.has_structural_overlay()?,
            "failed overlay could not be rolled back; retry the query"
        );
        REPORT.with(|current| current.replace(Some(report)));
        codesage_protocol::work::checkpoint()?;
        Ok(active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_config_preserves_indexed_reads_with_overlay_disclosure() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".codesage");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.toml");
        std::fs::write(
            root.path().join("retained.h"),
            "int retained(void) { return 1; }\n",
        )
        .unwrap();
        let db = Database::open(&directory.join("index.db")).unwrap();
        codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
        drop(db);
        let server = CodeSageServer::new();
        let _scope = ReportScope::enter();
        std::fs::write(
            root.path().join("retained.h"),
            "int edited(void) { return 2; }\n",
        )
        .unwrap();
        for malformed in [true, false] {
            if malformed {
                std::fs::write(&config, "embedding = { this is not valid toml ===").unwrap();
            } else {
                std::fs::remove_file(&config).unwrap();
                std::fs::create_dir(&config).unwrap();
            }
            let count = server
                .with_project_db(root.path().to_str().unwrap(), |db| {
                    assert!(!db.has_structural_overlay()?);
                    assert_eq!(db.find_symbols("retained", None)?.len(), 1);
                    assert!(db.find_symbols("edited", None)?.is_empty());
                    db.file_count()
                })
                .unwrap();
            assert_eq!(count, 1);
            let report = current().unwrap();
            assert_eq!(report.state, OverlayState::Dirty);
            assert!(!report.checked_all);
            assert!(
                report
                    .reason
                    .unwrap()
                    .contains("project configuration could not be read")
            );
        }
        std::fs::remove_dir(&config).unwrap();
        std::fs::write(&config, "").unwrap();
        server
            .with_project_db(root.path().to_str().unwrap(), |db| {
                assert!(db.has_structural_overlay()?);
                assert_eq!(db.find_symbols("edited", None)?.len(), 1);
                assert!(db.find_symbols("retained", None)?.is_empty());
                Ok(())
            })
            .unwrap();
        assert_eq!(current().unwrap().state, OverlayState::Overlay);
        let persisted = Database::open_read_only(&directory.join("index.db")).unwrap();
        assert_eq!(persisted.find_symbols("retained", None).unwrap().len(), 1);
        assert!(persisted.find_symbols("edited", None).unwrap().is_empty());
        std::fs::write(
            root.path().join("retained.h"),
            "int retained(void) { return 1; }\n",
        )
        .unwrap();
        server
            .with_project_db(root.path().to_str().unwrap(), |db| db.file_count())
            .unwrap();
        assert_eq!(current().unwrap().state, OverlayState::Clean);
    }

    #[test]
    fn malformed_config_keeps_non_header_working_tree_overlays() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".codesage");
        std::fs::create_dir_all(&directory).unwrap();
        let source = root.path().join("subject.py");
        std::fs::write(&source, "def before(): return 1\n").unwrap();
        let db = Database::open(&directory.join("index.db")).unwrap();
        codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
        drop(db);
        std::fs::write(directory.join("config.toml"), "embedding = { invalid ===").unwrap();
        std::fs::write(&source, "def after(): return 2\n").unwrap();
        let server = CodeSageServer::new();
        let _scope = ReportScope::enter();
        server
            .with_project_db(root.path().to_str().unwrap(), |db| {
                assert!(db.has_structural_overlay()?);
                assert_eq!(db.find_symbols("after", None)?.len(), 1);
                assert!(db.find_symbols("before", None)?.is_empty());
                Ok(())
            })
            .unwrap();
        assert_eq!(current().unwrap().state, OverlayState::Overlay);
        let persisted = Database::open_read_only(&directory.join("index.db")).unwrap();
        assert_eq!(persisted.find_symbols("before", None).unwrap().len(), 1);
        assert!(persisted.find_symbols("after", None).unwrap().is_empty());
    }
}
