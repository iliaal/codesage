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

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

mod codex;
mod opencode;

/// Resolved environment for one install/uninstall operation.
pub struct InstallCtx<'a> {
    /// The user's home directory (`$HOME`). Global config paths derive from
    /// it; passed in rather than read from the environment so targets are
    /// unit-testable without mutating process env.
    pub home: &'a Path,
    /// Absolute project root (for display and project-local config paths).
    /// `None` for a global registration made outside any onboarded project —
    /// the entry is then registered without a `--project` default and the
    /// server resolves the project per call instead.
    pub project: Option<&'a Path>,
    /// Canonical UTF-8 project root baked into `codesage mcp --project`.
    /// `None` exactly when `project` is; targets must omit `--project`
    /// rather than guess.
    pub project_utf8: Option<&'a str>,
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

/// Replace `path`'s contents atomically: write to a temp file in the *same*
/// directory, flush it to disk, then rename over the target. A crash, ENOSPC,
/// or interrupt mid-write leaves the original config intact instead of a
/// truncated one. Truncate-then-write would let a failed write destroy the
/// user's other MCP servers, settings, and comments — the very content the
/// read side works so hard not to clobber. The temp file shares the target's
/// directory so the rename stays on one filesystem (a cross-device rename is
/// not atomic and would fail); on any error the `NamedTempFile` is cleaned up
/// on drop, so no stray temp file lingers.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut tmp = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    tmp.write_all(contents.as_bytes())
        .with_context(|| format!("writing temp file in {}", parent.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("flushing temp file in {}", parent.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The argv CodeSage registers for every agent: this binary plus `mcp`, with
/// `--project <abs>` when the registration is project-bound. Returned split
/// so TOML (separate command/args) and JSON (single command array) targets
/// can each shape it.
///
/// The binary is [`std::env::current_exe`], never the literal `codesage`:
/// a PATH-installed name may resolve to another build (or nothing) where
/// the agent runs. A failure to resolve it errors rather than registering
/// a path that cannot work — the same rule as the git-hook installer.
pub(crate) fn mcp_command_args(project_utf8: Option<&str>) -> Result<(String, Vec<String>)> {
    let exe =
        std::env::current_exe().context("resolving current_exe for agent MCP registration")?;
    let command = exe
        .to_str()
        .with_context(|| format!("codesage binary path is not valid UTF-8: {}", exe.display()))?
        .to_owned();
    let mut args = vec!["mcp".to_string()];
    if let Some(project) = project_utf8 {
        args.push("--project".to_string());
        args.push(project.to_string());
    }
    Ok((command, args))
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
        let exe = std::env::current_exe().unwrap();
        let (cmd, args) = mcp_command_args(Some("/abs/proj")).unwrap();
        assert_eq!(cmd, exe.to_str().unwrap());
        assert_eq!(args, vec!["mcp", "--project", "/abs/proj"]);
    }

    #[test]
    fn mcp_command_args_without_project_omits_the_flag() {
        // A global registration made outside any project carries no
        // `--project` default; the server resolves the project per call.
        let (cmd, args) = mcp_command_args(None).unwrap();
        assert_eq!(cmd, std::env::current_exe().unwrap().to_str().unwrap());
        assert_eq!(args, vec!["mcp"]);
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "original\n").unwrap();

        atomic_write(&path, "replaced\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced\n");

        // The same-dir temp file must be renamed away, not left behind.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "temp file lingered: {entries:?}");
    }

    #[test]
    fn atomic_write_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deep/config.toml");
        atomic_write(&path, "hi\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi\n");
    }
}
