//! Multi-agent MCP installer. Registers CodeSage as an MCP server in
//! agents that have no CodeSage plugin (Codex CLI, opencode) by writing
//! their native MCP config. Each agent is one [`AgentTarget`] impl; the
//! registry is a flat list, so adding an agent is one file plus one line.
//!
//! Claude Code is intentionally *not* a target here — it keeps its existing
//! `claude mcp add` / plugin-marketplace registration.
//!
//! All targets register the command `codesage mcp --project <abs root>`, so
//! the spawned server defaults the per-call `project` argument to that root
//! (see `crate::mcp::inject_default_project_line`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

mod codex;
mod opencode;

/// Resolved environment for one install/uninstall operation.
pub struct InstallCtx<'a> {
    /// The user's home directory (`$HOME`). Global config paths derive from
    /// it; passed in rather than read from the environment so targets are
    /// unit-testable without mutating process env.
    pub home: &'a Path,
    /// Absolute project root, baked into the registered server args.
    pub project: &'a Path,
    /// Global (user-level) vs project-local registration. Some targets
    /// (Codex) are global-only and ignore this.
    pub global: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Config file was created or changed.
    Wrote,
    /// CodeSage was already registered identically; nothing changed.
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// CodeSage entry was found and removed.
    Removed,
    /// No CodeSage entry was present.
    NotConfigured,
}

pub trait AgentTarget {
    /// Stable id used on the CLI (`codesage install <id>`).
    fn id(&self) -> &'static str;
    /// Human-readable name for status output.
    fn display_name(&self) -> &'static str;
    /// The config file this target reads/writes for the given context.
    fn config_path(&self, ctx: &InstallCtx) -> PathBuf;
    /// Register CodeSage, preserving any unrelated config. Idempotent.
    fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome>;
    /// Remove only CodeSage's entry, preserving the rest of the file.
    fn uninstall(&self, ctx: &InstallCtx) -> Result<UninstallOutcome>;
}

/// Every registered target.
pub fn all_targets() -> Vec<Box<dyn AgentTarget>> {
    vec![
        Box::new(codex::CodexTarget),
        Box::new(opencode::OpencodeTarget),
    ]
}

/// Resolve a target by id, or `None` if unknown.
pub fn target_by_id(id: &str) -> Option<Box<dyn AgentTarget>> {
    all_targets().into_iter().find(|t| t.id() == id)
}

/// Read a config file, treating "not found" as `default_when_absent` but
/// surfacing any other read error (permission denied, invalid UTF-8) rather
/// than collapsing it to empty. The collapse is dangerous: the caller then
/// writes a fresh file and clobbers the user's real config (comments, other
/// servers) it couldn't read.
pub(crate) fn read_config(path: &Path, default_when_absent: &str) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default_when_absent.to_string()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The argv CodeSage registers for every agent: the `codesage` binary plus
/// `mcp --project <abs>`. Returned split so TOML (separate command/args) and
/// JSON (single command array) targets can each shape it.
pub(crate) fn mcp_command_args(project: &Path) -> (String, Vec<String>) {
    (
        "codesage".to_string(),
        vec![
            "mcp".to_string(),
            "--project".to_string(),
            project.to_string_lossy().into_owned(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_resolvable() {
        let targets = all_targets();
        let mut ids: Vec<&str> = targets.iter().map(|t| t.id()).collect();
        ids.sort_unstable();
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids, deduped, "target ids must be unique");
        for id in ids {
            assert!(target_by_id(id).is_some(), "{id} not resolvable");
        }
        assert!(target_by_id("nope").is_none());
    }

    #[test]
    fn mcp_command_args_bakes_project() {
        let (cmd, args) = mcp_command_args(Path::new("/abs/proj"));
        assert_eq!(cmd, "codesage");
        assert_eq!(args, vec!["mcp", "--project", "/abs/proj"]);
    }
}
