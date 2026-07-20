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
use std::path::PathBuf;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;

use crate::mappers::shared::{
    AUTH_SENSITIVE_TAG, SOURCE_FILE_CAP, collect_source_files, is_safe_dir, is_safe_file,
    read_to_string_bounded, route_is_auth_sensitive, strip_line_comments, walk_files,
};
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
        seeds.extend(setup_cfg_entry_points(ctx)?);
        // Walk the tree once and read each `.py` once (bounded); the four
        // source scanners below share this set instead of each re-walking
        // and re-reading every file with an unbounded `read_to_string`.
        let py_files = collect_python_source_files(ctx);
        seeds.extend(main_guard_modules(&py_files)?);
        seeds.extend(flask_routes(&py_files)?);
        seeds.extend(fastapi_routes(&py_files)?);
        seeds.extend(django_routes(&py_files)?);
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
    read_to_string_bounded(&path).ok().flatten()
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
        summary: match test_cmd {
            Some(cmd) => format!("Python project rooted at {manifest} (test: `{cmd}`)"),
            None => format!("Python project rooted at {manifest}"),
        },
        source: "python-project",
        entry_symbol: Some(project_name.clone()),
        // entry_command stays None on library features — it's part of the
        // feature_id hash and would destabilize identity whenever the
        // project's test runner changed. The runnable test invocation
        // goes in test_command instead.
        test_command: test_cmd.map(String::from),
        tags: vec!["python".to_string(), "package".to_string()],
        owned_files,
        context_files,
        test_prefixes: vec!["tests".to_string(), "test".to_string()],
        ..FeatureSeed::new(
            FeatureKind::Library,
            Language::Python,
            format!("Python project `{project_name}`"),
            manifest,
        )
    }])
}

/// Walk the project's source roots and collect `.py` files (cap 2_000 to keep
/// the routing-only `feature_files` rows bounded). Honors `.gitignore` and
/// `[index].exclude_patterns` via `walk_files`. Skips test/fixture files —
/// those land on the `python-test-suite` feature instead — and generated
/// stubs (`*_pb2.py`, `*.gen.py`).
fn python_project_source_files(ctx: &MapperContext) -> Vec<String> {
    let root = ctx.root;
    let scan_dirs: Vec<PathBuf> = ["src", "lib"]
        .into_iter()
        .map(|d| root.join(d))
        .filter(|d| is_safe_dir(root, d))
        .collect();
    // No `src/` layout: the project keeps its source at the repo root.
    // Walk the whole root, bounded a bit wider than the per-dir case.
    let (scan_dirs, walk_cap) = if scan_dirs.is_empty() {
        (vec![root.to_path_buf()], 10_000)
    } else {
        (scan_dirs, 5_000)
    };
    collect_source_files(
        ctx,
        &scan_dirs,
        walk_cap,
        |rel| rel.ends_with(".py"),
        |rel| {
            // Broad canonical shape for this exclusion scan: anything under
            // a tests/ dir (conftest.py, helpers, fixtures) belongs to the
            // python-test-suite slice, not the project's owned sources.
            crate::nearby_tests::is_test_file(rel, Language::Python)
                || rel.ends_with("_pb2.py")
                || rel.ends_with(".gen.py")
        },
        SOURCE_FILE_CAP,
    )
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
    let Ok(Some(raw)) = read_to_string_bounded(&path) else {
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
            summary: format!("pyproject.toml `[{section}]` entry `{name} = \"{target}\"`"),
            source,
            confidence: FeatureConfidence::High,
            entry_symbol: Some(module),
            entry_command: Some(name.clone()),
            tags: vec!["python".to_string(), "cli".to_string()],
            context_files: vec![SeedFile {
                path: "pyproject.toml".to_string(),
                reason: "package manifest".to_string(),
            }],
            test_prefixes: vec!["tests".to_string()],
            ..FeatureSeed::new(
                FeatureKind::CliCommand,
                Language::Python,
                title_for(&name),
                entry_path,
            )
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
    let raw = read_to_string_bounded(&path)
        .ok()
        .flatten()
        .unwrap_or_default();
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
            summary: format!("setup.py console_scripts entry `{name}={target}`"),
            source: "setup-py-script",
            confidence: FeatureConfidence::High,
            entry_symbol: Some(module),
            entry_command: Some(name.clone()),
            tags: vec!["python".to_string(), "cli".to_string()],
            context_files: vec![SeedFile {
                path: "setup.py".to_string(),
                reason: "package manifest".to_string(),
            }],
            test_prefixes: vec!["tests".to_string()],
            ..FeatureSeed::new(
                FeatureKind::CliCommand,
                Language::Python,
                format!("Python script `{name}` (setup.py)"),
                entry_path,
            )
        });
    }
    Ok(out)
}

/// `setup.cfg` `[options.entry_points]` `console_scripts = …` entries.
/// Sibling to [`setup_py_entry_points`]; setup.cfg is INI-style and
/// stores console_scripts as a multi-line `name = module:fn` block under
/// a single key. Ported from clawpatch PR #28's expansion of Python
/// packaging support beyond `pyproject.toml` / `setup.py`.
fn setup_cfg_entry_points(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let path = root.join("setup.cfg");
    if !is_safe_file(root, &path) {
        return Ok(out);
    }
    let Ok(Some(raw)) = read_to_string_bounded(&path) else {
        return Ok(out);
    };
    let body = strip_line_comments(&raw, '#');
    let Some(entry_points) = extract_section(&body, "options.entry_points") else {
        return Ok(out);
    };
    // Pull the `console_scripts = …` block. The value is a multi-line
    // INI continuation: subsequent indented lines belong to the same
    // key until the next bare-left-column line.
    let Some(block) = extract_ini_multiline_value(&entry_points, "console_scripts") else {
        return Ok(out);
    };
    let entry_re = Regex::new(r"(?m)^\s*([A-Za-z_][\w\-]*)\s*=\s*([^\s].*?)\s*$")?;
    for cap in entry_re.captures_iter(&block) {
        let name = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let target = cap
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if name.is_empty() || target.is_empty() {
            continue;
        }
        let module = target.split(':').next().unwrap_or(&target).to_string();
        let resolved = resolve_script_module_path(ctx, &module);
        let entry_path = resolved.clone().unwrap_or_else(|| "setup.cfg".to_string());
        out.push(FeatureSeed {
            summary: format!(
                "setup.cfg `[options.entry_points] console_scripts` entry `{name} = {target}`"
            ),
            source: "setup-cfg-script",
            confidence: FeatureConfidence::High,
            entry_symbol: Some(module),
            entry_command: Some(name.clone()),
            tags: vec!["python".to_string(), "cli".to_string()],
            context_files: vec![SeedFile {
                path: "setup.cfg".to_string(),
                reason: "package manifest".to_string(),
            }],
            test_prefixes: vec!["tests".to_string()],
            ..FeatureSeed::new(
                FeatureKind::CliCommand,
                Language::Python,
                format!("Python script `{name}` (setup.cfg)"),
                entry_path,
            )
        });
    }
    Ok(out)
}

/// INI-style multi-line value extractor: returns the body of `<key> = …`
/// in `section_body`, joining indented continuation lines. Stops at the
/// next unindented key or section header.
fn extract_ini_multiline_value(section_body: &str, key: &str) -> Option<String> {
    let mut lines = section_body.lines();
    let mut out = String::new();
    let mut in_value = false;
    let prefix = key.to_string();
    for line in lines.by_ref() {
        let trimmed_left = line.trim_start();
        if !in_value {
            if trimmed_left.starts_with(&prefix) {
                let rest = trimmed_left.trim_start_matches(&prefix).trim_start();
                if let Some(after_eq) = rest.strip_prefix('=') {
                    in_value = true;
                    let first = after_eq.trim();
                    if !first.is_empty() {
                        out.push_str(first);
                        out.push('\n');
                    }
                }
            }
            continue;
        }
        // Continuation: indented or blank. A non-empty, non-indented
        // line ends the value.
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            out.push_str(line.trim());
            out.push('\n');
        } else {
            break;
        }
    }
    if in_value { Some(out) } else { None }
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

/// A `.py` file read once during `map`, shared by every source scanner so
/// the tree is walked once and each file read once (bounded).
struct PyFile {
    rel: String,
    contents: String,
}

/// Walk the project once, reading each `.py` file with the bounded reader.
/// Files above the reader's size cap (or unreadable) are dropped. The
/// 30_000 file cap matches the per-scanner walk it replaces.
fn collect_python_source_files(ctx: &MapperContext) -> Vec<PyFile> {
    let root = ctx.root;
    let mut out = Vec::new();
    for rel in walk_files(root, root, 30_000, ctx.excludes) {
        if !rel.ends_with(".py") {
            continue;
        }
        let abs = root.join(&rel);
        if let Ok(Some(contents)) = read_to_string_bounded(&abs) {
            out.push(PyFile { rel, contents });
        }
    }
    out
}

fn main_guard_modules(files: &[PyFile]) -> Result<Vec<FeatureSeed>> {
    use codesage_protocol::FileCategory;
    let mut out = Vec::new();
    let guard_re = Regex::new(r#"(?m)^if\s+__name__\s*==\s*['"]__main__['"]"#)?;
    for f in files {
        let rel = &f.rel;
        // Test files with `if __name__ == "__main__":` are ad-hoc test
        // runners, not CLI commands. They're often excluded by the
        // project's `[index].exclude_patterns`, so emitting features
        // for them produces rows whose entry_path isn't in the `files`
        // table.
        if matches!(FileCategory::classify(rel), FileCategory::Test) {
            continue;
        }
        if !guard_re.is_match(&f.contents) {
            continue;
        }
        let stem = rel
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".py"))
            .unwrap_or("script");
        out.push(FeatureSeed {
            summary: format!("Top-level `if __name__ == \"__main__\":` at {rel}"),
            source: "python-main-guard",
            entry_symbol: Some("__main__".to_string()),
            entry_command: Some(stem.to_string()),
            tags: vec!["python".to_string(), "cli".to_string()],
            ..FeatureSeed::new(
                FeatureKind::CliCommand,
                Language::Python,
                format!("Python module `{stem}` (__main__)"),
                rel.clone(),
            )
        });
    }
    Ok(out)
}

// ---- Flask / FastAPI route mapping ------------------------------------

/// Flask route detection. Scans `.py` files that import flask for
/// `@<receiver>.route('/path', methods=['GET','POST'])` decorators where
/// `receiver` is a local variable initialized from `Flask(...)` or
/// `Blueprint(...)`. Emits one `route` feature per `(method, path)`.
/// Defaults to `GET` when `methods=` is absent (Flask's default).
///
/// Known limitations: blueprint `url_prefix` is NOT expanded into mounted
/// paths; non-literal paths or method lists are intentionally skipped
/// rather than guessed.
fn flask_routes(files: &[PyFile]) -> Result<Vec<FeatureSeed>> {
    python_framework_routes(files, PythonFramework::Flask)
}

/// FastAPI route detection. Scans `.py` files that import fastapi for
/// `@<receiver>.METHOD('/path')` or `@<receiver>.api_route('/path',
/// methods=[...])` decorators where `receiver` is a `FastAPI(...)` or
/// `APIRouter(...)` instance.
///
/// Known limitations: `include_router(prefix=…)` mount prefixes are NOT
/// expanded — upstream clawpatch doesn't expand them either. Non-literal
/// paths are skipped.
fn fastapi_routes(files: &[PyFile]) -> Result<Vec<FeatureSeed>> {
    python_framework_routes(files, PythonFramework::FastApi)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PythonFramework {
    Flask,
    FastApi,
}

impl PythonFramework {
    fn source(self) -> &'static str {
        match self {
            Self::Flask => "flask-route",
            Self::FastApi => "fastapi-route",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Flask => "Flask",
            Self::FastApi => "FastAPI",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Self::Flask => "framework:flask",
            Self::FastApi => "framework:fastapi",
        }
    }
    fn import_token(self) -> &'static str {
        match self {
            Self::Flask => "flask",
            Self::FastApi => "fastapi",
        }
    }
    /// Method tokens accepted on `@<recv>.<token>(`. `api_route` and
    /// `route` are variadic and pull their methods from a `methods=`
    /// kwarg; the per-HTTP-verb tokens encode their method directly.
    fn method_tokens(self) -> &'static [&'static str] {
        match self {
            Self::Flask => &["route"],
            Self::FastApi => &[
                "api_route",
                "get",
                "post",
                "put",
                "patch",
                "delete",
                "options",
                "head",
                "trace",
            ],
        }
    }
}

fn python_framework_routes(
    files: &[PyFile],
    framework: PythonFramework,
) -> Result<Vec<FeatureSeed>> {
    let mut out = Vec::new();
    let ctor_pattern: &str = match framework {
        PythonFramework::Flask => {
            r"(?m)^\s*([A-Za-z_][\w]*)\s*=\s*(?:flask\s*\.\s*)?(?:Flask|Blueprint)\s*\("
        }
        PythonFramework::FastApi => {
            r"(?m)^\s*([A-Za-z_][\w]*)\s*=\s*(?:fastapi\s*\.\s*)?(?:FastAPI|APIRouter)\s*\("
        }
    };
    let ctor_re = Regex::new(ctor_pattern)?;
    let import_re = Regex::new(&format!(
        r"(?m)^\s*(?:from\s+{name}(?:\s|\.)|import\s+{name}\b)",
        name = framework.import_token()
    ))?;

    for f in files {
        let raw = &f.contents;
        // Cheap gates: framework token must appear, and an import line
        // must confirm it (avoids matching a string literal that
        // mentions the framework name).
        if !raw.contains(framework.import_token()) {
            continue;
        }
        let source = strip_line_comments(raw, '#');
        if !import_re.is_match(&source) {
            continue;
        }
        let mut receivers: Vec<String> = Vec::new();
        for cap in ctor_re.captures_iter(&source) {
            if let Some(name) = cap.get(1) {
                receivers.push(name.as_str().to_string());
            }
        }
        if receivers.is_empty() {
            continue;
        }
        let mut emitted: BTreeSet<(String, String)> = BTreeSet::new();
        for recv in &receivers {
            emit_python_routes_for(&source, &f.rel, recv, framework, &mut emitted, &mut out)?;
        }
    }
    Ok(out)
}

/// Pending decorator captured at scan time, holding the args needed to
/// emit one or more route seeds once the handler `def` is reached. A
/// stack of these accumulates while stacked decorators bind to the same
/// function.
struct PendingDecorator {
    path: String,
    methods: Vec<String>,
}

/// Line-based scanner for both Flask and FastAPI. Walks the source line
/// by line, accumulates multi-line decorator argument lists by tracking
/// paren depth, then attaches a handler function name pulled from the
/// next `def` line as the seed's `entry_symbol`. Stacked decorators all
/// resolve to the same function; intervening blank lines / comments /
/// other decorators don't reset the pending list.
fn emit_python_routes_for(
    source: &str,
    rel: &str,
    recv: &str,
    framework: PythonFramework,
    emitted: &mut BTreeSet<(String, String)>,
    out: &mut Vec<FeatureSeed>,
) -> Result<()> {
    let start_re = build_decorator_start_re(recv, framework)?;
    let decorator_re = build_decorator_extract_re(recv, framework)?;
    let def_re = python_def_re();

    let mut pending: Vec<PendingDecorator> = Vec::new();
    let mut buf: Option<(String, i32)> = None;

    for line in source.split('\n') {
        if let Some((accum, depth)) = buf.as_mut() {
            accum.push(' ');
            accum.push_str(line.trim());
            *depth += paren_delta(line);
            if *depth <= 0 {
                let full = std::mem::take(accum);
                buf = None;
                if let Some(p) = parse_decorator(&full, &decorator_re, framework) {
                    pending.push(p);
                }
            }
            continue;
        }
        if start_re.is_match(line) {
            let delta = paren_delta(line);
            if delta <= 0 {
                if let Some(p) = parse_decorator(line, &decorator_re, framework) {
                    pending.push(p);
                }
            } else {
                buf = Some((line.to_string(), delta));
            }
            continue;
        }
        if let Some(fn_name) = def_re
            .captures(line)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        {
            let mut ctx = RouteEmitCtx {
                out,
                emitted,
                rel,
                recv,
                framework,
            };
            for p in pending.drain(..) {
                for method in &p.methods {
                    push_python_route_seed(&mut ctx, method, &p.path, Some(&fn_name));
                }
            }
            continue;
        }
        let trimmed = line.trim_start();
        if !pending.is_empty()
            && !trimmed.is_empty()
            && !trimmed.starts_with('@')
            && !trimmed.starts_with('#')
        {
            pending.clear();
        }
    }
    Ok(())
}

fn build_decorator_start_re(recv: &str, framework: PythonFramework) -> Result<Regex> {
    let methods = framework.method_tokens().join("|");
    Ok(Regex::new(&format!(
        r"^\s*@\s*{recv}\s*\.\s*(?:{methods})\s*\(",
        recv = regex::escape(recv),
    ))?)
}

fn build_decorator_extract_re(recv: &str, framework: PythonFramework) -> Result<Regex> {
    let methods = framework.method_tokens().join("|");
    Ok(Regex::new(&format!(
        r#"@\s*{recv}\s*\.\s*({methods})\s*\(\s*(?:path\s*=\s*)?['"]([^'"]+)['"](.*)"#,
        recv = regex::escape(recv),
    ))?)
}

fn python_def_re() -> Regex {
    Regex::new(r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(").expect("static regex")
}

fn parse_decorator(
    full: &str,
    decorator_re: &Regex,
    framework: PythonFramework,
) -> Option<PendingDecorator> {
    let caps = decorator_re.captures(full)?;
    let method_token = caps.get(1)?.as_str().to_string();
    let path = caps.get(2)?.as_str().to_string();
    if path.is_empty() {
        return None;
    }
    let rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");
    let methods = match method_token.as_str() {
        // Flask `route` defaults to GET when no `methods=` kwarg is supplied.
        "route" => parse_methods_kwarg(rest).unwrap_or_else(|| vec!["GET".to_string()]),
        // FastAPI `api_route` is variadic and REQUIRES `methods=[...]`; a
        // missing/unparseable methods list means we can't determine the
        // method set and drop the decorator rather than guessing.
        "api_route" => parse_methods_kwarg(rest)?,
        verb => vec![verb.to_uppercase()],
    };
    if methods.is_empty() {
        return None;
    }
    let _ = framework;
    Some(PendingDecorator { path, methods })
}

/// Parse `methods=[...]`, `methods=(...)`, or `methods={...}` out of a
/// decorator argument tail. Quoted method tokens are extracted and
/// upper-cased. Returns `None` when the kwarg is absent or the literal
/// can't be matched (e.g. `methods=ALLOWED_METHODS` referencing a name).
fn parse_methods_kwarg(args: &str) -> Option<Vec<String>> {
    let idx = args.find("methods")?;
    let tail = args[idx + "methods".len()..].trim_start();
    let tail = tail.strip_prefix('=')?.trim_start();
    let opener = tail.chars().next()?;
    let closer = match opener {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        _ => return None,
    };
    let chars: Vec<char> = tail.chars().collect();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut end = None;
    for (i, &c) in chars.iter().enumerate() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            continue;
        }
        if c == opener {
            depth += 1;
            continue;
        }
        if c == closer {
            depth -= 1;
            if depth == 0 {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    let inner: String = chars[1..end].iter().collect();
    let token_re = Regex::new(r#"['"]\s*([A-Za-z]+)\s*['"]"#).ok()?;
    let mut methods: Vec<String> = token_re
        .captures_iter(&inner)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_uppercase()))
        .collect();
    if methods.is_empty() {
        return None;
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    methods.retain(|m| seen.insert(m.clone()));
    Some(methods)
}

/// Net paren count for a line, ignoring `(` and `)` that appear inside
/// `"..."` / `'...'` string literals. Used to drive multi-line decorator
/// accumulation: a decorator continues until its paren depth returns to
/// zero.
fn paren_delta(line: &str) -> i32 {
    let mut delta = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '(' {
            delta += 1;
        } else if c == ')' {
            delta -= 1;
        }
    }
    delta
}

/// Per-file scanner state bundle: the output vector, dedup set, and the
/// file/receiver/framework triple that stays constant across every seed
/// emitted from the same `(file, receiver)` pair. Lifts the function-arg
/// count of [`push_python_route_seed`] under the clippy threshold and
/// keeps each call site focused on what varies (method, path, handler).
struct RouteEmitCtx<'a> {
    out: &'a mut Vec<FeatureSeed>,
    emitted: &'a mut BTreeSet<(String, String)>,
    rel: &'a str,
    recv: &'a str,
    framework: PythonFramework,
}

fn push_python_route_seed(
    ctx: &mut RouteEmitCtx<'_>,
    method: &str,
    path: &str,
    fn_name: Option<&str>,
) {
    let key = (method.to_string(), path.to_string());
    if !ctx.emitted.insert(key.clone()) {
        return;
    }
    let route_label = format!("{method} {path}");
    let summary = match fn_name {
        Some(name) => format!(
            "{} route {} declared in {} (receiver `{}`, handler `{}`)",
            ctx.framework.label(),
            route_label,
            ctx.rel,
            ctx.recv,
            name,
        ),
        None => format!(
            "{} route {} declared in {} (receiver `{}`)",
            ctx.framework.label(),
            route_label,
            ctx.rel,
            ctx.recv,
        ),
    };
    let mut tags = vec![
        "python".to_string(),
        ctx.framework.tag().to_string(),
        "route".to_string(),
    ];
    if route_is_auth_sensitive(method, path) {
        tags.push(AUTH_SENSITIVE_TAG.to_string());
    }
    ctx.out.push(FeatureSeed {
        summary,
        source: ctx.framework.source(),
        confidence: FeatureConfidence::High,
        entry_symbol: fn_name.map(String::from),
        entry_route: Some(route_label.clone()),
        tags,
        test_prefixes: vec!["tests".to_string()],
        ..FeatureSeed::new(
            FeatureKind::Route,
            Language::Python,
            format!("{} route `{}`", ctx.framework.label(), route_label),
            ctx.rel,
        )
    });
}

// ---- Django URLconf route mapping -------------------------------------

/// Django route detection. Scans `.py` files that import from `django.urls`
/// / `django.conf.urls` and declare a `urlpatterns = [...]` list for
/// `path()`, `re_path()`, and (legacy) `url()` route entries. Emits one
/// `route` feature per distinct normalized URL, with the view's symbol as
/// `entry_symbol` when it can be resolved.
///
/// Known limitations (deliberately conservative, matching the Flask /
/// FastAPI mappers): `include(...)` mounts are NOT expanded into their
/// sub-URLConf — the mount prefix is dropped rather than guessed, so the
/// child app's own `urls.py` contributes its routes without the prefix.
/// Non-literal route patterns (a variable instead of a string) are skipped.
/// Django binds no HTTP method at the URL layer, so route labels carry the
/// path only; the `auth-sensitive` tag therefore fires on path shape alone.
fn django_routes(files: &[PyFile]) -> Result<Vec<FeatureSeed>> {
    let mut out = Vec::new();
    let import_re = Regex::new(
        r"(?m)^\s*(?:from\s+django\.(?:urls|conf\.urls)\s+import\b|import\s+django\.(?:urls|conf\.urls)\b)",
    )?;
    // group 1 = the char before the helper (or start), used to reject
    // attribute access like `views.url(`; group 2 = the helper name.
    let call_re = Regex::new(r"(^|[^.\w])(path|re_path|url)\s*\(")?;

    for f in files {
        let rel = &f.rel;
        let raw = &f.contents;
        // Cheap gates before the comment strip + regex work.
        if !raw.contains("urlpatterns") || !raw.contains("django") {
            continue;
        }
        let source = strip_line_comments(raw, '#');
        if !import_re.is_match(&source) {
            continue;
        }
        let mut emitted: BTreeSet<String> = BTreeSet::new();
        for body in django_urlpatterns_bodies(&source) {
            let body_bytes = body.as_bytes();
            for cap in call_re.captures_iter(&body) {
                let whole = cap.get(0).expect("group 0");
                let helper = cap.get(2).expect("helper group").as_str();
                let open_idx = whole.end() - 1;
                if body_bytes.get(open_idx) != Some(&b'(') {
                    continue;
                }
                let Some(close) = find_balanced_close(body_bytes, open_idx) else {
                    continue;
                };
                let args = &body[open_idx + 1..close];
                let Some((route, symbol)) = parse_django_route(helper, args) else {
                    continue;
                };
                if route.is_empty() || !emitted.insert(route.clone()) {
                    continue;
                }
                push_django_route_seed(&mut out, rel, &route, symbol.as_deref());
            }
        }
    }
    Ok(out)
}

fn push_django_route_seed(
    out: &mut Vec<FeatureSeed>,
    rel: &str,
    route: &str,
    symbol: Option<&str>,
) {
    let summary = match symbol {
        Some(s) => format!("Django route {route} handled by `{s}` declared in {rel}"),
        None => format!("Django route {route} declared in {rel}"),
    };
    let mut tags = vec![
        "python".to_string(),
        "framework:django".to_string(),
        "route".to_string(),
    ];
    if route_is_auth_sensitive("", route) {
        tags.push(AUTH_SENSITIVE_TAG.to_string());
    }
    out.push(FeatureSeed {
        summary,
        source: "django-route",
        confidence: FeatureConfidence::High,
        entry_symbol: symbol.map(String::from),
        entry_route: Some(route.to_string()),
        tags,
        test_prefixes: vec!["tests".to_string()],
        ..FeatureSeed::new(
            FeatureKind::Route,
            Language::Python,
            format!("Django route `{route}`"),
            rel,
        )
    });
}

/// Extract the inner text of every `urlpatterns = [ … ]` (or `+= [ … ]`)
/// list literal in a source file. The opening `[` must sit on the same
/// line as the assignment; the matching `]` is found with bracket balance
/// that skips string literals (so a `]` inside a regex pattern string does
/// not close the list early).
fn django_urlpatterns_bodies(source: &str) -> Vec<String> {
    let re = match Regex::new(r"(?m)^\s*urlpatterns\s*\+?=\s*\[") {
        Ok(re) => re,
        Err(_) => return Vec::new(),
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    for m in re.find_iter(source) {
        let open_idx = m.end() - 1;
        if bytes.get(open_idx) != Some(&b'[') {
            continue;
        }
        if let Some(close) = find_balanced_close(bytes, open_idx) {
            out.push(source[open_idx + 1..close].to_string());
        }
    }
    out
}

/// Parse one `path()` / `re_path()` / `url()` call's argument string into a
/// `(normalized_route, view_symbol)` pair. Returns `None` to skip the call:
/// when the first positional arg isn't a string literal, when the second
/// positional arg is an `include(...)` mount (not expanded), or when a
/// regex route can't be normalized.
fn parse_django_route(helper: &str, args: &str) -> Option<(String, Option<String>)> {
    let parts = split_top_level_args(args);
    let raw_route = django_string_literal(parts.first()?)?;
    let view_raw = parts.get(1).map(|s| s.trim()).unwrap_or("");
    if view_raw.starts_with("include") {
        return None;
    }
    let route = if helper == "path" {
        ensure_leading_slash(&strip_django_converters(&raw_route))
    } else {
        normalize_django_regex_route(&raw_route)?
    };
    let symbol = if view_raw.is_empty() {
        None
    } else {
        django_view_symbol(view_raw)
    };
    Some((route, symbol))
}

/// Strip Django `path()` converter prefixes: `<int:year>` → `<year>`,
/// `<slug:title>` → `<title>`, while a bare `<year>` is left intact.
fn strip_django_converters(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut inner = String::new();
            for d in chars.by_ref() {
                if d == '>' {
                    break;
                }
                inner.push(d);
            }
            let name = inner.rsplit(':').next().unwrap_or(&inner);
            out.push('<');
            out.push_str(name);
            out.push('>');
        } else {
            out.push(c);
        }
    }
    out
}

/// Normalize a `re_path()` / `url()` regex into a readable path: drop the
/// `^`/`$` anchors, rewrite named capture groups `(?P<name>…)` to `<name>`,
/// and unescape `\/` and `\.`. Best-effort — remaining regex metacharacters
/// are left as-is rather than guessed at.
fn normalize_django_regex_route(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let no_caret = trimmed.strip_prefix('^').unwrap_or(trimmed);
    let no_anchor = no_caret.strip_suffix('$').unwrap_or(no_caret);
    let groups = convert_django_named_groups(no_anchor);
    let unescaped = groups.replace("\\/", "/").replace("\\.", ".");
    Some(ensure_leading_slash(&unescaped))
}

/// Rewrite each `(?P<name>pattern)` named capture group to `<name>`,
/// discarding the inner pattern. Matching parens are found with bracket
/// balance so nested groups collapse correctly.
fn convert_django_named_groups(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("(?P<")
            && let Some(gt) = s[i + 4..].find('>')
            && let Some(close) = find_balanced_close(bytes, i)
        {
            let name = &s[i + 4..i + 4 + gt];
            out.push('<');
            out.push_str(name);
            out.push('>');
            i = close + 1;
            continue;
        }
        let len = utf8_char_len(bytes[i]);
        out.push_str(&s[i..i + len]);
        i += len;
    }
    out
}

/// Resolve a Django view argument to a symbol name: `views.article_list`
/// → `article_list`, `ArticleView.as_view()` → `ArticleView`, a bare
/// `home` → `home`. Returns `None` when the result isn't a clean
/// identifier (e.g. a `view=` kwarg or an inline lambda), or when the
/// argument is a urlconf mount rather than a view: a bare dotted attribute
/// ending in `.urls` (`admin.site.urls`, `app.urls`) names an included
/// URLconf, so its leaf (`urls`) is a meaningless "handler" — keep the
/// route, drop the symbol. (`include('app.urls')` is filtered earlier; this
/// catches the attribute-style mount Django's own templates use.)
fn django_view_symbol(raw: &str) -> Option<String> {
    let head = raw.split('(').next().unwrap_or(raw).trim();
    // Attribute-style urlconf mount (no call): `x.y.urls` with ≥2 segments.
    if !head.contains("()") && head.ends_with(".urls") && head.matches('.').count() >= 1 {
        return None;
    }
    let head = head.strip_suffix(".as_view").unwrap_or(head);
    let last = head.rsplit('.').next().unwrap_or(head).trim();
    if last.is_empty() {
        return None;
    }
    let mut cs = last.chars();
    let first_ok = cs.next().is_some_and(|c| c.is_alphabetic() || c == '_');
    let rest_ok = last.chars().all(|c| c.is_alphanumeric() || c == '_');
    if first_ok && rest_ok {
        Some(last.to_string())
    } else {
        None
    }
}

/// Extract the value of a leading string literal from an argument slice,
/// tolerating Python string prefixes (`r`, `b`, `u`, `f`). Returns `None`
/// when the slice doesn't start with a quoted literal.
fn django_string_literal(arg: &str) -> Option<String> {
    let s = arg.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && matches!(
            bytes[i],
            b'r' | b'R' | b'b' | b'B' | b'u' | b'U' | b'f' | b'F'
        )
    {
        i += 1;
    }
    let quote = *bytes.get(i)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    i += 1;
    let start = i;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if c == quote {
            return Some(s[start..i].to_string());
        }
        i += 1;
    }
    None
}

/// Split a call's argument string on top-level commas, respecting string
/// literals and nested `()`/`[]`/`{}`. Each returned element is trimmed.
fn split_top_level_args(args: &str) -> Vec<String> {
    let bytes = args.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                b'"' | b'\'' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b',' if depth == 0 => {
                    parts.push(args[start..i].trim().to_string());
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    let last = args[start..].trim();
    if !last.is_empty() {
        parts.push(last.to_string());
    }
    parts
}

/// Find the index of the bracket that closes the one at `open_idx`,
/// skipping `'…'` / `"…"` string literals. Works for `(`, `[`, `{`.
fn find_balanced_close(bytes: &[u8], open_idx: usize) -> Option<usize> {
    let open = *bytes.get(open_idx)?;
    let close = match open {
        b'(' => b')',
        b'[' => b']',
        b'{' => b'}',
        _ => return None,
    };
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut i = open_idx;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if c == b'"' || c == b'\'' {
            quote = Some(c);
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Width in bytes of the UTF-8 sequence starting with `b` (falls back to 1
/// for stray continuation bytes so the caller always advances).
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn ensure_leading_slash(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
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
            summary,
            source: "python-test-suite",
            confidence: FeatureConfidence::High,
            entry_symbol: Some(label.clone()),
            // Test-suite features identify by the suite label (`tests` /
            // `test` / source-root name). The runnable command goes in
            // test_command, keeping entry_command free for the argv[0]-
            // shape contract (this seed has no such command).
            test_command: test_cmd.map(String::from),
            tags: vec!["python".to_string(), "tests".to_string()],
            owned_files: owned,
            tests,
            test_prefixes: if suite_root.is_empty() {
                Vec::new()
            } else {
                vec![suite_root]
            },
            ..FeatureSeed::new(
                FeatureKind::TestSuite,
                Language::Python,
                format!("Python tests `{label}`"),
                entry,
            )
        });
    }
    Ok(out)
}

/// Match `test_*.py` and `*_test.py`. Excludes fixture/testdata paths and
/// generated python (`*_pb2.py`, `*.gen.py`) — even when they live under a
/// test root, they're not the test files an agent runs. The basename
/// convention itself is single-sourced in `nearby_tests`; this adds the
/// runnable-file carve-outs on top.
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
    crate::nearby_tests::is_python_test_basename(rel)
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

    // ---------- Flask / FastAPI / setup.cfg (clawpatch #11 / #15 / #28) ----------

    #[test]
    fn flask_basic_route_defaults_to_get() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "app.py",
            r#"
from flask import Flask
app = Flask(__name__)

@app.route("/users")
def list_users():
    return []
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "flask-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert_eq!(routes, vec!["GET /users"]);
    }

    #[test]
    fn flask_methods_kwarg_expands_to_one_route_per_method() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "app.py",
            r#"
from flask import Flask
app = Flask(__name__)

@app.route("/users", methods=["GET", "POST"])
def users():
    return []

@app.route("/users/<id>", methods=['DELETE'])
def delete_user(id):
    return ""
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "flask-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert!(routes.contains("GET /users"));
        assert!(routes.contains("POST /users"));
        assert!(routes.contains("DELETE /users/<id>"));
    }

    #[test]
    fn flask_blueprint_receiver_recognized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "api.py",
            r#"
from flask import Blueprint
bp = Blueprint("api", __name__)

@bp.route("/ping")
def ping():
    return "ok"
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "flask-route" && s.entry_route.as_deref() == Some("GET /ping"))
        );
    }

    #[test]
    fn flask_requires_flask_import() {
        // No flask import → no route seeds, even if a `@app.route` decorator
        // happens to be present (could be a different library).
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "app.py",
            r#"
class App:
    def route(self, p): return lambda f: f

app = App()

@app.route("/users")
def users():
    return []
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(!seeds.iter().any(|s| s.source == "flask-route"));
    }

    #[test]
    fn fastapi_routes_recognized_per_method() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "main.py",
            r#"
from fastapi import FastAPI
app = FastAPI()

@app.get("/health")
async def health():
    return {"ok": True}

@app.post("/users")
async def create_user():
    return {}

@app.delete("/users/{id}")
async def delete_user(id: int):
    return None
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "fastapi-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        for expected in ["GET /health", "POST /users", "DELETE /users/{id}"] {
            assert!(
                routes.contains(expected),
                "{expected} missing in {routes:?}"
            );
        }
    }

    #[test]
    fn fastapi_apirouter_recognized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "routers.py",
            r#"
from fastapi import APIRouter
router = APIRouter()

@router.get("/v1/ping")
def ping():
    return "ok"
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(seeds.iter().any(
            |s| s.source == "fastapi-route" && s.entry_route.as_deref() == Some("GET /v1/ping")
        ));
    }

    #[test]
    fn fastapi_api_route_variadic_methods_expand() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "main.py",
            r#"
from fastapi import FastAPI
app = FastAPI()

@app.api_route("/anything", methods=["GET", "POST"])
async def handle_any():
    return {}
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "fastapi-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert!(routes.contains("GET /anything"), "got: {routes:?}");
        assert!(routes.contains("POST /anything"), "got: {routes:?}");
    }

    #[test]
    fn fastapi_api_route_without_methods_kwarg_is_dropped() {
        // `api_route` without `methods=[...]` has no determinable method
        // set. We drop it rather than guessing GET (Flask's default
        // doesn't apply here).
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "main.py",
            r#"
from fastapi import FastAPI
app = FastAPI()

@app.api_route("/unknown")
async def handler():
    return {}
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "fastapi-route"),
            "api_route without methods= should produce no seeds"
        );
    }

    #[test]
    fn fastapi_route_captures_handler_function_name() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "main.py",
            r#"
from fastapi import FastAPI
app = FastAPI()

@app.get("/users/{id}")
async def fetch_user(id: int):
    return {}
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let seed = seeds
            .iter()
            .find(|s| {
                s.source == "fastapi-route" && s.entry_route.as_deref() == Some("GET /users/{id}")
            })
            .expect("route seed missing");
        assert_eq!(seed.entry_symbol.as_deref(), Some("fetch_user"));
    }

    #[test]
    fn flask_route_captures_handler_function_name() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "app.py",
            r#"
from flask import Flask
app = Flask(__name__)

@app.route("/users")
def list_users():
    return []
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let seed = seeds
            .iter()
            .find(|s| s.source == "flask-route" && s.entry_route.as_deref() == Some("GET /users"))
            .expect("route seed missing");
        assert_eq!(seed.entry_symbol.as_deref(), Some("list_users"));
    }

    #[test]
    fn fastapi_multiline_decorator_recognized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "main.py",
            r#"
from fastapi import FastAPI
app = FastAPI()

@app.post(
    "/items",
    response_model=dict,
    status_code=201,
)
async def create_item(payload: dict):
    return payload
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let seed = seeds
            .iter()
            .find(|s| {
                s.source == "fastapi-route" && s.entry_route.as_deref() == Some("POST /items")
            })
            .expect("multi-line decorator missed");
        assert_eq!(seed.entry_symbol.as_deref(), Some("create_item"));
    }

    #[test]
    fn flask_methods_tuple_container_recognized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "app.py",
            r#"
from flask import Flask
app = Flask(__name__)

@app.route("/items", methods=("GET", "POST"))
def items():
    return []
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "flask-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert!(routes.contains("GET /items"), "got: {routes:?}");
        assert!(routes.contains("POST /items"), "got: {routes:?}");
    }

    #[test]
    fn flask_methods_set_container_recognized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "app.py",
            r#"
from flask import Flask
app = Flask(__name__)

@app.route("/items", methods={"GET", "PUT"})
def items():
    return []
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "flask-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert!(routes.contains("GET /items"), "got: {routes:?}");
        assert!(routes.contains("PUT /items"), "got: {routes:?}");
    }

    #[test]
    fn fastapi_stacked_decorators_share_handler() {
        // A second decorator between the route decorator and `def`
        // must not break the binding — both upstream and our scanner
        // hold the pending route until any `def` line is reached.
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "main.py",
            r#"
from fastapi import FastAPI
app = FastAPI()

def auth_required(fn):
    return fn

@app.get("/admin")
@auth_required
async def admin_panel():
    return {}
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let seed = seeds
            .iter()
            .find(|s| s.source == "fastapi-route" && s.entry_route.as_deref() == Some("GET /admin"))
            .expect("stacked decorator missed");
        assert_eq!(seed.entry_symbol.as_deref(), Some("admin_panel"));
    }

    #[test]
    fn django_path_routes_recognized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "urls.py",
            r#"
from django.urls import path
from . import views

urlpatterns = [
    path("", views.home, name="home"),
    path("articles/<int:year>/", views.year_archive),
    path("admin/", views.admin_dashboard),
]
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "django-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert!(routes.contains("/"), "got: {routes:?}");
        assert!(
            routes.contains("/articles/<year>/"),
            "converter not stripped: {routes:?}"
        );
        assert!(routes.contains("/admin/"), "got: {routes:?}");
        // view symbol resolved off the `views.` attribute access
        let home = seeds
            .iter()
            .find(|s| s.source == "django-route" && s.entry_route.as_deref() == Some("/"))
            .unwrap();
        assert_eq!(home.entry_symbol.as_deref(), Some("home"));
        // path-shape auth-sensitive tag fires on /admin/
        let admin = seeds
            .iter()
            .find(|s| s.source == "django-route" && s.entry_route.as_deref() == Some("/admin/"))
            .unwrap();
        assert!(
            admin.tags.iter().any(|t| t == "auth-sensitive"),
            "admin route should be auth-sensitive: {:?}",
            admin.tags
        );
        // a plain GET-shape article route should not be flagged
        let archive = seeds
            .iter()
            .find(|s| {
                s.source == "django-route" && s.entry_route.as_deref() == Some("/articles/<year>/")
            })
            .unwrap();
        assert!(!archive.tags.iter().any(|t| t == "auth-sensitive"));
    }

    #[test]
    fn django_re_path_normalized() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "urls.py",
            r#"
from django.urls import re_path
from . import views

urlpatterns = [
    re_path(r"^articles/(?P<slug>[\w-]+)/$", views.detail),
]
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let seed = seeds
            .iter()
            .find(|s| s.source == "django-route")
            .expect("re_path route missing");
        assert_eq!(seed.entry_route.as_deref(), Some("/articles/<slug>/"));
        assert_eq!(seed.entry_symbol.as_deref(), Some("detail"));
    }

    #[test]
    fn django_as_view_symbol_and_include_skipped() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "urls.py",
            r#"
from django.urls import path, include
from .views import ArticleList

urlpatterns = [
    path("articles/", ArticleList.as_view(), name="articles"),
    path("blog/", include("blog.urls")),
]
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "django-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        // class-based view resolves to the class name
        let cbv = seeds
            .iter()
            .find(|s| s.source == "django-route" && s.entry_route.as_deref() == Some("/articles/"))
            .expect("CBV route missing");
        assert_eq!(cbv.entry_symbol.as_deref(), Some("ArticleList"));
        // include() mount is NOT emitted as a route of its own
        assert!(
            !routes.contains("/blog/"),
            "include() mount should be skipped: {routes:?}"
        );
    }

    #[test]
    fn django_admin_site_urls_mount_keeps_route_drops_symbol() {
        // `path('admin/', admin.site.urls)` — the stock Django route present
        // in every project. It's a urlconf mount, not a view; the route is
        // real (and auth-sensitive) but `urls` is a junk handler symbol.
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "urls.py",
            r#"
from django.contrib import admin
from django.urls import path

urlpatterns = [
    path("admin/", admin.site.urls),
]
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let seed = seeds
            .iter()
            .find(|s| s.source == "django-route" && s.entry_route.as_deref() == Some("/admin/"))
            .expect("admin route missing");
        assert_eq!(
            seed.entry_symbol, None,
            "admin.site.urls mount should not yield a handler symbol, got {:?}",
            seed.entry_symbol
        );
        assert!(
            seed.tags.iter().any(|t| t == "auth-sensitive"),
            "/admin/ should still be auth-sensitive"
        );
    }

    #[test]
    fn django_ignores_urlpatterns_examples_in_docstring() {
        // The stock `startproject` urls.py carries example `path(...)` calls
        // inside its module docstring. The import gate + urlpatterns anchor
        // must not emit phantom routes from that prose. Real-repo trap
        // caught in the GitNexus Django fixture smoke test (2026-06-01).
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "urls.py",
            r#"
"""
URL configuration.
Examples:
    path('', views.home, name='home')
    path('blog/', include('blog.urls'))
"""
from django.contrib import admin
from django.urls import path

urlpatterns = [
    path("admin/", admin.site.urls),
]
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "django-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert_eq!(
            routes,
            std::collections::BTreeSet::from(["/admin/".to_string()]),
            "docstring examples leaked as routes: {routes:?}"
        );
    }

    #[test]
    fn django_requires_django_import() {
        // `urlpatterns` present but no django.urls import → skip (could be a
        // coincidental variable name).
        let dir = tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname=\"app\"\n");
        write(
            dir.path(),
            "urls.py",
            r#"
def path(p, v): return (p, v)
urlpatterns = [
    path("/users", lambda r: r),
]
"#,
        );
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(!seeds.iter().any(|s| s.source == "django-route"));
    }

    #[test]
    fn setup_cfg_console_scripts_recognized() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "setup.cfg",
            r#"
[metadata]
name = acme

[options.entry_points]
console_scripts =
    acme = acme.cli:main
    acme-admin = acme.admin:run
"#,
        );
        write(dir.path(), "acme/__init__.py", "");
        write(dir.path(), "acme/cli.py", "def main(): pass\n");
        write(dir.path(), "acme/admin.py", "def run(): pass\n");
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let scripts: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "setup-cfg-script")
            .filter_map(|s| s.entry_command.as_deref())
            .collect();
        assert!(scripts.contains(&"acme"), "got: {scripts:?}");
        assert!(scripts.contains(&"acme-admin"), "got: {scripts:?}");
        let acme = seeds
            .iter()
            .find(|s| s.source == "setup-cfg-script" && s.entry_command.as_deref() == Some("acme"))
            .unwrap();
        assert_eq!(acme.entry_path, "acme/cli.py", "module resolution failed");
    }
}
