//! Runnable test commands and inline test modules for `recommend_tests`.
//!
//! Everything here is derived from indexed paths, symbol rows, and manifest
//! files that happen to exist under the project root. Nothing is executed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{InlineTestModule, Symbol, SymbolKind, TestCommand};
use codesage_storage::Database;

pub(super) const SOURCE_CONVENTION: &str = "convention";
pub(super) const SOURCE_INLINE: &str = "inline";
pub(super) const SOURCE_FEATURE: &str = "feature_test_command";

/// Lines above a `mod` item scanned for `#[cfg(test)]`; other attributes and
/// doc comments may sit between the gate and the item.
const CFG_TEST_LOOKBACK: usize = 3;

pub(super) struct CommandContext<'a> {
    pub root: Option<&'a Path>,
    /// Test files to run: `primary` plus `reachable` paths.
    pub test_files: &'a [String],
    /// `.phpt` paths withheld from `primary`; only their directories are named.
    pub withheld_phpt: &'a [String],
    /// Changed inputs as `(normalized, as_given)`.
    pub changed: &'a [(String, String)],
}

pub(super) fn derive(
    db: &Database,
    ctx: &CommandContext<'_>,
) -> Result<(Vec<TestCommand>, Vec<InlineTestModule>)> {
    let convention = convention_commands(ctx.root, ctx.test_files, ctx.withheld_phpt);
    let (inline, modules) = inline_commands(db, ctx)?;
    let feature = feature_commands(db, ctx.changed)?;
    let mut merged: Vec<TestCommand> = Vec::new();
    for group in [convention, inline, feature] {
        for cmd in finish_group(group) {
            match merged.iter_mut().find(|m| m.command == cmd.command) {
                Some(existing) => {
                    existing.covers.extend(cmd.covers);
                    existing.covers.sort();
                    existing.covers.dedup();
                }
                None => merged.push(cmd),
            }
        }
    }
    Ok((merged, modules))
}

/// Sort one source group by command text and merge duplicates.
fn finish_group(group: Vec<TestCommand>) -> Vec<TestCommand> {
    let mut by_command: BTreeMap<String, TestCommand> = BTreeMap::new();
    for cmd in group {
        by_command
            .entry(cmd.command.clone())
            .and_modify(|existing| existing.covers.extend(cmd.covers.iter().cloned()))
            .or_insert(cmd);
    }
    by_command
        .into_values()
        .map(|mut cmd| {
            cmd.covers.sort();
            cmd.covers.dedup();
            cmd
        })
        .collect()
}

fn command(command: String, covers: Vec<String>, framework: &str, source: &str) -> TestCommand {
    TestCommand {
        command,
        covers,
        framework: framework.to_string(),
        source: source.to_string(),
    }
}

fn extension(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

fn stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn root_has(root: Option<&Path>, names: &[&str]) -> bool {
    root.is_some_and(|root| names.iter().any(|n| root.join(n).is_file()))
}

fn convention_commands(
    root: Option<&Path>,
    test_files: &[String],
    withheld_phpt: &[String],
) -> Vec<TestCommand> {
    let mut out = Vec::new();
    let mut python: Vec<String> = Vec::new();
    let mut php: Vec<String> = Vec::new();
    let mut phpt_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut go_dirs: BTreeSet<String> = BTreeSet::new();
    let mut go_paths: Vec<String> = Vec::new();
    let mut js: Vec<String> = Vec::new();
    let mut java: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for path in test_files {
        match extension(path) {
            "rs" => out.push(rust_integration_command(root, path)),
            "py" => python.push(path.clone()),
            "php" => php.push(path.clone()),
            "phpt" => phpt_dirs
                .entry(parent_dir(path).to_string())
                .or_default()
                .push(path.clone()),
            "go" => {
                let dir = parent_dir(path);
                go_dirs.insert(if dir.is_empty() {
                    ".".to_string()
                } else {
                    format!("./{dir}")
                });
                go_paths.push(path.clone());
            }
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" => js.push(path.clone()),
            "java" => java
                .entry(stem(path).to_string())
                .or_default()
                .push(path.clone()),
            _ => {}
        }
    }
    for path in withheld_phpt {
        phpt_dirs.entry(parent_dir(path).to_string()).or_default();
    }

    if !python.is_empty() {
        python.sort();
        out.push(command(
            format!("pytest {}", python.join(" ")),
            python,
            "pytest",
            SOURCE_CONVENTION,
        ));
    }
    if !php.is_empty() {
        php.sort();
        let (runner, framework) = if root_has(root, &["artisan"]) {
            ("php artisan test", "artisan")
        } else {
            ("vendor/bin/phpunit", "phpunit")
        };
        out.push(command(
            format!("{runner} {}", php.join(" ")),
            php,
            framework,
            SOURCE_CONVENTION,
        ));
    }
    for (dir, mut paths) in phpt_dirs {
        let target = if dir.is_empty() { "." } else { dir.as_str() };
        // A withheld directory is named as a whole rather than file by file.
        if paths.is_empty() {
            paths.push(target.to_string());
        }
        paths.sort();
        out.push(command(
            format!("php run-tests.php {target}"),
            paths,
            "run-tests",
            SOURCE_CONVENTION,
        ));
    }
    if !go_dirs.is_empty() {
        let dirs: Vec<String> = go_dirs.into_iter().collect();
        go_paths.sort();
        out.push(command(
            format!("go test {}", dirs.join(" ")),
            go_paths,
            "go",
            SOURCE_CONVENTION,
        ));
    }
    if !js.is_empty() {
        js.sort();
        let vitest = root_has(
            root,
            &[
                "vitest.config.ts",
                "vitest.config.js",
                "vitest.config.mts",
                "vitest.config.mjs",
                "vitest.config.cts",
                "vitest.config.cjs",
                "vitest.workspace.ts",
                "vitest.workspace.js",
            ],
        );
        let (runner, framework) = if vitest {
            ("npx vitest run", "vitest")
        } else {
            ("npx jest", "jest")
        };
        out.push(command(
            format!("{runner} {}", js.join(" ")),
            js,
            framework,
            SOURCE_CONVENTION,
        ));
    }
    if !java.is_empty() {
        let classes: Vec<&str> = java.keys().map(String::as_str).collect();
        let mut covers: Vec<String> = java.values().flatten().cloned().collect();
        covers.sort();
        let gradle =
            !root_has(root, &["pom.xml"]) && root_has(root, &["build.gradle", "build.gradle.kts"]);
        if gradle {
            let tests: Vec<String> = classes.iter().map(|c| format!("--tests {c}")).collect();
            out.push(command(
                format!("gradle test {}", tests.join(" ")),
                covers,
                "gradle",
                SOURCE_CONVENTION,
            ));
        } else {
            out.push(command(
                format!("mvn -Dtest={} test", classes.join(",")),
                covers,
                "maven",
                SOURCE_CONVENTION,
            ));
        }
    }
    out
}

/// The crate a Rust path belongs to: the components before the first `src`
/// or `tests` segment, plus the `[package] name` from that directory's
/// `Cargo.toml` when the root is known and the manifest is readable.
struct RustCrate {
    dir: String,
    name: Option<String>,
}

impl RustCrate {
    fn package_flag(&self) -> String {
        match &self.name {
            Some(name) => format!(" -p {name}"),
            None => String::new(),
        }
    }

    /// The path below `<dir>/<segment>/`, when the path lives there.
    fn rest_under<'p>(&self, path: &'p str, segment: &str) -> Option<&'p str> {
        let prefix = if self.dir.is_empty() {
            format!("{segment}/")
        } else {
            format!("{}/{segment}/", self.dir)
        };
        path.strip_prefix(prefix.as_str())
    }
}

fn rust_crate_for(root: Option<&Path>, path: &str) -> RustCrate {
    let parts: Vec<&str> = path.split('/').collect();
    let boundary = parts
        .iter()
        .position(|p| *p == "src" || *p == "tests")
        .unwrap_or(parts.len().saturating_sub(1));
    let dir = parts[..boundary].join("/");
    let manifest_name = root.and_then(|root| {
        let manifest = if dir.is_empty() {
            root.join("Cargo.toml")
        } else {
            root.join(&dir).join("Cargo.toml")
        };
        let text = std::fs::read_to_string(manifest).ok()?;
        cargo_package_name(&text)
    });
    let name = manifest_name.or_else(|| {
        dir.rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    RustCrate { dir, name }
}

/// `name = "..."` inside `[package]`. A line scan is enough: the value is a
/// bare string in every manifest Cargo accepts for a package name.
fn cargo_package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        let value = value.split('#').next().unwrap_or("").trim();
        let value = value.trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// `cargo test -p <crate> --test <target>` for `<crate>/tests/<target>.rs`
/// and `<crate>/tests/<target>/main.rs`; any other Rust test file falls back
/// to the crate's whole suite, since a module of an integration test has no
/// target of its own.
fn rust_integration_command(root: Option<&Path>, path: &str) -> TestCommand {
    let krate = rust_crate_for(root, path);
    let target = krate.rest_under(path, "tests").and_then(|rest| {
        if let Some(name) = rest.strip_suffix("/main.rs") {
            (!name.contains('/')).then(|| name.to_string())
        } else if !rest.contains('/') {
            rest.strip_suffix(".rs").map(str::to_string)
        } else {
            None
        }
    });
    let cmd = match target {
        Some(target) => format!("cargo test{} --test {target}", krate.package_flag()),
        None => format!("cargo test{}", krate.package_flag()),
    };
    command(cmd, vec![path.to_string()], "cargo", SOURCE_CONVENTION)
}

/// Crate-relative module path of a file under `src/`: `lib.rs`, `main.rs`,
/// and `bin/<x>.rs` are crate roots (empty path), `a/mod.rs` is `a`,
/// `a/b.rs` is `a::b`. `None` outside `src/`.
fn rust_module_path(krate: &RustCrate, path: &str) -> Option<String> {
    let rest = krate.rest_under(path, "src")?;
    let rest = rest.strip_suffix(".rs")?;
    if rest == "lib" || rest == "main" {
        return Some(String::new());
    }
    if let Some(bin) = rest.strip_prefix("bin/")
        && !bin.contains('/')
    {
        return Some(String::new());
    }
    let rest = rest.strip_suffix("/mod").unwrap_or(rest);
    Some(rest.replace('/', "::"))
}

fn is_rust_test_module_name(name: &str) -> bool {
    name == "tests" || name == "test" || name.ends_with("_tests") || name.ends_with("_test")
}

/// Whether `#[cfg(test)]` sits within [`CFG_TEST_LOOKBACK`] lines above the
/// item starting at 1-based `line_start`.
fn cfg_test_gated(lines: &[&str], line_start: u32) -> bool {
    let end = (line_start as usize).saturating_sub(1).min(lines.len());
    let begin = end.saturating_sub(CFG_TEST_LOOKBACK);
    lines[begin..end]
        .iter()
        .any(|l| l.trim_start().starts_with("#[cfg(test)]"))
}

fn count_test_attributes(lines: &[&str], line_start: u32, line_end: u32) -> usize {
    let begin = (line_start as usize).saturating_sub(1).min(lines.len());
    let end = (line_end as usize).min(lines.len()).max(begin);
    lines[begin..end]
        .iter()
        .map(|l| l.trim_start())
        .filter(|l| l.starts_with("#[test]") || l.starts_with("#[tokio::test"))
        .count()
}

fn within(sym: &Symbol, module: &Symbol) -> bool {
    sym.line_start > module.line_start && sym.line_end <= module.line_end
}

/// Outermost test modules of one Rust file with their test counts. Nested
/// test modules count toward the enclosing one so nothing is reported twice.
fn rust_inline_modules(symbols: &[Symbol], source: Option<&str>) -> Vec<(String, usize)> {
    let lines: Option<Vec<&str>> = source.map(|s| s.lines().collect());
    let mut modules: Vec<&Symbol> = symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Module)
        .filter(|s| {
            is_rust_test_module_name(&s.name)
                || lines
                    .as_deref()
                    .is_some_and(|lines| cfg_test_gated(lines, s.line_start))
        })
        .collect();
    modules.sort_by_key(|m| (m.line_start, std::cmp::Reverse(m.line_end)));
    let mut accepted: Vec<&Symbol> = Vec::new();
    for module in modules {
        if accepted.iter().any(|outer| within(module, outer)) {
            continue;
        }
        accepted.push(module);
    }
    accepted
        .into_iter()
        .map(|module| {
            let count = match lines.as_deref() {
                Some(lines) => count_test_attributes(lines, module.line_start, module.line_end),
                None => symbols
                    .iter()
                    .filter(|s| s.kind == SymbolKind::Function && within(s, module))
                    .filter(|s| s.name.starts_with("test"))
                    .count(),
            };
            (module.name.clone(), count)
        })
        .collect()
}

fn python_module_id(path: &str) -> String {
    let dotted = path.strip_suffix(".py").unwrap_or(path).replace('/', ".");
    dotted
        .strip_suffix(".__init__")
        .map(str::to_string)
        .unwrap_or(dotted)
}

fn python_test_count(symbols: &[Symbol]) -> usize {
    symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .filter(|s| s.name.starts_with("test_"))
        .count()
}

fn inline_commands(
    db: &Database,
    ctx: &CommandContext<'_>,
) -> Result<(Vec<TestCommand>, Vec<InlineTestModule>)> {
    let test_set: BTreeSet<&str> = ctx.test_files.iter().map(String::as_str).collect();
    let mut commands = Vec::new();
    let mut modules = Vec::new();
    let mut python_files: Vec<String> = Vec::new();
    for (path, _) in ctx.changed {
        codesage_protocol::work::checkpoint()?;
        if test_set.contains(path.as_str()) {
            continue;
        }
        match extension(path) {
            "rs" => {
                let symbols = db.symbols_for_file(path)?;
                if symbols.iter().all(|s| s.kind != SymbolKind::Module) {
                    continue;
                }
                let krate = rust_crate_for(ctx.root, path);
                let Some(module_path) = rust_module_path(&krate, path) else {
                    continue;
                };
                let source = ctx
                    .root
                    .and_then(|root| std::fs::read_to_string(root.join(path)).ok());
                for (name, count) in rust_inline_modules(&symbols, source.as_deref()) {
                    let module = if module_path.is_empty() {
                        name
                    } else {
                        format!("{module_path}::{name}")
                    };
                    commands.push(command(
                        format!("cargo test{} {module}::", krate.package_flag()),
                        vec![module.clone()],
                        "cargo",
                        SOURCE_INLINE,
                    ));
                    modules.push(InlineTestModule {
                        file: path.clone(),
                        module,
                        test_count: count,
                    });
                }
            }
            "py" => {
                let symbols = db.symbols_for_file(path)?;
                let count = python_test_count(&symbols);
                if count == 0 {
                    continue;
                }
                modules.push(InlineTestModule {
                    file: path.clone(),
                    module: python_module_id(path),
                    test_count: count,
                });
                python_files.push(path.clone());
            }
            _ => {}
        }
    }
    if !python_files.is_empty() {
        python_files.sort();
        commands.push(command(
            format!("pytest {}", python_files.join(" ")),
            python_files,
            "pytest",
            SOURCE_INLINE,
        ));
    }
    Ok((commands, modules))
}

fn feature_commands(db: &Database, changed: &[(String, String)]) -> Result<Vec<TestCommand>> {
    let mut by_command: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (normalized, given) in changed {
        codesage_protocol::work::checkpoint()?;
        for feature in db.features_for_file(normalized)? {
            let Some(cmd) = feature.test_command.as_deref() else {
                continue;
            };
            let cmd = cmd.trim();
            if cmd.is_empty() {
                continue;
            }
            by_command
                .entry(cmd.to_string())
                .or_default()
                .push(given.clone());
        }
    }
    Ok(by_command
        .into_iter()
        .map(|(cmd, covers)| {
            let framework = cmd.split_whitespace().next().unwrap_or("").to_string();
            command(cmd, covers, &framework, SOURCE_FEATURE)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_package_name_reads_the_package_section_only() {
        let manifest = "[workspace]\nmembers = [\"a\"]\n\n[package]\n# the crate\nname = \"acme-core\" # trailing\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"not-this\"\n";
        assert_eq!(cargo_package_name(manifest).as_deref(), Some("acme-core"));
        assert_eq!(cargo_package_name("[workspace]\nmembers = []\n"), None);
    }

    #[test]
    fn rust_module_path_maps_crate_roots_and_mod_files() {
        let krate = RustCrate {
            dir: "crates/graph".to_string(),
            name: Some("codesage-graph".to_string()),
        };
        let at = |p: &str| rust_module_path(&krate, p);
        assert_eq!(at("crates/graph/src/lib.rs").as_deref(), Some(""));
        assert_eq!(at("crates/graph/src/main.rs").as_deref(), Some(""));
        assert_eq!(at("crates/graph/src/bin/tool.rs").as_deref(), Some(""));
        assert_eq!(at("crates/graph/src/search.rs").as_deref(), Some("search"));
        assert_eq!(
            at("crates/graph/src/git_history/mod.rs").as_deref(),
            Some("git_history")
        );
        assert_eq!(
            at("crates/graph/src/git_history/tests_rec.rs").as_deref(),
            Some("git_history::tests_rec")
        );
        assert_eq!(at("crates/graph/tests/foo.rs"), None);
        assert_eq!(at("crates/other/src/x.rs"), None);
    }

    #[test]
    fn rust_integration_target_names_follow_cargo_layout() {
        let cmd = |p: &str| rust_integration_command(None, p).command;
        assert_eq!(
            cmd("crates/graph/tests/risk_test.rs"),
            "cargo test -p graph --test risk_test"
        );
        assert_eq!(
            cmd("crates/graph/tests/suite/main.rs"),
            "cargo test -p graph --test suite"
        );
        assert_eq!(
            cmd("crates/graph/tests/suite/helpers.rs"),
            "cargo test -p graph"
        );
        assert_eq!(cmd("tests/integration.rs"), "cargo test --test integration");
    }

    #[test]
    fn python_module_ids_drop_init() {
        assert_eq!(python_module_id("pkg/mod.py"), "pkg.mod");
        assert_eq!(python_module_id("pkg/__init__.py"), "pkg");
        assert_eq!(python_module_id("script.py"), "script");
    }

    #[test]
    fn cfg_test_lookback_is_bounded() {
        let lines = ["#[cfg(test)]", "#[allow(dead_code)]", "mod tests {"];
        assert!(cfg_test_gated(&lines, 3));
        let far = ["#[cfg(test)]", "", "", "", "mod tests {"];
        assert!(!cfg_test_gated(&far, 5));
    }
}
