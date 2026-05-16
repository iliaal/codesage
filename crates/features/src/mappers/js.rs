//! JavaScript / TypeScript mapper: workspace-aware `package.json` decomposition
//! (npm / yarn / pnpm `workspaces`, `pnpm-workspace.yaml`, fallback prefixes),
//! per-package `bin` and selected scripts (`start`/`build`/`test`/`lint`/
//! `typecheck`/`format`), Next.js `app/**/{page,route}.{ts,tsx,js,jsx}` and
//! `pages/**/*.{ts,tsx}`, React Router `<Route>` declarations.
//!
//! Workspace decomposition is the per-package surface ported from clawpatch
//! (`src/mappers/node.ts`). Source-group partitioning is intentionally NOT
//! ported — it produces browse-only `list_features` rows without an actionable
//! downstream consumer (LLM-utility-filter, codified in
//! `feedback_llm_utility_filter.md`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use serde_json::Value;

use crate::mappers::shared::{is_safe_dir, is_safe_file, should_skip, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};

pub struct JsMapper;

/// One Node/TypeScript package discovered in the repo. `root_rel` is the
/// workspace-relative directory (`""` for the repo root, `packages/api` for a
/// workspace member); `manifest_rel` is the repo-relative `package.json` path.
struct PackageInfo {
    root_rel: String,
    manifest_rel: String,
    pkg: Value,
}

impl PackageInfo {
    fn is_root(&self) -> bool {
        self.root_rel.is_empty() || self.root_rel == "."
    }
}

/// Detected package manager. Drives the `testCommand` formatting on per-package
/// bin seeds so an agent gets a runnable command directly from the feature
/// record instead of having to inspect `package.json` + lockfile separately.
#[derive(Clone, Copy)]
enum NodePm {
    Pnpm,
    Yarn,
    Bun,
    Npm,
}

impl FeatureMapper for JsMapper {
    fn name(&self) -> &'static str {
        "js"
    }
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let root = ctx.root;
        let mut seeds: Vec<FeatureSeed> = Vec::new();
        let pkg_path = root.join("package.json");
        let pkg_at_root = if is_safe_file(root, &pkg_path) {
            fs::read_to_string(&pkg_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        } else {
            None
        };
        let packages = discover_packages(root, pkg_at_root.as_ref());
        let pm = detect_node_package_manager(root);
        for info in &packages {
            seeds.extend(package_seeds_for(ctx, info, pm));
        }
        seeds.extend(next_app_routes(ctx)?);
        seeds.extend(next_pages_routes(ctx)?);
        // React Router `<Route path element>` declarations. Only run
        // when the root package.json declares a react dep — keeps the
        // tree-walk cost off non-React repos.
        if pkg_at_root.as_ref().is_some_and(has_react_dependency) {
            seeds.extend(react_router_routes(ctx)?);
        }
        seeds.retain(|s| ctx.allowed(&s.entry_path));
        Ok(seeds)
    }
}

fn has_react_dependency(pkg: &Value) -> bool {
    for field in ["dependencies", "devDependencies"] {
        if let Some(map) = pkg.get(field).and_then(|v| v.as_object())
            && map.contains_key("react")
        {
            return true;
        }
    }
    false
}

fn language_for_entry(entry: &str) -> Language {
    if entry.ends_with(".ts") || entry.ends_with(".tsx") {
        Language::TypeScript
    } else {
        Language::JavaScript
    }
}

/// Build the per-package seed set. `is_root` flips on the "common scripts"
/// gate (clawpatch's `includeCommonScripts`): only the root package emits
/// `start`/`build`/`test`/etc. as standalone seeds so a monorepo doesn't get
/// N copies of those entries. Workspace packages still emit their own bin
/// seeds, because per-package `bin` is the routing-actionable surface.
fn package_seeds_for(ctx: &MapperContext, info: &PackageInfo, pm: NodePm) -> Vec<FeatureSeed> {
    let root = ctx.root;
    let mut out = Vec::new();
    let pkg = &info.pkg;
    let has_test_script = package_scripts(pkg).contains_key("test");
    let test_cmd = if has_test_script {
        Some(script_command(pm, &info.root_rel, "test"))
    } else {
        None
    };

    // bin: string or { name: path } map. Each bin emits a cli-command seed.
    if let Some(bin_val) = pkg.get("bin") {
        let entries: Vec<(String, String)> = match bin_val {
            Value::String(s) => {
                let cmd = pkg
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("bin")
                    .to_string();
                vec![(cmd, s.clone())]
            }
            Value::Object(map) => map
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|p| (k.clone(), p.to_string())))
                .collect(),
            _ => Vec::new(),
        };
        for (cmd, path) in entries {
            // Source-back resolution: dist/foo.js → src/foo.ts when that
            // source file exists. Lets `feature-for src/foo.ts` route to
            // the bin feature instead of the build artifact (which an
            // agent should not be editing).
            let entry_rel = resolve_package_bin_entry(root, &info.root_rel, &path);
            let abs = root.join(&entry_rel);
            if !is_safe_file(root, &abs) {
                continue;
            }
            let language = language_for_entry(&entry_rel);
            let summary = if entry_rel
                == package_relative_path(&info.root_rel, &normalize_package_path(&path))
            {
                format!("package.json bin `{cmd}` at {entry_rel}")
            } else {
                format!("package.json bin `{cmd}` at {path}, source resolved to {entry_rel}")
            };
            let mut tags = vec![
                if language == Language::TypeScript {
                    "typescript"
                } else {
                    "javascript"
                }
                .to_string(),
                "cli".to_string(),
            ];
            if !info.is_root() {
                tags.push("workspace".to_string());
            }
            out.push(FeatureSeed {
                title: format!("npm bin `{cmd}`"),
                summary,
                kind: FeatureKind::CliCommand,
                source: "package-json-bin",
                confidence: FeatureConfidence::High,
                entry_path: entry_rel,
                entry_symbol: None,
                entry_route: None,
                entry_command: Some(cmd),
                language,
                tags,
                owned_files: Vec::new(),
                context_files: vec![SeedFile {
                    path: info.manifest_rel.clone(),
                    reason: "package manifest".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: vec![
                    "test".to_string(),
                    "tests".to_string(),
                    "__tests__".to_string(),
                ],
            });
        }
    }

    // Common scripts: only emit standalone seeds for the root package.
    // Workspace packages get the package seed below instead so a Turborepo
    // doesn't produce 30 copies of "npm script `build`".
    if info.is_root()
        && let Some(scripts) = pkg.get("scripts").and_then(|v| v.as_object())
    {
        for (name, value) in scripts {
            if !matches!(
                name.as_str(),
                "start" | "build" | "test" | "lint" | "typecheck" | "format"
            ) {
                continue;
            }
            let command = value.as_str().unwrap_or_default().to_string();
            if command.is_empty() {
                continue;
            }
            let kind = if name == "test" {
                FeatureKind::TestSuite
            } else {
                FeatureKind::Config
            };
            out.push(FeatureSeed {
                title: format!("npm script `{name}`"),
                summary: format!("package.json script `{name}: {command}`"),
                kind,
                source: "package-json-script",
                confidence: FeatureConfidence::Medium,
                entry_path: info.manifest_rel.clone(),
                entry_symbol: Some(name.clone()),
                entry_route: None,
                entry_command: Some(name.clone()),
                language: Language::JavaScript,
                tags: vec!["javascript".to_string(), "package-script".to_string()],
                owned_files: Vec::new(),
                context_files: vec![SeedFile {
                    path: info.manifest_rel.clone(),
                    reason: "package manifest".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: Vec::new(),
            });
        }
    }

    // Workspace package manifest seed. Routes `find_feature(packages/api/...)`
    // to the local manifest + test command so an agent gets the right `pnpm
    // --dir packages/api test` instead of the root-level fallback. Emitted
    // ONLY for workspace members so the root repo's existing per-script
    // seeds remain the agent-facing surface there.
    if !info.is_root() {
        let package_name = package_display_name(info);
        let mut context_files = Vec::new();
        for candidate in ["README.md", "AGENTS.md", "tsconfig.json"] {
            let rel = package_relative_path(&info.root_rel, candidate);
            if rel == info.manifest_rel {
                continue;
            }
            if is_safe_file(root, &root.join(&rel)) {
                context_files.push(SeedFile {
                    path: rel,
                    reason: "package context".to_string(),
                });
            }
        }
        // NOTE: the inferred test command surfaces via the summary string and
        // via `entry_command` only. Earlier drafts populated `tests` with a
        // SeedTest entry pointing at `package.json` to attach the command —
        // but `SeedTest.path` is documented as a test FILE, and the
        // orchestrator inserts `seed.tests[]` rows as `role: Test`. That
        // would surface `packages/api/package.json` as a test file in
        // `feature_bundle` output. Leave tests empty so nearby_tests
        // discovery attaches real test files.
        let summary = match &test_cmd {
            Some(cmd) => format!(
                "Node workspace package `{package_name}` at {} (test: `{cmd}`)",
                info.root_rel
            ),
            None => format!(
                "Node workspace package `{package_name}` at {}",
                info.root_rel
            ),
        };
        // Enumerate the workspace's source files (cap 2_000) and attach as
        // owned_files so `feature-for packages/api/src/auth.ts` actually
        // routes to this seed. Without this, the storage exact-match query
        // `feature_files.path = ?1` returned empty for any file under the
        // workspace — the routing-by-workspace contract advertised in
        // CHANGELOG was a no-op.
        let mut owned_files = vec![SeedFile {
            path: info.manifest_rel.clone(),
            reason: "package manifest".to_string(),
        }];
        for path in workspace_source_files(ctx, &info.root_rel) {
            if path == info.manifest_rel {
                continue;
            }
            owned_files.push(SeedFile {
                path,
                reason: "workspace source".to_string(),
            });
        }
        out.push(FeatureSeed {
            title: format!("Node package `{package_name}`"),
            summary,
            kind: FeatureKind::Library,
            source: "node-package",
            confidence: FeatureConfidence::Medium,
            entry_path: info.manifest_rel.clone(),
            entry_symbol: Some(package_name),
            entry_route: None,
            entry_command: test_cmd.clone(),
            language: Language::JavaScript,
            tags: vec![
                "javascript".to_string(),
                "package".to_string(),
                "workspace".to_string(),
            ],
            owned_files,
            context_files,
            tests: Vec::new(),
            test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
        });
    }

    out
}

// ---- Workspace discovery ------------------------------------------------

fn discover_packages(root: &Path, root_pkg: Option<&Value>) -> Vec<PackageInfo> {
    let mut roots: BTreeSet<String> = BTreeSet::new();
    if root_pkg.is_some() {
        roots.insert(String::new()); // "" represents repo root
    }
    let patterns = workspace_patterns(root, root_pkg);
    let excludes: Vec<String> = patterns
        .iter()
        .filter(|p| p.starts_with('!'))
        .filter_map(|p| normalize_workspace_pattern(&p[1..]))
        .collect();
    for include in patterns.iter().filter(|p| !p.starts_with('!')) {
        for package_root in expand_workspace_pattern(root, include) {
            roots.insert(package_root);
        }
    }
    let mut out: Vec<PackageInfo> = Vec::new();
    for r in roots
        .into_iter()
        .filter(|p| !is_excluded_workspace(p, &excludes))
    {
        let manifest_rel = if r.is_empty() {
            "package.json".to_string()
        } else {
            format!("{r}/package.json")
        };
        let abs = root.join(&manifest_rel);
        if !is_safe_file(root, &abs) {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&abs) else {
            continue;
        };
        let Ok(pkg) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if !pkg.is_object() {
            continue;
        }
        out.push(PackageInfo {
            root_rel: r,
            manifest_rel,
            pkg,
        });
    }
    out
}

fn workspace_patterns(root: &Path, root_pkg: Option<&Value>) -> Vec<String> {
    let mut patterns: BTreeSet<String> = BTreeSet::new();
    if let Some(pkg) = root_pkg {
        for p in package_workspace_patterns(pkg) {
            patterns.insert(p);
        }
    }
    let pnpm_ws = root.join("pnpm-workspace.yaml");
    if is_safe_file(root, &pnpm_ws)
        && let Ok(raw) = fs::read_to_string(&pnpm_ws)
    {
        for p in parse_pnpm_workspace(&raw) {
            patterns.insert(p);
        }
    }
    // Convention fallback: ONLY when no explicit workspace declaration
    // exists (either as `workspaces` in package.json or in pnpm-workspace.yaml).
    // An incidental `packages/stray/package.json` in a repo that declares
    // `workspaces: ["apps/*"]` should NOT be treated as a workspace — that
    // would inflate `list_features` with seeds for sidecar directories the
    // author didn't opt in to.
    let has_explicit = patterns.iter().any(|p| !p.starts_with('!'));
    if !has_explicit {
        for fallback in ["packages", "apps", "extensions", "plugins"] {
            if is_safe_dir(root, &root.join(fallback)) {
                patterns.insert(format!("{fallback}/*"));
            }
        }
    }
    patterns.into_iter().collect()
}

fn package_workspace_patterns(pkg: &Value) -> Vec<String> {
    let workspaces = match pkg.get("workspaces") {
        Some(v) => v,
        None => return Vec::new(),
    };
    if let Some(arr) = workspaces.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    if let Some(obj) = workspaces.as_object()
        && let Some(arr) = obj.get("packages").and_then(|v| v.as_array())
    {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    Vec::new()
}

/// Hand-rolled `pnpm-workspace.yaml` parser. Avoids pulling a YAML crate for
/// a 5-line file. Reads the top-level `packages:` block and returns the
/// `- <path>` entries (quoted or unquoted). Mirrors clawpatch's `parsePnpmWorkspace`.
fn parse_pnpm_workspace(source: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut in_packages = false;
    let key_re = Regex::new(r"^\s*-\s*['\x22]?([^'\x22\s]+)['\x22]?\s*$").unwrap();
    for raw_line in source.lines() {
        let line = match raw_line.find('#') {
            Some(i) => &raw_line[..i],
            None => raw_line,
        };
        // Top-level key changes when the first column is non-whitespace.
        if line.starts_with(|c: char| !c.is_whitespace() && c != '\u{FEFF}') {
            in_packages = line.trim_start().starts_with("packages:");
            continue;
        }
        if !in_packages {
            continue;
        }
        if let Some(cap) = key_re.captures(line)
            && let Some(m) = cap.get(1)
        {
            patterns.push(m.as_str().to_string());
        }
    }
    patterns
}

fn normalize_workspace_pattern(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim();
    let stripped = trimmed
        .strip_suffix("/package.json")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let normalized = stripped.replace('\\', "/");
    if normalized.starts_with('/') || normalized.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(normalized)
}

fn is_excluded_workspace(package_root: &str, excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|p| workspace_pattern_matches(p, package_root))
}

fn workspace_pattern_matches(pattern: &str, package_root: &str) -> bool {
    if pattern == package_root {
        return true;
    }
    if has_workspace_glob(pattern) {
        return glob_segments_match(
            &pattern.split('/').collect::<Vec<_>>(),
            &package_root.split('/').collect::<Vec<_>>(),
        );
    }
    if let Some(parent) = pattern.strip_suffix("/**") {
        return path_matches_prefix(package_root, parent);
    }
    if let Some(parent) = pattern.strip_suffix("/*") {
        if !path_matches_prefix(package_root, parent) {
            return false;
        }
        return package_root[parent.len() + 1..].split('/').count() == 1;
    }
    false
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn has_workspace_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn glob_segments_match(pattern: &[&str], candidate: &[&str]) -> bool {
    let Some((segment, remaining_pattern)) = pattern.split_first() else {
        return candidate.is_empty();
    };
    if *segment == "**" {
        if glob_segments_match(remaining_pattern, candidate) {
            return true;
        }
        if !candidate.is_empty() && glob_segments_match(pattern, &candidate[1..]) {
            return true;
        }
        return false;
    }
    let Some((candidate_segment, remaining_candidate)) = candidate.split_first() else {
        return false;
    };
    if !glob_segment_matches(segment, candidate_segment) {
        return false;
    }
    glob_segments_match(remaining_pattern, remaining_candidate)
}

fn glob_segment_matches(segment: &str, candidate: &str) -> bool {
    // Build a tiny regex for each segment. `*` → `[^/]*`, `?` → `[^/]`,
    // everything else literal.
    let mut re = String::from("^");
    for c in segment.chars() {
        match c {
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    Regex::new(&re).ok().is_some_and(|r| r.is_match(candidate))
}

fn expand_workspace_pattern(root: &Path, pattern: &str) -> Vec<String> {
    let Some(normalized) = normalize_workspace_pattern(pattern) else {
        return Vec::new();
    };
    if normalized.is_empty() || normalized == "." {
        return vec![String::new()];
    }
    if let Some(prefix) = normalized.strip_suffix("/**")
        && !has_workspace_glob(prefix)
    {
        return discover_package_roots(root, prefix, 4);
    }
    if let Some(parent) = normalized.strip_suffix("/*")
        && !has_workspace_glob(parent)
    {
        let mut out: Vec<String> = Vec::new();
        for entry in safe_directory_entries(root, parent) {
            let candidate = format!("{parent}/{entry}");
            if is_safe_file(root, &root.join(&candidate).join("package.json")) {
                out.push(candidate);
            }
        }
        out.sort();
        return out;
    }
    if has_workspace_glob(&normalized) {
        return expand_workspace_glob(root, &normalized);
    }
    if is_safe_dir(root, &root.join(&normalized))
        && is_safe_file(root, &root.join(&normalized).join("package.json"))
    {
        return vec![normalized];
    }
    Vec::new()
}

fn expand_workspace_glob(root: &Path, pattern: &str) -> Vec<String> {
    let segments: Vec<&str> = pattern.split('/').collect();
    let mut out: Vec<String> = Vec::new();
    visit_glob(root, "", &segments, &mut out);
    out.sort();
    out.dedup();
    out
}

fn visit_glob(root: &Path, base: &str, remaining: &[&str], out: &mut Vec<String>) {
    let Some((segment, rest)) = remaining.split_first() else {
        if !base.is_empty()
            && is_safe_dir(root, &root.join(base))
            && is_safe_file(root, &root.join(base).join("package.json"))
        {
            out.push(base.to_string());
        }
        return;
    };
    if !has_workspace_glob(segment) {
        let next_base = if base.is_empty() {
            segment.to_string()
        } else {
            format!("{base}/{segment}")
        };
        visit_glob(root, &next_base, rest, out);
        return;
    }
    if *segment == "**" {
        // ** matches zero segments here…
        visit_glob(root, base, rest, out);
        // …or one segment that recurses with the same pattern.
        for entry in safe_directory_entries(root, base) {
            let next_base = if base.is_empty() {
                entry.clone()
            } else {
                format!("{base}/{entry}")
            };
            visit_glob(root, &next_base, remaining, out);
        }
        return;
    }
    for entry in safe_directory_entries(root, base) {
        if !glob_segment_matches(segment, &entry) {
            continue;
        }
        let next_base = if base.is_empty() {
            entry
        } else {
            format!("{base}/{entry}")
        };
        visit_glob(root, &next_base, rest, out);
    }
}

fn discover_package_roots(root: &Path, prefix: &str, max_depth: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    discover_into(root, prefix, max_depth, &mut out);
    out.sort();
    out.dedup();
    out
}

fn discover_into(root: &Path, prefix: &str, remaining_depth: usize, out: &mut Vec<String>) {
    if should_skip(prefix) {
        return;
    }
    if is_safe_file(root, &root.join(prefix).join("package.json")) {
        out.push(prefix.to_string());
    }
    if remaining_depth == 0 {
        return;
    }
    for entry in safe_directory_entries(root, prefix) {
        let next = if prefix.is_empty() {
            entry
        } else {
            format!("{prefix}/{entry}")
        };
        discover_into(root, &next, remaining_depth - 1, out);
    }
}

/// Enumerate immediate child directories under `<root>/<prefix>`, honoring
/// `.gitignore`. Workspace discovery (`packages/*`, `apps/*`, …) drives
/// `find_feature` routing, so a gitignored workspace must NOT be surfaced
/// as a feature — it'd point at files the structural indexer skipped.
/// Uses `ignore::WalkBuilder` with `max_depth(1)` to get gitignore-aware
/// listing in one shot. Falls back to plain `fs::read_dir` only if the
/// walker fails to construct.
fn safe_directory_entries(root: &Path, prefix: &str) -> Vec<String> {
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    if !is_safe_dir(root, &dir) {
        return Vec::new();
    }
    let walker = ignore::WalkBuilder::new(&dir)
        .max_depth(Some(1))
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .build();
    let mut out: Vec<String> = Vec::new();
    for entry in walker.flatten() {
        // WalkBuilder yields the root dir itself at depth 0; skip it.
        if entry.path() == dir {
            continue;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if should_skip(&rel) {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out
}

// ---- Per-package helpers ------------------------------------------------

fn package_scripts(pkg: &Value) -> serde_json::Map<String, Value> {
    pkg.get("scripts")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Caller contract: only invoked for workspace members (`!info.is_root()`).
/// The root package never emits a `node-package` seed, so the root branch
/// is unreachable and intentionally absent.
fn package_display_name(info: &PackageInfo) -> String {
    debug_assert!(!info.is_root(), "package_display_name called for root info");
    if let Some(name) = info.pkg.get("name").and_then(|v| v.as_str())
        && !name.is_empty()
    {
        return name.to_string();
    }
    info.root_rel
        .rsplit_once('/')
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_else(|| info.root_rel.clone())
}

fn detect_node_package_manager(root: &Path) -> NodePm {
    if is_safe_file(root, &root.join("pnpm-lock.yaml"))
        || is_safe_file(root, &root.join("pnpm-workspace.yaml"))
    {
        return NodePm::Pnpm;
    }
    if is_safe_file(root, &root.join("yarn.lock")) {
        return NodePm::Yarn;
    }
    if is_safe_file(root, &root.join("bun.lockb")) {
        return NodePm::Bun;
    }
    NodePm::Npm
}

fn script_command(pm: NodePm, package_root: &str, script: &str) -> String {
    if package_root.is_empty() || package_root == "." {
        return match pm {
            NodePm::Npm => format!("npm run {script}"),
            NodePm::Pnpm => format!("pnpm {script}"),
            NodePm::Yarn => format!("yarn {script}"),
            NodePm::Bun => format!("bun run {script}"),
        };
    }
    match pm {
        NodePm::Pnpm => format!("pnpm --dir {package_root} {script}"),
        NodePm::Yarn => format!("yarn --cwd {package_root} {script}"),
        NodePm::Bun => format!("bun --cwd {package_root} run {script}"),
        NodePm::Npm => format!("npm --prefix {package_root} run {script}"),
    }
}

/// Resolve a `bin` entry path, mapping `dist/foo.js` → `src/foo.ts` when the
/// TypeScript source exists. Falls back to the declared dist path when no
/// source candidate exists on disk. Lets agents edit the source instead of
/// the build artifact.
fn resolve_package_bin_entry(root: &Path, package_root: &str, path: &str) -> String {
    let normalized = normalize_package_path(path);
    let source_candidate = source_candidate_for_generated_bin(&normalized);
    let candidate = package_relative_path(
        package_root,
        source_candidate.as_deref().unwrap_or(&normalized),
    );
    if source_candidate.is_none() {
        return candidate;
    }
    if is_safe_file(root, &root.join(&candidate)) {
        candidate
    } else {
        package_relative_path(package_root, &normalized)
    }
}

fn source_candidate_for_generated_bin(path: &str) -> Option<String> {
    // Three common output dirs: `dist/` (TS / Rollup / esbuild), `build/`
    // (Babel default), `lib/` (older Babel / TypeScript libraries that
    // publish via `lib/`). Each one maps back to `src/<stem>.ts`.
    let stripped = path
        .strip_prefix("dist/")
        .or_else(|| path.strip_prefix("build/"))
        .or_else(|| path.strip_prefix("lib/"))?;
    let dot = stripped.rfind('.')?;
    let (stem, ext) = stripped.split_at(dot);
    if !matches!(ext, ".js" | ".mjs" | ".cjs") {
        return None;
    }
    Some(format!("src/{stem}.ts"))
}

fn normalize_package_path(path: &str) -> String {
    let p: PathBuf = PathBuf::from(path);
    let s = p.to_string_lossy().into_owned();
    s.replace('\\', "/").trim_start_matches("./").to_string()
}

fn package_relative_path(package_root: &str, path: &str) -> String {
    let stripped = path.trim_start_matches("./");
    if package_root.is_empty() || package_root == "." {
        return stripped.to_string();
    }
    format!("{package_root}/{stripped}")
}

/// Walk a workspace's directory and return its source files (cap 2_000).
/// Used to populate the `node-package` seed's `owned_files` so storage's
/// exact-match `features_for_file` query resolves any workspace-member
/// file to the package feature. Honors `.gitignore` and project
/// `[index].exclude_patterns` via `walk_files`. Filters out test/spec
/// files, type declarations, and generated bundles — those belong on
/// other seeds (or not at all). Excludes the manifest itself; callers
/// dedupe it back in.
fn workspace_source_files(ctx: &MapperContext, workspace_root: &str) -> Vec<String> {
    const CAP: usize = 2_000;
    let root = ctx.root;
    let dir = if workspace_root.is_empty() {
        root.to_path_buf()
    } else {
        root.join(workspace_root)
    };
    if !is_safe_dir(root, &dir) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for rel in walk_files(root, &dir, 10_000, ctx.excludes) {
        if !is_reviewable_node_source(&rel) {
            continue;
        }
        out.push(rel);
        if out.len() >= CAP {
            break;
        }
    }
    out.sort();
    out
}

/// Match what clawpatch's `isReviewableNodeSourceFile` accepts but without
/// porting the source-group seeds. Used by `workspace_source_files` to
/// attach a sensible owned-file set to each workspace feature for routing.
fn is_reviewable_node_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let has_source_ext = ends_with_any(
        &lower,
        &[".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"],
    );
    if !has_source_ext {
        return false;
    }
    if ends_with_any(
        &lower,
        &[
            ".test.ts",
            ".test.tsx",
            ".test.js",
            ".test.jsx",
            ".test.mts",
            ".test.cts",
            ".test.mjs",
            ".test.cjs",
            ".spec.ts",
            ".spec.tsx",
            ".spec.js",
            ".spec.jsx",
            ".spec.mts",
            ".spec.cts",
            ".spec.mjs",
            ".spec.cjs",
        ],
    ) {
        return false;
    }
    if lower.ends_with(".d.ts") || lower.ends_with(".d.cts") || lower.ends_with(".d.mts") {
        return false;
    }
    for segment in path.split('/') {
        if matches!(segment, "__fixtures__" | "fixtures" | "testdata") {
            return false;
        }
    }
    let basename = path.rsplit_once('/').map(|(_, tail)| tail).unwrap_or(path);
    if basename.contains(".generated.") || basename.contains(".gen.") {
        return false;
    }
    true
}

fn next_app_routes(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let app_dir = root.join("app");
    if !is_safe_dir(root, &app_dir) {
        return Ok(out);
    }
    for rel in walk_files(root, &app_dir, 5_000, ctx.excludes) {
        let is_page = ends_with_any(&rel, &["/page.tsx", "/page.ts", "/page.jsx", "/page.js"]);
        let is_route = ends_with_any(
            &rel,
            &["/route.tsx", "/route.ts", "/route.jsx", "/route.js"],
        );
        if !is_page && !is_route {
            continue;
        }
        let inside_app = rel.strip_prefix("app/").unwrap_or(&rel);
        let segments: Vec<&str> = inside_app
            .rsplit_once('/')
            .map(|(head, _)| head.split('/').collect())
            .unwrap_or_default();
        let url = if segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", segments.join("/"))
        };
        let language = language_for_entry(&rel);
        out.push(FeatureSeed {
            title: format!("Next.js {} `{url}`", if is_page { "page" } else { "route" }),
            summary: format!("Next.js app router file at {rel}"),
            kind: FeatureKind::Route,
            source: if is_page {
                "next-app-page"
            } else {
                "next-app-route"
            },
            confidence: FeatureConfidence::High,
            entry_path: rel.clone(),
            entry_symbol: None,
            entry_route: Some(url),
            entry_command: None,
            language,
            tags: vec![
                if language == Language::TypeScript {
                    "typescript"
                } else {
                    "javascript"
                }
                .to_string(),
                "framework:next".to_string(),
                "route".to_string(),
            ],
            owned_files: Vec::new(),
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
        });
    }
    Ok(out)
}

fn next_pages_routes(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let pages_dir = root.join("pages");
    if !is_safe_dir(root, &pages_dir) {
        return Ok(out);
    }
    for rel in walk_files(root, &pages_dir, 5_000, ctx.excludes) {
        if !ends_with_any(&rel, &[".tsx", ".ts", ".jsx", ".js"]) {
            continue;
        }
        let language = language_for_entry(&rel);
        // Path is the file path under pages/, stripped of extension and
        // /index.
        let inside_pages = rel.strip_prefix("pages/").unwrap_or(&rel);
        let stripped = inside_pages
            .rsplit_once('.')
            .map(|(head, _)| head)
            .unwrap_or(&rel);
        let url_part = stripped.trim_end_matches("/index");
        let url = if url_part.is_empty() {
            "/".to_string()
        } else {
            format!("/{url_part}")
        };
        out.push(FeatureSeed {
            title: format!("Next.js page `{url}`"),
            summary: format!("Next.js pages-router file at {rel}"),
            kind: FeatureKind::Route,
            source: "next-pages-route",
            confidence: FeatureConfidence::High,
            entry_path: rel.clone(),
            entry_symbol: None,
            entry_route: Some(url),
            entry_command: None,
            language,
            tags: vec![
                if language == Language::TypeScript {
                    "typescript"
                } else {
                    "javascript"
                }
                .to_string(),
                "framework:next".to_string(),
                "route".to_string(),
            ],
            owned_files: Vec::new(),
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
        });
    }
    Ok(out)
}

fn ends_with_any(s: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|sfx| s.ends_with(sfx))
}

/// Scan `src/` and `app/` for React Router `<Route path="..." element={<C/>}>`
/// declarations. Each matched `<Route>` becomes a `route` feature, keyed
/// by the path. The entry file is the route-declaration source — we
/// don't resolve the component back to its import here; that would
/// require parsing TS/JSX imports, and the route declaration is the
/// most reliable single anchor for "what handles /users?" agent
/// questions.
fn react_router_routes(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    // <Route path="..." ... element={<Component
    let route_re = Regex::new(
        r#"<Route\s+[^>]*path=(?:["']([^"']+)["'])[^>]*element=\{\s*<([A-Z][A-Za-z0-9_]*)"#,
    )?;
    let mut framework_components: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();
    framework_components.insert("Navigate");
    framework_components.insert("Outlet");
    framework_components.insert("Fragment");
    framework_components.insert("Suspense");

    for prefix in ["src", "app"] {
        let scan_dir = root.join(prefix);
        if !is_safe_dir(root, &scan_dir) {
            continue;
        }
        let files = walk_files(root, &scan_dir, 10_000, ctx.excludes);
        for rel in files {
            if !(rel.ends_with(".tsx")
                || rel.ends_with(".ts")
                || rel.ends_with(".jsx")
                || rel.ends_with(".js"))
            {
                continue;
            }
            // Skip test/declaration files — agents asking "what route
            // handles /users?" want the real declaration site.
            if rel.ends_with(".test.tsx")
                || rel.ends_with(".test.ts")
                || rel.ends_with(".test.jsx")
                || rel.ends_with(".test.js")
                || rel.ends_with(".spec.tsx")
                || rel.ends_with(".spec.ts")
                || rel.ends_with(".spec.jsx")
                || rel.ends_with(".spec.js")
                || rel.ends_with(".d.ts")
            {
                continue;
            }
            let abs = root.join(&rel);
            let Ok(raw) = fs::read_to_string(&abs) else {
                continue;
            };
            if !raw.contains("<Route") {
                continue;
            }
            for cap in route_re.captures_iter(&raw) {
                let path = match cap.get(1).map(|m| m.as_str()) {
                    Some(p) if !p.is_empty() => p.to_string(),
                    _ => continue,
                };
                let component = cap
                    .get(2)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                if component.is_empty() || framework_components.contains(component.as_str()) {
                    continue;
                }
                let language = language_for_entry(&rel);
                out.push(FeatureSeed {
                    title: format!("React route `{path}`"),
                    summary: format!(
                        "React Router route '{path}' rendered by <{component}/> (declared in {rel})"
                    ),
                    kind: FeatureKind::Route,
                    source: "react-router-route",
                    confidence: FeatureConfidence::High,
                    entry_path: rel.clone(),
                    entry_symbol: Some(component),
                    entry_route: Some(path),
                    entry_command: None,
                    language,
                    tags: vec![
                        if language == Language::TypeScript {
                            "typescript"
                        } else {
                            "javascript"
                        }
                        .to_string(),
                        "framework:react-router".to_string(),
                        "react".to_string(),
                        "route".to_string(),
                    ],
                    owned_files: Vec::new(),
                    context_files: Vec::new(),
                    tests: Vec::new(),
                    test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn package_bin_emits_cli_seed() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"acme","bin":{"acme":"./bin/cli.js"}}"#,
        );
        write(dir.path(), "bin/cli.js", "#!/usr/bin/env node\n");
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "package-json-bin")
            .expect("npm bin seed");
        assert_eq!(s.entry_command.as_deref(), Some("acme"));
    }

    #[test]
    fn react_router_routes_extracted() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"acme","dependencies":{"react":"^18.0.0","react-router-dom":"^6.0.0"}}"#,
        );
        write(
            dir.path(),
            "src/App.tsx",
            r#"import { Route, Routes } from "react-router-dom";
const App = () => (
  <Routes>
    <Route path="/users" element={<UsersPage />} />
    <Route path="/settings" element={<SettingsPage />} />
    <Route index element={<Outlet />} />
  </Routes>
);
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let react_seeds: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "react-router-route")
            .collect();
        let routes: Vec<&str> = react_seeds
            .iter()
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.contains(&"/users"),
            "expected /users route, got {routes:?}"
        );
        assert!(
            routes.contains(&"/settings"),
            "expected /settings route, got {routes:?}"
        );
        // Framework components (Outlet etc.) must not produce features.
        let bad: Vec<&str> = react_seeds
            .iter()
            .filter_map(|s| s.entry_symbol.as_deref())
            .filter(|s| matches!(*s, "Outlet" | "Navigate" | "Fragment" | "Suspense"))
            .collect();
        assert!(bad.is_empty(), "framework component leaked, got {bad:?}");
    }

    #[test]
    fn react_router_skipped_when_react_missing() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"acme","dependencies":{}}"#,
        );
        write(
            dir.path(),
            "src/App.tsx",
            r#"<Route path="/x" element={<X />} />"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "react-router-route"),
            "react-router mapper ran without a react dep"
        );
    }

    #[test]
    fn next_app_page_extracted() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "app/dashboard/page.tsx",
            "export default function Page() { return null }",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "next-app-page")
            .expect("next app page");
        assert_eq!(s.entry_route.as_deref(), Some("/dashboard"));
        assert_eq!(s.language, Language::TypeScript);
    }

    // ---- Workspace decomposition ----------------------------------------

    #[test]
    fn npm_workspaces_array_emits_per_package_seeds() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/api/package.json",
            r#"{"name":"@acme/api","scripts":{"test":"jest"}}"#,
        );
        write(
            dir.path(),
            "packages/web/package.json",
            r#"{"name":"@acme/web","bin":{"acme-web":"./bin/run.js"},"scripts":{"test":"vitest"}}"#,
        );
        write(
            dir.path(),
            "packages/web/bin/run.js",
            "#!/usr/bin/env node\n",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();

        let pkg_seeds: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .collect();
        let names: Vec<&str> = pkg_seeds
            .iter()
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(
            names.contains(&"@acme/api"),
            "expected @acme/api workspace seed, got {names:?}"
        );
        assert!(
            names.contains(&"@acme/web"),
            "expected @acme/web workspace seed, got {names:?}"
        );

        // The api workspace seed carries a per-package test command on
        // `entry_command`. The manifest must NOT appear in tests[] —
        // `SeedTest.path` is documented as a test FILE and the orchestrator
        // inserts every tests[] row as `role: Test`; populating it with the
        // package.json would surface the manifest as a test in
        // `feature_bundle` output.
        let api = pkg_seeds
            .iter()
            .find(|s| s.entry_symbol.as_deref() == Some("@acme/api"))
            .unwrap();
        assert_eq!(api.entry_path, "packages/api/package.json");
        assert!(
            api.tests.is_empty(),
            "node-package seed must not populate tests[] with the manifest"
        );
        assert_eq!(
            api.entry_command.as_deref(),
            Some("npm --prefix packages/api run test")
        );
    }

    #[test]
    fn pnpm_workspace_yaml_parsed() {
        let dir = tempdir().unwrap();
        write(dir.path(), "package.json", r#"{"name":"monorepo"}"#);
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages:\n  - 'apps/*'\n  - \"libs/*\"\n",
        );
        write(
            dir.path(),
            "apps/admin/package.json",
            r#"{"name":"@acme/admin","scripts":{"test":"vitest"}}"#,
        );
        write(
            dir.path(),
            "libs/util/package.json",
            r#"{"name":"@acme/util"}"#,
        );
        // pnpm-lock.yaml flips pm detection.
        write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: '9'\n");

        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(names.contains(&"@acme/admin"));
        assert!(names.contains(&"@acme/util"));

        // pnpm package-manager detection drives the test-command formatting,
        // surfaced on `entry_command`.
        let admin = seeds
            .iter()
            .find(|s| s.entry_symbol.as_deref() == Some("@acme/admin"))
            .unwrap();
        assert_eq!(
            admin.entry_command.as_deref(),
            Some("pnpm --dir apps/admin test")
        );
    }

    #[test]
    fn fallback_workspace_prefixes_discovered() {
        // No explicit `workspaces` declaration; a `packages/` directory with
        // child manifests should still produce per-package seeds.
        let dir = tempdir().unwrap();
        write(dir.path(), "package.json", r#"{"name":"monorepo"}"#);
        write(
            dir.path(),
            "packages/lib/package.json",
            r#"{"name":"@acme/lib"}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(
            names.contains(&"@acme/lib"),
            "fallback packages/ prefix not discovered, got {names:?}"
        );
    }

    #[test]
    fn bin_source_back_resolves_to_typescript() {
        // package.json points at dist/cli.js, but a src/cli.ts exists. The
        // bin seed's entry path should resolve to the source, not the dist.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"acme","bin":{"acme":"./dist/cli.js"}}"#,
        );
        write(dir.path(), "src/cli.ts", "// source\n");
        write(dir.path(), "dist/cli.js", "// generated\n");
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "package-json-bin")
            .expect("npm bin seed");
        assert_eq!(s.entry_path, "src/cli.ts");
        assert_eq!(s.language, Language::TypeScript);
    }

    #[test]
    fn bin_source_back_falls_back_when_no_typescript() {
        // dist/foo.js with no src/foo.ts → keep the dist path. We don't
        // skip the seed; the bin is still real, we just don't have a
        // better entry to point at.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"acme","bin":{"acme":"./dist/cli.js"}}"#,
        );
        write(dir.path(), "dist/cli.js", "// only artifact\n");
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "package-json-bin")
            .expect("npm bin seed");
        assert_eq!(s.entry_path, "dist/cli.js");
    }

    #[test]
    fn workspace_excludes_negation_pattern() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*","!packages/legacy"]}"#,
        );
        write(
            dir.path(),
            "packages/api/package.json",
            r#"{"name":"@acme/api"}"#,
        );
        write(
            dir.path(),
            "packages/legacy/package.json",
            r#"{"name":"@acme/legacy"}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(names.contains(&"@acme/api"));
        assert!(
            !names.contains(&"@acme/legacy"),
            "negation pattern not honored, got {names:?}"
        );
    }

    #[test]
    fn root_package_does_not_emit_node_package_seed() {
        // node-package seed is workspace-only. The root keeps the existing
        // per-script and per-bin behavior; one less list_features row.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"acme","scripts":{"build":"tsc"}}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "node-package"),
            "root-only repo should not produce a node-package seed"
        );
        // But the script seed should still be there.
        assert!(seeds.iter().any(|s| s.source == "package-json-script"));
    }

    #[test]
    fn workspace_seed_owns_source_files_for_routing() {
        // Regression: `find_feature("packages/api/src/auth.ts")` had no
        // chance because the node-package seed only persisted the
        // manifest. The workspace walk now attaches .ts/.js/.tsx files as
        // owned so the storage exact-match query resolves any workspace
        // member to its package feature.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/api/package.json",
            r#"{"name":"@acme/api"}"#,
        );
        write(
            dir.path(),
            "packages/api/src/auth.ts",
            "export const auth = 1;\n",
        );
        write(
            dir.path(),
            "packages/api/src/helpers.ts",
            "export const helper = 1;\n",
        );
        // Spec/test files belong on a future test-suite surface, not the
        // package seed; .d.ts files are type-declaration stubs.
        write(
            dir.path(),
            "packages/api/src/auth.test.ts",
            "test('x', () => {});\n",
        );
        write(
            dir.path(),
            "packages/api/src/types.d.ts",
            "export interface X {}\n",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_symbol.as_deref() == Some("@acme/api"))
            .expect("api workspace seed");
        let owned: std::collections::BTreeSet<&str> =
            s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            owned.contains("packages/api/src/auth.ts"),
            "src/auth.ts missing from owned_files: {owned:?}"
        );
        assert!(owned.contains("packages/api/src/helpers.ts"));
        assert!(owned.contains("packages/api/package.json"));
        assert!(
            !owned.contains("packages/api/src/auth.test.ts"),
            "test files must not bleed into workspace owned set"
        );
        assert!(
            !owned.contains("packages/api/src/types.d.ts"),
            ".d.ts files must not bleed into workspace owned set"
        );
    }

    #[test]
    fn fallback_prefixes_skipped_when_explicit_workspaces_declared() {
        // Regression: an incidental `packages/stray/package.json` sitting
        // in a repo that declares `workspaces: ["apps/*"]` must NOT be
        // discovered as a workspace. The fallback prefix list
        // (`packages/`, `apps/`, …) only applies when no explicit
        // declaration was made.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["apps/*"]}"#,
        );
        write(
            dir.path(),
            "apps/web/package.json",
            r#"{"name":"@acme/web"}"#,
        );
        write(
            dir.path(),
            "packages/stray/package.json",
            r#"{"name":"@acme/stray"}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(names.contains(&"@acme/web"));
        assert!(
            !names.contains(&"@acme/stray"),
            "fallback prefix leaked when explicit workspaces declared: {names:?}"
        );
    }

    #[test]
    fn gitignored_workspace_is_excluded() {
        // Regression: a workspace matching `packages/*` but listed in
        // .gitignore must NOT be discovered as a feature. The structural
        // indexer skips its files via gitignore; surfacing a feature whose
        // entry_path points at gitignored content would route `find_feature`
        // to files that don't exist in the project's index.
        let dir = tempdir().unwrap();
        write(dir.path(), ".gitignore", "packages/internal-only/\n");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/visible/package.json",
            r#"{"name":"@acme/visible"}"#,
        );
        write(
            dir.path(),
            "packages/internal-only/package.json",
            r#"{"name":"@acme/internal"}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(names.contains(&"@acme/visible"));
        assert!(
            !names.contains(&"@acme/internal"),
            "gitignored workspace leaked into mapper output: {names:?}"
        );
    }

    #[test]
    fn workspace_packages_skip_common_scripts() {
        // Per the includeCommonScripts gate: only the root emits standalone
        // `npm script `build`` seeds. Workspace packages produce a single
        // `node-package` seed instead so a Turborepo doesn't get 30 build
        // seeds.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/api/package.json",
            r#"{"name":"@acme/api","scripts":{"build":"tsc","test":"jest"}}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let api_scripts: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "package-json-script")
            .filter(|s| s.entry_path == "packages/api/package.json")
            .collect();
        assert!(
            api_scripts.is_empty(),
            "workspace package leaked script seeds: {:?}",
            api_scripts.iter().map(|s| &s.title).collect::<Vec<_>>()
        );
    }
}
