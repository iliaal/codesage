//! Index-time reading of `tsconfig*.json` / `jsconfig*.json` into
//! [`JsPathConfig`] records: JSONC parsing, the `extends` chain, and
//! project-relative `paths` / `baseUrl` / `include` / `exclude` / `references`.
//!
//! A leading UTF-8 BOM is ignored. Only configs inside the project are read,
//! by their real path, so a symlink cannot pull a file from outside it.
//! Package-name `extends` (`@tsconfig/node20`) would need `node_modules` and
//! are skipped. `${configDir}` is replaced with the directory of the config
//! the record is built for.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use codesage_protocol::js_paths::{
    CONFIG_DIR, JsPathConfig, is_config_file_name, lexical_join, parent_dir,
};
use jsonc_parser::{JsonObject, JsonValue, ParseOptions};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CONFIGS: usize = 4096;
const MAX_EXTENDS_DEPTH: usize = 16;

/// Every config that can govern an indexed JavaScript/TypeScript file (the
/// discoverable config files in each directory holding such a file or above
/// it, plus the configs their `references` name), and every config whose
/// declared `paths` one of those inherits.
pub(crate) fn read_js_path_configs<'a>(
    root: &Path,
    js_files: impl IntoIterator<Item = &'a str>,
) -> Vec<JsPathConfig> {
    let dirs = enclosing_dirs(js_files);
    if dirs.is_empty() {
        return Vec::new();
    }
    let mut reader = ConfigReader::new(root);
    let mut pending: Vec<(String, bool)> = config_files_in(root, &dirs)
        .into_iter()
        .map(|path| (path, false))
        .collect();
    let mut out: HashMap<String, JsPathConfig> = HashMap::new();
    let mut required = BTreeSet::new();
    while let Some((path, referenced)) = pending.pop() {
        if out.len() >= MAX_CONFIGS {
            tracing::warn!(
                cap = MAX_CONFIGS,
                "too many tsconfig/jsconfig files; ignoring the rest"
            );
            break;
        }
        if referenced {
            required.insert(path.clone());
        }
        if out.contains_key(&path) {
            continue;
        }
        let Some(config) = reader.record(&path) else {
            // Unreadable or outside the project: recorded by path only.
            out.insert(path.clone(), stub(path));
            continue;
        };
        if config.discoverable() || referenced {
            pending.extend(config.references.iter().map(|r| (r.clone(), true)));
            if let Some(from) = &config.paths_from {
                pending.push((from.clone(), true));
            }
        }
        out.insert(path, config);
    }
    // A config that governs nothing and that nothing inherits from keeps only
    // its path, so a later pass can tell it was already read.
    let mut out: Vec<JsPathConfig> = out
        .into_values()
        .map(|config| {
            if config.discoverable() || required.contains(&config.path) {
                config
            } else {
                stub(config.path)
            }
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// A config recorded by path alone: it selects no file and maps nothing.
/// Only a `tsconfig.json` / `jsconfig.json` stub is a governing candidate,
/// with an empty `files` so it never selects anything; any other name with
/// neither `files` nor `include` is skipped by the governing walk.
fn stub(path: String) -> JsPathConfig {
    let discovered_by_name = matches!(
        path.rsplit('/').next(),
        Some("tsconfig.json" | "jsconfig.json")
    );
    JsPathConfig {
        files: discovered_by_name.then(Vec::new),
        path,
        ..JsPathConfig::default()
    }
}

/// The directories holding a file of `js_files`, and every directory above them.
pub(crate) fn enclosing_dirs<'a>(js_files: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for file in js_files {
        let mut dir = parent_dir(file);
        while dirs.insert(dir.to_string()) && !dir.is_empty() {
            dir = parent_dir(dir);
        }
    }
    dirs
}

/// The config files present in `dirs`, by project-relative path.
pub(crate) fn config_files_in(root: &Path, dirs: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| is_config_file_name(name))
            .collect();
        names.sort();
        out.extend(names.into_iter().map(|name| join(dir, &name)));
    }
    out
}

/// A raw path value with the directory of the config that wrote it.
type Located<T> = (String, T);

/// The options one config file declares itself, as written.
#[derive(Clone, Default)]
struct Declared {
    extends: Vec<String>,
    files: Option<Located<Vec<String>>>,
    include: Option<Located<Vec<String>>>,
    exclude: Option<Located<Vec<String>>>,
    references: Vec<String>,
    paths: Option<Vec<(String, Vec<String>)>>,
    base_url: Option<Located<String>>,
}

/// `extends` applied: inherited fields without the file's own `references`.
#[derive(Clone, Default)]
struct Effective {
    files: Option<Located<Vec<String>>>,
    include: Option<Located<Vec<String>>>,
    exclude: Option<Located<Vec<String>>>,
    /// The config whose own `paths` apply.
    paths_from: Option<String>,
    base_url: Option<Located<String>>,
    /// Every config reached through `extends`.
    extends: Vec<String>,
}

impl Effective {
    fn inherit(&mut self, base: Effective) {
        let Effective {
            files,
            include,
            exclude,
            paths_from,
            base_url,
            extends,
        } = base;
        for (slot, value) in [
            (&mut self.files, files),
            (&mut self.include, include),
            (&mut self.exclude, exclude),
        ] {
            if value.is_some() {
                *slot = value;
            }
        }
        if paths_from.is_some() {
            self.paths_from = paths_from;
        }
        if base_url.is_some() {
            self.base_url = base_url;
        }
        for path in extends {
            if !self.extends.contains(&path) {
                self.extends.push(path);
            }
        }
    }
}

struct ConfigReader {
    root: PathBuf,
    canonical_root: Option<PathBuf>,
    declared: HashMap<String, Option<Declared>>,
    effective: HashMap<String, Option<Effective>>,
}

impl ConfigReader {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            canonical_root: std::fs::canonicalize(root).ok(),
            declared: HashMap::new(),
            effective: HashMap::new(),
        }
    }

    fn record(&mut self, path: &str) -> Option<JsPathConfig> {
        let declared = self.declared(path)?;
        let effective = self.effective(path, &mut Vec::new())?;
        let leaf = parent_dir(path);
        let resolve = |(dir, raw): &Located<String>| match raw.strip_prefix(CONFIG_DIR) {
            Some(rest) => lexical_join(leaf, rest),
            None => lexical_join(dir, raw),
        };
        let list = |value: &Option<Located<Vec<String>>>| {
            value.as_ref().map(|(dir, raws)| {
                raws.iter()
                    .filter_map(|raw| resolve(&(dir.clone(), raw.clone())))
                    .collect()
            })
        };
        Some(JsPathConfig {
            path: path.to_string(),
            files: list(&effective.files),
            include: list(&effective.include),
            exclude: list(&effective.exclude),
            references: declared.references,
            extends: effective.extends.clone(),
            base_url: effective.base_url.as_ref().and_then(resolve),
            paths_from: effective.paths_from,
            declared_paths: declared.paths.unwrap_or_default(),
        })
    }

    fn effective(&mut self, path: &str, stack: &mut Vec<String>) -> Option<Effective> {
        if let Some(cached) = self.effective.get(path) {
            return cached.clone();
        }
        if stack.iter().any(|entry| entry == path) || stack.len() >= MAX_EXTENDS_DEPTH {
            tracing::warn!(config = %path, "tsconfig extends chain is cyclic or too deep; ignoring the repeated base");
            return None;
        }
        let declared = self.declared(path)?;
        stack.push(path.to_string());
        let mut effective = Effective::default();
        for base in &declared.extends {
            effective.inherit(Effective {
                extends: vec![base.clone()],
                ..Effective::default()
            });
            if let Some(inherited) = self.effective(base, stack) {
                effective.inherit(inherited);
            }
        }
        stack.pop();
        effective.inherit(Effective {
            files: declared.files,
            include: declared.include,
            exclude: declared.exclude,
            paths_from: declared.paths.is_some().then(|| path.to_string()),
            base_url: declared.base_url,
            extends: Vec::new(),
        });
        self.effective
            .insert(path.to_string(), Some(effective.clone()));
        Some(effective)
    }

    fn declared(&mut self, path: &str) -> Option<Declared> {
        if let Some(cached) = self.declared.get(path) {
            return cached.clone();
        }
        let declared = self.read(path);
        self.declared.insert(path.to_string(), declared.clone());
        declared
    }

    fn read(&self, path: &str) -> Option<Declared> {
        let full = self.root.join(path);
        let canonical = std::fs::canonicalize(&full).ok()?;
        if !self
            .canonical_root
            .as_ref()
            .is_some_and(|root| canonical.starts_with(root))
        {
            tracing::warn!(config = %path, "tsconfig resolves outside the project; ignoring it");
            return None;
        }
        let metadata = std::fs::metadata(&canonical).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return None;
        }
        let text = std::fs::read_to_string(&canonical).ok()?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let value = match jsonc_parser::parse_to_value(text, &ParseOptions::default()) {
            Ok(Some(JsonValue::Object(object))) => object,
            Ok(_) => return None,
            Err(error) => {
                tracing::warn!(config = %path, %error, "unparseable tsconfig; ignoring it");
                return None;
            }
        };
        Some(declared(&self.root, path, &value))
    }
}

fn declared(root: &Path, path: &str, config: &JsonObject<'_>) -> Declared {
    let dir = parent_dir(path);
    let list = |name: &str| -> Option<Located<Vec<String>>> {
        let array = config.get_array(name)?;
        Some((dir.to_string(), strings(array.iter())))
    };
    let extends = match config.get("extends") {
        Some(JsonValue::String(value)) => vec![value.to_string()],
        Some(JsonValue::Array(values)) => strings(values.iter()),
        _ => Vec::new(),
    }
    .iter()
    .filter_map(|spec| extends_target(root, dir, spec))
    .collect();
    let references = config
        .get_array("references")
        .map(|array| {
            array
                .iter()
                .filter_map(|reference| match reference {
                    JsonValue::Object(reference) => reference.get_string("path").cloned(),
                    _ => None,
                })
                .filter_map(|reference| reference_target(root, dir, &reference))
                .collect()
        })
        .unwrap_or_default();
    let options = config.get_object("compilerOptions");
    let base_url = options
        .and_then(|options| options.get_string("baseUrl"))
        .map(|value| (dir.to_string(), value.to_string()));
    let paths = options
        .and_then(|options| options.get_object("paths"))
        .map(|paths| {
            paths
                .clone()
                .into_iter()
                .map(|(pattern, substitutions)| {
                    let substitutions = match substitutions {
                        JsonValue::Array(values) => strings(values.take_inner().iter()),
                        _ => Vec::new(),
                    };
                    (pattern.into_owned(), substitutions)
                })
                .collect()
        });
    Declared {
        extends,
        files: list("files"),
        include: list("include"),
        exclude: list("exclude"),
        references,
        paths,
        base_url,
    }
}

/// The string elements of a JSON array, skipping any other value.
fn strings<'a, 'b: 'a>(values: impl Iterator<Item = &'a JsonValue<'b>>) -> Vec<String> {
    values
        .filter_map(|value| match value {
            JsonValue::String(value) => Some(value.to_string()),
            _ => None,
        })
        .collect()
}

/// A relative `extends` names a file, with `.json` appended when the
/// written name does not exist, as TypeScript does.
fn extends_target(root: &Path, dir: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let path = lexical_join(dir, spec)?;
    if path.ends_with(".json") || root.join(&path).is_file() {
        Some(path)
    } else {
        Some(format!("{path}.json"))
    }
}

/// A reference names a config file or a directory holding `tsconfig.json`.
fn reference_target(root: &Path, dir: &str, spec: &str) -> Option<String> {
    let path = lexical_join(dir, spec)?;
    if root.join(&path).is_dir() {
        Some(join(&path, "tsconfig.json"))
    } else {
        Some(path)
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}
