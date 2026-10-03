use std::collections::HashSet;

use anyhow::Result;
pub(crate) use codesage_protocol::python::module_candidates;
use codesage_protocol::python::{
    PythonBindingCapture, PythonBindingTarget, PythonImportSite, PythonModuleLoad,
};
use codesage_protocol::{Reference, ReferenceKind, Symbol};
use codesage_storage::Database;

pub(crate) fn resolve_reference(db: &Database, row: &Reference) -> Result<Option<Vec<Symbol>>> {
    if !codesage_protocol::python::is_python_path(&row.from_file) {
        return Ok(None);
    }
    if !db.file_interpretation_matches(&row.from_file, crate::index::STRUCTURAL_INTERPRETATION)? {
        return Ok(Some(Vec::new()));
    }
    if row.kind == ReferenceKind::Import {
        return Ok(Some(Vec::new()));
    }
    let Some(target) = db.python_reference_binding(row)? else {
        return Ok(Some(Vec::new()));
    };
    Ok(Some(resolve_target(
        db,
        &row.from_file,
        &target,
        &mut HashSet::new(),
    )?))
}

fn resolve_target(
    db: &Database,
    from: &str,
    target: &PythonBindingTarget,
    visited: &mut HashSet<(String, String, String)>,
) -> Result<Vec<Symbol>> {
    codesage_protocol::work::checkpoint()?;
    if visited.len() >= 32 {
        return Ok(Vec::new());
    }
    if target.deleted {
        return Ok(Vec::new());
    }
    if target.wildcard {
        let Some(module) = &target.module else {
            return Ok(Vec::new());
        };
        let Some(loaded) = db.python_loaded_module(from, module)? else {
            return Ok(Vec::new());
        };
        let head = target.name.split('.').next().unwrap_or("");
        let presence = match loaded.file {
            Some(file) => db.python_wildcard_exports_name(&file, head)?,
            None => Some(false),
        };
        match presence {
            Some(false) => {
                return match &target.fallback {
                    Some(fallback) => resolve_target(db, from, fallback, visited),
                    None => Ok(Vec::new()),
                };
            }
            Some(true) => {}
            None => return Ok(Vec::new()),
        }
    }
    if let Some(module) = &target.module {
        if !visited.insert((from.into(), module.clone(), target.name.clone())) {
            return Ok(Vec::new());
        }
        return resolve_import(db, from, target, visited);
    }
    let Some(line) = target.definition_line else {
        return Ok(Vec::new());
    };
    let symbols = db.symbols_for_file(from)?;
    let owner = symbols.iter().find(|s| s.line_start == line);
    if owner.is_some_and(|owner| {
        owner.kind == codesage_protocol::SymbolKind::Class
            && target
                .name
                .starts_with(&format!("{}.", owner.qualified_name))
    }) {
        let owner = owner.unwrap();
        let member = target
            .name
            .strip_prefix(&format!("{}.", owner.qualified_name))
            .unwrap();
        let (head, tail) = member.split_once('.').unwrap_or((member, ""));
        return match db.python_class_binding(from, &owner.qualified_name, owner.line_start, head)? {
            Some(mut binding) => {
                let prefix = capture_prefix(db, from, &binding)?;
                binding
                    .captures
                    .extend(
                        advance_captures(&target.captures)
                            .into_iter()
                            .map(|mut capture| {
                                capture.attributes += prefix;
                                capture
                            }),
                    );
                binding.import_sites.extend_from_slice(&target.import_sites);
                append_suffix(&mut binding, tail);
                resolve_target(db, from, &binding, visited)
            }
            None => Ok(Vec::new()),
        };
    }
    Ok(symbols
        .iter()
        .filter(|s| {
            s.qualified_name == target.name
                && (s.line_start == line
                    || owner.is_some_and(|owner| {
                        owner.kind == codesage_protocol::SymbolKind::Class
                            && owner.line_start <= s.line_start
                            && s.line_end <= owner.line_end
                    }))
        })
        .cloned()
        .collect())
}

fn resolve_import(
    db: &Database,
    from: &str,
    target: &PythonBindingTarget,
    visited: &mut HashSet<(String, String, String)>,
) -> Result<Vec<Symbol>> {
    let Some(module) = target.module.as_deref() else {
        return Ok(Vec::new());
    };
    let name = &target.name;
    let import_sites = target.import_sites.as_slice();
    let captures = target.captures.as_slice();
    let allow_member_import = !target.module_binding;
    let Some(loaded) = db.python_loaded_module(from, module)? else {
        return Ok(Vec::new());
    };
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let module_key = module_candidates(module, from).first().map(|path| {
        path.trim_end_matches("/__init__.py")
            .trim_end_matches(".py")
            .trim_start_matches("src/")
            .replace('/', ".")
    });
    let (head, tail) = name.split_once('.').unwrap_or((name, ""));
    let lookup_modules = captures
        .first()
        .map_or(import_sites, |capture| capture.import_sites.as_slice());
    if loaded.package {
        let separator = if module.ends_with('.') { "" } else { "." };
        let child_module = format!("{module}{separator}{head}");
        if let Some(child) = db.python_loaded_module(from, &child_module)? {
            let state = match (&loaded.file, child.module_keys.last()) {
                (Some(initializer), Some(key)) => db.python_first_module_load(initializer, key)?,
                _ => PythonModuleLoad::Unloaded,
            };
            let explicit = match &module_key {
                Some(key) => {
                    db.python_import_sites_load_module(lookup_modules, &format!("{key}.{head}"))?
                }
                None => Some(false),
            };
            let export = match &loaded.file {
                Some(file) => db.python_export_binding(file, head)?,
                None => None,
            };
            let captured = match (&loaded.file, &child.file) {
                (Some(initializer), Some(child)) if !captures.is_empty() => {
                    captured_self_import_load(db, initializer, head, child, lookup_modules)?
                }
                _ => None,
            };
            let installed = match (captured, state) {
                (Some(false), _) => return Ok(Vec::new()),
                (Some(true), _) => true,
                (None, PythonModuleLoad::Loaded(order)) => export
                    .as_ref()
                    .is_none_or(|export| export.event_order <= order),
                (None, PythonModuleLoad::Unloaded) => match explicit {
                    Some(loaded) => loaded,
                    None => return Ok(Vec::new()),
                },
                (None, PythonModuleLoad::Unknown) => return Ok(Vec::new()),
            };
            if installed {
                return resolve_target(
                    db,
                    from,
                    &PythonBindingTarget {
                        module: Some(child_module),
                        name: tail.into(),
                        module_binding: true,
                        import_sites: import_sites.into(),
                        captures: advance_captures(captures),
                        ..Default::default()
                    },
                    visited,
                );
            }
        }
    }
    if let Some(file) = &loaded.file {
        match db.python_export_presence(file, head)? {
            Some(true) => {
                if let Some(mut export) = db.python_export_binding(file, head)? {
                    let prefix = capture_prefix(db, file, &export)?;
                    export
                        .captures
                        .extend(advance_captures(captures).into_iter().map(|mut capture| {
                            capture.attributes += prefix;
                            capture
                        }));
                    export.import_sites.extend_from_slice(import_sites);
                    export.import_sites.sort();
                    export.import_sites.dedup();
                    append_suffix(&mut export, tail);
                    return resolve_target(db, file, &export, visited);
                }
                if let Some(mut glob) = db.python_export_binding(file, "*")? {
                    glob.captures.extend_from_slice(captures);
                    glob.import_sites.extend_from_slice(import_sites);
                    glob.import_sites.sort();
                    glob.import_sites.dedup();
                    set_wildcard_name(&mut glob, name);
                    return resolve_target(db, file, &glob, visited);
                }
                return Ok(Vec::new());
            }
            None => return Ok(Vec::new()),
            Some(false) => {}
        }
    }
    if loaded.package && allow_member_import {
        resolve_submodule(db, from, module, name, import_sites, captures, visited)
    } else {
        Ok(Vec::new())
    }
}

fn captured_self_import_load(
    db: &Database,
    initializer: &str,
    name: &str,
    child: &str,
    sites: &[PythonImportSite],
) -> Result<Option<bool>> {
    if sites.len() > 32 {
        return Ok(Some(false));
    }
    let mut selected = None;
    for site in sites.iter().filter(|site| site.file == initializer) {
        let row = Reference {
            from_file: site.file.clone(),
            from_symbol: None,
            to_name: site.to_name.clone(),
            kind: ReferenceKind::Import,
            line: site.line,
            col: site.col,
            lazy: false,
            is_test: false,
            to: None,
            from_line: None,
        };
        let Some(target) = db.python_reference_binding(&row)? else {
            return Ok(Some(false));
        };
        let Some(namespace) = &target.namespace else {
            continue;
        };
        let own = target.module.as_ref().is_some_and(|module| {
            module_candidates(module, initializer).contains(&initializer.into())
        });
        let named = target.name == name
            || (target.wildcard
                && namespace.iter().any(|(key, value)| {
                    key == "__all__"
                        && value.bound
                        && !value.deleted
                        && value
                            .export_names
                            .as_ref()
                            .is_some_and(|names| names.iter().any(|key| key == name))
                }));
        if own
            && named
            && selected
                .as_ref()
                .is_none_or(|(order, _)| *order < target.event_order)
        {
            selected = Some((target.event_order, row));
        }
    }
    match selected {
        Some((_, row)) => Ok(Some(
            db.python_import_targets(&row)?
                .iter()
                .any(|path| path == child),
        )),
        None => Ok(None),
    }
}

fn advance_captures(captures: &[PythonBindingCapture]) -> Vec<PythonBindingCapture> {
    captures
        .iter()
        .filter(|capture| capture.attributes > 1)
        .map(|capture| PythonBindingCapture {
            attributes: capture.attributes - 1,
            import_sites: capture.import_sites.clone(),
        })
        .collect()
}

fn capture_prefix(db: &Database, from: &str, target: &PythonBindingTarget) -> Result<usize> {
    if target.module.is_some() {
        return Ok(if target.name.is_empty() {
            0
        } else {
            target.name.split('.').count()
        });
    }
    let Some(line) = target.definition_line else {
        return Ok(0);
    };
    Ok(db
        .symbols_for_file(from)?
        .iter()
        .find(|symbol| symbol.line_start == line)
        .and_then(|symbol| {
            target
                .name
                .strip_prefix(&format!("{}.", symbol.qualified_name))
        })
        .map_or(0, |suffix| suffix.split('.').count()))
}

fn append_suffix(target: &mut PythonBindingTarget, suffix: &str) {
    if suffix.is_empty() {
        return;
    }
    if !target.name.is_empty() {
        target.name.push('.');
    }
    target.name.push_str(suffix);
    if let Some(fallback) = &mut target.fallback {
        append_suffix(fallback, suffix);
    }
}

fn set_wildcard_name(target: &mut PythonBindingTarget, name: &str) {
    target.name = name.into();
    if let Some(fallback) = &mut target.fallback {
        set_wildcard_name(fallback, name);
    }
}

fn resolve_submodule(
    db: &Database,
    from: &str,
    module: &str,
    name: &str,
    import_sites: &[PythonImportSite],
    captures: &[PythonBindingCapture],
    visited: &mut HashSet<(String, String, String)>,
) -> Result<Vec<Symbol>> {
    let Some((head, tail)) = name.split_once('.') else {
        return Ok(Vec::new());
    };
    let separator = if module.ends_with('.') { "" } else { "." };
    resolve_target(
        db,
        from,
        &PythonBindingTarget {
            module: Some(format!("{module}{separator}{head}")),
            name: tail.into(),
            definition_line: None,
            module_binding: true,
            import_sites: import_sites.into(),
            captures: advance_captures(captures),
            ..Default::default()
        },
        visited,
    )
}
