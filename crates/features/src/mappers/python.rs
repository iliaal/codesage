//! Python mapper. Emits:
//! - `python-project` library seed for any project with a manifest
//!   (`pyproject.toml`/`setup.py`/`setup.cfg`/`requirements.txt`), so
//!   `find_feature("src/foo.py")` resolves to the package even without an
//!   explicit script entry.
//! - `pyproject.toml` `[project.scripts]` and `[tool.poetry.scripts]` entry
//!   points as `cli-command` seeds.
//! - `setup.py` `entry_points={'console_scripts': [...]}` as `cli-command`
//!   seeds (best-effort regex fallback).
//! - Files containing top-level `if __name__ == "__main__":` as
//!   `cli-command` seeds (test-shaped files filtered out).
//! - `python-test-suite` seeds per suite-root with pytest files, with a
//!   package-manager-aware test command (uv/poetry/pdm/hatch/bare pytest)
//!   attached per test.
//!
//! Source-group partitioning (clawpatch's `python-source-group`) is
//! intentionally NOT ported per the LLM-utility filter — browse-only rows
//! dilute `list_features` without unlocking a new agent action.

use std::collections::BTreeSet;
use std::fs;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;

use crate::mappers::shared::{is_safe_dir, is_safe_file, strip_line_comments, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile, SeedTest};

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
        let pyproject_raw = read_pyproject(ctx.root);
        let test_cmd = detect_python_test_command(ctx.root, pyproject_raw.as_deref());
        seeds.extend(python_project_seed(
            ctx,
            pyproject_raw.as_deref(),
            test_cmd.as_deref(),
        )?);
        seeds.extend(pyproject_scripts(ctx)?);
        seeds.extend(pyproject_poetry_scripts(ctx)?);
        seeds.extend(setup_py_entry_points(ctx)?);
        seeds.extend(main_guard_modules(ctx)?);
        seeds.extend(pytest_test_suites(ctx, test_cmd.as_deref())?);
        seeds.retain(|s| ctx.allowed(&s.entry_path));
        Ok(seeds)
    }
}

fn read_pyproject(root: &std::path::Path) -> Option<String> {
    let path = root.join("pyproject.toml");
    if !is_safe_file(root, &path) {
        return None;
    }
    fs::read_to_string(&path).ok()
}

/// Detect the project's test driver. Priority order: uv → poetry → pdm →
/// hatch → bare pytest, keyed off the **lockfile** only. A `[tool.X]`
/// section in `pyproject.toml` was previously also accepted but produced
/// false positives — projects that declare uv/poetry/pdm metadata in
/// pyproject for dev-dep management while still running `pytest` directly.
/// The lockfile is the higher-precision signal (the team committed to that
/// driver). When no lockfile exists, return bare `pytest` so an agent gets
/// a runnable command rather than nothing.
fn detect_python_test_command(root: &std::path::Path, pyproject: Option<&str>) -> Option<String> {
    let probe_lock = |name: &str| -> bool { is_safe_file(root, &root.join(name)) };
    if probe_lock("uv.lock") {
        return Some("uv run pytest".to_string());
    }
    if probe_lock("poetry.lock") {
        return Some("poetry run pytest".to_string());
    }
    if probe_lock("pdm.lock") {
        return Some("pdm run pytest".to_string());
    }
    // No hatch.lock convention exists; hatch is purely lockfile-less, so we
    // don't have a precise way to detect it. Fall through to bare `pytest`
    // for hatch users — still runnable in the hatch shell.
    if pyproject.is_some()
        || is_safe_file(root, &root.join("setup.py"))
        || is_safe_file(root, &root.join("setup.cfg"))
        || is_safe_file(root, &root.join("requirements.txt"))
    {
        return Some("pytest".to_string());
    }
    None
}

/// One project-level library seed for projects with any Python manifest.
/// Routes `find_feature("src/foo.py")` to the project even when no script
/// entry resolves to it — clawpatch's `python-project` shape.
fn python_project_seed(
    ctx: &MapperContext,
    pyproject: Option<&str>,
    test_cmd: Option<&str>,
) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let manifest = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
    ]
    .into_iter()
    .find(|name| is_safe_file(root, &root.join(name)));
    let Some(manifest) = manifest else {
        return Ok(Vec::new());
    };
    let project_name = pyproject_project_name(pyproject).unwrap_or_else(|| {
        root.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("python-project")
            .to_string()
    });
    let mut context_files = vec![SeedFile {
        path: manifest.to_string(),
        reason: "package manifest".to_string(),
    }];
    for extra in ["README.md", "AGENTS.md"] {
        if is_safe_file(root, &root.join(extra)) {
            context_files.push(SeedFile {
                path: extra.to_string(),
                reason: "project context".to_string(),
            });
        }
    }
    // Source-file ownership for `find_feature` routing. Without these
    // entries, `feature-for src/acme/foo.py` returned nothing — the storage
    // query is an exact `feature_files.path = ?` lookup and the only files
    // attached previously were the manifest + README / AGENTS / tsconfig.
    // Walk the project's Python source roots (cap 2_000 to keep the
    // feature_files table bounded for monolithic monorepos) and tag each
    // hit as `project source`.
    let project_files = python_project_source_files(ctx);
    // NOTE: the inferred test command is surfaced via the summary string
    // (and via `entry_command`) only. Earlier drafts populated `tests`
    // with a SeedTest entry pointing at the manifest just to attach the
    // command — but `SeedTest.path` is documented as a test FILE, and the
    // mapper orchestrator inserts every `seed.tests[]` row as a
    // `role: Test` `FeatureFileRef`. That would surface `pyproject.toml`
    // as a "test" in `feature_bundle` output, which is wrong. Leaving
    // tests empty here lets the orchestrator's `nearby_tests` discovery
    // attach real test files instead.
    let mut owned_files = vec![SeedFile {
        path: manifest.to_string(),
        reason: "package manifest".to_string(),
    }];
    for path in project_files {
        if path == manifest {
            continue;
        }
        owned_files.push(SeedFile {
            path,
            reason: "project source".to_string(),
        });
    }
    Ok(vec![FeatureSeed {
        title: format!("Python project `{project_name}`"),
        summary: match test_cmd {
            Some(cmd) => format!("Python project rooted at {manifest} (test: `{cmd}`)"),
            None => format!("Python project rooted at {manifest}"),
        },
        kind: FeatureKind::Library,
        source: "python-project",
        confidence: FeatureConfidence::Medium,
        entry_path: manifest.to_string(),
        entry_symbol: Some(project_name),
        entry_route: None,
        // entry_command stays None on library features — it's part of the
        // feature_id hash and would destabilize identity whenever the
        // project's test runner changed. The runnable test invocation
        // goes in test_command instead.
        entry_command: None,
        test_command: test_cmd.map(String::from),
        language: Language::Python,
        tags: vec!["python".to_string(), "package".to_string()],
        owned_files,
        context_files,
        tests: Vec::new(),
        test_prefixes: vec!["tests".to_string(), "test".to_string()],
    }])
}

/// Walk the project's source roots and collect `.py` files (cap 2_000 to keep
/// the routing-only `feature_files` rows bounded). Honors `.gitignore` and
/// `[index].exclude_patterns` via `walk_files`. Skips test/fixture files —
/// those land on the `python-test-suite` feature instead — and generated
/// stubs (`*_pb2.py`, `*.gen.py`).
fn python_project_source_files(ctx: &MapperContext) -> Vec<String> {
    const CAP: usize = 2_000;
    let root = ctx.root;
    let scan_dirs: Vec<&str> = ["src", "lib"]
        .into_iter()
        .filter(|d| is_safe_dir(root, &root.join(d)))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let consider = |rel: String, out: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        if !rel.ends_with(".py") {
            return false;
        }
        if is_pytest_file(&rel) {
            return false;
        }
        if rel.ends_with("_pb2.py") || rel.ends_with(".gen.py") {
            return false;
        }
        if !seen.insert(rel.clone()) {
            return false;
        }
        out.push(rel);
        out.len() >= CAP
    };
    if scan_dirs.is_empty() {
        // Project keeps its source at the repo root (no `src/` layout).
        // Walk the whole root, bounded.
        for rel in walk_files(root, root, 10_000, ctx.excludes) {
            if consider(rel, &mut out, &mut seen) {
                break;
            }
        }
    } else {
        for dir in scan_dirs {
            let abs = root.join(dir);
            let mut hit_cap = false;
            for rel in walk_files(root, &abs, 5_000, ctx.excludes) {
                if consider(rel, &mut out, &mut seen) {
                    hit_cap = true;
                    break;
                }
            }
            if hit_cap {
                break;
            }
        }
    }
    out.sort();
    out
}

fn pyproject_project_name(pyproject: Option<&str>) -> Option<String> {
    let raw = pyproject?;
    let body = strip_line_comments(raw, '#');
    let project_body = extract_section(&body, "project")?;
    let re = Regex::new(r#"(?m)^\s*name\s*=\s*"([^"]+)""#).ok()?;
    re.captures(&project_body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn pyproject_scripts(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    pyproject_scripts_section(ctx, "project.scripts", "pyproject-script", |name| {
        format!("Python script `{name}`")
    })
}

fn pyproject_poetry_scripts(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    pyproject_scripts_section(
        ctx,
        "tool.poetry.scripts",
        "pyproject-poetry-script",
        |name| format!("Python script `{name}` (poetry)"),
    )
}

/// Shared parser for `[project.scripts]` and `[tool.poetry.scripts]` (PEP 621
/// vs Poetry conventions). Both sections share `name = "module:fn"` syntax;
/// only the section header, the `source` tag, and the title format differ.
fn pyproject_scripts_section(
    ctx: &MapperContext,
    section: &str,
    source: &'static str,
    title_for: impl Fn(&str) -> String,
) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let path = root.join("pyproject.toml");
    if !is_safe_file(root, &path) {
        return Ok(out);
    }
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(out);
    };
    let body = strip_line_comments(&raw, '#');
    let Some(scripts_body) = extract_section(&body, section) else {
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
        // Target shape: `module.path:fn_name`. Resolve the dotted module to
        // a real `.py` file so `codesage feature-for <module.py>` can find
        // the script — recording `pyproject.toml` as entry_path makes the
        // file→feature contract a lie ("what feature owns acme/cli.py?"
        // would return nothing).
        let module = target.split(':').next().unwrap_or(&target).to_string();
        let resolved = resolve_script_module_path(ctx, &module);
        let entry_path = resolved
            .clone()
            .unwrap_or_else(|| "pyproject.toml".to_string());
        out.push(FeatureSeed {
            title: title_for(&name),
            summary: format!("pyproject.toml `[{section}]` entry `{name} = \"{target}\"`"),
            kind: FeatureKind::CliCommand,
            source,
            confidence: FeatureConfidence::High,
            entry_path,
            entry_symbol: Some(module),
            entry_route: None,
            entry_command: Some(name),
            test_command: None,
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
            test_command: None,
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
            test_command: None,
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

// ---- pytest test-suite mapping ----------------------------------------

/// Per-suite-root pytest seeds. Walks `tests/`, `test/`, and any source-
/// root the project declares (`src/`, top-level packages); filters to
/// `test_*.py` / `*_test.py`; skips `__fixtures__` / `fixtures` /
/// `testdata` directories; caps at 200 files per project; groups files by
/// their suite root (the top-level dir containing the test). One seed per
/// suite root keeps `list_features` clean — a project with `tests/api/`
/// and `tests/unit/` produces ONE `python-test-suite` for `tests`, not
/// two, matching how a developer thinks about pytest runs.
fn pytest_test_suites(ctx: &MapperContext, test_cmd: Option<&str>) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let scan_dirs = ["tests", "test", "src"];
    const PYTEST_FILE_CAP: usize = 200;
    let mut test_files: Vec<String> = Vec::new();
    'outer: for dir in scan_dirs {
        let abs = root.join(dir);
        if !is_safe_dir(root, &abs) {
            continue;
        }
        for rel in walk_files(root, &abs, 5_000, ctx.excludes) {
            if !is_pytest_file(&rel) {
                continue;
            }
            test_files.push(rel);
            if test_files.len() >= PYTEST_FILE_CAP {
                // The cap is global, not per-scan-dir; once 200 pytest
                // files have been collected from any combination of
                // tests/, test/, and src/, bail entirely.
                break 'outer;
            }
        }
    }
    if test_files.is_empty() {
        return Ok(Vec::new());
    }
    test_files.sort();
    test_files.dedup();

    // Bucket by suite root (top-level dir). `tests/api/test_x.py` and
    // `tests/unit/test_y.py` both go under `tests/`. A top-level
    // `test_something.py` (no dir) buckets under `""` and gets a single
    // umbrella seed.
    let mut by_root: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for file in test_files {
        let suite_root = file
            .split_once('/')
            .map(|(head, _)| head.to_string())
            .unwrap_or_default();
        by_root.entry(suite_root).or_default().push(file);
    }

    let mut out = Vec::new();
    for (suite_root, files) in by_root {
        let label = if suite_root.is_empty() {
            "tests".to_string()
        } else {
            suite_root.clone()
        };
        let entry = files.first().cloned().unwrap_or_else(|| label.clone());
        let tests: Vec<SeedTest> = files
            .iter()
            .map(|p| SeedTest {
                path: p.clone(),
                command: test_cmd.map(String::from),
            })
            .collect();
        let owned: Vec<SeedFile> = files
            .iter()
            .filter(|p| **p != entry)
            .map(|p| SeedFile {
                path: p.clone(),
                reason: "pytest test file".to_string(),
            })
            .collect();
        let summary = match test_cmd {
            Some(cmd) => format!(
                "Pytest suite at `{label}` ({} files, run with `{cmd}`)",
                files.len()
            ),
            None => format!("Pytest suite at `{label}` ({} files)", files.len()),
        };
        out.push(FeatureSeed {
            title: format!("Python tests `{label}`"),
            summary,
            kind: FeatureKind::TestSuite,
            source: "python-test-suite",
            confidence: FeatureConfidence::High,
            entry_path: entry,
            entry_symbol: Some(label.clone()),
            entry_route: None,
            // Test-suite features identify by the suite label (`tests` /
            // `test` / source-root name). The runnable command goes in
            // test_command, keeping entry_command free for the argv[0]-
            // shape contract (this seed has no such command).
            entry_command: None,
            test_command: test_cmd.map(String::from),
            language: Language::Python,
            tags: vec!["python".to_string(), "tests".to_string()],
            owned_files: owned,
            context_files: Vec::new(),
            tests,
            test_prefixes: if suite_root.is_empty() {
                Vec::new()
            } else {
                vec![suite_root]
            },
        });
    }
    Ok(out)
}

/// Match `test_*.py` and `*_test.py`. Excludes fixture/testdata paths and
/// generated python (`*_pb2.py`, `*.gen.py`) — even when they live under a
/// test root, they're not the test files an agent runs.
fn is_pytest_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    if !lower.ends_with(".py") {
        return false;
    }
    if lower.ends_with("_pb2.py") || lower.ends_with(".gen.py") {
        return false;
    }
    for segment in rel.split('/') {
        if matches!(segment, "__fixtures__" | "fixtures" | "testdata") {
            return false;
        }
    }
    let basename = rel.rsplit_once('/').map(|(_, tail)| tail).unwrap_or(rel);
    basename.starts_with("test_") || basename.ends_with("_test.py")
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

    // ---- Python-project + pytest + test-command -------------------------

    #[test]
    fn python_project_seed_emitted_with_pyproject() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[project]
name = "acme"
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-project")
            .expect("python-project seed");
        assert_eq!(s.entry_symbol.as_deref(), Some("acme"));
        assert_eq!(s.entry_path, "pyproject.toml");
        assert_eq!(s.kind, FeatureKind::Library);
    }

    #[test]
    fn python_project_seed_falls_back_to_setup_py() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "setup.py",
            "from setuptools import setup\nsetup(name='legacy')\n",
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-project")
            .expect("python-project seed");
        assert_eq!(s.entry_path, "setup.py");
    }

    #[test]
    fn no_python_project_seed_without_any_manifest() {
        let dir = tempdir().unwrap();
        write(dir.path(), "random.py", "# nothing\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(!seeds.iter().any(|s| s.source == "python-project"));
    }

    #[test]
    fn poetry_scripts_section_extracted() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            r#"[tool.poetry]
name = "acme"

[tool.poetry.scripts]
acme = "acme.cli:main"
"#,
        );
        write(dir.path(), "acme/cli.py", "def main(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "pyproject-poetry-script")
            .expect("poetry script seed");
        assert_eq!(s.entry_command.as_deref(), Some("acme"));
        assert_eq!(s.entry_path, "acme/cli.py");
    }

    #[test]
    fn pytest_test_suite_grouped_by_root() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(
            dir.path(),
            "tests/api/test_users.py",
            "def test_x(): pass\n",
        );
        write(
            dir.path(),
            "tests/unit/test_helpers.py",
            "def test_y(): pass\n",
        );
        write(
            dir.path(),
            "tests/integration/something_test.py",
            "def test_z(): pass\n",
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let suite_seeds: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "python-test-suite")
            .collect();
        // ONE seed per top-level suite root (`tests`), not three.
        assert_eq!(
            suite_seeds.len(),
            1,
            "expected 1 test-suite seed (per suite root), got {}",
            suite_seeds.len()
        );
        let s = suite_seeds[0];
        assert_eq!(s.entry_symbol.as_deref(), Some("tests"));
        assert_eq!(s.kind, FeatureKind::TestSuite);
        // All three test files attach to the seed.
        let test_paths: BTreeSet<&str> = s.tests.iter().map(|t| t.path.as_str()).collect();
        assert!(test_paths.contains("tests/api/test_users.py"));
        assert!(test_paths.contains("tests/unit/test_helpers.py"));
        assert!(test_paths.contains("tests/integration/something_test.py"));
    }

    #[test]
    fn pytest_test_suite_uses_uv_command_when_uv_lock_present() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "uv.lock", "version = 1\n");
        write(dir.path(), "tests/test_x.py", "def test_x(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-test-suite")
            .expect("test-suite seed");
        let cmd = s.tests.first().and_then(|t| t.command.as_deref());
        assert_eq!(cmd, Some("uv run pytest"));
    }

    #[test]
    fn pytest_test_suite_uses_poetry_command_when_poetry_lock_present() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "poetry.lock", "# poetry\n");
        write(dir.path(), "tests/test_x.py", "def test_x(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-test-suite")
            .expect("test-suite seed");
        let cmd = s.tests.first().and_then(|t| t.command.as_deref());
        assert_eq!(cmd, Some("poetry run pytest"));
    }

    #[test]
    fn pytest_test_suite_bare_pytest_when_no_pm_detected() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "tests/test_x.py", "def test_x(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-test-suite")
            .expect("test-suite seed");
        let cmd = s.tests.first().and_then(|t| t.command.as_deref());
        assert_eq!(cmd, Some("pytest"));
    }

    #[test]
    fn pytest_skips_fixtures_and_generated() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "tests/test_real.py", "def test_real(): pass\n");
        write(
            dir.path(),
            "tests/__fixtures__/test_fixture.py",
            "# fixture, not a runnable test\n",
        );
        write(dir.path(), "tests/fixtures/data.py", "# fixture\n");
        write(
            dir.path(),
            "tests/test_messages_pb2.py",
            "# generated protobuf stub\n",
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-test-suite")
            .expect("test-suite seed");
        let paths: BTreeSet<&str> = s.tests.iter().map(|t| t.path.as_str()).collect();
        assert!(paths.contains("tests/test_real.py"));
        assert!(
            !paths.iter().any(|p| p.contains("__fixtures__")),
            "fixtures leaked: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains("/fixtures/")),
            "fixtures/ leaked: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with("_pb2.py")),
            "generated pb2 leaked: {paths:?}"
        );
    }

    #[test]
    fn no_test_suite_seed_when_no_tests_exist() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "src/acme/__init__.py", "");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(!seeds.iter().any(|s| s.source == "python-test-suite"));
    }

    #[test]
    fn project_seed_summary_includes_test_command() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "poetry.lock", "# poetry\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-project")
            .expect("python-project seed");
        assert!(
            s.summary.contains("poetry run pytest"),
            "summary should include the inferred test command: {}",
            s.summary
        );
    }

    #[test]
    fn python_project_seed_owns_source_files_for_routing() {
        // Regression: `find_feature("src/acme/foo.py")` had no chance
        // because the project seed only persisted the manifest. Walking the
        // source root and attaching the .py files lets the storage exact-
        // match query (`feature_files.path = ?1`) resolve any project file.
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"acme\"\n");
        write(dir.path(), "src/acme/__init__.py", "");
        write(dir.path(), "src/acme/foo.py", "def foo(): pass\n");
        write(dir.path(), "src/acme/bar.py", "def bar(): pass\n");
        // Test files belong on the python-test-suite seed, not the project
        // seed; verify they don't bleed in.
        write(dir.path(), "tests/test_foo.py", "def test_foo(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "python-project")
            .expect("python-project seed");
        let owned_paths: std::collections::BTreeSet<&str> =
            s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            owned_paths.contains("src/acme/foo.py"),
            "src/acme/foo.py must be in owned_files for routing, got {owned_paths:?}"
        );
        assert!(owned_paths.contains("src/acme/bar.py"));
        assert!(owned_paths.contains("pyproject.toml"));
        assert!(
            !owned_paths.contains("tests/test_foo.py"),
            "test files must NOT be on the project seed: got {owned_paths:?}"
        );
    }
}
