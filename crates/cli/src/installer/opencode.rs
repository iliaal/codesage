//! opencode target: `mcp.codesage` in `opencode.jsonc` (global
//! `$XDG_CONFIG_HOME/opencode/` or `~/.config/opencode/`, or project-local
//! `./opencode.jsonc`). Edited through `jsonc-parser`'s CST so user comments
//! and formatting survive a re-run. An existing `opencode.json` (no `c`) is
//! preferred when present; otherwise `.jsonc` is created.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstRootNode;
use jsonc_parser::json;

use super::{AgentTarget, InstallCtx, InstallOutcome, UninstallOutcome};

pub struct OpencodeTarget;

impl OpencodeTarget {
    fn dir(&self, ctx: &InstallCtx) -> PathBuf {
        if ctx.global {
            global_config_dir(ctx.home)
        } else {
            // `cmd_install`/`cmd_uninstall` resolve the project before any
            // project-local target runs, so this is always `Some` here.
            ctx.project
                .expect("project-local opencode install requires a project root")
                .to_path_buf()
        }
    }

    fn path(&self, ctx: &InstallCtx) -> PathBuf {
        let dir = self.dir(ctx);
        let jsonc = dir.join("opencode.jsonc");
        let json = dir.join("opencode.json");
        // Prefer an existing plain `.json`; otherwise default to `.jsonc`.
        if !jsonc.exists() && json.exists() {
            json
        } else {
            jsonc
        }
    }
}

/// Global opencode config dir, with the env read split out so the mapping
/// itself is unit-testable without mutating process env.
fn global_config_dir(home: &Path) -> PathBuf {
    global_config_dir_for(home, std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
}

fn global_config_dir_for(home: &Path, xdg: Option<PathBuf>) -> PathBuf {
    let base = xdg.unwrap_or_else(|| home.join(".config"));
    base.join("opencode")
}

/// The `mcp.codesage` entry's command array: this binary plus `mcp`, with
/// `--project <abs>` only when the registration is project-bound. Split
/// out so both shapes are unit-testable without touching the filesystem.
fn codesage_command(project_utf8: Option<&str>) -> Result<Vec<String>> {
    let (command, args) = super::mcp_command_args(project_utf8)?;
    Ok(std::iter::once(command).chain(args).collect())
}

impl AgentTarget for OpencodeTarget {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "opencode"
    }

    fn config_path(&self, ctx: &InstallCtx) -> PathBuf {
        self.path(ctx)
    }

    fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome> {
        let path = self.path(ctx);
        let original = super::read_config(&path, "{}\n")?;
        let root = CstRootNode::parse(&original, &ParseOptions::default())
            .map_err(|e| anyhow!("parsing JSONC at {}: {e}", path.display()))?;

        let root_obj = root.object_value_or_set();
        let mcp_obj = root_obj.object_value_or_set("mcp");

        let full = codesage_command(ctx.project_utf8)?;
        let entry = json!({
            "type": "local",
            "command": full,
            "enabled": true,
        });
        if let Some(prop) = mcp_obj.get("codesage") {
            prop.set_value(entry);
        } else {
            mcp_obj.append("codesage", entry);
        }

        let new_text = root.to_string();
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
        if !path.exists() {
            return Ok(UninstallOutcome::NotConfigured);
        }
        let original = super::read_config(&path, "{}\n")?;
        let root = CstRootNode::parse(&original, &ParseOptions::default())
            .map_err(|e| anyhow!("parsing JSONC at {}: {e}", path.display()))?;

        // Navigate read-only: never create `mcp` as a side effect of removal.
        let removed = root
            .object_value()
            .and_then(|o| o.get("mcp"))
            .and_then(|p| p.object_value())
            .and_then(|m| m.get("codesage"))
            .map(|prop| {
                prop.remove();
                true
            })
            .unwrap_or(false);
        if !removed {
            return Ok(UninstallOutcome::NotConfigured);
        }
        super::atomic_write(&path, &root.to_string())?;
        Ok(UninstallOutcome::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    // Project-local mode keeps tests hermetic (no XDG/HOME env reads).
    fn ctx<'a>(project: &'a Path) -> InstallCtx<'a> {
        InstallCtx {
            home: Path::new("/unused"),
            project: Some(project),
            project_utf8: Some(project.to_str().expect("test project path must be UTF-8")),
            global: false,
        }
    }

    #[test]
    fn global_config_dir_prefers_xdg_over_dot_config() {
        let home = Path::new("/home/u");
        assert_eq!(
            global_config_dir_for(home, Some(PathBuf::from("/xdg"))),
            PathBuf::from("/xdg/opencode")
        );
        assert_eq!(
            global_config_dir_for(home, None),
            PathBuf::from("/home/u/.config/opencode")
        );
    }

    #[test]
    fn codesage_command_bakes_project_only_when_bound() {
        // A global registration made outside any onboarded project carries
        // no `--project` default; the server resolves the project per call.
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();
        assert_eq!(
            codesage_command(Some("/abs/proj")).unwrap(),
            vec![exe, "mcp", "--project", "/abs/proj"]
        );
        assert_eq!(codesage_command(None).unwrap(), vec![exe, "mcp"]);
    }

    #[test]
    fn install_creates_file_and_is_idempotent() {
        let proj = tempdir().unwrap();
        let t = OpencodeTarget;
        let c = ctx(proj.path());

        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Wrote);
        let cfg = proj.path().join("opencode.jsonc");
        let written = fs::read_to_string(&cfg).unwrap();
        assert!(written.contains("\"codesage\""));
        assert!(written.contains("--project"));
        assert!(written.contains(proj.path().to_string_lossy().as_ref()));

        // Atomic write must not leave its same-dir temp file behind.
        let leftovers: Vec<_> = fs::read_dir(proj.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("opencode.jsonc")]);

        assert_eq!(t.install(&c).unwrap(), InstallOutcome::Unchanged);
    }

    #[test]
    fn install_preserves_comments_and_other_servers() {
        let proj = tempdir().unwrap();
        let cfg = proj.path().join("opencode.jsonc");
        fs::write(
            &cfg,
            "{\n  // keep me\n  \"mcp\": {\n    \"other\": { \"type\": \"local\", \"command\": [\"foo\"] }\n  }\n}\n",
        )
        .unwrap();

        let t = OpencodeTarget;
        assert_eq!(t.install(&ctx(proj.path())).unwrap(), InstallOutcome::Wrote);
        let written = fs::read_to_string(&cfg).unwrap();
        assert!(written.contains("// keep me"), "comment lost: {written}");
        assert!(
            written.contains("\"other\""),
            "other server lost: {written}"
        );
        assert!(written.contains("\"codesage\""));
    }

    #[test]
    fn install_errors_on_unreadable_file_without_clobbering() {
        // Invalid UTF-8: read fails but the file exists. Must propagate, not
        // replace the user's config with a fresh `{}`.
        let proj = tempdir().unwrap();
        let cfg = proj.path().join("opencode.jsonc");
        fs::write(&cfg, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        let t = OpencodeTarget;
        assert!(t.install(&ctx(proj.path())).is_err());
        assert_eq!(fs::read(&cfg).unwrap(), vec![0xff, 0xfe, 0x00, 0x01]);
    }

    #[test]
    fn uninstall_removes_only_codesage_without_creating_mcp() {
        let proj = tempdir().unwrap();
        let t = OpencodeTarget;
        let c = ctx(proj.path());

        // No file yet → NotConfigured, no file created.
        assert_eq!(t.uninstall(&c).unwrap(), UninstallOutcome::NotConfigured);
        assert!(!proj.path().join("opencode.jsonc").exists());

        // File without mcp → NotConfigured, mcp not synthesized.
        let cfg = proj.path().join("opencode.jsonc");
        fs::write(&cfg, "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        assert_eq!(t.uninstall(&c).unwrap(), UninstallOutcome::NotConfigured);
        assert!(!fs::read_to_string(&cfg).unwrap().contains("mcp"));

        // After install, uninstall removes only codesage.
        t.install(&c).unwrap();
        assert_eq!(t.uninstall(&c).unwrap(), UninstallOutcome::Removed);
        let after = fs::read_to_string(&cfg).unwrap();
        assert!(!after.contains("codesage"), "codesage remained: {after}");
        assert!(after.contains("\"theme\""), "theme lost: {after}");
    }
}
