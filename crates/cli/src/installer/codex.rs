//! Codex CLI target: `~/.codex/config.toml`, `[mcp_servers.codesage]`.
//! Codex is global-only (no project-local MCP config), so the `global` flag
//! is ignored. Edited with `toml_edit` to preserve the user's formatting and
//! comments.

use std::path::PathBuf;

use anyhow::{Context, Result};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::{AgentTarget, InstallCtx, InstallOutcome, UninstallOutcome, mcp_command_args};

pub struct CodexTarget;

impl CodexTarget {
    fn path(&self, ctx: &InstallCtx) -> PathBuf {
        ctx.home.join(".codex").join("config.toml")
    }
}

impl AgentTarget for CodexTarget {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn config_path(&self, ctx: &InstallCtx) -> PathBuf {
        self.path(ctx)
    }

    fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome> {
        let path = self.path(ctx);
        let original = super::read_config(&path, "")?;
        let mut doc = original
            .parse::<DocumentMut>()
            .with_context(|| format!("parsing existing TOML at {}", path.display()))?;

        let (command, args) = mcp_command_args(ctx.project_utf8)?;
        let mut arg_arr = Array::new();
        for a in &args {
            arg_arr.push(a.as_str());
        }
        // Explicit tables render as `[mcp_servers.codesage]` rather than inline TOML.
        let mut tbl = Table::new();
        tbl["command"] = value(command);
        tbl["args"] = value(arg_arr);

        // Normalize inline tables while preserving existing servers.
        let servers = doc.entry("mcp_servers").or_insert(Item::Table({
            let mut t = Table::new();
            t.set_implicit(true);
            t
        }));
        if !servers.is_table() {
            // Replace malformed scalar/array values so installation cannot silently do nothing.
            *servers = match servers.as_inline_table().cloned() {
                Some(inline) => Item::Table(inline.into_table()),
                None => Item::Table({
                    let mut t = Table::new();
                    t.set_implicit(true);
                    t
                }),
            };
        }
        if let Some(t) = servers.as_table_mut() {
            t.set_implicit(true);
            t.insert("codesage", Item::Table(tbl));
        }

        let new_text = doc.to_string();
        if new_text == original {
            return Ok(InstallOutcome::Unchanged);
        }
        super::atomic_write(&path, &new_text)?;
        Ok(InstallOutcome::Wrote)
    }

    fn uninstall(&self, ctx: &InstallCtx) -> Result<UninstallOutcome> {
        let path = self.path(ctx);
        let original = super::read_config(&path, "")?;
        if original.is_empty() {
            return Ok(UninstallOutcome::NotConfigured);
        }
        let mut doc = original
            .parse::<DocumentMut>()
            .with_context(|| format!("parsing existing TOML at {}", path.display()))?;

        let removed = match doc.get_mut("mcp_servers") {
            Some(item) if item.is_table() => item
                .as_table_mut()
                .map(|t| t.remove("codesage").is_some())
                .unwrap_or(false),
            Some(item) if item.is_inline_table() => item
                .as_inline_table_mut()
                .map(|t| t.remove("codesage").is_some())
                .unwrap_or(false),
            _ => false,
        };
        if !removed {
            return Ok(UninstallOutcome::NotConfigured);
        }
        super::atomic_write(&path, &doc.to_string())?;
        Ok(UninstallOutcome::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn ctx<'a>(home: &'a Path, project: &'a Path) -> InstallCtx<'a> {
        InstallCtx {
            home,
            project: Some(project),
            project_utf8: Some(project.to_str().expect("test project path must be UTF-8")),
            global: true,
        }
    }

    #[test]
    fn install_is_idempotent_and_preserves_other_servers() {
        let home = tempdir().unwrap();
        let cfg = home.path().join(".codex/config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(
            &cfg,
            "# my codex config\n[mcp_servers.other]\ncommand = \"foo\"\n",
        )
        .unwrap();

        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Wrote);

        let written = fs::read_to_string(&cfg).unwrap();
        assert!(written.contains("# my codex config"), "comment preserved");
        assert!(written.contains("[mcp_servers.other]"), "other server kept");
        assert!(written.contains("[mcp_servers.codesage]"));
        assert!(written.contains("--project"));
        assert!(written.contains("/abs/proj"));

        let leftovers: Vec<_> = fs::read_dir(cfg.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("config.toml")]);

        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Unchanged);
    }

    #[test]
    fn install_into_fresh_file_renders_header_and_round_trips() {
        let home = tempdir().unwrap();
        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Wrote);

        let cfg = home.path().join(".codex/config.toml");
        let written = fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("[mcp_servers.codesage]"),
            "expected header form, got: {written}"
        );
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Unchanged);
        assert_eq!(t.uninstall(&c).unwrap(), UninstallOutcome::Removed);
        assert!(!fs::read_to_string(&cfg).unwrap().contains("codesage"));
    }

    #[test]
    fn global_install_without_project_registers_current_exe_without_project_flag() {
        let home = tempdir().unwrap();
        let t = CodexTarget;
        let c = InstallCtx {
            home: home.path(),
            project: None,
            project_utf8: None,
            global: true,
        };
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Wrote);
        let cfg = home.path().join(".codex/config.toml");
        let written = fs::read_to_string(&cfg).unwrap();
        let exe = std::env::current_exe().unwrap();
        assert!(
            written.contains(exe.to_str().unwrap()),
            "must register this binary, got: {written}"
        );
        assert!(
            !written.contains("--project"),
            "no project, no flag, got: {written}"
        );
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Unchanged);
    }

    #[test]
    fn install_coerces_non_table_mcp_servers() {
        let home = tempdir().unwrap();
        let cfg = home.path().join(".codex/config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(&cfg, "mcp_servers = \"oops\"\n").unwrap();

        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Wrote);
        let written = fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("[mcp_servers.codesage]"),
            "expected header form, got: {written}"
        );
    }

    #[test]
    fn install_errors_on_unreadable_file_without_clobbering() {
        let home = tempdir().unwrap();
        let cfg = home.path().join(".codex/config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(&cfg, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        assert!(t.install(&c).is_err(), "should error on unreadable file");
        assert_eq!(fs::read(&cfg).unwrap(), vec![0xff, 0xfe, 0x00, 0x01]);
    }

    #[test]
    fn uninstall_removes_only_codesage() {
        let home = tempdir().unwrap();
        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        assert_eq!(t.uninstall(&c).unwrap(), UninstallOutcome::NotConfigured);

        let cfg = home.path().join(".codex/config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(&cfg, "[mcp_servers.other]\ncommand = \"foo\"\n").unwrap();
        t.install(&c).unwrap();

        assert_eq!(t.uninstall(&c).unwrap(), UninstallOutcome::Removed);
        let after = fs::read_to_string(&cfg).unwrap();
        assert!(!after.contains("codesage"), "codesage removed: {after}");
        assert!(after.contains("[mcp_servers.other]"), "other server kept");
    }
}
