//! Per-session MCP call ledgers under `.codesage/sessions/<id>.calls.jsonl`.
//!
//! Each line records which handles a tool call served, never its arguments,
//! so query text, code snippets, and stack traces stay out of the ledger.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use codesage_protocol::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LEDGER_SUFFIX: &str = ".calls.jsonl";
const SESSIONS_DIR: &str = "sessions";
const CONNECTIONS_DIR: &str = "connections";

/// Record and byte bounds match the daemon diagnostics ring (`DETAIL_LIMIT`
/// records, `BYTE_LIMIT` bytes): a ledger stops growing at the byte bound,
/// and each ledger directory keeps at most this many ledgers.
pub const MAX_LEDGER_FILES: usize = 256;
pub const MAX_LEDGER_BYTES: u64 = 2 * 1024 * 1024;
/// Oldest ledgers are evicted first once a directory exceeds this total.
pub const MAX_LEDGER_TOTAL_BYTES: u64 = 8 * MAX_LEDGER_BYTES;
const MAX_HANDLES: usize = 64;
const MAX_HANDLE_BYTES: usize = 1024;

/// Keys whose subtrees are suggestions, echoes of caller input, or response
/// metadata rather than served evidence.
const SKIP_KEYS: &[&str] = &[
    "next",
    "expand",
    "recover",
    "recovery",
    "remedy",
    "retry",
    "index",
    "cost",
    "completeness",
    "query",
    "input",
    "pattern",
    "code",
    "trace",
    "candidates",
    "notes",
    "summary_notes",
    "why",
    "legend",
    "commands",
    "error",
    "snapshot_path",
    "structural_overlay_paths",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerCall {
    /// Unix epoch milliseconds.
    pub ts: u64,
    pub tool: String,
    pub target_handles: Vec<String>,
    pub detail: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum LedgerScope<'a> {
    /// Calls bound to a `session_start` id on the same connection.
    Session(&'a str),
    /// Calls on a connection with no bound session.
    Connection(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The line was written; `created` when the ledger was empty before it.
    Written { created: bool },
    /// The ledger is at its byte cap and the line was dropped.
    Capped,
}

#[derive(Debug, Default)]
pub(crate) struct LedgerRead {
    pub present: bool,
    pub capped: bool,
    pub calls: Vec<LedgerCall>,
}

pub fn ledger_enabled() -> bool {
    crate::search::env_default_on("CODESAGE_SESSION_LEDGER")
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub fn ledger_path(project_root: &Path, scope: LedgerScope<'_>) -> Result<PathBuf> {
    let sessions = project_root.join(".codesage").join(SESSIONS_DIR);
    Ok(match scope {
        LedgerScope::Session(id) => {
            crate::session::validate_session_id(id)?;
            sessions.join(format!("{id}{LEDGER_SUFFIX}"))
        }
        LedgerScope::Connection(id) => {
            crate::session::validate_session_id(id)?;
            sessions
                .join(CONNECTIONS_DIR)
                .join(format!("{id}{LEDGER_SUFFIX}"))
        }
    })
}

/// Append one call without fsync. The project must already be onboarded
/// (`.codesage/index.db` exists): the ledger never creates `.codesage` itself.
pub fn append_call(
    project_root: &Path,
    scope: LedgerScope<'_>,
    call: &LedgerCall,
) -> Result<AppendOutcome> {
    let path = ledger_path(project_root, scope)?;
    let codesage = project_root.join(".codesage");
    ensure!(
        std::fs::symlink_metadata(&codesage).is_ok_and(|meta| meta.is_dir())
            && std::fs::symlink_metadata(codesage.join("index.db"))
                .is_ok_and(|meta| meta.is_file()),
        "{} is not an onboarded project directory",
        codesage.display()
    );
    let parent = path.parent().context("ledger path has no parent")?;
    crate::session::reject_symlinked_parents(project_root, parent)?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating ledger dir {}", parent.display()))?;
    let mut line = serde_json::to_vec(call).context("serializing ledger call")?;
    line.push(b'\n');
    let mut file = crate::state_file::open(&path, true)
        .with_context(|| format!("opening ledger {}", path.display()))?;
    let len = file.metadata()?.len();
    if len >= MAX_LEDGER_BYTES {
        return Ok(AppendOutcome::Capped);
    }
    if len > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.insert(0, b'\n');
        }
    }
    file.write_all(&line)
        .with_context(|| format!("appending ledger {}", path.display()))?;
    Ok(AppendOutcome::Written { created: len == 0 })
}

/// Drop a session's ledger so a reused id starts empty.
pub fn reset_session_ledger(project_root: &Path, session_id: &str) -> Result<()> {
    let path = ledger_path(project_root, LedgerScope::Session(session_id))?;
    if let Some(parent) = path.parent() {
        crate::session::reject_symlinked_parents(project_root, parent)?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// Evict the oldest ledgers (by mtime) in the directory holding `keep` until
/// at most [`MAX_LEDGER_FILES`] files and [`MAX_LEDGER_TOTAL_BYTES`] remain.
/// `keep` itself is never evicted.
pub fn prune_ledgers(keep: &Path) -> Result<usize> {
    prune_ledgers_with_limits(keep, MAX_LEDGER_FILES, MAX_LEDGER_TOTAL_BYTES)
}

fn prune_ledgers_with_limits(keep: &Path, max_files: usize, max_bytes: u64) -> Result<usize> {
    let dir = keep.parent().context("ledger path has no parent")?;
    let mut ledgers = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path == keep
            || !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(LEDGER_SUFFIX))
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        ledgers.push((mtime, meta.len(), path));
    }
    ledgers.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    let mut files = 1usize;
    let mut bytes = std::fs::symlink_metadata(keep).map_or(0, |meta| meta.len());
    let mut removed = 0;
    for (_, len, path) in ledgers {
        if files < max_files && bytes.saturating_add(len) <= max_bytes {
            files += 1;
            bytes = bytes.saturating_add(len);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::debug!(error = %err, path = %path.display(), "ledger prune failed");
            }
        }
    }
    Ok(removed)
}

/// Read a session ledger's calls at or after `since_ms`. Malformed lines,
/// including a torn final append, are skipped.
pub(crate) fn read_session_ledger(
    project_root: &Path,
    session_id: &str,
    since_ms: u64,
) -> Result<LedgerRead> {
    let path = ledger_path(project_root, LedgerScope::Session(session_id))?;
    if let Some(parent) = path.parent() {
        crate::session::reject_symlinked_parents(project_root, parent)?;
    }
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LedgerRead::default());
        }
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    if !meta.is_file() {
        bail!(
            "refusing to read ledger {}: not a regular file",
            path.display()
        );
    }
    let limit = MAX_LEDGER_BYTES + (MAX_HANDLES * MAX_HANDLE_BYTES) as u64 + 4096;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(limit)
        .read_to_end(&mut bytes)?;
    let calls = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<LedgerCall>(line).ok())
        .filter(|call| call.ts >= since_ms)
        .collect();
    Ok(LedgerRead {
        present: true,
        capped: meta.len() >= MAX_LEDGER_BYTES,
        calls,
    })
}

/// File, symbol, and chunk handles a response served, in traversal order,
/// deduplicated and capped. Bare paths count only under path-named keys.
pub fn response_handles(value: &Value, project_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    collect(value, false, project_root, &mut out, &mut seen);
    out
}

fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "file" | "path" | "files" | "paths" | "primary" | "reachable" | "coupled"
    ) || key.ends_with("_file")
        || key.ends_with("_files")
        || key.ends_with("_path")
}

fn collect(
    value: &Value,
    path_key: bool,
    root: &Path,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if out.len() >= MAX_HANDLES {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if SKIP_KEYS.contains(&key.as_str()) {
                    continue;
                }
                collect(child, is_path_key(key), root, out, seen);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, path_key, root, out, seen);
            }
        }
        Value::String(text) if text.len() <= MAX_HANDLE_BYTES => {
            let handle = match Handle::parse(text) {
                Some(
                    handle @ (Handle::File { .. } | Handle::Symbol { .. } | Handle::Chunk { .. }),
                ) => Some(handle),
                Some(_) => None,
                None if path_key => {
                    let relative = Path::new(text)
                        .strip_prefix(root)
                        .ok()
                        .and_then(Path::to_str)
                        .unwrap_or(text);
                    Handle::file(relative.to_owned())
                }
                None => None,
            };
            if let Some(handle) = handle {
                let rendered = handle.to_string();
                if seen.insert(rendered.clone()) {
                    out.push(rendered);
                }
            }
        }
        _ => {}
    }
}

/// The repository-relative file a ledger handle names.
pub(crate) fn handle_path(handle: &str) -> Option<String> {
    match Handle::parse(handle)? {
        Handle::File { path } | Handle::Symbol { path, .. } | Handle::Chunk { path, .. } => {
            Some(path)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, handles: &[&str]) -> LedgerCall {
        LedgerCall {
            ts: now_millis(),
            tool: tool.into(),
            target_handles: handles.iter().map(|h| (*h).to_string()).collect(),
            detail: Some("compact".into()),
            bytes: 10,
        }
    }

    fn onboarded() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".codesage")).unwrap();
        std::fs::write(dir.path().join(".codesage/index.db"), b"").unwrap();
        dir
    }

    #[test]
    fn response_handles_skip_echoed_input_and_suggestions() {
        let root = Path::new("/repo");
        let value = json!({
            "query": "src/secret.rs how is auth wired",
            "target": {"input": "src/typed.rs", "resolved": [{"path": "src/a.rs", "handle": "sym:src/a.rs#run"}]},
            "results": [{"file_path": "/repo/src/b.rs", "snippet": "fn main() {}"}],
            "primary": ["tests/a_test.rs"],
            "next": [{"tool": "describe", "arguments": {"target": "file:src/c.rs"}}],
            "notes": ["file:src/d.rs"],
            "kind": "file",
        });
        let handles = response_handles(&value, root);
        assert_eq!(
            handles,
            vec![
                "file:tests/a_test.rs",
                "file:src/b.rs",
                "sym:src/a.rs#run",
                "file:src/a.rs"
            ]
        );
    }

    #[test]
    fn response_handles_are_capped() {
        let rows: Vec<_> = (0..200)
            .map(|i| json!({"file": format!("f{i}.rs")}))
            .collect();
        assert_eq!(
            response_handles(&json!({"rows": rows}), Path::new("/")).len(),
            MAX_HANDLES
        );
    }

    #[test]
    fn append_requires_an_onboarded_project() {
        let dir = tempfile::tempdir().unwrap();
        assert!(append_call(dir.path(), LedgerScope::Session("s"), &call("search", &[])).is_err());
        assert!(!dir.path().join(".codesage").exists());
        std::fs::create_dir(dir.path().join(".codesage")).unwrap();
        assert!(append_call(dir.path(), LedgerScope::Session("s"), &call("search", &[])).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path().join(".codesage"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn append_round_trips_and_skips_a_torn_line() {
        let dir = onboarded();
        let first = call("describe", &["file:src/a.rs"]);
        assert_eq!(
            append_call(dir.path(), LedgerScope::Session("s"), &first).unwrap(),
            AppendOutcome::Written { created: true }
        );
        let path = ledger_path(dir.path(), LedgerScope::Session("s")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"ts\":1,\"tool\"").unwrap();
        let second = call("find_symbol", &["sym:src/a.rs#f"]);
        append_call(dir.path(), LedgerScope::Session("s"), &second).unwrap();
        let read = read_session_ledger(dir.path(), "s", 0).unwrap();
        assert!(read.present);
        assert_eq!(read.calls, vec![first, second]);
    }

    #[test]
    fn append_stops_at_the_byte_cap() {
        let dir = onboarded();
        let path = ledger_path(dir.path(), LedgerScope::Session("full")).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'\n'; MAX_LEDGER_BYTES as usize]).unwrap();
        assert_eq!(
            append_call(
                dir.path(),
                LedgerScope::Session("full"),
                &call("search", &[])
            )
            .unwrap(),
            AppendOutcome::Capped
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_LEDGER_BYTES);
        assert!(read_session_ledger(dir.path(), "full", 0).unwrap().capped);
    }

    #[test]
    fn prune_evicts_the_oldest_ledgers_first_and_keeps_the_active_one() {
        let dir = onboarded();
        let sessions = dir.path().join(".codesage").join(SESSIONS_DIR);
        std::fs::create_dir_all(&sessions).unwrap();
        let now = SystemTime::now();
        let day = std::time::Duration::from_secs(86_400);
        for (name, age_days) in [("aged", 90u32), ("old", 30), ("recent", 1)] {
            let path = sessions.join(format!("{name}{LEDGER_SUFFIX}"));
            std::fs::write(&path, b"{}\n").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(now - day * age_days).unwrap();
        }
        let snapshot = sessions.join("aged.json");
        std::fs::write(&snapshot, b"{}").unwrap();
        let keep = sessions.join(format!("active{LEDGER_SUFFIX}"));
        std::fs::write(&keep, b"{}\n").unwrap();
        let file = std::fs::File::options().write(true).open(&keep).unwrap();
        file.set_modified(now - day * 365).unwrap();

        assert_eq!(prune_ledgers_with_limits(&keep, 3, u64::MAX).unwrap(), 1);
        assert!(!sessions.join(format!("aged{LEDGER_SUFFIX}")).exists());
        assert!(sessions.join(format!("old{LEDGER_SUFFIX}")).exists());
        assert!(
            keep.exists(),
            "the active ledger must survive regardless of age"
        );
        assert!(snapshot.exists(), "snapshots are not ledgers");

        assert_eq!(prune_ledgers_with_limits(&keep, 256, 7).unwrap(), 1);
        assert!(!sessions.join(format!("old{LEDGER_SUFFIX}")).exists());
        assert!(sessions.join(format!("recent{LEDGER_SUFFIX}")).exists());
    }

    #[test]
    fn reset_removes_only_the_named_session_ledger() {
        let dir = onboarded();
        append_call(dir.path(), LedgerScope::Session("a"), &call("search", &[])).unwrap();
        append_call(dir.path(), LedgerScope::Session("b"), &call("search", &[])).unwrap();
        reset_session_ledger(dir.path(), "a").unwrap();
        reset_session_ledger(dir.path(), "a").unwrap();
        assert!(!read_session_ledger(dir.path(), "a", 0).unwrap().present);
        assert!(read_session_ledger(dir.path(), "b", 0).unwrap().present);
    }

    #[test]
    fn connection_ledgers_live_apart_from_session_ledgers() {
        let dir = onboarded();
        let path = ledger_path(dir.path(), LedgerScope::Connection("c-1")).unwrap();
        assert!(path.ends_with("sessions/connections/c-1.calls.jsonl"));
        assert!(ledger_path(dir.path(), LedgerScope::Session("../x")).is_err());
    }
}
