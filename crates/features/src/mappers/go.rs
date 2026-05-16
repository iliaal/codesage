//! Go mapper: `cmd/<name>/main.go` binaries and the root `main.go` (when
//! the repo declares `package main`). Library packages aren't emitted in
//! v1 — `go list ./...` requires a Go toolchain at map time, which we
//! intentionally avoid to keep the mapper hermetic.

use std::fs;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};

use crate::mappers::shared::{is_safe_dir, is_safe_file, rel_path};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext};

pub struct GoMapper;

impl FeatureMapper for GoMapper {
    fn name(&self) -> &'static str {
        "go"
    }
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let root = ctx.root;
        let mut seeds = Vec::new();
        // cmd/<name>/main.go
        let cmd_dir = root.join("cmd");
        if is_safe_dir(root, &cmd_dir) {
            for entry in fs::read_dir(&cmd_dir)?.flatten() {
                let p = entry.path();
                if !is_safe_dir(root, &p) {
                    continue;
                }
                let main_go = p.join("main.go");
                if !is_safe_file(root, &main_go) {
                    continue;
                }
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("cmd")
                    .to_string();
                let entry_rel = rel_path(root, &main_go);
                seeds.push(FeatureSeed {
                    title: format!("Go binary `{name}`"),
                    summary: format!("cmd/{name}/main.go"),
                    kind: FeatureKind::CliCommand,
                    source: "go-cmd",
                    confidence: FeatureConfidence::High,
                    entry_path: entry_rel,
                    entry_symbol: Some("main".to_string()),
                    entry_route: None,
                    entry_command: Some(name),
                    language: Language::Go,
                    tags: vec!["go".to_string(), "cli".to_string()],
                    owned_files: Vec::new(),
                    context_files: Vec::new(),
                    tests: Vec::new(),
                    test_prefixes: vec![rel_path(root, &p)],
                });
            }
        }
        // Repo-root main.go (a single-binary repo).
        let root_main = root.join("main.go");
        if is_safe_file(root, &root_main) {
            let raw = fs::read_to_string(&root_main).unwrap_or_default();
            if raw.contains("package main") {
                let name = root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("app")
                    .to_string();
                seeds.push(FeatureSeed {
                    title: format!("Go binary `{name}` (root)"),
                    summary: "main.go at repo root".to_string(),
                    kind: FeatureKind::CliCommand,
                    source: "go-root-main",
                    confidence: FeatureConfidence::High,
                    entry_path: "main.go".to_string(),
                    entry_symbol: Some("main".to_string()),
                    entry_route: None,
                    entry_command: Some(name),
                    language: Language::Go,
                    tags: vec!["go".to_string(), "cli".to_string()],
                    owned_files: Vec::new(),
                    context_files: Vec::new(),
                    tests: Vec::new(),
                    test_prefixes: Vec::new(),
                });
            }
        }
        seeds.retain(|s| !ctx.excluded(&s.entry_path));
        Ok(seeds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn cmd_subdir_main_yields_seed() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "cmd/server/main.go",
            "package main\nfunc main() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "go-cmd")
            .expect("go-cmd seed");
        assert_eq!(s.entry_command.as_deref(), Some("server"));
    }

    #[test]
    fn root_main_go_only_when_package_main() {
        let dir = tempdir().unwrap();
        write(dir.path(), "main.go", "package main\nfunc main() {}\n");
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(seeds.iter().any(|s| s.source == "go-root-main"));
    }

    #[test]
    fn root_main_go_skipped_when_not_package_main() {
        let dir = tempdir().unwrap();
        write(dir.path(), "main.go", "package other\n");
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(!seeds.iter().any(|s| s.source == "go-root-main"));
    }
}
