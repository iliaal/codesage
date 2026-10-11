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
        let slot = self.state.overlay_cache.project(root);
        let report = match slot.try_lock() {
            Some(mut cache) => cache.apply(root, db, generation),
            None => WorkingTreeOverlayCache::default().apply(root, db, generation),
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
