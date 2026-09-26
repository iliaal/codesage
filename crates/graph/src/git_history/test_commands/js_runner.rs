//! JavaScript/TypeScript runner selection for `recommend_tests` commands.
//!
//! A runner is named only from evidence under the project root: the test
//! file's own import of a runner-owned module, then the nearest
//! `package.json` (`scripts.test`, declared dependencies, a `jest` key), then
//! a vitest config beside it. A path with no evidence gets no command, since
//! a suggested runner that is not installed fails or makes `npx` download it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use codesage_protocol::TestCommand;
use serde_json::Value;

use super::{
    MAX_SOURCE_BYTES, SOURCE_CONVENTION, command, extension, parent_dir, path_arg,
    read_source_bounded, safe_relative_dir, shell_quote,
};

const VITEST_CONFIGS: [&str; 8] = [
    "vitest.config.ts",
    "vitest.config.js",
    "vitest.config.mts",
    "vitest.config.mjs",
    "vitest.config.cts",
    "vitest.config.cjs",
    "vitest.workspace.ts",
    "vitest.workspace.js",
];

/// Dependencies that name a runner; everything else in the manifest is dropped.
const RUNNER_DEPS: [&str; 4] = ["vitest", "jest", "mocha", "@playwright/test"];

/// Paths named in the no-evidence note before the rest are counted.
const NOTE_PATH_CAP: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum JsRunner {
    Vitest,
    Jest,
    Mocha,
    NodeTest,
    Bun,
    Playwright,
}

impl JsRunner {
    pub(super) fn framework(self) -> &'static str {
        match self {
            Self::Vitest => "vitest",
            Self::Jest => "jest",
            Self::Mocha => "mocha",
            Self::NodeTest => "node:test",
            Self::Bun => "bun",
            Self::Playwright => "playwright",
        }
    }

    fn invocation(self) -> &'static str {
        match self {
            Self::Vitest => "npx vitest run",
            Self::Jest => "npx jest",
            Self::Mocha => "npx mocha",
            Self::NodeTest => "node --test",
            Self::Bun => "bun test",
            Self::Playwright => "npx playwright test",
        }
    }

    /// `bun test` reads a bare argument as a name filter and only a `./`
    /// prefixed one as a file path.
    fn arg(self, rel: &str) -> String {
        match self {
            Self::Bun => shell_quote(&format!("./{rel}")),
            _ => path_arg(rel),
        }
    }
}

#[derive(Default)]
struct PackageInfo {
    scripts_test: Option<String>,
    deps: BTreeSet<String>,
    jest_key: bool,
}

/// One `package.json` read per directory per request. `None` means the
/// directory holds no manifest; an unreadable, oversized, or malformed one is
/// still a package boundary, just one with no evidence.
#[derive(Default)]
pub(super) struct PackageMemo {
    packages: HashMap<String, Option<PackageInfo>>,
}

impl PackageMemo {
    fn get(&mut self, root: &Path, dir: &str) -> Option<&PackageInfo> {
        if !self.packages.contains_key(dir) {
            let info = read_package(root, dir);
            self.packages.insert(dir.to_string(), info);
        }
        self.packages.get(dir).and_then(Option::as_ref)
    }
}

fn read_package(root: &Path, dir: &str) -> Option<PackageInfo> {
    if !safe_relative_dir(dir) {
        return None;
    }
    let manifest = if dir.is_empty() {
        root.join("package.json")
    } else {
        root.join(dir).join("package.json")
    };
    let meta = std::fs::metadata(&manifest).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_SOURCE_BYTES {
        return Some(PackageInfo::default());
    }
    let parsed = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    Some(parsed.map(|v| package_info(&v)).unwrap_or_default())
}

fn package_info(v: &Value) -> PackageInfo {
    let scripts_test = v
        .get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut deps = BTreeSet::new();
    for section in ["devDependencies", "dependencies"] {
        if let Some(obj) = v.get(section).and_then(Value::as_object) {
            deps.extend(
                RUNNER_DEPS
                    .iter()
                    .filter(|d| obj.contains_key(**d))
                    .map(|d| d.to_string()),
            );
        }
    }
    PackageInfo {
        scripts_test,
        deps,
        jest_key: v.get("jest").is_some_and(|j| !j.is_null()),
    }
}

/// The first runner a `scripts.test` value invokes, reading its shell
/// segments left to right. Wrappers (`c8`, `nyc`, `cross-env`, `npx`) are
/// stepped over because every token is inspected; a script that only
/// delegates (`npm run test:unit`, `turbo run test`) names no runner.
pub(super) fn runner_from_script(script: &str) -> Option<JsRunner> {
    for segment in script.split(['&', '|', ';', '\n']) {
        let tokens: Vec<&str> = segment
            .split_whitespace()
            .map(|t| t.trim_matches(['"', '\'']))
            .collect();
        for (i, token) in tokens.iter().enumerate() {
            let name = token.rsplit('/').next().unwrap_or(token);
            let next = tokens.get(i + 1).copied();
            let found = match name {
                "vitest" => Some(JsRunner::Vitest),
                "jest" => Some(JsRunner::Jest),
                "mocha" | "_mocha" => Some(JsRunner::Mocha),
                "bun" if next == Some("test") => Some(JsRunner::Bun),
                "playwright" if next == Some("test") => Some(JsRunner::Playwright),
                "node" if tokens[i + 1..].contains(&"--test") => Some(JsRunner::NodeTest),
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

/// A single declared unit runner; `@playwright/test` only when no unit
/// runner is declared. Several unit runners decide nothing.
fn runner_from_deps(deps: &BTreeSet<String>) -> Option<JsRunner> {
    let unit: Vec<JsRunner> = [
        ("vitest", JsRunner::Vitest),
        ("jest", JsRunner::Jest),
        ("mocha", JsRunner::Mocha),
    ]
    .into_iter()
    .filter(|(dep, _)| deps.contains(*dep))
    .map(|(_, runner)| runner)
    .collect();
    match unit.as_slice() {
        [one] => Some(*one),
        [] if deps.contains("@playwright/test") => Some(JsRunner::Playwright),
        _ => None,
    }
}

/// Whether `text` imports or requires `module` by its exact specifier.
pub(super) fn imports_module(text: &str, module: &str) -> bool {
    ['\'', '"'].into_iter().any(|q| {
        let literal = format!("{q}{module}{q}");
        text.match_indices(&literal).any(|(at, _)| {
            let before = text[..at].trim_end();
            ["from", "import", "require(", "import("]
                .iter()
                .any(|kw| before.ends_with(kw))
        })
    })
}

/// A runner-owned module the file imports. These modules only exist inside
/// their runner, so the import outranks what the package declares. A
/// TypeScript file importing `node:test` is left to package evidence, since
/// `node --test` runs TypeScript only on Node versions that strip types.
pub(super) fn runner_from_imports(text: &str, path: &str) -> Option<JsRunner> {
    let js = matches!(extension(path), "js" | "jsx" | "mjs" | "cjs");
    [
        ("node:test", JsRunner::NodeTest),
        ("bun:test", JsRunner::Bun),
        ("@playwright/test", JsRunner::Playwright),
        ("vitest", JsRunner::Vitest),
        ("@jest/globals", JsRunner::Jest),
    ]
    .into_iter()
    .filter(|(_, runner)| js || *runner != JsRunner::NodeTest)
    .find(|(module, _)| imports_module(text, module))
    .map(|(_, runner)| runner)
}

fn has_vitest_config(root: &Path, dir: &str) -> bool {
    let base = if dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir)
    };
    VITEST_CONFIGS.iter().any(|name| base.join(name).is_file())
}

fn package_evidence(root: &Path, dir: &str, memo: &mut PackageMemo) -> Option<JsRunner> {
    if let Some(pkg) = memo.get(root, dir) {
        let found = pkg
            .scripts_test
            .as_deref()
            .and_then(runner_from_script)
            .or_else(|| runner_from_deps(&pkg.deps))
            .or_else(|| pkg.jest_key.then_some(JsRunner::Jest));
        if found.is_some() {
            return found;
        }
    }
    has_vitest_config(root, dir).then_some(JsRunner::Vitest)
}

/// Ancestor directories of `path`, nearest first, ending at the root (`""`).
fn ancestors(path: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut dir = parent_dir(path).to_string();
    loop {
        let done = dir.is_empty();
        dirs.push(dir.clone());
        if done {
            return dirs;
        }
        dir = parent_dir(&dir).to_string();
    }
}

/// The directory the runner is started from and the runner itself.
/// Directories are walked nearest first; one without a `package.json` is
/// skipped (except the root, whose vitest config also counts), and a package
/// without evidence defers to its enclosing package.
pub(super) fn resolve(
    root: &Path,
    path: &str,
    memo: &mut PackageMemo,
) -> Option<(String, JsRunner)> {
    if !safe_relative_dir(parent_dir(path)) {
        return None;
    }
    let dirs = ancestors(path);
    if let Some(runner) =
        read_source_bounded(Some(root), path).and_then(|text| runner_from_imports(&text, path))
    {
        let package_dir = dirs
            .iter()
            .find(|d| memo.get(root, d).is_some())
            .cloned()
            .unwrap_or_default();
        return Some((package_dir, runner));
    }
    for dir in &dirs {
        if !dir.is_empty() && memo.get(root, dir).is_none() {
            continue;
        }
        if let Some(runner) = package_evidence(root, dir, memo) {
            return Some((dir.clone(), runner));
        }
    }
    None
}

/// Commands for JavaScript/TypeScript test paths, grouped per start
/// directory and runner, plus the paths no evidence names a runner for.
/// A nested package's command runs in a subshell that enters the package,
/// so its config and local binaries apply and the caller's working
/// directory is left at the project root.
pub(super) fn commands(root: Option<&Path>, paths: &[String]) -> (Vec<TestCommand>, Vec<String>) {
    let Some(root) = root else {
        return (Vec::new(), paths.to_vec());
    };
    let mut memo = PackageMemo::default();
    let mut groups: BTreeMap<(String, JsRunner), Vec<String>> = BTreeMap::new();
    let mut unresolved = Vec::new();
    for path in paths {
        match resolve(root, path, &mut memo) {
            Some(key) => groups.entry(key).or_default().push(path.clone()),
            None => unresolved.push(path.clone()),
        }
    }
    let commands = groups
        .into_iter()
        .map(|((dir, runner), mut covers)| {
            covers.sort();
            let args: Vec<String> = covers
                .iter()
                .map(|p| {
                    let rel = if dir.is_empty() {
                        p.as_str()
                    } else {
                        p.strip_prefix(&format!("{dir}/")).unwrap_or(p)
                    };
                    runner.arg(rel)
                })
                .collect();
            let run = format!("{} {}", runner.invocation(), args.join(" "));
            let text = if dir.is_empty() {
                run
            } else {
                format!("(cd {} && {run})", shell_quote(&dir))
            };
            command(text, covers, runner.framework(), SOURCE_CONVENTION)
        })
        .collect();
    (commands, unresolved)
}

pub(super) fn unresolved_note(paths: &[String]) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(NOTE_PATH_CAP)
        .map(String::as_str)
        .collect();
    let more = paths.len().saturating_sub(NOTE_PATH_CAP);
    let list = if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    };
    format!(
        "no JavaScript/TypeScript test runner named for {list}: no runner import, `package.json` \
         `scripts.test`, runner dependency, `jest` key, or vitest config names one, so no command \
         was emitted"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_runner_reads_segments_and_steps_over_wrappers() {
        let at = runner_from_script;
        assert_eq!(at("vitest run"), Some(JsRunner::Vitest));
        assert_eq!(at("jest --coverage"), Some(JsRunner::Jest));
        assert_eq!(at("c8 mocha 'test/**/*.js'"), Some(JsRunner::Mocha));
        assert_eq!(
            at("cross-env NODE_ENV=test ./node_modules/.bin/jest"),
            Some(JsRunner::Jest)
        );
        assert_eq!(at("node --test test/"), Some(JsRunner::NodeTest));
        assert_eq!(
            at("node --test-reporter=spec --test"),
            Some(JsRunner::NodeTest)
        );
        assert_eq!(at("node scripts/test.js"), None);
        assert_eq!(at("bun test"), Some(JsRunner::Bun));
        assert_eq!(at("bun run build"), None);
        assert_eq!(at("playwright test"), Some(JsRunner::Playwright));
        assert_eq!(at("tsc && vitest run"), Some(JsRunner::Vitest));
        assert_eq!(at("echo \"Error: no test specified\" && exit 1"), None);
        assert_eq!(at("npm run test:unit"), None);
        assert_eq!(at("turbo run test"), None);
        assert_eq!(at("eslint --config vitest.config.ts ."), None);
    }

    #[test]
    fn dependency_runner_needs_a_single_unit_runner() {
        let deps = |names: &[&str]| names.iter().map(|n| n.to_string()).collect();
        assert_eq!(runner_from_deps(&deps(&["vitest"])), Some(JsRunner::Vitest));
        assert_eq!(
            runner_from_deps(&deps(&["jest", "@playwright/test"])),
            Some(JsRunner::Jest)
        );
        assert_eq!(
            runner_from_deps(&deps(&["@playwright/test"])),
            Some(JsRunner::Playwright)
        );
        assert_eq!(runner_from_deps(&deps(&["vitest", "jest"])), None);
        assert_eq!(runner_from_deps(&deps(&[])), None);
    }

    #[test]
    fn imports_match_specifiers_not_mentions() {
        assert!(imports_module("import test from 'node:test';", "node:test"));
        assert!(imports_module(
            "import { it } from \"node:test\"",
            "node:test"
        ));
        assert!(imports_module(
            "const t = require('node:test')",
            "node:test"
        ));
        assert!(imports_module("await import(\"node:test\")", "node:test"));
        assert!(!imports_module(
            "// run with 'node:test' later",
            "node:test"
        ));
        assert!(!imports_module("const s = 'node:test';", "node:test"));
        assert!(!imports_module(
            "import x from 'node:test-helpers'",
            "node:test"
        ));

        let js = "import { test } from 'node:test';";
        assert_eq!(
            runner_from_imports(js, "a.test.mjs"),
            Some(JsRunner::NodeTest)
        );
        assert_eq!(runner_from_imports(js, "a.test.ts"), None);
        assert_eq!(
            runner_from_imports(
                "import { test, expect } from '@playwright/test';",
                "e.spec.ts"
            ),
            Some(JsRunner::Playwright)
        );
        assert_eq!(
            runner_from_imports("import { describe } from 'vitest'", "a.test.ts"),
            Some(JsRunner::Vitest)
        );
        assert_eq!(
            runner_from_imports("test('x', () => {})", "a.test.js"),
            None
        );
    }

    #[test]
    fn malformed_and_escaping_manifests_carry_no_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("package.json"), "{ not json").unwrap();
        let mut memo = PackageMemo::default();
        let pkg = memo
            .get(root, "")
            .expect("a malformed manifest still marks a package");
        assert!(pkg.scripts_test.is_none() && pkg.deps.is_empty() && !pkg.jest_key);
        assert!(memo.get(root, "../elsewhere").is_none());
        assert_eq!(resolve(root, "../x/a.test.js", &mut memo), None);
        assert_eq!(resolve(root, "src/a.test.js", &mut memo), None);
    }

    #[test]
    fn bun_paths_are_anchored_and_the_note_is_capped() {
        assert_eq!(JsRunner::Bun.arg("src/a.test.ts"), "./src/a.test.ts");
        assert_eq!(JsRunner::Jest.arg("src/a.test.ts"), "src/a.test.ts");
        let paths: Vec<String> = (0..7).map(|i| format!("t{i}.test.js")).collect();
        let note = unresolved_note(&paths);
        assert!(note.contains("t4.test.js and 2 more"), "{note}");
        assert!(!note.contains("t5.test.js"), "{note}");
    }
}
