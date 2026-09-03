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
        // Build an explicit table so it renders as a standard
        // `[mcp_servers.codesage]` header (what Codex's docs show) rather
        // than an inline table.
        let mut tbl = Table::new();
        tbl["command"] = value(command);
        tbl["args"] = value(arg_arr);

        // Get-or-create `mcp_servers` as a standard (non-inline, implicit)
        // table. On an empty file `doc["mcp_servers"]["codesage"] = …` would
        // render the whole thing as one inline table — valid TOML, but ugly
        // and (worse) not matchable by `as_table_mut` on uninstall. Coerce a
        // pre-existing inline `mcp_servers` to a standard table too.
        let servers = doc.entry("mcp_servers").or_insert(Item::Table({
            let mut t = Table::new();
            t.set_implicit(true);
            t
        }));
        if !servers.is_table() {
            // Coerce an inline table; replace any other non-table value
            // (scalar/array — malformed for Codex) with a fresh table so we
            // never silently report "unchanged" without writing the entry.
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
        // Absent file → nothing to remove; a real read error propagates so we
        // don't misreport an unreadable-but-present config as "not configured".
        let original = super::read_config(&path, "")?;
        if original.is_empty() {
            return Ok(UninstallOutcome::NotConfigured);
        }
        let mut doc = original
            .parse::<DocumentMut>()
            .with_context(|| format!("parsing existing TOML at {}", path.display()))?;

        // `mcp_servers` may be a standard table or an inline table depending
        // on how it was written; handle both so uninstall always matches.
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

        // Atomic write must not leave its same-dir temp file behind.
        let leftovers: Vec<_> = fs::read_dir(cfg.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("config.toml")]);

        // Second run changes nothing.
        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Unchanged);
    }

    #[test]
    fn install_into_fresh_file_renders_header_and_round_trips() {
        // Regression: an empty config used to render `mcp_servers` inline,
        // which then couldn't be removed on uninstall.
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
        // Codex is global-only; outside any onboarded project there is no
        // root to bake in, so the entry carries no `--project` default.
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
        // A scalar `mcp_servers` used to make install silently report
        // "unchanged" without adding the entry.
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
        // Invalid UTF-8: read fails, but the file exists. Must propagate the
        // error, not overwrite the user's config with a fresh one.
        let home = tempdir().unwrap();
        let cfg = home.path().join(".codex/config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(&cfg, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        assert!(t.install(&c).is_err(), "should error on unreadable file");
        // File bytes untouched.
        assert_eq!(fs::read(&cfg).unwrap(), vec![0xff, 0xfe, 0x00, 0x01]);
    }

    #[test]
    fn uninstall_removes_only_codesage() {
        let home = tempdir().unwrap();
        let t = CodexTarget;
        let c = ctx(home.path(), Path::new("/abs/proj"));
        // Nothing configured yet.
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
