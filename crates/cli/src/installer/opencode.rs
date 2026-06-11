//! opencode target: `mcp.codesage` in `opencode.jsonc` (global
//! `$XDG_CONFIG_HOME/opencode/` or `~/.config/opencode/`, or project-local
//! `./opencode.jsonc`). Edited through `jsonc-parser`'s CST so user comments
//! and formatting survive a re-run. An existing `opencode.json` (no `c`) is
//! preferred when present; otherwise `.jsonc` is created.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstRootNode;
use jsonc_parser::json;

use super::{AgentTarget, InstallCtx, InstallOutcome, UninstallOutcome};

pub struct OpencodeTarget;

impl OpencodeTarget {
    fn dir(&self, ctx: &InstallCtx) -> PathBuf {
        if ctx.global {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.home.join(".config"));
            base.join("opencode")
        } else {
            ctx.project.to_path_buf()
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

        let project = ctx.project_utf8;
        let entry = json!({
            "type": "local",
            "command": ["codesage", "mcp", "--project", project],
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, new_text).with_context(|| format!("writing {}", path.display()))?;
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
        fs::write(&path, root.to_string())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(UninstallOutcome::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    // Project-local mode keeps tests hermetic (no XDG/HOME env reads).
    fn ctx<'a>(project: &'a Path) -> InstallCtx<'a> {
        InstallCtx {
            home: Path::new("/unused"),
            project,
            project_utf8: project.to_str().expect("test project path must be UTF-8"),
            global: false,
        }
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
