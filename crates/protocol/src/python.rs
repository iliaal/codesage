use crate::ReferenceKind;

pub const BINDING_INTERPRETATION: &str =
    "codesage/structural/v1;parser-queries=5;extraction=16;trust-boundaries=8";

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
    let (mut bases, parts) = if dots == 0 {
        (
            vec![String::new(), "src".into()],
            name.split('.').map(str::to_string).collect::<Vec<_>>(),
        )
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
            directory.into_iter().map(str::to_string).collect(),
        )
    };
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
