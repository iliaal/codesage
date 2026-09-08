//! JavaScript / TypeScript mapper: workspace-aware `package.json` decomposition
//! (npm / yarn / pnpm `workspaces`, `pnpm-workspace.yaml`, fallback prefixes),
//! per-package `bin` and selected scripts (`start`/`build`/`test`/`lint`/
//! `typecheck`/`format`), Next.js `app/**/{page,route}.{ts,tsx,js,jsx}` and
//! `pages/**/*.{ts,tsx}`, React Router `<Route>` declarations.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use serde_json::Value;

use crate::mappers::shared::{
    AUTH_SENSITIVE_TAG, CommentSyntax, SOURCE_FILE_CAP, StringMode, collect_source_files,
    is_safe_dir, is_safe_file, list_dir_subdirs, read_to_string_bounded, route_is_auth_sensitive,
    should_skip, strip_comments, walk_files,
};
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
            read_to_string_bounded(&pkg_path)
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        } else {
            None
        };
        let packages = discover_packages(root, pkg_at_root.as_ref());
        let pm = detect_node_package_manager(root);
        let turbo = read_turbo_config(root);
        for info in &packages {
            seeds.extend(package_seeds_for(ctx, info, pm));
        }
        let scan_roots = next_scan_roots(&packages, root);
        for prefix in &scan_roots {
            seeds.extend(next_app_routes_at(ctx, prefix)?);
            seeds.extend(next_pages_routes_at(ctx, prefix)?);
        }
        if any_package_has_dep(&packages, pkg_at_root.as_ref(), has_react_dependency) {
            seeds.extend(react_router_routes(ctx, &packages)?);
            // Route-owned files must not also emit component seeds.
            seeds.extend(react_components(ctx, &packages, &seeds)?);
        }
        if any_package_has_dep(&packages, pkg_at_root.as_ref(), has_node_server_dependency) {
            seeds.extend(node_server_routes(ctx)?);
        }
        // Rewrite after all mappers have contributed their seeds.
        if let Some(turbo_cfg) = &turbo
            && turbo_cfg.has_test_task
        {
            apply_turbo_test_commands(&mut seeds, &packages);
        }
        seeds.retain(|s| ctx.allowed(&s.entry_path));
        Ok(seeds)
    }
}

/// Framework dependencies may belong to a workspace member rather than the root.
fn any_package_has_dep(
    packages: &[PackageInfo],
    root_pkg: Option<&Value>,
    predicate: fn(&Value) -> bool,
) -> bool {
    if root_pkg.is_some_and(predicate) {
        return true;
    }
    packages.iter().any(|p| predicate(&p.pkg))
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

/// Gate server scanning on framework dependencies, including known Hono adapters.
fn has_node_server_dependency(pkg: &Value) -> bool {
    let server_deps = [
        "express",
        "fastify",
        "hono",
        "@hono/node-server",
        "@hono/zod-validator",
    ];
    for field in ["dependencies", "devDependencies"] {
        let Some(map) = pkg.get(field).and_then(|v| v.as_object()) else {
            continue;
        };
        if server_deps.iter().any(|d| map.contains_key(*d)) {
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

/// Only the root emits common-script seeds; workspace members emit package and bin seeds.
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
                summary,
                source: "package-json-bin",
                confidence: FeatureConfidence::High,
                entry_command: Some(cmd.clone()),
                tags,
                context_files: vec![SeedFile {
                    path: info.manifest_rel.clone(),
                    reason: "package manifest".to_string(),
                }],
                test_prefixes: vec![
                    "test".to_string(),
                    "tests".to_string(),
                    "__tests__".to_string(),
                ],
                ..FeatureSeed::new(
                    FeatureKind::CliCommand,
                    language,
                    format!("npm bin `{cmd}`"),
                    entry_rel,
                )
            });
        }
    }

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
                summary: format!("package.json script `{name}: {command}`"),
                source: "package-json-script",
                entry_symbol: Some(name.clone()),
                entry_command: Some(name.clone()),
                tags: vec!["javascript".to_string(), "package-script".to_string()],
                context_files: vec![SeedFile {
                    path: info.manifest_rel.clone(),
                    reason: "package manifest".to_string(),
                }],
                ..FeatureSeed::new(
                    kind,
                    Language::JavaScript,
                    format!("npm script `{name}`"),
                    info.manifest_rel.clone(),
                )
            });
        }
    }

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
        // Keep the test command separate from test files: package.json is not a test.
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
        // Feature routing matches exact file paths, not workspace prefixes.
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
            summary,
            source: "node-package",
            entry_symbol: Some(package_name.clone()),
            // entry_command affects feature identity; test-script changes must not.
            test_command: test_cmd.clone(),
            tags: vec![
                "javascript".to_string(),
                "package".to_string(),
                "workspace".to_string(),
            ],
            owned_files,
            context_files,
            test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
            ..FeatureSeed::new(
                FeatureKind::Library,
                Language::JavaScript,
                format!("Node package `{package_name}`"),
                info.manifest_rel.clone(),
            )
        });
    }

    out
}

fn discover_packages(root: &Path, root_pkg: Option<&Value>) -> Vec<PackageInfo> {
    let mut roots: BTreeSet<String> = BTreeSet::new();
    if root_pkg.is_some() {
        roots.insert(String::new());
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
        let Ok(Some(raw)) = read_to_string_bounded(&abs) else {
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
        && let Ok(Some(raw)) = read_to_string_bounded(&pnpm_ws)
    {
        for p in parse_pnpm_workspace(&raw) {
            patterns.insert(p);
        }
    }
    // Fallback prefixes must not widen an explicit workspace include set.
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

/// Read quoted or unquoted list entries under the top-level `packages:` key.
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
        // `**` consumes zero or more directory segments.
        visit_glob(root, base, rest, out);
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

/// Enumerate child directory names using the structural indexer's ignore rules.
fn safe_directory_entries(root: &Path, prefix: &str) -> Vec<String> {
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    list_dir_subdirs(root, &dir, None)
        .into_iter()
        .filter_map(|rel| rel.rsplit('/').next().map(String::from))
        .collect()
}

fn package_scripts(pkg: &Value) -> serde_json::Map<String, Value> {
    pkg.get("scripts")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

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

/// Prefer an existing TypeScript source for generated bins; otherwise keep the declared path.
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

/// Collect bounded workspace-owned sources, honoring ignores and excluding support files.
fn workspace_source_files(ctx: &MapperContext, workspace_root: &str) -> Vec<String> {
    let root = ctx.root;
    let dir = if workspace_root.is_empty() {
        root.to_path_buf()
    } else {
        root.join(workspace_root)
    };
    collect_source_files(
        ctx,
        &[dir],
        10_000,
        has_node_source_ext,
        |rel| !is_reviewable_node_source(rel),
        SOURCE_FILE_CAP,
    )
}

fn has_node_source_ext(path: &str) -> bool {
    ends_with_any(
        &path.to_ascii_lowercase(),
        &[".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"],
    )
}

fn is_reviewable_node_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if !has_node_source_ext(path) {
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

fn next_app_routes_at(ctx: &MapperContext, package_rel: &str) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let pkg_root = if package_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(package_rel)
    };
    let app_dir = pkg_root.join("app");
    if !is_safe_dir(root, &app_dir) {
        return Ok(out);
    }
    let pkg_path_prefix = if package_rel.is_empty() {
        "app/".to_string()
    } else {
        format!("{}/app/", package_rel.trim_end_matches('/'))
    };
    for rel in walk_files(root, &app_dir, 5_000, ctx.excludes) {
        let is_page = ends_with_any(&rel, &["/page.tsx", "/page.ts", "/page.jsx", "/page.js"]);
        let is_route = ends_with_any(
            &rel,
            &["/route.tsx", "/route.ts", "/route.jsx", "/route.js"],
        );
        if !is_page && !is_route {
            continue;
        }
        let inside_app = rel.strip_prefix(&pkg_path_prefix).unwrap_or(&rel);
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
        let mut tags = vec![
            if language == Language::TypeScript {
                "typescript"
            } else {
                "javascript"
            }
            .to_string(),
            "framework:next".to_string(),
            "route".to_string(),
        ];
        if !package_rel.is_empty() {
            tags.push("workspace".to_string());
        }
        out.push(FeatureSeed {
            summary: if package_rel.is_empty() {
                format!("Next.js app router file at {rel}")
            } else {
                format!("Next.js app router file at {rel} (package {package_rel})")
            },
            source: if is_page {
                "next-app-page"
            } else {
                "next-app-route"
            },
            confidence: FeatureConfidence::High,
            entry_route: Some(url.clone()),
            tags,
            test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
            ..FeatureSeed::new(
                FeatureKind::Route,
                language,
                format!("Next.js {} `{url}`", if is_page { "page" } else { "route" }),
                rel.clone(),
            )
        });
    }
    Ok(out)
}

fn next_pages_routes_at(ctx: &MapperContext, package_rel: &str) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    let pkg_root = if package_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(package_rel)
    };
    let pages_dir = pkg_root.join("pages");
    if !is_safe_dir(root, &pages_dir) {
        return Ok(out);
    }
    let pkg_path_prefix = if package_rel.is_empty() {
        "pages/".to_string()
    } else {
        format!("{}/pages/", package_rel.trim_end_matches('/'))
    };
    for rel in walk_files(root, &pages_dir, 5_000, ctx.excludes) {
        if !ends_with_any(&rel, &[".tsx", ".ts", ".jsx", ".js"]) {
            continue;
        }
        // Next.js convention files are not URL routes.
        let basename = rel.rsplit('/').next().unwrap_or(&rel);
        let stem = basename.rsplit_once('.').map_or(basename, |(head, _)| head);
        if stem.starts_with('_') || stem == "middleware" {
            continue;
        }
        let language = language_for_entry(&rel);
        let inside_pages = rel.strip_prefix(&pkg_path_prefix).unwrap_or(&rel);
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
        let mut tags = vec![
            if language == Language::TypeScript {
                "typescript"
            } else {
                "javascript"
            }
            .to_string(),
            "framework:next".to_string(),
            "route".to_string(),
        ];
        if !package_rel.is_empty() {
            tags.push("workspace".to_string());
        }
        out.push(FeatureSeed {
            summary: if package_rel.is_empty() {
                format!("Next.js pages-router file at {rel}")
            } else {
                format!("Next.js pages-router file at {rel} (package {package_rel})")
            },
            source: "next-pages-route",
            confidence: FeatureConfidence::High,
            entry_route: Some(url.clone()),
            tags,
            test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
            ..FeatureSeed::new(
                FeatureKind::Route,
                language,
                format!("Next.js page `{url}`"),
                rel.clone(),
            )
        });
    }
    Ok(out)
}

/// Scan package roots and, when the root has no Next routes, conventional frontend dirs.
fn next_scan_roots(packages: &[PackageInfo], root: &Path) -> Vec<String> {
    let mut out: Vec<String> = packages
        .iter()
        .map(|p| {
            if p.is_root() {
                String::new()
            } else {
                p.root_rel.clone()
            }
        })
        .collect();
    if out.is_empty() {
        out.push(String::new());
    }
    let root_has_next =
        is_safe_dir(root, &root.join("app")) || is_safe_dir(root, &root.join("pages"));
    let root_already_scanned = out.iter().any(|s| s.is_empty());
    if !root_has_next && root_already_scanned {
        for guess in ["frontend", "client", "web", "ui"] {
            let candidate = root.join(guess);
            if !is_safe_dir(root, &candidate) {
                continue;
            }
            if is_safe_dir(root, &candidate.join("app"))
                || is_safe_dir(root, &candidate.join("pages"))
            {
                let s = guess.to_string();
                if !out.iter().any(|existing| existing == &s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

struct TurboConfig {
    has_test_task: bool,
}

fn read_turbo_config(root: &Path) -> Option<TurboConfig> {
    let path = root.join("turbo.json");
    if !is_safe_file(root, &path) {
        return None;
    }
    let raw = read_to_string_bounded(&path).ok().flatten()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    // Turbo 1.x uses `pipeline`, Turbo 2.x uses `tasks`. Either may
    // declare a `test` key.
    let has_test_task = ["pipeline", "tasks"].iter().any(|section| {
        value
            .get(section)
            .and_then(|v| v.as_object())
            .is_some_and(|m| m.contains_key("test"))
    });
    Some(TurboConfig { has_test_task })
}

/// Use Turbo for workspace tests; root tests retain their unfiltered command.
fn apply_turbo_test_commands(seeds: &mut [FeatureSeed], packages: &[PackageInfo]) {
    use std::collections::HashMap;
    let by_root: HashMap<&str, &Value> = packages
        .iter()
        .filter(|p| !p.is_root())
        .map(|p| (p.root_rel.as_str(), &p.pkg))
        .collect();
    for seed in seeds.iter_mut() {
        if seed.test_command.is_none() {
            continue;
        }
        let owning = by_root
            .iter()
            .filter(|(root_rel, _)| seed.entry_path.starts_with(*root_rel))
            .max_by_key(|(root_rel, _)| root_rel.len());
        let Some((_, pkg)) = owning else { continue };
        let Some(name) = pkg.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        seed.test_command = Some(format!("turbo run test --filter={name}"));
    }
}

fn ends_with_any(s: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|sfx| s.ends_with(sfx))
}

/// Anchor React Router features to route declarations; component imports are unresolved.
fn react_router_routes(ctx: &MapperContext, packages: &[PackageInfo]) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();
    // Extract attributes independently so JSX prop order does not matter.
    let path_attr_re = Regex::new(r#"\bpath\s*=\s*["']([^"']+)["']"#)?;
    let element_attr_re = Regex::new(r"\belement\s*=\s*\{\s*<([A-Z][A-Za-z0-9_]*)")?;

    let framework_components: std::collections::HashSet<&'static str> =
        ["Navigate", "Outlet", "Fragment", "Suspense"]
            .into_iter()
            .collect();

    let scan_prefixes: Vec<String> = react_router_scan_roots(packages);
    for prefix in &scan_prefixes {
        let scan_dir = if prefix.is_empty() {
            root.to_path_buf()
        } else {
            root.join(prefix)
        };
        if !is_safe_dir(root, &scan_dir) {
            continue;
        }
        for subdir in ["src", "app"] {
            let sub_root = scan_dir.join(subdir);
            if !is_safe_dir(root, &sub_root) {
                continue;
            }
            let files = walk_files(root, &sub_root, 10_000, ctx.excludes);
            for rel in files {
                if !(rel.ends_with(".tsx")
                    || rel.ends_with(".ts")
                    || rel.ends_with(".jsx")
                    || rel.ends_with(".js"))
                {
                    continue;
                }
                if is_node_test_or_decl(&rel) {
                    continue;
                }
                let abs = root.join(&rel);
                let Ok(Some(raw)) = read_to_string_bounded(&abs) else {
                    continue;
                };
                if !raw.contains("<Route") {
                    continue;
                }
                for body in extract_route_tag_bodies(&raw) {
                    let Some(path_cap) = path_attr_re.captures(&body) else {
                        continue;
                    };
                    let Some(elem_cap) = element_attr_re.captures(&body) else {
                        continue;
                    };
                    let path = path_cap
                        .get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let component = elem_cap
                        .get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    if path.is_empty()
                        || component.is_empty()
                        || framework_components.contains(component.as_str())
                    {
                        continue;
                    }
                    let language = language_for_entry(&rel);
                    out.push(FeatureSeed {
                        summary: format!(
                            "React Router route '{path}' rendered by <{component}/> (declared in {rel})"
                        ),
                        source: "react-router-route",
                        confidence: FeatureConfidence::High,
                        entry_symbol: Some(component),
                        entry_route: Some(path.clone()),
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
                        test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
                        ..FeatureSeed::new(
                            FeatureKind::Route,
                            language,
                            format!("React route `{path}`"),
                            rel.clone(),
                        )
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Track brace depth so nested JSX cannot close the outer Route tag.
/// ASCII delimiters keep slice boundaries valid in UTF-8 input.
fn extract_route_tag_bodies(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let needle = b"<Route";
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let Some(pos) = bytes[i..].windows(needle.len()).position(|w| w == needle) else {
            break;
        };
        let body_start = i + pos + needle.len();
        // Reject longer names such as Routes and RouteSwitch.
        if body_start >= bytes.len() {
            break;
        }
        let next = bytes[body_start];
        if !(next == b' ' || next == b'\t' || next == b'\n' || next == b'/' || next == b'>') {
            i = body_start;
            continue;
        }
        let mut j = body_start;
        let mut depth: i32 = 0;
        while j < bytes.len() {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' if depth > 0 => depth -= 1,
                b'>' if depth == 0 => break,
                _ => {}
            }
            j += 1;
        }
        if j > body_start
            && let Ok(body) = std::str::from_utf8(&bytes[body_start..j])
        {
            out.push(body.to_string());
        }
        i = j.max(body_start + 1);
    }
    out
}

/// Include every package and the repo root, even without a root package.json.
fn react_router_scan_roots(packages: &[PackageInfo]) -> Vec<String> {
    let mut out: Vec<String> = packages
        .iter()
        .map(|p| {
            if p.is_root() {
                String::new()
            } else {
                p.root_rel.clone()
            }
        })
        .collect();
    if out.is_empty() {
        out.push(String::new());
    }
    if !out.iter().any(|s| s.is_empty()) {
        out.push(String::new());
    }
    out
}

/// Require JSX extensions to avoid inferring components from ordinary TS/JS files.
fn is_react_component_file(rel: &str) -> bool {
    if !(rel.ends_with(".tsx") || rel.ends_with(".jsx")) {
        return false;
    }
    if is_node_test_or_decl(rel) {
        return false;
    }
    is_reviewable_react_support_path(rel)
}

fn is_reviewable_react_support_path(rel: &str) -> bool {
    let support_segments = [
        "/stories/",
        "/__stories__/",
        "/.storybook/",
        "/fixtures/",
        "/__fixtures__/",
        "/testdata/",
    ];
    if support_segments.iter().any(|s| rel.contains(s)) {
        return false;
    }
    let support_prefixes = [
        "stories/",
        "__stories__/",
        ".storybook/",
        "fixtures/",
        "__fixtures__/",
        "testdata/",
    ];
    if support_prefixes.iter().any(|p| rel.starts_with(p)) {
        return false;
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    if base.contains(".stories.") || base.contains(".story.") {
        return false;
    }
    true
}

fn react_component_name(rel: &str) -> String {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.rsplit_once('.')
        .map(|(stem, _)| stem.to_string())
        .unwrap_or_else(|| base.to_string())
}

/// Map at most 100 component files per package, excluding files already owned by routes.
fn react_components(
    ctx: &MapperContext,
    packages: &[PackageInfo],
    existing_seeds: &[FeatureSeed],
) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();

    const ROUTE_SOURCES_TO_EXCLUDE: &[&str] = &[
        "react-router-route",
        "next-app-route",
        "next-app-page",
        "next-pages-route",
    ];
    let route_owned: std::collections::HashSet<String> = existing_seeds
        .iter()
        .filter(|s| ROUTE_SOURCES_TO_EXCLUDE.contains(&s.source))
        .map(|s| s.entry_path.clone())
        .collect();

    const COMPONENTS_CAP_PER_PACKAGE: usize = 100;
    const COMPONENT_SCAN_DIRS: &[&str] = &["src/pages", "src/components", "components"];

    for prefix in react_router_scan_roots(packages) {
        let scan_dir = if prefix.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&prefix)
        };
        if !is_safe_dir(root, &scan_dir) {
            continue;
        }
        let mut per_pkg_count = 0usize;
        let mut emitted_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
        for subdir in COMPONENT_SCAN_DIRS {
            if per_pkg_count >= COMPONENTS_CAP_PER_PACKAGE {
                break;
            }
            let sub_root = scan_dir.join(subdir);
            if !is_safe_dir(root, &sub_root) {
                continue;
            }
            let files = walk_files(root, &sub_root, 10_000, ctx.excludes);
            for rel in files {
                if per_pkg_count >= COMPONENTS_CAP_PER_PACKAGE {
                    break;
                }
                if !is_react_component_file(&rel) {
                    continue;
                }
                if route_owned.contains(&rel) {
                    continue;
                }
                if !emitted_paths.insert(rel.clone()) {
                    continue;
                }
                let component_name = react_component_name(&rel);
                let language = language_for_entry(&rel);
                let lang_tag = if language == Language::TypeScript {
                    "typescript"
                } else {
                    "javascript"
                };
                out.push(FeatureSeed {
                    summary: format!("React component declared in {rel}"),
                    source: "react-component",
                    entry_symbol: Some(component_name.clone()),
                    tags: vec![
                        lang_tag.to_string(),
                        "react".to_string(),
                        "react-component".to_string(),
                    ],
                    owned_files: vec![SeedFile {
                        path: rel.clone(),
                        reason: "component implementation".to_string(),
                    }],
                    test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
                    ..FeatureSeed::new(
                        FeatureKind::Library,
                        language,
                        format!("React component `{component_name}`"),
                        rel.clone(),
                    )
                });
                per_pkg_count += 1;
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeServerFramework {
    Express,
    Fastify,
    Hono,
}

impl NodeServerFramework {
    fn tag(self) -> &'static str {
        match self {
            Self::Express => "framework:express",
            Self::Fastify => "framework:fastify",
            Self::Hono => "framework:hono",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Express => "Express",
            Self::Fastify => "Fastify",
            Self::Hono => "Hono",
        }
    }
    fn source(self) -> &'static str {
        match self {
            Self::Express => "express-route",
            Self::Fastify => "fastify-route",
            Self::Hono => "hono-route",
        }
    }
}

/// Require a same-file framework constructor before treating receiver calls as routes.
/// Cross-file mount prefixes remain unresolved.
fn node_server_routes(ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out = Vec::new();

    let express_ctor = Regex::new(
        r"(?m)\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:\(\s*\)\s*=>\s*)?(?:new\s+)?(?:express\s*\(\s*\)|express\s*\.\s*Router\s*\(\s*\)|Router\s*\(\s*\))",
    )?;
    let fastify_ctor = Regex::new(
        r"(?mi)\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:await\s+)?Fastify\s*\(",
    )?;
    let hono_ctor = Regex::new(
        r"(?m)\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*new\s+Hono\s*\(",
    )?;

    {
        let files = walk_files(root, root, 10_000, ctx.excludes);
        for rel in files {
            if !(rel.ends_with(".ts")
                || rel.ends_with(".tsx")
                || rel.ends_with(".mts")
                || rel.ends_with(".cts")
                || rel.ends_with(".js")
                || rel.ends_with(".mjs")
                || rel.ends_with(".cjs")
                || rel.ends_with(".jsx"))
            {
                continue;
            }
            if is_node_test_or_decl(&rel) {
                continue;
            }
            let abs = root.join(&rel);
            let Ok(Some(raw)) = read_to_string_bounded(&abs) else {
                continue;
            };
            if !raw.contains("express")
                && !raw.contains("Fastify")
                && !raw.contains("fastify")
                && !raw.contains("Hono")
            {
                continue;
            }
            // Constructor scans exclude strings; route scans need quoted path arguments.
            let ctor_src = strip_js_comments_strings_and_templates(&raw);
            let route_src = strip_js_comments_and_templates(&raw);
            let mut receivers: Vec<(String, NodeServerFramework)> = Vec::new();
            for cap in express_ctor.captures_iter(&ctor_src) {
                if let Some(name) = cap.get(1) {
                    receivers.push((name.as_str().to_string(), NodeServerFramework::Express));
                }
            }
            for cap in fastify_ctor.captures_iter(&ctor_src) {
                if let Some(name) = cap.get(1) {
                    receivers.push((name.as_str().to_string(), NodeServerFramework::Fastify));
                }
            }
            for cap in hono_ctor.captures_iter(&ctor_src) {
                if let Some(name) = cap.get(1) {
                    receivers.push((name.as_str().to_string(), NodeServerFramework::Hono));
                }
            }
            if receivers.is_empty() {
                continue;
            }
            let mut emitted: BTreeSet<(String, String)> = BTreeSet::new();
            for (recv, framework) in &receivers {
                let pattern = format!(
                    r#"\b{}\s*\.\s*(get|post|put|patch|delete|options|head|all)\s*\(\s*['"]([^'"]+)['"]"#,
                    regex::escape(recv)
                );
                let Ok(re) = Regex::new(&pattern) else {
                    continue;
                };
                for cap in re.captures_iter(&route_src) {
                    let method = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let path = cap.get(2).map(|m| m.as_str()).unwrap_or("");
                    if path.is_empty() {
                        continue;
                    }
                    let key = (method.to_uppercase(), path.to_string());
                    if !emitted.insert(key.clone()) {
                        continue;
                    }
                    let language = language_for_entry(&rel);
                    // Include the method in feature identity to distinguish same-path routes.
                    let route_label = format!("{} {}", key.0, path);
                    let mut tags = vec![
                        if language == Language::TypeScript {
                            "typescript"
                        } else {
                            "javascript"
                        }
                        .to_string(),
                        framework.tag().to_string(),
                        "route".to_string(),
                    ];
                    // `all` binds every verb — treat as state-changing.
                    if route_is_auth_sensitive(&key.0, path) {
                        tags.push(AUTH_SENSITIVE_TAG.to_string());
                    }
                    out.push(FeatureSeed {
                        summary: format!(
                            "{} route {} declared in {} (receiver `{}`)",
                            framework.label(),
                            route_label,
                            rel,
                            recv
                        ),
                        source: framework.source(),
                        confidence: FeatureConfidence::High,
                        entry_route: Some(route_label.clone()),
                        tags,
                        test_prefixes: vec!["__tests__".to_string(), "tests".to_string()],
                        ..FeatureSeed::new(
                            FeatureKind::Route,
                            language,
                            format!("{} route `{}`", framework.label(), route_label),
                            rel.clone(),
                        )
                    });
                }
            }
        }
    }
    Ok(out)
}

/// The shared TypeScript test predicate covers both JS and TS extensions.
fn is_node_test_or_decl(rel: &str) -> bool {
    crate::nearby_tests::is_test_file(rel, Language::TypeScript) || rel.ends_with(".d.ts")
}

/// Blank comments and templates while retaining quoted route paths.
fn strip_js_comments_and_templates(src: &str) -> String {
    strip_comments(
        src,
        CommentSyntax {
            hash_line_comments: false,
            strings: StringMode::Preserve,
            template_literals: true,
        },
    )
}

/// Also blank quoted strings so constructor-like examples cannot match.
fn strip_js_comments_strings_and_templates(src: &str) -> String {
    strip_comments(
        src,
        CommentSyntax {
            hash_line_comments: false,
            strings: StringMode::BlankContents,
            template_literals: true,
        },
    )
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

        let api = pkg_seeds
            .iter()
            .find(|s| s.entry_symbol.as_deref() == Some("@acme/api"))
            .unwrap();
        assert_eq!(api.entry_path, "packages/api/package.json");
        assert!(
            api.tests.is_empty(),
            "node-package seed must not populate tests[] with the manifest"
        );
        assert!(api.entry_command.is_none());
        assert_eq!(
            api.test_command.as_deref(),
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
        write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: '9'\n");

        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "node-package")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert!(names.contains(&"@acme/admin"));
        assert!(names.contains(&"@acme/util"));

        let admin = seeds
            .iter()
            .find(|s| s.entry_symbol.as_deref() == Some("@acme/admin"))
            .unwrap();
        assert!(admin.entry_command.is_none());
        assert_eq!(
            admin.test_command.as_deref(),
            Some("pnpm --dir apps/admin test")
        );
    }

    #[test]
    fn fallback_workspace_prefixes_discovered() {
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
        assert!(seeds.iter().any(|s| s.source == "package-json-script"));
    }

    #[test]
    fn workspace_seed_owns_source_files_for_routing() {
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

    fn write_pkg_with_dep(dir: &Path, dep: &str) {
        write(
            dir,
            "package.json",
            &format!(r#"{{"name":"api","dependencies":{{"{dep}":"*"}}}}"#),
        );
    }

    #[test]
    fn express_app_routes_emit_for_get_post_put_patch_delete() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "express");
        write(
            dir.path(),
            "src/server.ts",
            r#"
import express from "express";
const app = express();
app.get("/users", (req, res) => res.json([]));
app.post("/users", (req, res) => res.json({}));
app.put("/users/:id", (req, res) => res.json({}));
app.patch("/users/:id", (req, res) => res.json({}));
app.delete("/users/:id", (req, res) => res.status(204).end());
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "express-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        for expected in [
            "GET /users",
            "POST /users",
            "PUT /users/:id",
            "PATCH /users/:id",
            "DELETE /users/:id",
        ] {
            assert!(
                routes.contains(expected),
                "{expected} missing in {routes:?}"
            );
        }
    }

    #[test]
    fn express_router_receiver_recognized() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "express");
        write(
            dir.path(),
            "src/routes.ts",
            r#"
import { Router } from "express";
const router = Router();
router.get("/health", (_, res) => res.send("ok"));
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(seeds.iter().any(
            |s| s.source == "express-route" && s.entry_route.as_deref() == Some("GET /health")
        ));
    }

    #[test]
    fn fastify_routes_recognized() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "fastify");
        write(
            dir.path(),
            "src/index.ts",
            r#"
import Fastify from "fastify";
const fastify = Fastify({ logger: true });
fastify.get("/ping", async () => ({ ok: true }));
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "fastify-route"
                    && s.entry_route.as_deref() == Some("GET /ping")),
            "fastify GET /ping missing"
        );
    }

    #[test]
    fn hono_routes_recognized() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "hono");
        write(
            dir.path(),
            "src/app.ts",
            r#"
import { Hono } from "hono";
const app = new Hono();
app.get("/", (c) => c.text("hi"));
app.delete("/items/:id", (c) => c.text("gone"));
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let hono: Vec<&FeatureSeed> = seeds.iter().filter(|s| s.source == "hono-route").collect();
        assert!(
            hono.iter()
                .any(|s| s.entry_route.as_deref() == Some("GET /"))
        );
        assert!(
            hono.iter()
                .any(|s| s.entry_route.as_deref() == Some("DELETE /items/:id"))
        );
    }

    #[test]
    fn route_in_comment_or_string_does_not_match() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "express");
        write(
            dir.path(),
            "src/server.ts",
            r#"
import express from "express";
const app = express();
// app.get("/should-not-match", handler)
const _example = `app.get("/template-literal", handler)`;
app.get("/real", (_, res) => res.send("ok"));
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "express-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert_eq!(routes, vec!["GET /real"], "got: {routes:?}");
    }

    #[test]
    fn unrelated_receiver_does_not_emit_routes() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "express");
        write(
            dir.path(),
            "src/client.ts",
            r#"
import axios from "axios";
const client = axios.create();
client.get("/api/users");
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "express-route"),
            "axios .get() must not become an express route"
        );
    }

    #[test]
    fn no_server_dep_skips_scan() {
        let dir = tempdir().unwrap();
        write(dir.path(), "package.json", r#"{"name":"plain"}"#);
        write(
            dir.path(),
            "src/server.ts",
            r#"const app = express(); app.get("/x", () => {});"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| matches!(s.source, "express-route" | "fastify-route" | "hono-route"))
        );
    }

    #[test]
    fn test_files_are_skipped() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "express");
        write(
            dir.path(),
            "src/server.test.ts",
            r#"
import express from "express";
const app = express();
app.get("/should-not-surface", () => {});
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(!seeds.iter().any(|s| s.source == "express-route"));
    }

    #[test]
    fn react_router_matches_element_before_path() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"react":"^18","react-router-dom":"^6"}}"#,
        );
        write(
            dir.path(),
            "src/Routes.tsx",
            r#"
import { Route } from "react-router-dom";
export const r = (
    <>
        <Route element={<UsersPage />} path="/users" />
        <Route path="/items" element={<ItemsPage />} />
    </>
);
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: std::collections::BTreeSet<&str> = seeds
            .iter()
            .filter(|s| s.source == "react-router-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.contains("/users"),
            "element-before-path missed: {routes:?}"
        );
        assert!(
            routes.contains("/items"),
            "path-before-element missed: {routes:?}"
        );
    }

    #[test]
    fn react_router_gates_on_workspace_package_dep() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["apps/*"]}"#,
        );
        write(
            dir.path(),
            "apps/web/package.json",
            r#"{"name":"@m/web","dependencies":{"react":"^18","react-router-dom":"^6"}}"#,
        );
        write(
            dir.path(),
            "apps/web/src/Routes.tsx",
            r#"
import { Route } from "react-router-dom";
export const r = <Route path="/users" element={<UsersPage />} />;
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "react-router-route"
                    && s.entry_route.as_deref() == Some("/users")),
            "React Router gate didn't fire for workspace-only react dep"
        );
    }

    #[test]
    fn express_routes_gate_on_workspace_package_dep() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["apps/*"]}"#,
        );
        write(
            dir.path(),
            "apps/api/package.json",
            r#"{"name":"@m/api","dependencies":{"express":"^5"}}"#,
        );
        write(
            dir.path(),
            "apps/api/src/server.ts",
            r#"
import express from "express";
const app = express();
app.get("/health", (_, res) => res.send("ok"));
"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "express-route"
                    && s.entry_route.as_deref() == Some("GET /health")),
            "Express gate didn't fire for workspace-only dep"
        );
    }

    #[test]
    fn route_path_preserves_non_ascii_utf8() {
        let dir = tempdir().unwrap();
        write_pkg_with_dep(dir.path(), "express");
        write(
            dir.path(),
            "src/server.ts",
            "import express from \"express\";\nconst app = express();\n\
             app.get(\"/héllo\", (_, res) => res.send(\"ok\"));\n\
             app.get(\"/привет\", (_, res) => res.send(\"ok\"));\n",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: std::collections::BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.source == "express-route")
            .filter_map(|s| s.entry_route.clone())
            .collect();
        assert!(
            routes.contains("GET /héllo"),
            "non-ASCII Latin (/héllo) corrupted: {routes:?}"
        );
        assert!(
            routes.contains("GET /привет"),
            "non-ASCII Cyrillic (/привет) corrupted: {routes:?}"
        );
    }

    #[test]
    fn next_routes_emitted_per_workspace_package() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["apps/*"]}"#,
        );
        write(
            dir.path(),
            "apps/web/package.json",
            r#"{"name":"@acme/web","dependencies":{"next":"15.0.0"}}"#,
        );
        write(
            dir.path(),
            "apps/web/app/page.tsx",
            "export default function Page() { return null; }\n",
        );
        write(
            dir.path(),
            "apps/web/app/users/page.tsx",
            "export default function Users() { return null; }\n",
        );
        write(
            dir.path(),
            "apps/marketing/package.json",
            r#"{"name":"@acme/marketing","dependencies":{"next":"15.0.0"}}"#,
        );
        write(
            dir.path(),
            "apps/marketing/pages/about.tsx",
            "export default function About() { return null; }\n",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let next_routes: BTreeSet<(&str, &str)> = seeds
            .iter()
            .filter(|s| matches!(s.source, "next-app-page" | "next-pages-route"))
            .map(|s| {
                (
                    s.entry_path.as_str(),
                    s.entry_route.as_deref().unwrap_or(""),
                )
            })
            .collect();
        assert!(
            next_routes.contains(&("apps/web/app/page.tsx", "/")),
            "missing apps/web root page: {next_routes:?}"
        );
        assert!(
            next_routes.contains(&("apps/web/app/users/page.tsx", "/users")),
            "missing apps/web /users page: {next_routes:?}"
        );
        assert!(
            next_routes.contains(&("apps/marketing/pages/about.tsx", "/about")),
            "missing apps/marketing /about page: {next_routes:?}"
        );
    }

    #[test]
    fn next_pages_router_skips_special_files() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"site","dependencies":{"next":"15.0.0"}}"#,
        );
        write(
            dir.path(),
            "pages/index.tsx",
            "export default function Home() { return null; }\n",
        );
        write(
            dir.path(),
            "pages/about.tsx",
            "export default function About() { return null; }\n",
        );
        write(
            dir.path(),
            "pages/_app.tsx",
            "export default function App() { return null; }\n",
        );
        write(
            dir.path(),
            "pages/_document.tsx",
            "export default function Doc() { return null; }\n",
        );
        write(
            dir.path(),
            "pages/_error.tsx",
            "export default function Err() { return null; }\n",
        );
        write(dir.path(), "pages/middleware.ts", "export default {};\n");
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: BTreeSet<&str> = seeds
            .iter()
            .filter(|s| s.source == "next-pages-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(routes.contains("/about"), "about page missing: {routes:?}");
        assert!(
            routes.contains("/index"),
            "root index page missing: {routes:?}"
        );
        for bogus in ["/_app", "/_document", "/_error", "/middleware"] {
            assert!(
                !routes.contains(bogus),
                "special file leaked as route {bogus}: {routes:?}"
            );
        }
    }

    #[test]
    fn nested_frontend_discovered_when_root_has_no_next() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"backend\"]\n",
        );
        write(
            dir.path(),
            "frontend/package.json",
            r#"{"name":"frontend","dependencies":{"next":"15.0.0"}}"#,
        );
        write(
            dir.path(),
            "frontend/app/page.tsx",
            "export default function Page() { return null; }\n",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "next-app-page" && s.entry_path == "frontend/app/page.tsx"),
            "nested frontend/ root not discovered"
        );
    }

    #[test]
    fn turbo_rewrites_workspace_test_command_when_test_task_declared() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(dir.path(), "turbo.json", r#"{"tasks":{"test":{}}}"#);
        write(
            dir.path(),
            "packages/api/package.json",
            r#"{"name":"@acme/api","scripts":{"test":"jest"}}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let api_pkg = seeds
            .iter()
            .find(|s| s.source == "node-package" && s.entry_path == "packages/api/package.json")
            .expect("node-package seed");
        assert_eq!(
            api_pkg.test_command.as_deref(),
            Some("turbo run test --filter=@acme/api"),
            "turbo rewrite missing"
        );
    }

    #[test]
    fn turbo_without_test_task_keeps_plain_test_command() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(dir.path(), "turbo.json", r#"{"tasks":{"build":{}}}"#);
        write(
            dir.path(),
            "packages/api/package.json",
            r#"{"name":"@acme/api","scripts":{"test":"jest"}}"#,
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let api_pkg = seeds
            .iter()
            .find(|s| s.source == "node-package" && s.entry_path == "packages/api/package.json")
            .expect("node-package seed");
        let cmd = api_pkg.test_command.as_deref().unwrap_or("");
        assert!(
            !cmd.starts_with("turbo "),
            "turbo prefix applied without test task: {cmd}"
        );
    }

    #[test]
    fn react_component_seed_for_each_component_file() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"react":"^18.0.0"}}"#,
        );
        write(
            dir.path(),
            "src/components/Button.tsx",
            "export const Button = () => <button/>;",
        );
        write(
            dir.path(),
            "src/components/Card.tsx",
            "export const Card = () => <div/>;",
        );
        write(
            dir.path(),
            "src/pages/Home.tsx",
            "export default function Home(){ return <h1/>; }",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let names: std::collections::BTreeSet<&str> = seeds
            .iter()
            .filter(|s| s.source == "react-component")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        for expected in ["Button", "Card", "Home"] {
            assert!(names.contains(expected), "missing {expected} in {names:?}");
        }
        let button = seeds
            .iter()
            .find(|s| s.source == "react-component" && s.entry_symbol.as_deref() == Some("Button"))
            .expect("Button seed missing");
        assert_eq!(button.kind, FeatureKind::Library);
        assert!(button.tags.iter().any(|t| t == "react"));
        assert!(button.tags.iter().any(|t| t == "react-component"));
    }

    #[test]
    fn react_component_skips_tests_stories_and_decls() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"react":"^18.0.0"}}"#,
        );
        write(
            dir.path(),
            "src/components/Real.tsx",
            "export const Real = () => <div/>;",
        );
        write(
            dir.path(),
            "src/components/Button.test.tsx",
            "test('x', () => {});",
        );
        write(
            dir.path(),
            "src/components/Button.stories.tsx",
            "export default { title: 'Btn' };",
        );
        write(
            dir.path(),
            "src/components/types.d.ts",
            "export type X = number;",
        );
        write(dir.path(), "src/components/__stories__/Demo.tsx", "");
        write(dir.path(), "src/components/fixtures/sample.tsx", "");
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let component_paths: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "react-component")
            .map(|s| s.entry_path.as_str())
            .collect();
        assert!(
            component_paths.contains(&"src/components/Real.tsx"),
            "Real.tsx should be a component: {component_paths:?}"
        );
        for skipped in [
            "src/components/Button.test.tsx",
            "src/components/Button.stories.tsx",
            "src/components/types.d.ts",
            "src/components/__stories__/Demo.tsx",
            "src/components/fixtures/sample.tsx",
        ] {
            assert!(
                !component_paths.contains(&skipped),
                "{skipped} should NOT be a component"
            );
        }
    }

    #[test]
    fn react_component_excludes_files_owned_by_router_seeds() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"react":"^18.0.0","react-router-dom":"^6.0.0"}}"#,
        );
        write(
            dir.path(),
            "src/pages/Routes.tsx",
            r#"import { Route, Routes } from "react-router-dom";
export default () => (<Routes><Route path="/" element={<Home/>}/></Routes>);"#,
        );
        write(
            dir.path(),
            "src/components/Card.tsx",
            "export const Card = () => <div/>;",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let route_owned: std::collections::BTreeSet<&str> = seeds
            .iter()
            .filter(|s| s.source == "react-router-route")
            .map(|s| s.entry_path.as_str())
            .collect();
        assert!(route_owned.contains("src/pages/Routes.tsx"));
        assert!(
            !seeds
                .iter()
                .any(|s| s.source == "react-component" && s.entry_path == "src/pages/Routes.tsx"),
            "Routes.tsx should not also emit as react-component"
        );
        assert!(
            seeds.iter().any(|s| s.source == "react-component" && s.entry_path == "src/components/Card.tsx"),
            "Card.tsx should still emit as a component"
        );
    }

    #[test]
    fn react_component_skipped_when_react_dep_missing() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"plain","dependencies":{"vue":"^3.0.0"}}"#,
        );
        write(
            dir.path(),
            "src/components/Looks.tsx",
            "export const Looks = () => null;",
        );
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "react-component"),
            "no react dep → no component seeds"
        );
    }

    #[test]
    fn react_component_cap_enforced_per_package() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"app","dependencies":{"react":"^18.0.0"}}"#,
        );
        for i in 0..110 {
            write(
                dir.path(),
                &format!("src/components/Auto{i:03}.tsx"),
                &format!("export const Auto{i:03} = () => <div/>;\n"),
            );
        }
        let seeds = JsMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let count = seeds
            .iter()
            .filter(|s| s.source == "react-component")
            .count();
        assert_eq!(count, 100, "expected per-package cap of 100, got {count}");
    }
}
