use crate::ReferenceKind;

pub const BINDING_INTERPRETATION: &str =
    "codesage/structural/v1;parser-queries=11;extraction=26;trust-boundaries=9";

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct PythonImportSite {
    pub file: String,
    pub to_name: String,
    pub line: u32,
    pub col: u32,
}

/// Stores each import site as a `[file, to_name, line, col]` array rather than
/// a `{"file", "to_name", "line", "col"}` object. Every binding in a module
/// copies the module's visible imports, so these sites dominate
/// `python_bindings.target`. The derived `Deserialize` already accepts a struct
/// as a sequence of its fields in declaration order, so this binary and older
/// ones read both forms; order and duplicates round-trip unchanged.
fn import_sites_as_arrays<S: serde::Serializer>(
    sites: &[PythonImportSite],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(sites.len()))?;
    for site in sites {
        seq.serialize_element(&(&site.file, &site.to_name, site.line, site.col))?;
    }
    seq.end()
}

pub fn is_python_path(path: &str) -> bool {
    path.ends_with(".py") || path.ends_with(".pyi")
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PythonBindingCapture {
    pub attributes: usize,
    #[serde(default, serialize_with = "import_sites_as_arrays")]
    pub import_sites: Vec<PythonImportSite>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PythonBindingTarget {
    pub module: Option<String>,
    pub name: String,
    pub definition_line: Option<u32>,
    #[serde(default)]
    pub wildcard: bool,
    #[serde(default)]
    pub fallback: Option<Box<PythonBindingTarget>>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub export_names: Option<Vec<String>>,
    #[serde(default)]
    pub bound: bool,
    #[serde(default, serialize_with = "import_sites_as_arrays")]
    pub import_sites: Vec<PythonImportSite>,
    #[serde(default)]
    pub module_binding: bool,
    #[serde(default)]
    pub event_order: usize,
    #[serde(default)]
    pub captures: Vec<PythonBindingCapture>,
    #[serde(default)]
    pub namespace: Option<Vec<(String, PythonBindingTarget)>>,
}

#[derive(Debug, Clone)]
pub struct PythonModule {
    pub file: Option<String>,
    pub package: bool,
    pub loaded: Vec<String>,
    pub module_keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonModuleLoad {
    Unloaded,
    Loaded(usize),
    Unknown,
}

/// Resolve `module` as imported from `from`. An absolute import that neither
/// the project root nor `src/` loads falls back to the importer's directory
/// when [`script_directory`] admits it and `imported_as_package` reports that
/// no indexed import names that directory as a package.
pub fn load_module<E>(
    module: &str,
    from: &str,
    indexed: &std::collections::HashSet<String>,
    imported_as_package: impl FnOnce(&str) -> Result<bool, E>,
) -> Result<Option<PythonModule>, E> {
    let module = module.strip_suffix('*').map_or(module, |prefix| {
        if prefix.bytes().all(|b| b == b'.') {
            prefix
        } else {
            prefix.strip_suffix('.').unwrap_or(prefix)
        }
    });
    let dots = module.bytes().take_while(|b| *b == b'.').count();
    let name = &module[dots..];
    if !name.is_empty()
        && !name
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return Ok(None);
    }
    let (bases, parts) = if dots == 0 {
        let parts = name.split('.').map(str::to_string).collect::<Vec<_>>();
        if let Some(loaded) = walk_module(vec![String::new(), "src".into()], &parts, indexed) {
            return Ok(Some(loaded));
        }
        let Some(directory) = script_directory(from, &parts, indexed) else {
            return Ok(None);
        };
        if imported_as_package(directory)? {
            return Ok(None);
        }
        return Ok(walk_module(vec![directory.into()], &parts, indexed)
            .filter(|loaded| loaded.file.as_deref() != Some(from)));
    } else {
        let Some((directory, _)) = from.rsplit_once('/') else {
            return Ok(None);
        };
        let mut directory: Vec<_> = directory.split('/').collect();
        for _ in 1..dots {
            if directory.pop().is_none() {
                return Ok(None);
            }
        }
        if directory.is_empty() {
            return Ok(None);
        }
        let root = if directory.first() == Some(&"src") {
            directory.remove(0);
            "src"
        } else {
            ""
        };
        directory.extend(name.split('.').filter(|p| !p.is_empty()));
        (
            vec![root.into()],
            directory
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    };
    Ok(walk_module(bases, &parts, indexed))
}

/// The dotted module names under which an indexed import would load
/// `directory` as a package: the path itself and, under `src/`, the path
/// without that prefix.
pub fn directory_module_names(directory: &str) -> Vec<String> {
    let mut names = vec![directory.replace('/', ".")];
    if let Some(stripped) = directory.strip_prefix("src/") {
        names.push(stripped.replace('/', "."));
    }
    names
}

/// The directory of a script outside any package, which Python puts on
/// `sys.path` when the script runs. A top-level stdlib name is never taken
/// from it, so a project `json.py` beside a script does not shadow `json`.
fn script_directory<'a>(
    from: &'a str,
    parts: &[String],
    indexed: &std::collections::HashSet<String>,
) -> Option<&'a str> {
    let (directory, _) = from.rsplit_once('/')?;
    let top = parts.first()?;
    if crate::python_stdlib::STDLIB_MODULES
        .binary_search(&top.as_str())
        .is_ok()
        || indexed.contains(&format!("{directory}/__init__.py"))
    {
        return None;
    }
    Some(directory)
}

fn walk_module(
    mut bases: Vec<String>,
    parts: &[String],
    indexed: &std::collections::HashSet<String>,
) -> Option<PythonModule> {
    let mut loaded = Vec::new();
    let mut module_keys = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        let mut namespaces = Vec::new();
        let mut selected = None;
        for base in &bases {
            let path = if base.is_empty() {
                part.clone()
            } else {
                format!("{base}/{part}")
            };
            let init = format!("{path}/__init__.py");
            let file = format!("{path}.py");
            if indexed.contains(&init) {
                selected = Some((path, init, true));
                break;
            }
            if indexed.contains(&file) {
                selected = Some((path, file, false));
                break;
            }
            let directory = format!("{path}/");
            if indexed.iter().any(|p| p.starts_with(&directory)) {
                namespaces.push(path);
            }
        }
        if let Some((path, file, package)) = selected {
            module_keys.push(parts[..=index].join("."));
            loaded.push(file.clone());
            if last {
                return Some(PythonModule {
                    file: Some(file),
                    package,
                    loaded,
                    module_keys,
                });
            }
            if !package {
                return None;
            }
            bases = vec![path];
        } else {
            if namespaces.is_empty() {
                return None;
            }
            module_keys.push(parts[..=index].join("."));
            if last {
                return Some(PythonModule {
                    file: None,
                    package: true,
                    loaded,
                    module_keys,
                });
            }
            bases = namespaces;
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonReferenceBinding {
    pub to_name: String,
    pub kind: ReferenceKind,
    pub line: u32,
    pub col: u32,
    pub target: PythonBindingTarget,
}

#[derive(Debug, Clone, Default)]
pub struct PythonBindings {
    pub references: Vec<PythonReferenceBinding>,
    pub exports: Vec<(String, PythonBindingTarget)>,
    pub classes: Vec<PythonClassBinding>,
}

#[derive(Debug, Clone)]
pub struct PythonClassBinding {
    pub qualified_class: String,
    pub definition_line: u32,
    pub name: String,
    pub target: PythonBindingTarget,
}

pub fn module_candidates(module: &str, from: &str) -> Vec<String> {
    let module = module.strip_suffix('*').map_or(module, |prefix| {
        if prefix.bytes().all(|byte| byte == b'.') {
            prefix
        } else {
            prefix.strip_suffix('.').unwrap_or(prefix)
        }
    });
    let dots = module.bytes().take_while(|b| *b == b'.').count();
    let name = &module[dots..];
    if !name.is_empty()
        && !name
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return Vec::new();
    }
    let mut parts: Vec<_> = if dots == 0 {
        Vec::new()
    } else {
        from.rsplit_once('/')
            .map_or("", |(dir, _)| dir)
            .split('/')
            .filter(|p| !p.is_empty())
            .collect()
    };
    for _ in 1..dots {
        if parts.pop().is_none() {
            return Vec::new();
        }
    }
    parts.extend(name.split('.').filter(|part| !part.is_empty()));
    let path = parts.join("/");
    let mut candidates = if path.is_empty() {
        vec!["__init__.py".into()]
    } else {
        vec![format!("{path}/__init__.py"), format!("{path}.py")]
    };
    if dots == 0 {
        candidates.push(format!("src/{path}/__init__.py"));
        candidates.push(format!("src/{path}.py"));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed(paths: &[&str]) -> std::collections::HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    fn load(module: &str, from: &str, paths: &[&str]) -> Option<PythonModule> {
        load_module(module, from, &indexed(paths), |_| Ok::<_, ()>(false)).unwrap()
    }

    fn loaded_file(module: &str, from: &str, paths: &[&str]) -> Option<String> {
        load(module, from, paths).and_then(|loaded| loaded.file)
    }

    #[test]
    fn stdlib_module_list_is_sorted_for_binary_search() {
        assert!(
            crate::python_stdlib::STDLIB_MODULES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        for name in ["json", "html", "os", "__future__", "distutils"] {
            assert!(
                crate::python_stdlib::STDLIB_MODULES
                    .binary_search(&name)
                    .is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn script_imports_resolve_to_a_sibling_module_or_package() {
        let paths = [
            "scripts/run.py",
            "scripts/sib.py",
            "scripts/tools/__init__.py",
        ];
        assert_eq!(
            loaded_file("sib", "scripts/run.py", &paths).as_deref(),
            Some("scripts/sib.py")
        );
        assert_eq!(
            loaded_file("tools", "scripts/run.py", &paths).as_deref(),
            Some("scripts/tools/__init__.py")
        );
    }

    #[test]
    fn project_root_and_src_win_over_the_script_directory() {
        let paths = ["scripts/run.py", "scripts/sib.py", "sib.py", "src/other.py"];
        assert_eq!(
            loaded_file("sib", "scripts/run.py", &paths).as_deref(),
            Some("sib.py")
        );
    }

    #[test]
    fn package_modules_never_import_siblings_absolutely() {
        let paths = [
            "pkg/__init__.py",
            "pkg/views.py",
            "pkg/html.py",
            "pkg/helper.py",
        ];
        assert_eq!(load("html", "pkg/views.py", &paths).map(|m| m.file), None);
        assert_eq!(load("helper", "pkg/views.py", &paths).map(|m| m.file), None);
    }

    #[test]
    fn stdlib_names_never_bind_a_script_sibling() {
        let paths = [
            "scripts/run.py",
            "scripts/json.py",
            "scripts/html/__init__.py",
        ];
        assert!(load("json", "scripts/run.py", &paths).is_none());
        assert!(load("html.parser", "scripts/run.py", &paths).is_none());
    }

    #[test]
    fn a_script_importing_its_own_name_does_not_bind_itself() {
        let paths = ["scripts/run.py", "scripts/sib.py"];
        assert!(load("run", "scripts/run.py", &paths).is_none());
    }

    #[test]
    fn a_directory_imported_as_a_package_is_not_a_script_directory() {
        let paths = ["app/api/client.py", "app/api/requests.py"];
        let mut asked = Vec::new();
        let loaded = load_module("requests", "app/api/client.py", &indexed(&paths), |dir| {
            asked.push(dir.to_string());
            Ok::<_, ()>(true)
        })
        .unwrap();
        assert!(loaded.is_none());
        assert_eq!(asked, ["app/api"]);
        assert_eq!(
            directory_module_names("src/app/api"),
            ["src.app.api", "app.api"]
        );
    }

    fn site(file: &str, to_name: &str, line: u32, col: u32) -> PythonImportSite {
        PythonImportSite {
            file: file.into(),
            to_name: to_name.into(),
            line,
            col,
        }
    }

    #[test]
    fn import_sites_store_as_arrays_and_round_trip_order_and_duplicates() {
        let sites = vec![
            site("a.py", "os", 1, 0),
            site("b.py", "re", 3, 4),
            site("a.py", "sys", 2, 7),
            site("a.py", "sys", 2, 7),
        ];
        let target = PythonBindingTarget {
            name: "x".into(),
            import_sites: sites.clone(),
            captures: vec![PythonBindingCapture {
                attributes: 1,
                import_sites: sites[1..].to_vec(),
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&target).unwrap();
        assert!(
            json.contains(r#""import_sites":[["a.py","os",1,0],["b.py","re",3,4],["a.py","sys",2,7],["a.py","sys",2,7]]"#),
            "{json}"
        );
        assert!(
            json.contains(r#"{"attributes":1,"import_sites":[["b.py","re",3,4],["a.py","sys",2,7],["a.py","sys",2,7]]}"#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<PythonBindingTarget>(&json).unwrap(),
            target
        );
    }

    #[test]
    fn import_sites_decode_legacy_objects_and_missing_fields() {
        let legacy = r#"{"module":null,"name":"x","definition_line":null,"import_sites":[{"file":"a.py","to_name":"os","line":1,"col":0},["b.py","re",3,4]],"captures":[{"attributes":2,"import_sites":[{"file":"c.py","to_name":"json","line":5,"col":1}]},{"attributes":3}]}"#;
        let target: PythonBindingTarget = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            target.import_sites,
            [site("a.py", "os", 1, 0), site("b.py", "re", 3, 4)]
        );
        assert_eq!(
            target.captures[0].import_sites,
            [site("c.py", "json", 5, 1)]
        );
        assert!(target.captures[1].import_sites.is_empty());
        let missing: PythonBindingTarget =
            serde_json::from_str(r#"{"module":null,"name":"x","definition_line":null}"#).unwrap();
        assert!(missing.import_sites.is_empty());
    }

    /// Differential against the previous object form: rewrites every stored
    /// `import_sites` array back to `{"file","to_name","line","col"}` objects,
    /// and requires both encodings to decode to the same target.
    #[test]
    fn array_and_legacy_object_encodings_decode_identically() {
        fn to_legacy(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map.iter_mut() {
                        if key == "import_sites" {
                            let rows = child.as_array_mut().expect("import_sites is an array");
                            for row in rows.iter_mut() {
                                let a = row.as_array().expect("site is an array").clone();
                                assert_eq!(a.len(), 4, "{a:?}");
                                *row = serde_json::json!({
                                    "file": a[0], "to_name": a[1], "line": a[2], "col": a[3]
                                });
                            }
                        } else {
                            to_legacy(child);
                        }
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(to_legacy),
                _ => {}
            }
        }
        let names = [
            "",
            "os",
            "a\"b\\c",
            "p/\u{e9}\u{4e2d}.py",
            "x\ny",
            "\u{1f600}",
        ];
        let sites_for = |seed: usize| -> Vec<PythonImportSite> {
            (0..seed % 5)
                .map(|n| {
                    site(
                        names[(seed + n) % names.len()],
                        names[(seed * 3 + n) % names.len()],
                        (seed * 7 + n) as u32,
                        u32::MAX - (n as u32),
                    )
                })
                .chain((seed.is_multiple_of(3)).then(|| site("dup.py", "dup", 1, 1)))
                .chain((seed.is_multiple_of(3)).then(|| site("dup.py", "dup", 1, 1)))
                .collect()
        };
        let mut checked = 0;
        for seed in 0..60usize {
            let leaf = |n: usize| PythonBindingTarget {
                name: names[n % names.len()].into(),
                import_sites: sites_for(seed + n),
                ..Default::default()
            };
            let target = PythonBindingTarget {
                module: (seed.is_multiple_of(2)).then(|| "m".to_string()),
                name: names[seed % names.len()].into(),
                bound: seed.is_multiple_of(4),
                import_sites: sites_for(seed),
                captures: (0..seed % 3)
                    .map(|n| PythonBindingCapture {
                        attributes: n,
                        import_sites: sites_for(seed + n + 1),
                    })
                    .collect(),
                fallback: (seed.is_multiple_of(5)).then(|| Box::new(leaf(seed))),
                namespace: (seed.is_multiple_of(6))
                    .then(|| vec![("n".to_string(), leaf(1)), ("o".to_string(), leaf(2))]),
                ..Default::default()
            };
            let mut value = serde_json::to_value(&target).unwrap();
            let stored = serde_json::to_string(&value).unwrap();
            to_legacy(&mut value);
            let legacy = serde_json::to_string(&value).unwrap();
            assert_eq!(
                serde_json::from_str::<PythonBindingTarget>(&stored).unwrap(),
                target,
                "{stored}"
            );
            assert_eq!(
                serde_json::from_str::<PythonBindingTarget>(&legacy).unwrap(),
                target,
                "{legacy}"
            );
            checked += 1;
        }
        assert_eq!(checked, 60);
    }
}
