//! Per-connection attribution of tool calls to session call ledgers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use codesage_graph::session_ledger::{
    AppendOutcome, LedgerCall, LedgerScope, append_call, ledger_enabled, now_millis, prune_ledgers,
    reset_session_ledger, response_handles, retain_indexed,
};
use parking_lot::Mutex;
use rmcp::model::CallToolResult;
use serde_json::{Map, Value};

/// Tools whose responses serve per-file evidence. Orientation tools are
/// recorded as calls without handles; `help` and the session tools are not
/// recorded.
const HANDLE_TOOLS: &[&str] = &[
    "describe",
    "find_symbol",
    "find_references",
    "find_similar",
    "list_dependencies",
    "search",
    "trace_call_path",
    "from_trace",
    "impact_analysis",
    "export_context",
    "find_coupling",
    "assess_risk",
    "assess_risk_batch",
    "assess_risk_diff",
    "recommend_tests",
    "review_rehearsal",
    "find_feature",
    "feature_bundle",
    "edit_check",
];

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

pub(super) struct ConnectionLedger {
    enabled: bool,
    connection: String,
    sessions: Mutex<HashMap<PathBuf, String>>,
}

impl ConnectionLedger {
    pub(super) fn new() -> Self {
        Self::with_enabled(ledger_enabled())
    }

    pub(super) fn with_enabled(enabled: bool) -> Self {
        let seq = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
        Self {
            enabled,
            connection: format!("mcp-{}-{}-{seq}", now_millis() / 1000, std::process::id()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Best effort: a ledger failure never changes the tool's response.
    pub(super) fn record(
        &self,
        project: &Path,
        tool: &str,
        arguments: &Map<String, Value>,
        response: &CallToolResult,
    ) {
        if !self.enabled
            || !project.is_absolute()
            || tool.len() > 64
            || !tool.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return;
        }
        let ok = response.is_error != Some(true);
        if ok && tool == "session_start" {
            let id = arguments
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if let Err(error) = reset_session_ledger(project, id) {
                tracing::debug!(error = %error, "session ledger reset failed");
            }
            self.sessions
                .lock()
                .insert(project.to_path_buf(), id.to_owned());
        }
        if matches!(tool, "session_start" | "session_end" | "help") {
            return;
        }
        let structured = response.structured_content.as_ref();
        let target_handles = match structured {
            Some(value) if ok && HANDLE_TOOLS.contains(&tool) => {
                indexed_only(project, response_handles(value, project))
            }
            _ => Vec::new(),
        };
        let cost = structured.and_then(|value| value.get("cost"));
        let bytes = cost
            .and_then(|cost| cost.get("bytes"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                response
                    .content
                    .iter()
                    .filter_map(|content| content.as_text())
                    .map(|text| text.text.len() as u64)
                    .sum()
            });
        let call = LedgerCall {
            ts: now_millis(),
            tool: tool.to_owned(),
            target_handles,
            detail: cost
                .and_then(|cost| cost.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            bytes,
        };
        let session = self.sessions.lock().get(project).cloned();
        let scope = match &session {
            Some(id) => LedgerScope::Session(id),
            None => LedgerScope::Connection(&self.connection),
        };
        match append_call(project, scope, &call) {
            Ok(AppendOutcome::Written { prune: true }) => {
                let pruned = codesage_graph::session_ledger::ledger_path(project, scope)
                    .and_then(|path| prune_ledgers(&path));
                if let Err(error) = pruned {
                    tracing::debug!(error = %error, "session ledger prune failed");
                }
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(error = %error, "session ledger append failed"),
        }
    }
}

/// Drop every handle whose file is not in the project's index; an unreadable
/// index drops them all rather than record unverified, caller-supplied paths.
fn indexed_only(project: &Path, handles: Vec<String>) -> Vec<String> {
    if handles.is_empty() {
        return handles;
    }
    match codesage_storage::Database::open_read_only(&project.join(".codesage").join("index.db")) {
        Ok(db) => retain_indexed(&db, handles),
        Err(error) => {
            tracing::debug!(error = %error, "session ledger index check failed; dropping handles");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn onboarded() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".codesage")).unwrap();
        let db = codesage_storage::Database::open(&dir.path().join(".codesage/index.db")).unwrap();
        for path in ["src/a.rs", "src/b.rs"] {
            db.upsert_file(&codesage_protocol::FileInfo {
                path: path.into(),
                language: codesage_protocol::Language::Rust,
                content_hash: "x".into(),
                is_test: false,
            })
            .unwrap();
        }
        dir
    }

    fn result(value: Value) -> CallToolResult {
        CallToolResult::structured(value)
    }

    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn calls_before_session_start_go_to_the_connection_ledger() {
        let dir = onboarded();
        let ledger = ConnectionLedger::with_enabled(true);
        ledger.record(
            dir.path(),
            "describe",
            &Map::new(),
            &result(json!({"card": {"handle": "file:src/a.rs"}})),
        );
        let connection = dir
            .path()
            .join(".codesage/sessions/connections")
            .join(format!("{}.calls.jsonl", ledger.connection));
        assert_eq!(
            lines(&connection)[0]["target_handles"],
            json!(["file:src/a.rs"])
        );

        let args = json!({"session_id": "s1"}).as_object().unwrap().clone();
        ledger.record(
            dir.path(),
            "session_start",
            &args,
            &result(json!({"session_id": "s1"})),
        );
        ledger.record(
            dir.path(),
            "find_symbol",
            &Map::new(),
            &result(json!({"results": [{"file_path": "src/b.rs"}], "cost": {"bytes": 77, "detail": "standard"}})),
        );
        let session = lines(&dir.path().join(".codesage/sessions/s1.calls.jsonl"));
        assert_eq!(
            session,
            vec![
                json!({"ts": session[0]["ts"], "tool": "find_symbol", "target_handles": ["file:src/b.rs"], "detail": "standard", "bytes": 77})
            ]
        );
        assert_eq!(lines(&connection).len(), 1);
    }

    #[test]
    fn handles_outside_the_index_are_never_recorded() {
        let dir = onboarded();
        let ledger = ConnectionLedger::with_enabled(true);
        ledger.record(
            dir.path(),
            "from_trace",
            &Map::new(),
            &result(json!({"frames": [
                {"status": "resolved", "file": "src/a.rs"},
                {"status": "resolved", "file": "app/secret_trace_path.py"}
            ]})),
        );
        let connection = dir
            .path()
            .join(".codesage/sessions/connections")
            .join(format!("{}.calls.jsonl", ledger.connection));
        assert_eq!(
            lines(&connection)[0]["target_handles"],
            json!(["file:src/a.rs"])
        );
        assert!(
            !std::fs::read_to_string(&connection)
                .unwrap()
                .contains("secret")
        );
    }

    #[test]
    fn disabled_ledger_writes_nothing() {
        let dir = onboarded();
        let ledger = ConnectionLedger::with_enabled(false);
        ledger.record(
            dir.path(),
            "describe",
            &Map::new(),
            &result(json!({"card": {"handle": "file:src/a.rs"}})),
        );
        assert!(!dir.path().join(".codesage/sessions").exists());
    }

    #[test]
    fn error_responses_record_no_handles_and_do_not_bind_sessions() {
        let dir = onboarded();
        let ledger = ConnectionLedger::with_enabled(true);
        let mut failed = result(json!({"card": {"handle": "file:src/a.rs"}}));
        failed.is_error = Some(true);
        ledger.record(dir.path(), "session_start", &Map::new(), &failed);
        ledger.record(dir.path(), "describe", &Map::new(), &failed);
        assert!(ledger.sessions.lock().is_empty());
        let connection = dir
            .path()
            .join(".codesage/sessions/connections")
            .join(format!("{}.calls.jsonl", ledger.connection));
        let rows = lines(&connection);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["tool"], "describe");
        assert_eq!(rows[0]["target_handles"], json!([]));
    }
}
