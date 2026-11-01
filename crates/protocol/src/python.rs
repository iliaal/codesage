use crate::ReferenceKind;

pub const BINDING_INTERPRETATION: &str =
    "codesage/structural/v1;parser-queries=9;extraction=20;trust-boundaries=8";

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct PythonImportSite {
    pub file: String,
    pub to_name: String,
    pub line: u32,
    pub col: u32,
}

pub fn is_python_path(path: &str) -> bool {
    path.ends_with(".py") || path.ends_with(".pyi")
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PythonBindingCapture {
    pub attributes: usize,
    #[serde(default)]
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
    #[serde(default)]
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

#[derive(Debug)]
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

pub fn load_module(
    module: &str,
    from: &str,
    indexed: &std::collections::HashSet<String>,
) -> Option<PythonModule> {
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
        return None;
    }
    let (bases, parts) = if dots == 0 {
        let parts = name.split('.').map(str::to_string).collect::<Vec<_>>();
        if let Some(loaded) = walk_module(vec![String::new(), "src".into()], &parts, indexed) {
            return Some(loaded);
        }
        let directory = script_directory(from, &parts, indexed)?;
        return walk_module(vec![directory.into()], &parts, indexed)
            .filter(|loaded| loaded.file.as_deref() != Some(from));
    } else {
        let mut directory: Vec<_> = from.rsplit_once('/')?.0.split('/').collect();
        for _ in 1..dots {
            directory.pop()?;
        }
        if directory.is_empty() {
            return None;
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
    walk_module(bases, &parts, indexed)
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
            if indexed.iter().any(|p| p.starts_with(&format!("{path}/"))) {
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

    fn loaded_file(module: &str, from: &str, paths: &[&str]) -> Option<String> {
        load_module(module, from, &indexed(paths)).and_then(|loaded| loaded.file)
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
        assert_eq!(
            load_module("html", "pkg/views.py", &indexed(&paths)).map(|m| m.file),
            None
        );
        assert_eq!(
            load_module("helper", "pkg/views.py", &indexed(&paths)).map(|m| m.file),
            None
        );
    }

    #[test]
    fn stdlib_names_never_bind_a_script_sibling() {
        let paths = [
            "scripts/run.py",
            "scripts/json.py",
            "scripts/html/__init__.py",
        ];
        assert!(load_module("json", "scripts/run.py", &indexed(&paths)).is_none());
        assert!(load_module("html.parser", "scripts/run.py", &indexed(&paths)).is_none());
    }

    #[test]
    fn a_script_importing_its_own_name_does_not_bind_itself() {
        let paths = ["scripts/run.py", "scripts/sib.py"];
        assert!(load_module("run", "scripts/run.py", &indexed(&paths)).is_none());
    }
}
