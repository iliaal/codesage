//! Runnable test commands and inline test modules for `recommend_tests`.
//!
//! Everything here is derived from indexed paths, symbol rows, and manifest
//! files that happen to exist under the project root. Nothing is executed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use anyhow::Result;
use codesage_protocol::{InlineTestModule, Symbol, SymbolKind, TestCommand};
use codesage_storage::Database;

pub(super) const SOURCE_CONVENTION: &str = "convention";
pub(super) const SOURCE_INLINE: &str = "inline";
pub(super) const SOURCE_FEATURE: &str = "feature_test_command";

/// Lines above a `mod` item scanned for `#[cfg(test)]`; other attributes and
/// doc comments may sit between the gate and the item.
const CFG_TEST_LOOKBACK: usize = 3;

/// Above this many integration-test targets in one crate the per-target
/// commands collapse into the crate's whole suite.
const CARGO_PER_TARGET_CAP: usize = 3;

/// Largest source file read to count `#[test]` attributes; the same bound
/// `edit_check` puts on a source file. Larger files use the symbol count.
const MAX_SOURCE_BYTES: u64 = 1_048_576;

/// Test-function attributes counted inside a Rust test module. Matched as a
/// line prefix after trimming, so `#[tokio::test(flavor = ...)]` counts.
const TEST_ATTRIBUTES: [&str; 6] = [
    "#[test]",
    "#[tokio::test",
    "#[rstest",
    "#[sqlx::test",
    "#[async_std::test",
    "#[test_case",
];

pub(super) struct CommandContext<'a> {
    pub root: Option<&'a Path>,
    /// Test files to run: `primary` plus `reachable` paths.
    pub test_files: &'a [String],
    /// `.phpt` paths withheld from `primary`; only their directories are named.
    pub withheld_phpt: &'a [String],
    /// Changed inputs as `(normalized, as_given)`.
    pub changed: &'a [(String, String)],
}

pub(super) struct Derived {
    pub commands: Vec<TestCommand>,
    pub modules: Vec<InlineTestModule>,
    pub notes: Vec<String>,
}

/// Bytes that need no quoting in a POSIX shell word.
fn shell_safe_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_./:@%+=,-".contains(&b)
}

/// Quote one shell word with POSIX single quotes when it holds anything
/// outside `[A-Za-z0-9_./:@%+=,-]`; a clean token is returned unchanged so
/// ordinary commands stay readable.
fn shell_quote(token: &str) -> String {
    if !token.is_empty() && token.bytes().all(shell_safe_byte) {
        return token.to_string();
    }
    format!("'{}'", token.replace('\'', "'\\''"))
}

/// A repo-relative path as a positional argument: one starting with `-`
/// would be read as a runner flag, so it is anchored with `./` first.
fn path_arg(path: &str) -> String {
    if path.starts_with('-') {
        shell_quote(&format!("./{path}"))
    } else {
        shell_quote(path)
    }
}

fn path_args(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| path_arg(p))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a name token (cargo target, module filter, Java class) can be
/// placed after a flag. None of these can legally start with `-`, so a
/// token that does is dropped and named in a note rather than emitted.
fn flag_safe(token: &str) -> bool {
    !token.starts_with('-')
}

fn dropped_token_note(what: &str, token: &str) -> String {
    format!("{what} `{token}` starts with `-` and was left out of `commands`")
}

/// One `[package] name` lookup per directory per request. `reads` counts
/// manifest opens so tests can pin the memo.
#[derive(Default)]
struct ManifestMemo {
    names: HashMap<String, Option<String>>,
    reads: usize,
}

impl ManifestMemo {
    /// `dir` is repo-relative; anything absolute or climbing with `..`
    /// never reaches the filesystem.
    fn package_name(&mut self, root: &Path, dir: &str) -> Option<String> {
        if !safe_relative_dir(dir) {
            return None;
        }
        if let Some(cached) = self.names.get(dir) {
            return cached.clone();
        }
        let manifest = if dir.is_empty() {
            root.join("Cargo.toml")
        } else {
            root.join(dir).join("Cargo.toml")
        };
        self.reads += 1;
        let name = std::fs::metadata(&manifest)
            .ok()
            .filter(|m| m.is_file() && m.len() <= MAX_SOURCE_BYTES)
            .and_then(|_| std::fs::read_to_string(&manifest).ok())
            .and_then(|text| cargo_package_name(&text));
        self.names.insert(dir.to_string(), name.clone());
        name
    }
}

fn safe_relative_dir(dir: &str) -> bool {
    let path = Path::new(dir);
    !path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn source_rank(source: &str) -> u8 {
    match source {
        SOURCE_FEATURE => 3,
        SOURCE_INLINE => 2,
        _ => 1,
    }
}

pub(super) fn derive(db: &Database, ctx: &CommandContext<'_>) -> Result<Derived> {
    let mut memo = ManifestMemo::default();
    let mut notes = Vec::new();
    let (inline, modules) = inline_commands(db, ctx, &mut memo, &mut notes)?;
    let (feature, feature_notes) = feature_commands(db, ctx.changed)?;
    notes.extend(feature_notes);
    let convention = convention_commands(
        ctx.root,
        ctx.test_files,
        ctx.withheld_phpt,
        &mut memo,
        &mut notes,
    );
    let mut merged: Vec<TestCommand> = Vec::new();
    // The edited file's own tests lead; the mapped runner follows; the
    // convention sweep comes last. An identical string keeps the most
    // specific source.
    for group in [inline, feature, convention] {
        for cmd in finish_group(group) {
            match merged.iter_mut().find(|m| m.command == cmd.command) {
                Some(existing) => {
                    if source_rank(&cmd.source) > source_rank(&existing.source) {
                        existing.source = cmd.source;
                        existing.framework = cmd.framework;
                    }
                    existing.covers.extend(cmd.covers);
                    existing.covers.sort();
                    existing.covers.dedup();
                }
                None => merged.push(cmd),
            }
        }
    }
    Ok(Derived {
        commands: merged,
        modules,
        notes,
    })
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

/// Integration-test entries per crate dir: `(target, path)` pairs, target
/// `None` for a file with no `--test` target of its own.
type RustTargets = BTreeMap<String, (RustCrate, Vec<(Option<String>, String)>)>;

fn convention_commands(
    root: Option<&Path>,
    test_files: &[String],
    withheld_phpt: &[String],
    memo: &mut ManifestMemo,
    notes: &mut Vec<String>,
) -> Vec<TestCommand> {
    let mut out = Vec::new();
    // Rust targets are keyed by crate dir so one crate's targets collapse together.
    let mut rust: RustTargets = BTreeMap::new();
    let mut python: Vec<String> = Vec::new();
    let mut php: Vec<String> = Vec::new();
    let mut phpt_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut go_dirs: BTreeSet<String> = BTreeSet::new();
    let mut go_paths: Vec<String> = Vec::new();
    let mut js: Vec<String> = Vec::new();
    let mut java: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for path in test_files {
        match extension(path) {
            "rs" => {
                let krate = RustCrate::for_path(root, path, memo);
                let target = rust_integration_target(&krate, path);
                if let Some(t) = &target
                    && !flag_safe(t)
                {
                    notes.push(dropped_token_note("cargo test target", t));
                    continue;
                }
                rust.entry(krate.dir.clone())
                    .or_insert_with(|| (krate, Vec::new()))
                    .1
                    .push((target, path.clone()));
            }
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

    for (_, (krate, entries)) in rust {
        let named_targets = entries.iter().filter(|(t, _)| t.is_some()).count();
        let whole_suite = format!("cargo test{}", krate.package_flag());
        if named_targets > CARGO_PER_TARGET_CAP {
            let covers = entries.into_iter().map(|(_, p)| p).collect();
            out.push(command(whole_suite, covers, "cargo", SOURCE_CONVENTION));
            continue;
        }
        for (target, path) in entries {
            let cmd = match target {
                Some(target) => format!("{whole_suite} --test {}", shell_quote(&target)),
                None => whole_suite.clone(),
            };
            out.push(command(cmd, vec![path], "cargo", SOURCE_CONVENTION));
        }
    }
    if !python.is_empty() {
        python.sort();
        out.push(command(
            format!("pytest {}", path_args(&python)),
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
            format!("{runner} {}", path_args(&php)),
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
            format!("php run-tests.php {}", path_arg(target)),
            paths,
            "run-tests",
            SOURCE_CONVENTION,
        ));
    }
    if !go_dirs.is_empty() {
        let dirs: Vec<String> = go_dirs.into_iter().collect();
        go_paths.sort();
        out.push(command(
            format!("go test {}", path_args(&dirs)),
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
            format!("{runner} {}", path_args(&js)),
            js,
            framework,
            SOURCE_CONVENTION,
        ));
    }
    for class in java.keys().filter(|c| !flag_safe(c)) {
        notes.push(dropped_token_note("Java test class", class));
    }
    java.retain(|class, _| flag_safe(class));
    if !java.is_empty() {
        let classes: Vec<String> = java.keys().map(|c| shell_quote(c)).collect();
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
            // `-Dtest=` takes a comma list, so the list is quoted as one word.
            out.push(command(
                format!(
                    "mvn -Dtest={} test",
                    shell_quote(&java.keys().cloned().collect::<Vec<_>>().join(","))
                ),
                covers,
                "maven",
                SOURCE_CONVENTION,
            ));
        }
    }
    out
}

/// The crate a Rust path belongs to. With a project root, the nearest
/// ancestor directory holding a `Cargo.toml` with a `[package] name`; the
/// name feeds `-p`. Without one (or when no manifest is found) the crate dir
/// is the prefix before the first `src` or `tests` segment and `-p` is
/// omitted, since a guessed package name makes Cargo fail outright.
struct RustCrate {
    dir: String,
    name: Option<String>,
}

impl RustCrate {
    fn for_path(root: Option<&Path>, path: &str, memo: &mut ManifestMemo) -> Self {
        if let Some(root) = root
            && let Some(found) = Self::nearest_manifest(root, path, memo)
        {
            return found;
        }
        let parts: Vec<&str> = path.split('/').collect();
        let boundary = parts
            .iter()
            .position(|p| *p == "src" || *p == "tests")
            .unwrap_or(parts.len().saturating_sub(1));
        Self {
            dir: parts[..boundary].join("/"),
            name: None,
        }
    }

    fn nearest_manifest(root: &Path, path: &str, memo: &mut ManifestMemo) -> Option<Self> {
        let mut dir = parent_dir(path).to_string();
        loop {
            if let Some(name) = memo.package_name(root, &dir) {
                return Some(Self {
                    dir,
                    name: Some(name),
                });
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent_dir(&dir).to_string();
        }
    }

    fn package_flag(&self) -> String {
        match &self.name {
            Some(name) => format!(" -p {}", shell_quote(name)),
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

/// The `--test` target for `<crate>/tests/<target>.rs` and
/// `<crate>/tests/<target>/main.rs`. `None` for any other Rust test file: a
/// module of an integration test has no target of its own, so the caller
/// falls back to the crate's whole suite.
fn rust_integration_target(krate: &RustCrate, path: &str) -> Option<String> {
    let rest = krate.rest_under(path, "tests")?;
    if let Some(name) = rest.strip_suffix("/main.rs") {
        (!name.contains('/')).then(|| name.to_string())
    } else if !rest.contains('/') {
        rest.strip_suffix(".rs").map(str::to_string)
    } else {
        None
    }
}

/// Crate-relative module path of a file under `src/`: `lib.rs`, `main.rs`,
/// and `bin/<x>.rs` / `bin/<x>/main.rs` are crate roots (empty path),
/// `a/mod.rs` is `a`, `a/b.rs` is `a::b`. `None` outside `src/`.
fn rust_module_path(krate: &RustCrate, path: &str) -> Option<String> {
    let rest = krate.rest_under(path, "src")?;
    let rest = rest.strip_prefix("bin/").map_or(rest, |bin| {
        bin.split_once('/').map_or(bin, |(_, module)| module)
    });
    let rest = rest.strip_suffix(".rs")?;
    if rest == "lib" || rest == "main" {
        return Some(String::new());
    }
    if krate
        .rest_under(path, "src/bin")
        .is_some_and(|bin| !bin.contains('/'))
    {
        return Some(String::new());
    }
    let rest = rest.strip_suffix("/mod").unwrap_or(rest);
    Some(rest.replace('/', "::"))
}

/// Directory binaries may mount descendants with `#[path]`, so recommend the
/// whole target rather than inventing a substring filter from its disk layout.
fn rust_directory_binary<'p>(krate: &RustCrate, path: &'p str) -> Option<&'p str> {
    let rest = krate.rest_under(path, "src/bin")?;
    rest.split_once('/').map(|(name, _)| name)
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
        .filter(|l| TEST_ATTRIBUTES.iter().any(|attr| l.starts_with(attr)))
        .count()
}

fn within(sym: &Symbol, module: &Symbol) -> bool {
    sym.line_start > module.line_start && sym.line_end <= module.line_end
}

/// The changed file's text when the root is known and the file is within
/// [`MAX_SOURCE_BYTES`]; otherwise `None` and the symbol count is used.
fn read_source_bounded(root: Option<&Path>, path: &str) -> Option<String> {
    let full = root?.join(path);
    let size = std::fs::metadata(&full).ok()?.len();
    if size > MAX_SOURCE_BYTES {
        return None;
    }
    std::fs::read_to_string(full).ok()
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
    memo: &mut ManifestMemo,
    notes: &mut Vec<String>,
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
                let krate = RustCrate::for_path(ctx.root, path, memo);
                let Some(module_path) = rust_module_path(&krate, path) else {
                    continue;
                };
                let source = read_source_bounded(ctx.root, path);
                let directory_binary = rust_directory_binary(&krate, path);
                if let Some(target) = directory_binary
                    && !flag_safe(target)
                {
                    notes.push(dropped_token_note("cargo binary target", target));
                    continue;
                }
                for (name, count) in rust_inline_modules(&symbols, source.as_deref()) {
                    let module = if module_path.is_empty() {
                        name
                    } else {
                        format!("{module_path}::{name}")
                    };
                    if !flag_safe(&module) {
                        notes.push(dropped_token_note("cargo test filter", &module));
                        continue;
                    }
                    let invocation = if let Some(target) = directory_binary {
                        format!(
                            "cargo test{} --bin {}",
                            krate.package_flag(),
                            shell_quote(target)
                        )
                    } else {
                        format!(
                            "cargo test{} {}",
                            krate.package_flag(),
                            shell_quote(&format!("{module}::"))
                        )
                    };
                    commands.push(command(
                        invocation,
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
            format!("pytest {}", path_args(&python_files)),
            python_files,
            "pytest",
            SOURCE_INLINE,
        ));
    }
    Ok((commands, modules))
}

/// Mapped feature `test_command` values, re-exported verbatim. A value with
/// a newline or NUL cannot be one shell command and is dropped with a note.
fn feature_commands(
    db: &Database,
    changed: &[(String, String)],
) -> Result<(Vec<TestCommand>, Vec<String>)> {
    let mut by_command: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();
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
            if cmd.contains(['\n', '\r', '\0']) {
                dropped.insert(feature.feature_id.clone());
                continue;
            }
            by_command
                .entry(cmd.to_string())
                .or_default()
                .push(given.clone());
        }
    }
    let commands = by_command
        .into_iter()
        .map(|(cmd, covers)| {
            let framework = cmd.split_whitespace().next().unwrap_or("").to_string();
            command(cmd, covers, &framework, SOURCE_FEATURE)
        })
        .collect();
    let mut notes = Vec::new();
    if !dropped.is_empty() {
        let ids: Vec<String> = dropped.into_iter().collect();
        notes.push(format!(
            "feature test_command dropped from `commands` for {} (contains a line break or NUL); \
             see `feature_bundle` for the raw value",
            ids.join(", ")
        ));
    }
    Ok((commands, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_clean_tokens_alone_and_quotes_the_rest() {
        assert_eq!(shell_quote("tests/test_a.py"), "tests/test_a.py");
        assert_eq!(
            shell_quote("crate-name_1:x@y%z+w=v,u"),
            "crate-name_1:x@y%z+w=v,u"
        );
        assert_eq!(shell_quote("a b.py"), "'a b.py'");
        assert_eq!(shell_quote("x;id.py"), "'x;id.py'");
        assert_eq!(shell_quote("y$(whoami).py"), "'y$(whoami).py'");
        assert_eq!(shell_quote("z`id`.py"), "'z`id`.py'");
        assert_eq!(shell_quote("it's.py"), "'it'\\''s.py'");
        assert_eq!(shell_quote("a\nb.py"), "'a\nb.py'");
        assert_eq!(shell_quote("*.py"), "'*.py'");
        assert_eq!(shell_quote(""), "''");
    }

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
    fn path_arg_anchors_leading_dashes() {
        assert_eq!(path_arg("-x_test.py"), "./-x_test.py");
        assert_eq!(path_arg("-p evil.py"), "'./-p evil.py'");
        assert_eq!(path_arg("tests/-x.py"), "tests/-x.py");
        assert!(!flag_safe("-Dfoo"));
        assert!(flag_safe("FooTest"));
    }

    #[test]
    fn manifest_memo_rejects_escaping_dirs_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let mut memo = ManifestMemo::default();
        for bad in ["../x", "a/../../x", "/etc", "/"] {
            assert_eq!(memo.package_name(dir.path(), bad), None, "{bad}");
        }
        assert_eq!(memo.reads, 0);
        assert!(safe_relative_dir(""));
        assert!(safe_relative_dir("crates/graph"));
        assert!(safe_relative_dir("./crates"));
    }

    #[test]
    fn manifest_memo_reads_each_directory_once_per_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write("crates/graph/Cargo.toml", "[package]\nname = \"graph\"\n");
        let mut memo = ManifestMemo::default();
        for path in [
            "crates/graph/src/a.rs",
            "crates/graph/src/b.rs",
            "crates/graph/src/deep/c.rs",
            "crates/graph/tests/t.rs",
            "crates/other/src/x.rs",
        ] {
            RustCrate::for_path(Some(root), path, &mut memo);
        }
        // Distinct directories visited: crates/graph/src, crates/graph/src/deep,
        // crates/graph, crates/graph/tests, crates/other/src, crates/other,
        // crates, "" — each opened once.
        assert_eq!(memo.reads, 8, "{:?}", memo.names.keys().collect::<Vec<_>>());
        assert_eq!(
            memo.names.get("crates/graph").cloned().flatten().as_deref(),
            Some("graph")
        );
    }

    #[test]
    fn without_a_manifest_the_package_flag_is_omitted() {
        let mut memo = ManifestMemo::default();
        let krate = RustCrate::for_path(None, "crates/graph/tests/risk_test.rs", &mut memo);
        assert_eq!(krate.dir, "crates/graph");
        assert_eq!(krate.name, None);
        assert_eq!(krate.package_flag(), "");
        let target = |p: &str| {
            let mut memo = ManifestMemo::default();
            rust_integration_target(&RustCrate::for_path(None, p, &mut memo), p)
        };
        assert_eq!(
            target("crates/graph/tests/risk_test.rs").as_deref(),
            Some("risk_test")
        );
        assert_eq!(
            target("crates/graph/tests/suite/main.rs").as_deref(),
            Some("suite")
        );
        assert_eq!(target("crates/graph/tests/suite/helpers.rs"), None);
        assert_eq!(
            target("tests/integration.rs").as_deref(),
            Some("integration")
        );
    }

    #[test]
    fn nearest_manifest_wins_over_the_first_segment_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write("Cargo.toml", "[workspace]\nmembers = [\"tests/helper\"]\n");
        write("tests/helper/Cargo.toml", "[package]\nname = \"helper\"\n");
        write("tests/helper/src/lib.rs", "");
        let mut memo = ManifestMemo::default();
        let krate = RustCrate::for_path(Some(root), "tests/helper/src/lib.rs", &mut memo);
        assert_eq!(krate.dir, "tests/helper");
        assert_eq!(krate.name.as_deref(), Some("helper"));
        assert_eq!(
            rust_module_path(&krate, "tests/helper/src/lib.rs").as_deref(),
            Some("")
        );

        // A workspace-only root manifest names no package.
        let krate = RustCrate::for_path(Some(root), "src/lib.rs", &mut memo);
        assert_eq!(krate.dir, "");
        assert_eq!(krate.name, None);
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

    #[test]
    fn test_attribute_count_recognizes_the_documented_runners() {
        let lines = [
            "mod tests {",
            "    #[test]",
            "    fn a() {}",
            "    #[tokio::test(flavor = \"multi_thread\")]",
            "    async fn b() {}",
            "    #[rstest]",
            "    fn c() {}",
            "    #[sqlx::test]",
            "    async fn d() {}",
            "    #[async_std::test]",
            "    async fn e() {}",
            "    #[test_case(1)]",
            "    fn f() {}",
            "    #[cfg(test)]",
            "    fn helper() {}",
            "}",
        ];
        assert_eq!(count_test_attributes(&lines, 1, lines.len() as u32), 6);
    }
}
