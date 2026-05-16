//! Python mapper: `pyproject.toml` `[project.scripts]` entry points,
//! `setup.py` `entry_points` (best-effort regex fallback), and top-level
//! files containing `if __name__ == "__main__":`.

use std::fs;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;

use crate::mappers::shared::{is_safe_file, strip_line_comments, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};

/// Extract the body of a `[name]` section from a TOML-like document
/// (returns until the next `[...]` header or EOF). Avoids look-around
/// regex which the Rust `regex` crate doesn't support.
fn extract_section(body: &str, section: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut lines = body.lines();
    let mut in_section = false;
    let mut out = String::new();
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if !in_section {
            if trimmed == header {
                in_section = true;
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if in_section { Some(out) } else { None }
}

pub struct PythonMapper;

impl FeatureMapper for PythonMapper {
    fn name(&self) -> &'static str {
        "python"
    }
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let mut seeds: Vec<FeatureSeed> = Vec::new();
        seeds.extend(pyproject_scripts(ctx)?);
        seeds.extend(setup_py_entry_points(ctx)?);
        seeds.extend(main_guard_modules(ctx)?);
        seeds.retain(|s| ctx.allowed(&s.entry_path));
        Ok(seeds)
    }
}

fn pyproject_scripts(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let path = root.join("pyproject.toml");
    if !is_safe_file(root, &path) {
        return Ok(out);
    }
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(out),
    };
    let body = strip_line_comments(&raw, '#');
    let Some(scripts_body) = extract_section(&body, "project.scripts") else {
        return Ok(out);
    };
    let line_re = Regex::new(r#"(?m)^\s*([A-Za-z_][\w\-]*)\s*=\s*"([^"]+)""#)?;
    for cap in line_re.captures_iter(&scripts_body) {
        let name = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let target = cap
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        // Target shape: `module.path:fn_name`. Resolve the dotted module
        // to a real `.py` file so `codesage feature-for <module.py>` can
        // find the script — recording `pyproject.toml` as entry_path
        // makes the file-→-feature contract a lie (the user's question
        // "what feature owns acme/cli.py?" returns nothing).
        let module = target.split(':').next().unwrap_or(&target).to_string();
        let resolved = resolve_script_module_path(ctx, &module);
        let entry_path = resolved
            .clone()
            .unwrap_or_else(|| "pyproject.toml".to_string());
        out.push(FeatureSeed {
            title: format!("Python script `{name}`"),
            summary: format!("pyproject.toml `[project.scripts]` entry `{name} = \"{target}\"`"),
            kind: FeatureKind::CliCommand,
            source: "pyproject-script",
            confidence: FeatureConfidence::High,
            entry_path,
            entry_symbol: Some(module),
            entry_route: None,
            entry_command: Some(name),
            language: Language::Python,
            tags: vec!["python".to_string(), "cli".to_string()],
            owned_files: Vec::new(),
            context_files: vec![SeedFile {
                path: "pyproject.toml".to_string(),
                reason: "package manifest".to_string(),
            }],
            tests: Vec::new(),
            test_prefixes: vec!["tests".to_string()],
        });
    }
    Ok(out)
}

fn setup_py_entry_points(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let path = root.join("setup.py");
    if !is_safe_file(root, &path) {
        return Ok(out);
    }
    let raw = fs::read_to_string(&path).unwrap_or_default();
    let console_re = Regex::new(r#"(?ms)['"]console_scripts['"]\s*:\s*\[([^\]]*)\]"#)?;
    let Some(cap) = console_re.captures(&raw) else {
        return Ok(out);
    };
    let body = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
    let entry_re = Regex::new(r#"['"]([\w\-]+)\s*=\s*([\w\.\:]+)['"]"#)?;
    for cap in entry_re.captures_iter(body) {
        let name = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let target = cap
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let module = target.split(':').next().unwrap_or(&target).to_string();
        let resolved = resolve_script_module_path(ctx, &module);
        let entry_path = resolved.clone().unwrap_or_else(|| "setup.py".to_string());
        out.push(FeatureSeed {
            title: format!("Python script `{name}` (setup.py)"),
            summary: format!("setup.py console_scripts entry `{name}={target}`"),
            kind: FeatureKind::CliCommand,
            source: "setup-py-script",
            confidence: FeatureConfidence::High,
            entry_path,
            entry_symbol: Some(module),
            entry_route: None,
            entry_command: Some(name),
            language: Language::Python,
            tags: vec!["python".to_string(), "cli".to_string()],
            owned_files: Vec::new(),
            context_files: vec![SeedFile {
                path: "setup.py".to_string(),
                reason: "package manifest".to_string(),
            }],
            tests: Vec::new(),
            test_prefixes: vec!["tests".to_string()],
        });
    }
    Ok(out)
}

/// Convert a dotted module path (`acme.cli`) into a repo-relative file
/// path (`acme/cli.py` or `src/acme/cli.py` or `acme/__init__.py`).
/// Falls back to `None` when no candidate resolves to an existing file
/// or every candidate is filtered out by `[index].exclude_patterns` —
/// callers should then record the manifest as `entry_path` so the seed
/// still emits.
fn resolve_script_module_path(ctx: &MapperContext, module: &str) -> Option<String> {
    let root = ctx.root;
    let parts: Vec<&str> = module.split('.').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("/");
    // Probe `<module>.py`, `<module>/__init__.py`, and the same shapes
    // under `src/`. Order matters: a top-level `<module>.py` beats a
    // namespace-package `<module>/__init__.py`, because real projects
    // most commonly have the former.
    let candidates = [
        format!("{joined}.py"),
        format!("{joined}/__init__.py"),
        format!("src/{joined}.py"),
        format!("src/{joined}/__init__.py"),
    ];
    for rel in candidates {
        let abs = root.join(&rel);
        if !is_safe_file(root, &abs) {
            continue;
        }
        if !ctx.allowed(&rel) {
            continue;
        }
        return Some(rel);
    }
    None
}

fn main_guard_modules(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    use codesage_protocol::FileCategory;
    let root = ctx.root;
    let mut out = Vec::new();
    let guard_re = Regex::new(r#"(?m)^if\s+__name__\s*==\s*['"]__main__['"]"#)?;
    let files = walk_files(root, root, 30_000, ctx.excludes);
    for rel in files.iter().filter(|p| p.ends_with(".py")) {
        // Test files with `if __name__ == "__main__":` are ad-hoc test
        // runners, not CLI commands. They're often excluded by the
        // project's `[index].exclude_patterns`, so emitting features
        // for them produces rows whose entry_path isn't in the `files`
        // table.
        if matches!(FileCategory::classify(rel), FileCategory::Test) {
            continue;
        }
        let abs = root.join(rel);
        let Ok(raw) = fs::read_to_string(&abs) else {
            continue;
        };
        if !guard_re.is_match(&raw) {
            continue;
        }
        let stem = rel
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".py"))
            .unwrap_or("script");
        out.push(FeatureSeed {
            title: format!("Python module `{stem}` (__main__)"),
            summary: format!("Top-level `if __name__ == \"__main__\":` at {rel}"),
            kind: FeatureKind::CliCommand,
            source: "python-main-guard",
            confidence: FeatureConfidence::Medium,
            entry_path: rel.clone(),
            entry_symbol: Some("__main__".to_string()),
            entry_route: None,
            entry_command: Some(stem.to_string()),
            language: Language::Python,
            tags: vec!["python".to_string(), "cli".to_string()],
            owned_files: Vec::new(),
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: Vec::new(),
        });
    }
    Ok(out)
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
    fn pyproject_scripts_emits_cli_seed() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[project]
name = "acme"

[project.scripts]
acme = "acme.cli:main"
"#,
        );
        // No module file on disk → entry falls back to the manifest.
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "pyproject-script")
            .expect("script seed");
        assert_eq!(s.entry_command.as_deref(), Some("acme"));
        assert_eq!(s.entry_symbol.as_deref(), Some("acme.cli"));
        assert_eq!(s.entry_path, "pyproject.toml");
    }

    #[test]
    fn pyproject_script_entry_resolves_to_module_file() {
        // The advertised contract: `codesage feature-for acme/cli.py`
        // must find this feature. That requires entry_path to be the
        // module file, not the manifest.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[project]
name = "acme"

[project.scripts]
acme = "acme.cli:main"
"#,
        );
        write(dir.path(), "acme/cli.py", "def main(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "pyproject-script")
            .expect("script seed");
        assert_eq!(s.entry_path, "acme/cli.py");
        assert_eq!(s.entry_symbol.as_deref(), Some("acme.cli"));
    }

    #[test]
    fn pyproject_script_entry_resolves_src_layout() {
        // `src/<pkg>/<mod>.py` is the other common Python layout.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[project]
name = "acme"

[project.scripts]
acme = "acme.cli:main"
"#,
        );
        write(dir.path(), "src/acme/cli.py", "def main(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "pyproject-script")
            .expect("script seed");
        assert_eq!(s.entry_path, "src/acme/cli.py");
    }

    #[test]
    fn pyproject_script_entry_resolves_package_init() {
        // Namespace/package script: target `acme:main` → `acme/__init__.py`.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[project]
name = "acme"

[project.scripts]
acme = "acme:main"
"#,
        );
        write(dir.path(), "acme/__init__.py", "def main(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "pyproject-script")
            .expect("script seed");
        assert_eq!(s.entry_path, "acme/__init__.py");
    }

    #[test]
    fn pyproject_script_entry_falls_back_when_excluded() {
        // If the resolved module file is excluded by [index].exclude_patterns,
        // we can't ship a feature pointing at a file the rest of the
        // pipeline ignores. Fall back to the manifest so the seed at
        // least exists with a valid entry_path.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[project]
name = "acme"

[project.scripts]
acme = "acme.cli:main"
"#,
        );
        write(dir.path(), "acme/cli.py", "def main(): pass\n");
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new("acme/**").unwrap());
        let excludes = builder.build().unwrap();
        let ctx = MapperContext {
            root: dir.path(),
            excludes: Some(&excludes),
        };
        let seeds = PythonMapper.map(&ctx).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "pyproject-script")
            .expect("script seed");
        assert_eq!(s.entry_path, "pyproject.toml");
    }

    #[test]
    fn main_guard_modules_detected() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "run.py",
            "import sys\nif __name__ == '__main__':\n    sys.exit(0)\n",
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(seeds.iter().any(|s| s.source == "python-main-guard"));
    }

    #[test]
    fn main_guard_skips_test_files() {
        // `test_*.py` / `*_test.py` / files under `tests/` with a
        // `__main__` guard must not produce cli-command features.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "test_foo.py",
            "if __name__ == '__main__':\n    pass\n",
        );
        write(
            dir.path(),
            "foo_test.py",
            "if __name__ == '__main__':\n    pass\n",
        );
        write(
            dir.path(),
            "tests/something.py",
            "if __name__ == '__main__':\n    pass\n",
        );
        write(
            dir.path(),
            "real_cli.py",
            "if __name__ == '__main__':\n    pass\n",
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let entries: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "python-main-guard")
            .map(|s| s.entry_path.as_str())
            .collect();
        assert_eq!(
            entries,
            vec!["real_cli.py"],
            "expected only the non-test main-guard, got {:?}",
            entries
        );
    }

    #[test]
    fn setup_py_console_scripts_extracted() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "setup.py",
            "from setuptools import setup\nsetup(\n  name='acme',\n  entry_points={\n    'console_scripts': [\n        'acme=acme.cli:main',\n    ],\n  }\n)\n",
        );
        write(dir.path(), "acme/cli.py", "def main(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "setup-py-script")
            .expect("setup-py seed");
        assert_eq!(s.entry_command.as_deref(), Some("acme"));
        assert_eq!(s.entry_path, "acme/cli.py");
    }
}
