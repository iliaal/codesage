//! JavaScript / TypeScript mapper: `package.json` `bin` and selected
//! scripts (`start`/`build`/`test`/`lint`/`typecheck`/`format`), plus
//! Next.js `app/**/{page,route}.{ts,tsx,js,jsx}` and `pages/**/*.{ts,tsx}`.

use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use serde_json::Value;

use crate::mappers::shared::{is_safe_dir, is_safe_file, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};

pub struct JsMapper;

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
        if let Some(pkg) = &pkg_at_root {
            seeds.extend(package_seeds(root, pkg));
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

fn package_seeds(root: &Path, pkg: &Value) -> Vec<FeatureSeed> {
    let mut out = Vec::new();
    // bin: string or { name: path } map.
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
            let entry = path.trim_start_matches("./").to_string();
            let abs = root.join(&entry);
            if !is_safe_file(root, &abs) {
                continue;
            }
            let language = language_for_entry(&entry);
            out.push(FeatureSeed {
                title: format!("npm bin `{cmd}`"),
                summary: format!("package.json bin entry at {entry}"),
                kind: FeatureKind::CliCommand,
                source: "package-json-bin",
                confidence: FeatureConfidence::High,
                entry_path: entry,
                entry_symbol: None,
                entry_route: None,
                entry_command: Some(cmd),
                language,
                tags: vec![
                    if language == Language::TypeScript {
                        "typescript"
                    } else {
                        "javascript"
                    }
                    .to_string(),
                    "cli".to_string(),
                ],
                owned_files: Vec::new(),
                context_files: vec![SeedFile {
                    path: "package.json".to_string(),
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
    // scripts: subset of known names.
    if let Some(scripts) = pkg.get("scripts").and_then(|v| v.as_object()) {
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
                entry_path: "package.json".to_string(),
                entry_symbol: Some(name.clone()),
                entry_route: None,
                entry_command: Some(name.clone()),
                language: Language::JavaScript,
                tags: vec!["javascript".to_string(), "package-script".to_string()],
                owned_files: Vec::new(),
                context_files: vec![SeedFile {
                    path: "package.json".to_string(),
                    reason: "package manifest".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: Vec::new(),
            });
        }
    }
    out
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
}
