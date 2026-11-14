//! JavaScript/TypeScript module specifiers mapped through a governing
//! `tsconfig*.json` / `jsconfig*.json`: `compilerOptions.paths` and `baseUrl`.
//!
//! Index time records one [`JsPathConfig`] per config file with its `extends`
//! chain applied and every path made project-relative; `paths` are stored
//! once, on the config that declares them. Query time picks the config that
//! governs an importer and maps a bare specifier to the project-relative
//! paths TypeScript would try, in its order.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// TypeScript 5.5's placeholder for the directory of the config in use.
pub const CONFIG_DIR: &str = "${configDir}";

/// One config file with its `extends` chain applied. Every path is
/// project-relative and lexically normalized; entries that would leave the
/// project are dropped when the record is built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsPathConfig {
    pub path: String,
    /// `files`, absent when no config in the chain declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    /// `include` patterns, absent when no config in the chain declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// `exclude` patterns, absent when no config in the chain declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// Referenced config files (`references[].path`), never inherited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// The config files this one extends, directly or transitively.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    /// The effective `baseUrl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The config whose own `paths` are in effect: this one or an ancestor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths_from: Option<String>,
    /// This file's own `paths`, as written, in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declared_paths: Vec<(String, Vec<String>)>,
}

impl JsPathConfig {
    fn dir(&self) -> &str {
        parent_dir(&self.path)
    }

    /// TypeScript discovers `tsconfig.json` and `jsconfig.json` by name. Any
    /// other `tsconfig.<name>.json` is a governing candidate only when its
    /// chain declares `files` or `include`; one without, such as a
    /// `tsconfig.base.json`, exists to be extended and would otherwise claim
    /// every file beneath it. Either kind is still reached through `references`.
    pub fn discoverable(&self) -> bool {
        config_rank(&self.path) < 2 || self.files.is_some() || self.include.is_some()
    }

    fn maps_modules(&self) -> bool {
        self.paths_from.is_some() || self.base_url.is_some()
    }

    /// How this config's `files` / `include` / `exclude` treat `file`, with
    /// TypeScript's defaults: `include` defaults to everything under the
    /// config's directory unless `files` is set, and `exclude` defaults to
    /// the package-manager directories.
    pub fn selection(&self, file: &str) -> Selection {
        Selector::new(self).selection(file)
    }

    pub fn includes(&self, file: &str) -> bool {
        self.selection(file) == Selection::Included
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Included,
    /// `include` matches but `exclude` removes the file.
    Excluded,
    Outside,
}

/// A config's `files` / `include` / `exclude`, compiled once.
#[derive(Debug)]
struct Selector {
    files: HashSet<String>,
    include: Vec<Glob>,
    exclude: Vec<Glob>,
}

impl Selector {
    fn new(config: &JsPathConfig) -> Self {
        let dir = config.dir();
        let include = match (&config.include, &config.files) {
            (Some(include), _) => include.iter().map(|p| Glob::include(p)).collect(),
            (None, Some(_)) => Vec::new(),
            (None, None) => vec![Glob::new(&join_pattern(dir, "**/*"))],
        };
        let exclude = match &config.exclude {
            Some(exclude) => exclude.iter().map(|p| Glob::new(p)).collect(),
            None => ["node_modules", "bower_components", "jspm_packages"]
                .iter()
                .map(|name| Glob::new(&join_pattern(dir, name)))
                .collect(),
        };
        Self {
            files: config.files.iter().flatten().cloned().collect(),
            include,
            exclude,
        }
    }

    fn selection(&self, file: &str) -> Selection {
        if self.files.contains(file) {
            return Selection::Included;
        }
        let segments: Vec<&str> = file.split('/').filter(|s| !s.is_empty()).collect();
        if !self
            .include
            .iter()
            .any(|glob| glob.matches(&segments, false))
        {
            Selection::Outside
        } else if self
            .exclude
            .iter()
            .any(|glob| glob.matches(&segments, true))
        {
            Selection::Excluded
        } else {
            Selection::Included
        }
    }
}

#[derive(Debug)]
enum Segment {
    /// `**`: any number of path segments.
    Any,
    Literal(String),
    /// A segment with `*` or `?`, as chars and, when ASCII, as bytes.
    Wild(Vec<char>, Option<Vec<u8>>),
}

/// A compiled include/exclude spec: `*` and `?` match within one path
/// segment, `**` any number of segments.
#[derive(Debug)]
struct Glob(Vec<Segment>);

impl Glob {
    fn new(pattern: &str) -> Self {
        Self(
            pattern
                .split('/')
                .filter(|s| !s.is_empty())
                .map(|segment| match segment {
                    "**" => Segment::Any,
                    s if s.contains(['*', '?']) => Segment::Wild(
                        s.chars().collect(),
                        s.is_ascii().then(|| s.as_bytes().to_vec()),
                    ),
                    s => Segment::Literal(s.to_string()),
                })
                .collect(),
        )
    }

    /// An `include` entry whose last component has no `.`, `*`, or `?` names
    /// a directory and selects everything beneath it, as in TypeScript.
    fn include(pattern: &str) -> Self {
        let last = pattern.rsplit('/').next().unwrap_or(pattern);
        if last.contains(['.', '*', '?']) {
            Self::new(pattern)
        } else {
            Self::new(&join_pattern(pattern, "**/*"))
        }
    }

    /// Whether the glob matches `path`, or with `prefix` any leading part
    /// of it: an `exclude` entry removes everything beneath what it matches.
    fn matches(&self, path: &[&str], prefix: bool) -> bool {
        segments_match(&self.0, path, prefix)
    }
}

/// One declaring config's `paths`, compiled for lookup.
#[derive(Debug, Default)]
struct PatternSet {
    exact: HashMap<String, usize>,
    /// `(prefix, suffix, entry)` by descending prefix length, declaration
    /// order on a tie, so the first match is TypeScript's choice.
    wildcards: Vec<(String, String, usize)>,
    substitutions: Vec<Vec<String>>,
}

impl PatternSet {
    fn new(paths: &[(String, Vec<String>)]) -> Self {
        let mut set = Self::default();
        for (pattern, substitutions) in paths {
            let entry = set.substitutions.len();
            set.substitutions.push(substitutions.clone());
            let mut parts = pattern.split('*');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(_), None, _) => {
                    set.exact.entry(pattern.clone()).or_insert(entry);
                }
                (Some(prefix), Some(suffix), None) => {
                    set.wildcards
                        .push((prefix.to_string(), suffix.to_string(), entry));
                }
                // More than one `*` is invalid and never matches.
                _ => {}
            }
        }
        set.wildcards
            .sort_by_key(|(prefix, _, entry)| (std::cmp::Reverse(prefix.len()), *entry));
        set
    }

    /// The matched substitutions and the text `*` stands for.
    fn matching<'a>(&'a self, specifier: &'a str) -> Option<(&'a [String], &'a str)> {
        if let Some(&entry) = self.exact.get(specifier) {
            return Some((&self.substitutions[entry], ""));
        }
        self.wildcards.iter().find_map(|(prefix, suffix, entry)| {
            (specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix.as_str())
                && specifier.ends_with(suffix.as_str()))
            .then(|| {
                (
                    self.substitutions[*entry].as_slice(),
                    &specifier[prefix.len()..specifier.len() - suffix.len()],
                )
            })
        })
    }
}

/// Every recorded config, indexed by directory for governing-config lookup.
#[derive(Debug, Default)]
pub struct JsPathConfigs {
    configs: Vec<JsPathConfig>,
    by_path: HashMap<String, usize>,
    /// Discoverable configs per directory: `tsconfig.json`, then
    /// `jsconfig.json`, then every other `tsconfig.*.json` /
    /// `jsconfig.*.json` by name.
    by_dir: HashMap<String, Vec<usize>>,
    /// Compiled `paths` per declaring config index.
    patterns: HashMap<usize, PatternSet>,
    /// Compiled `files` / `include` / `exclude`, by config index.
    selectors: Vec<Selector>,
    maps_nothing: bool,
    /// Every recorded config path and every `extends` / `references`
    /// target: the files whose edit can change resolution.
    inputs: HashSet<String>,
    /// Governing config per importer, filled on first lookup.
    governing: Mutex<HashMap<String, Option<usize>>>,
    /// Config selections and pattern-set probes performed, for tests that
    /// bound the work a lookup does.
    work: AtomicUsize,
}

/// Importers whose governing config [`JsPathConfigs`] retains.
const MAX_MEMO_ENTRIES: usize = 262_144;

/// How deep `references` are followed when looking for an including config.
const MAX_REFERENCE_DEPTH: usize = 4;

impl JsPathConfigs {
    pub fn new(mut configs: Vec<JsPathConfig>) -> Self {
        configs.sort_by(|a, b| {
            (parent_dir(&a.path), config_rank(&a.path), &a.path).cmp(&(
                parent_dir(&b.path),
                config_rank(&b.path),
                &b.path,
            ))
        });
        configs.dedup_by(|a, b| a.path == b.path);
        let mut by_path = HashMap::new();
        let mut by_dir: HashMap<String, Vec<usize>> = HashMap::new();
        let mut patterns = HashMap::new();
        let mut inputs = HashSet::new();
        for (index, config) in configs.iter().enumerate() {
            by_path.insert(config.path.clone(), index);
            if config.discoverable() {
                by_dir
                    .entry(parent_dir(&config.path).to_string())
                    .or_default()
                    .push(index);
            }
            if !config.declared_paths.is_empty() {
                patterns.insert(index, PatternSet::new(&config.declared_paths));
            }
            inputs.insert(config.path.clone());
            inputs.extend(config.extends.iter().cloned());
            inputs.extend(config.references.iter().cloned());
        }
        let maps_nothing = !configs.iter().any(JsPathConfig::maps_modules);
        let selectors = configs.iter().map(Selector::new).collect();
        Self {
            configs,
            by_path,
            by_dir,
            patterns,
            selectors,
            maps_nothing,
            inputs,
            governing: Mutex::default(),
            work: AtomicUsize::new(0),
        }
    }

    /// True when no recorded config declares `paths` or `baseUrl`.
    pub fn maps_nothing(&self) -> bool {
        self.maps_nothing
    }

    pub fn configs(&self) -> &[JsPathConfig] {
        &self.configs
    }

    /// Whether `path` is a recorded config or a file one extends or references.
    pub fn is_input(&self, path: &str) -> bool {
        self.inputs.contains(path)
    }

    /// Config selections and pattern-set probes performed so far.
    pub fn work(&self) -> usize {
        self.work.load(Ordering::Relaxed)
    }

    /// The config that governs `file`: walking up from its directory, the
    /// first discoverable config (or config reached through its
    /// `references`) whose `files` / `include` / `exclude` select it. When
    /// none selects it but one's `include` matches and its `exclude` removes
    /// it (a test file left out of the app config), the nearest such config
    /// applies: editors would leave the file without aliases, but the test
    /// runners that load it share the app's. A file no `include` matches
    /// gets no config, as in TypeScript.
    pub fn governing(&self, file: &str) -> Option<&JsPathConfig> {
        let memo = |memo: &mut HashMap<String, Option<usize>>| memo.get(file).copied();
        if let Some(found) = self.governing.lock().ok().as_deref_mut().and_then(memo) {
            return found.map(|index| &self.configs[index]);
        }
        let found = self.find_governing(file);
        if let Ok(mut memo) = self.governing.lock()
            && memo.len() < MAX_MEMO_ENTRIES
        {
            memo.insert(file.to_string(), found);
        }
        found.map(|index| &self.configs[index])
    }

    fn find_governing(&self, file: &str) -> Option<usize> {
        let mut dir = parent_dir(file);
        let mut excluded = None;
        let mut visited = HashSet::new();
        loop {
            if let Some(indexes) = self.by_dir.get(dir) {
                for &index in indexes {
                    if let Some(found) = self.including(index, file, &mut visited, &mut excluded, 0)
                    {
                        return Some(found);
                    }
                }
            }
            if dir.is_empty() {
                break;
            }
            dir = parent_dir(dir);
        }
        excluded
    }

    fn including(
        &self,
        index: usize,
        file: &str,
        visited: &mut HashSet<usize>,
        excluded: &mut Option<usize>,
        depth: usize,
    ) -> Option<usize> {
        if !visited.insert(index) {
            return None;
        }
        self.work.fetch_add(1, Ordering::Relaxed);
        let config = &self.configs[index];
        match self.selectors[index].selection(file) {
            Selection::Included => return Some(index),
            Selection::Excluded => {
                excluded.get_or_insert(index);
            }
            Selection::Outside => {}
        }
        if depth >= MAX_REFERENCE_DEPTH {
            return None;
        }
        config.references.iter().find_map(|reference| {
            let &next = self.by_path.get(reference)?;
            self.including(next, file, visited, excluded, depth + 1)
        })
    }

    /// The project-relative paths, in TypeScript's order, that a bare
    /// `specifier` written in `importer` names through its governing config:
    /// the substitutions of the matching `paths` pattern, then the
    /// `baseUrl` join. Each path still takes the importer's extension and
    /// directory-index probing. Relative and rooted specifiers never map.
    pub fn module_paths(&self, specifier: &str, importer: &str) -> Vec<String> {
        if self.maps_nothing || !is_bare_specifier(specifier) {
            return Vec::new();
        }
        let Some(config) = self.governing(importer) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(from) = &config.paths_from
            && let Some(&declarer) = self.by_path.get(from)
            && let Some(set) = self.patterns.get(&declarer)
        {
            self.work.fetch_add(1, Ordering::Relaxed);
            if let Some((substitutions, star)) = set.matching(specifier) {
                let base = config.base_url.as_deref().unwrap_or(parent_dir(from));
                for substitution in substitutions {
                    let substitution = substitution.replacen('*', star, 1);
                    let path = match substitution.strip_prefix(CONFIG_DIR) {
                        Some(rest) => lexical_join(config.dir(), rest),
                        None => lexical_join(base, &substitution),
                    };
                    out.extend(path);
                }
            }
        }
        if let Some(base) = &config.base_url
            && let Some(path) = lexical_join(base, specifier)
        {
            out.push(path);
        }
        out
    }
}

/// Bare import specifiers mapped through the configs at index time, by
/// importer: each to the importer-relative specifier (`./x`, `../lib/y`)
/// that names the same indexed file.
#[derive(Debug, Default)]
pub struct JsImportMappings {
    by_importer: HashMap<String, HashMap<String, String>>,
    len: usize,
}

impl JsImportMappings {
    /// From `(importer, specifier, relative specifier)` rows.
    pub fn new(rows: impl IntoIterator<Item = (String, String, String)>) -> Self {
        let mut mappings = Self::default();
        for (importer, specifier, mapped) in rows {
            mappings
                .by_importer
                .entry(importer)
                .or_default()
                .insert(specifier, mapped);
            mappings.len += 1;
        }
        mappings
    }

    pub fn get(&self, importer: &str, specifier: &str) -> Option<&str> {
        self.by_importer
            .get(importer)?
            .get(specifier)
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

/// A specifier `paths` and `baseUrl` may map: not relative (`./`, `../`,
/// `.`, `..`), not rooted, and not a URL-like scheme such as `node:`.
pub fn is_bare_specifier(specifier: &str) -> bool {
    !(specifier.is_empty()
        || specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || specifier.contains("::")
        || specifier
            .split_once(':')
            .is_some_and(|(scheme, _)| !scheme.contains('/')))
}

fn config_rank(path: &str) -> u8 {
    match path.rsplit('/').next().unwrap_or(path) {
        "tsconfig.json" => 0,
        "jsconfig.json" => 1,
        _ => 2,
    }
}

/// True for the config file names TypeScript and editors discover or that
/// projects conventionally split a config into: `tsconfig.json`,
/// `jsconfig.json`, and `tsconfig.<name>.json` / `jsconfig.<name>.json`.
pub fn is_config_file_name(name: &str) -> bool {
    ["tsconfig", "jsconfig"].iter().any(|stem| {
        name.strip_prefix(stem)
            .and_then(|rest| rest.strip_suffix(".json"))
            .is_some_and(|middle| {
                middle.is_empty() || (middle.len() > 1 && middle.starts_with('.'))
            })
    })
}

/// The directory part of a project-relative path; `""` for the root.
pub fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Joins `relative` onto `base`, resolving `.` and `..` textually. `None`
/// when the result would leave the project root.
pub fn lexical_join(base: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn join_pattern(dir: &str, pattern: &str) -> String {
    if dir.is_empty() {
        pattern.to_string()
    } else {
        format!("{dir}/{pattern}")
    }
}

fn segments_match(pattern: &[Segment], path: &[&str], prefix: bool) -> bool {
    match pattern.split_first() {
        None => prefix || path.is_empty(),
        Some((Segment::Any, rest)) => {
            (0..=path.len()).any(|skip| segments_match(rest, &path[skip..], prefix))
        }
        Some((first, rest)) => path.split_first().is_some_and(|(segment, tail)| {
            let matched = match first {
                Segment::Literal(literal) => literal == segment,
                Segment::Wild(wild, ascii) => match ascii {
                    Some(bytes) if segment.is_ascii() => {
                        wildcard_matches(bytes, segment.as_bytes())
                    }
                    _ => segment_matches(wild, segment),
                },
                Segment::Any => unreachable!("handled above"),
            };
            matched && segments_match(rest, tail, prefix)
        }),
    }
}

fn segment_matches(pattern: &[char], segment: &str) -> bool {
    if segment.is_ascii() && pattern.iter().all(char::is_ascii) {
        let pattern: Vec<u8> = pattern.iter().map(|&c| c as u8).collect();
        return wildcard_matches(&pattern, segment.as_bytes());
    }
    let segment: Vec<char> = segment.chars().collect();
    wildcard_matches(pattern, &segment)
}

fn wildcard_matches<T: Copy + PartialEq + WildcardUnit>(pattern: &[T], segment: &[T]) -> bool {
    let (mut p, mut s) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while s < segment.len() {
        if p < pattern.len() && (pattern[p] == T::ONE || pattern[p] == segment[s]) {
            p += 1;
            s += 1;
        } else if p < pattern.len() && pattern[p] == T::ANY {
            backtrack = Some((p, s));
            p += 1;
        } else if let Some((star, matched)) = backtrack {
            p = star + 1;
            s = matched + 1;
            backtrack = Some((star, matched + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == T::ANY)
}

/// The `?` and `*` units of a wildcard pattern, as bytes or chars.
trait WildcardUnit {
    const ONE: Self;
    const ANY: Self;
}

impl WildcardUnit for u8 {
    const ONE: Self = b'?';
    const ANY: Self = b'*';
}

impl WildcardUnit for char {
    const ONE: Self = '?';
    const ANY: Self = '*';
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path: &str) -> JsPathConfig {
        JsPathConfig {
            path: path.into(),
            ..JsPathConfig::default()
        }
    }

    fn with_paths(path: &str, entries: &[(&str, &[&str])]) -> JsPathConfig {
        JsPathConfig {
            path: path.into(),
            paths_from: Some(path.into()),
            declared_paths: entries
                .iter()
                .map(|(pattern, subs)| {
                    (
                        pattern.to_string(),
                        subs.iter().map(|s| s.to_string()).collect(),
                    )
                })
                .collect(),
            ..JsPathConfig::default()
        }
    }

    #[test]
    fn exact_pattern_beats_wildcard_and_longest_prefix_wins() {
        let app = with_paths(
            "tsconfig.json",
            &[
                ("@/*", &["src/*"]),
                ("@/lib/*", &["lib/*"]),
                ("@/lib/exact", &["special/exact"]),
                ("*", &["types/*"]),
                ("@lib/*-kit", &["kits/*/kit.ts"]),
                ("a*b*c", &["never/*"]),
            ],
        );
        let configs = JsPathConfigs::new(vec![app]);
        assert_eq!(configs.module_paths("@/lib/a", "src/x.ts"), ["lib/a"]);
        assert_eq!(
            configs.module_paths("@/lib/exact", "src/x.ts"),
            ["special/exact"]
        );
        assert_eq!(configs.module_paths("@/other", "src/x.ts"), ["src/other"]);
        assert_eq!(configs.module_paths("react", "src/x.ts"), ["types/react"]);
        assert_eq!(
            configs.module_paths("@lib/ui-kit", "src/x.ts"),
            ["kits/ui/kit.ts"]
        );
        assert_eq!(configs.module_paths("axbxc", "src/x.ts"), ["types/axbxc"]);
        assert!(configs.module_paths("./local", "src/x.ts").is_empty());
        assert!(configs.module_paths("node:fs", "src/x.ts").is_empty());
    }

    #[test]
    fn substitutions_keep_order_then_base_url_follows() {
        let mut app = with_paths("tsconfig.json", &[("~/*", &["first/*", "second/*.ts"])]);
        app.base_url = Some("src".into());
        let configs = JsPathConfigs::new(vec![app]);
        assert_eq!(
            configs.module_paths("~/a/b", "src/x.ts"),
            ["src/first/a/b", "src/second/a/b.ts", "src/~/a/b"]
        );
        assert_eq!(configs.module_paths("util", "src/x.ts"), ["src/util"]);
        // `src/first/../../up` and `src/second/../../up.ts` stay inside the
        // project; the `baseUrl` join `src/~/../../up` does too.
        assert_eq!(
            configs.module_paths("~/../../up", "src/x.ts"),
            ["up", "up.ts", "up"]
        );
        assert_eq!(
            configs.module_paths("~/../../../../escape", "src/x.ts"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn inherited_paths_resolve_against_the_declarer_and_config_dir_against_the_leaf() {
        let base = with_paths(
            "cfg/tsconfig.shared.json",
            &[("~/*", &["${configDir}/src/*"]), ("#lib/*", &["../lib/*"])],
        );
        let mut leaf = config("pkg/tsconfig.json");
        leaf.paths_from = Some("cfg/tsconfig.shared.json".into());
        leaf.extends = vec!["cfg/tsconfig.shared.json".into()];
        let configs = JsPathConfigs::new(vec![base, leaf]);
        assert_eq!(configs.module_paths("~/a", "pkg/main.ts"), ["pkg/src/a"]);
        assert_eq!(configs.module_paths("#lib/x", "pkg/main.ts"), ["lib/x"]);
        assert!(configs.is_input("cfg/tsconfig.shared.json"));
        assert!(!configs.is_input("pkg/main.ts"));
    }

    #[test]
    fn solution_config_defers_to_the_reference_that_includes_the_file() {
        let mut solution = with_paths("apps/web/tsconfig.json", &[("@/*", &["src/*"])]);
        solution.files = Some(Vec::new());
        solution.references = vec!["apps/web/tsconfig.app.json".into()];
        let mut app = with_paths(
            "apps/web/tsconfig.app.json",
            &[("@pages/*", &["src/pages/*"])],
        );
        app.include = Some(vec!["apps/web/src".into()]);
        app.exclude = Some(vec!["apps/web/src/**/*.test.ts".into()]);
        let mut node = config("apps/web/tsconfig.node.json");
        node.include = Some(vec!["apps/web/vite.config.ts".into()]);
        let base = config("tsconfig.base.json");
        let configs = JsPathConfigs::new(vec![node, app, solution, base]);
        let path = |file| configs.governing(file).map(|c| c.path.as_str());
        assert_eq!(
            path("apps/web/src/router.tsx"),
            Some("apps/web/tsconfig.app.json")
        );
        assert_eq!(
            path("apps/web/vite.config.ts"),
            Some("apps/web/tsconfig.node.json")
        );
        assert_eq!(
            path("apps/web/src/a.test.ts"),
            Some("apps/web/tsconfig.app.json"),
            "a file the app config's exclude removes falls back to that config"
        );
        assert_eq!(
            path("apps/web/scripts/gen.ts"),
            None,
            "a file no include matches gets no config"
        );
        assert_eq!(
            path("other/x.ts"),
            None,
            "a base config without files or include governs nothing by itself"
        );
    }

    #[test]
    fn lookups_do_bounded_work_however_many_configs_and_patterns_exist() {
        let patterns: Vec<(String, Vec<String>)> = (0..400)
            .map(|i| {
                (
                    format!("@org/lib{i}"),
                    vec![format!("libs/lib{i}/src/index.ts")],
                )
            })
            .collect();
        let mut configs = vec![JsPathConfig {
            path: "tsconfig.base.json".into(),
            paths_from: Some("tsconfig.base.json".into()),
            declared_paths: patterns,
            ..JsPathConfig::default()
        }];
        for i in 0..400 {
            let mut lib = config(&format!("libs/lib{i}/tsconfig.json"));
            lib.files = Some(Vec::new());
            lib.references = vec![format!("libs/lib{i}/tsconfig.lib.json")];
            lib.paths_from = Some("tsconfig.base.json".into());
            let mut inner = config(&format!("libs/lib{i}/tsconfig.lib.json"));
            inner.include = Some(vec![format!("libs/lib{i}/src/**/*.ts")]);
            inner.paths_from = Some("tsconfig.base.json".into());
            configs.extend([lib, inner]);
        }
        let configs = JsPathConfigs::new(configs);
        let importers: Vec<String> = (0..400)
            .flat_map(|i| (0..7).map(move |f| format!("libs/lib{i}/src/f{f}.ts")))
            .collect();
        let specifiers = ["react", "lodash", "@org/lib7", "@tanstack/react-query"];
        for importer in &importers {
            for specifier in specifiers {
                configs.module_paths(specifier, importer);
            }
        }
        // Two selections per importer (the solution config, then its
        // reference), memoized, plus one pattern-set probe per lookup.
        let lookups = importers.len() * specifiers.len();
        assert_eq!(configs.work(), importers.len() * 2 + lookups);
        assert_eq!(
            configs.module_paths("@org/lib7", "libs/lib1/src/f0.ts"),
            ["libs/lib7/src/index.ts"]
        );
    }

    #[test]
    fn include_and_exclude_follow_typescript_spec_shapes() {
        let mut c = config("pkg/tsconfig.json");
        c.include = Some(vec!["pkg/src".into(), "pkg/scripts/*.ts".into()]);
        c.exclude = Some(vec!["pkg/src/generated".into()]);
        assert!(c.includes("pkg/src/a/b.ts"));
        assert!(c.includes("pkg/scripts/run.ts"));
        assert_eq!(c.selection("pkg/scripts/deep/run.ts"), Selection::Outside);
        assert_eq!(c.selection("pkg/src/generated/x.ts"), Selection::Excluded);
        assert!(!c.includes("other/src/a.ts"));
        let defaults = config("pkg/tsconfig.json");
        assert!(defaults.includes("pkg/a/b.ts"));
        assert!(!defaults.includes("pkg/node_modules/x/index.ts"));
    }

    #[test]
    fn config_file_names() {
        for name in ["tsconfig.json", "jsconfig.json", "tsconfig.app.json"] {
            assert!(is_config_file_name(name), "{name}");
        }
        for name in [
            "tsconfig..json",
            "tsconfig.json5",
            "mytsconfig.json",
            "tsconfig",
        ] {
            assert!(!is_config_file_name(name), "{name}");
        }
    }

    #[test]
    fn segment_wildcards() {
        let wild = |p: &str| p.chars().collect::<Vec<_>>();
        assert!(segment_matches(&wild("*.test.ts"), "a.test.ts"));
        assert!(!segment_matches(&wild("*.test.ts"), "a.ts"));
        assert!(segment_matches(&wild("a?c"), "abc"));
        let glob = |p: &str, path: &str| {
            let path: Vec<&str> = path.split('/').collect();
            Glob::new(p).matches(&path, false)
        };
        assert!(glob("src/**/*.ts", "src/x.ts"));
        assert!(glob("src/**/*.ts", "src/a/b/x.ts"));
        assert!(!glob("src/**/*.ts", "lib/x.ts"));
        assert!(Glob::new("src/gen").matches(&["src", "gen", "x.ts"], true));
        assert!(!Glob::new("src/gen").matches(&["src", "gen", "x.ts"], false));
    }
}
