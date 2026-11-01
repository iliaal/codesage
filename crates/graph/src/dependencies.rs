use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, ExternalDependencyPackage, Handle, RawDependencyImports, Reference,
    ReferenceKind, Symbol, SymbolKind, UnresolvedImport,
};
use codesage_storage::Database;

use crate::rust_modules::{RustImportEvidence, RustModules};
use codesage_storage::db::PythonPackageProbes;

pub(crate) fn resolve_outgoing(
    db: &Database,
    entry: &mut DependencyEntry,
    modules: &RustModules,
    indexed: &HashSet<String>,
    probes: &PythonPackageProbes,
) -> Result<()> {
    let raw = RawDependencyImports {
        imports: std::mem::take(&mut entry.imports),
        test_imports: std::mem::take(&mut entry.test_imports),
    };
    let rows = db.references_in_file_range(&entry.file_path, 1, u32::MAX)?;
    let symbols = db.symbols_for_file(&entry.file_path)?;
    let evidence = if entry.file_path.ends_with(".rs") {
        modules.import_evidence(db, &entry.file_path)?
    } else {
        RustImportEvidence::default()
    };
    let mut packages: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for name in raw.imports.iter().chain(&raw.test_imports) {
        let mut exclusively_self = true;
        for is_test in [false, true] {
            codesage_protocol::work::checkpoint()?;
            let sites: Vec<_> = rows
                .iter()
                .filter(|row| {
                    row.to_name == *name
                        && row.is_test == is_test
                        && (matches!(row.kind, ReferenceKind::Import | ReferenceKind::Include)
                            || row.kind == ReferenceKind::ImportBinding
                                && codesage_protocol::python::is_python_path(&entry.file_path))
                })
                .collect();
            if sites.is_empty() {
                continue;
            }
            let mut targets = BTreeSet::new();
            let mut proven = BTreeSet::new();
            let mut unresolved_sites = Vec::new();
            let mut ambiguous = false;
            for row in &sites {
                let (files, uncertain) = resolve_site(
                    db,
                    row,
                    modules,
                    &IndexedFiles {
                        paths: indexed,
                        python_probes: probes,
                    },
                    &symbols,
                    &rows,
                    &evidence,
                )?;
                if files.is_empty() {
                    unresolved_sites.push(*row);
                }
                ambiguous |= uncertain;
                if !uncertain {
                    proven.extend(files.iter().cloned());
                }
                targets.extend(files);
            }
            let mut unknown = !unresolved_sites.is_empty();
            let mut has_external = false;
            if unknown
                && entry.file_path.ends_with(".rs")
                && let Some((package, partial)) = modules.external_package(
                    db,
                    &entry.file_path,
                    name,
                    is_test,
                    &unresolved_sites,
                )?
            {
                unknown = partial;
                has_external = true;
                let package = packages.entry(package).or_default();
                if is_test {
                    package.1.insert(name.clone());
                } else {
                    package.0.insert(name.clone());
                }
            }
            let has_self = proven.remove(&entry.file_path);
            let has_proven = has_self || !proven.is_empty();
            let imports = if is_test {
                &mut entry.test_imports
            } else {
                &mut entry.imports
            };
            imports.extend(proven.iter().cloned().filter_map(file_handle));
            exclusively_self &=
                !unknown && !ambiguous && !has_external && proven.is_empty() && has_self;
            if !unknown && !ambiguous {
                continue;
            }
            entry.unresolved_imports.push(UnresolvedImport {
                    target: name.clone(),
                    is_test,
                    candidates: targets.into_iter().filter_map(file_handle).collect(),
                    reason: if ambiguous {
                        "multiple indexed files match an import site; ambiguous candidates are not dependency edges"
                    } else if has_proven {
                        "some import sites remain unresolved; proven file dependencies are retained"
                    } else if has_external {
                        "some import sites remain unresolved; the proven external package is retained"
                    } else {
                        "indexed evidence does not establish an internal file or external package"
                    }
                    .into(),
            });
        }
        if exclusively_self {
            entry.intra_file_imports += 1;
        }
    }
    entry.imports.sort();
    entry.imports.dedup();
    entry.test_imports.sort();
    entry.test_imports.dedup();
    entry
        .test_imports
        .retain(|file| !entry.imports.contains(file));
    entry.externals.packages_total = packages.len();
    let mut packages: Vec<_> = packages
        .into_iter()
        .map(|(package, (product, tests))| {
            let imports = product.len();
            let test_imports = tests.difference(&product).count();
            entry.externals.imports += imports;
            entry.externals.test_imports += test_imports;
            ExternalDependencyPackage {
                package,
                imports,
                test_imports,
            }
        })
        .collect();
    packages.sort_by(|a, b| {
        (b.imports + b.test_imports)
            .cmp(&(a.imports + a.test_imports))
            .then_with(|| a.package.cmp(&b.package))
    });
    packages.truncate(5);
    entry.externals.top_packages = packages;
    entry.raw_imports = Some(raw);
    Ok(())
}

fn file_handle(file: String) -> Option<String> {
    Handle::file(file).map(|handle| handle.to_string())
}

struct IndexedFiles<'a> {
    paths: &'a HashSet<String>,
    python_probes: &'a PythonPackageProbes,
}

fn resolve_site(
    db: &Database,
    row: &Reference,
    modules: &RustModules,
    indexed: &IndexedFiles<'_>,
    symbols: &[Symbol],
    rows: &[Reference],
    evidence: &RustImportEvidence,
) -> Result<(BTreeSet<String>, bool)> {
    let mut files = BTreeSet::new();
    if codesage_protocol::python::is_python_path(&row.from_file) {
        files.extend(db.python_import_targets_indexed(
            row,
            indexed.paths,
            indexed.python_probes,
        )?);
        return Ok((files, false));
    }
    if row.from_file.ends_with(".rs") {
        if let Some(definitions) = modules.external_definitions(db, &row.from_file, &row.to_name)? {
            files.extend(definitions.into_iter().map(|symbol| symbol.file_path));
        }
        if files.is_empty()
            && let Some(file) = modules.external_import_file(db, row)?
        {
            files.insert(file);
        }
        if files.is_empty() {
            let explicit = row.to_name.starts_with("./")
                || matches!(
                    row.to_name.split("::").next(),
                    Some("crate" | "self" | "super")
                );
            if !explicit && !module_item(symbols, row.line, row.col) {
                return Ok((files, false));
            }
            let name = rust_scoped_name(row, symbols);
            let module = name.strip_suffix("::*").unwrap_or(&name);
            if rust_local_target(&name, row, symbols, rows, evidence) {
                files.insert(row.from_file.clone());
                return Ok((files, false));
            }
            files.extend(
                modules
                    .candidates(&row.from_file, module)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|file| {
                        indexed.paths.contains(file)
                            && modules.same_crate(&row.from_file, file) == Some(true)
                    }),
            );
            if files.is_empty()
                && let Some((parent, tail)) = name.rsplit_once("::")
            {
                for file in modules
                    .candidates(&row.from_file, parent)
                    .unwrap_or_default()
                {
                    if indexed.paths.contains(&file)
                        && modules.same_crate(&row.from_file, &file) == Some(true)
                        && (tail == "*" || {
                            let definitions = db.symbols_for_file(&file)?;
                            let evidence = modules.import_evidence(db, &file)?;
                            definitions.iter().any(|symbol| {
                                symbol.name == tail
                                    && !matches!(symbol.kind, SymbolKind::Module)
                                    && enclosing_modules(
                                        &definitions,
                                        symbol.line_start,
                                        symbol.col_start,
                                    )
                                    .is_empty()
                                    && module_definition(symbol, &definitions, &evidence)
                            })
                        })
                    {
                        files.insert(file);
                    }
                }
            }
        }
    } else {
        files.extend(
            crate::bundle::path_import_candidates(&row.to_name, &row.from_file)
                .into_iter()
                .filter(|file| indexed.paths.contains(file)),
        );
        if files.is_empty()
            && (row.from_file.ends_with(".php") || row.from_file.ends_with(".java"))
            && (row.to_name.contains('\\') || row.to_name.contains('.'))
        {
            files.extend(
                db.find_symbols(&row.to_name, None)?
                    .into_iter()
                    .filter(|symbol| symbol.qualified_name == row.to_name)
                    .map(|symbol| symbol.file_path),
            );
        }
    }
    let ambiguous = files.len() > 1;
    Ok((files, ambiguous))
}

fn enclosing_modules(symbols: &[Symbol], line: u32, col: u32) -> Vec<&str> {
    let mut owners: Vec<_> = symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == SymbolKind::Module
                && (symbol.line_start, symbol.col_start) < (line, col)
                && (line, col) < (symbol.line_end, symbol.col_end)
        })
        .collect();
    owners.sort_by_key(|symbol| (symbol.line_start, symbol.col_start));
    owners
        .into_iter()
        .map(|symbol| symbol.name.as_str())
        .collect()
}

fn rust_scoped_name(row: &Reference, symbols: &[Symbol]) -> String {
    if let Some(declaration) = row.to_name.strip_prefix("./") {
        return format!("self::{}", declaration.replace('/', "::"));
    }
    let mut segments = row.to_name.split("::").peekable();
    if segments.peek() == Some(&"crate") || row.to_name.starts_with("::") {
        return row.to_name.clone();
    }
    let mut inline = enclosing_modules(symbols, row.line, row.col);
    let mut parents = 0;
    if segments.peek() == Some(&"self") {
        segments.next();
    } else {
        while segments.peek() == Some(&"super") {
            if inline.pop().is_none() {
                parents += 1;
            }
            segments.next();
        }
    }
    let mut normalized = if parents == 0 {
        vec!["self"]
    } else {
        vec!["super"; parents]
    };
    normalized.extend(inline);
    normalized.extend(segments);
    normalized.join("::")
}

fn module_item(symbols: &[Symbol], line: u32, col: u32) -> bool {
    !symbols.iter().any(|symbol| {
        symbol.kind != SymbolKind::Module
            && (symbol.line_start, symbol.col_start) < (line, col)
            && (line, col) < (symbol.line_end, symbol.col_end)
    })
}

fn module_definition(symbol: &Symbol, symbols: &[Symbol], evidence: &RustImportEvidence) -> bool {
    evidence.module_items.as_ref().map_or_else(
        || {
            !matches!(symbol.kind, SymbolKind::Constant | SymbolKind::Method)
                && module_item(symbols, symbol.line_start, symbol.col_start)
        },
        |items| items.contains(&(symbol.line_start, symbol.col_start)),
    )
}

fn rust_local_target(
    name: &str,
    row: &Reference,
    symbols: &[Symbol],
    rows: &[Reference],
    evidence: &RustImportEvidence,
) -> bool {
    let Some(local) = name.strip_prefix("self::") else {
        return name == "self";
    };
    let mut parts: Vec<_> = local.split("::").collect();
    let Some(tail) = parts.pop() else {
        return false;
    };
    if tail == "*" {
        if parts.is_empty() {
            return true;
        }
        return symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Module
                && symbol.name == *parts.last().unwrap()
                && module_definition(symbol, symbols, evidence)
                && enclosing_modules(symbols, symbol.line_start, symbol.col_start)
                    == parts[..parts.len() - 1]
                && !rows
                    .iter()
                    .any(|reference| reference.to_name == format!("./{}", parts.join("/")))
        });
    }
    symbols.iter().any(|symbol| {
        symbol.name == tail
            && module_definition(symbol, symbols, evidence)
            && enclosing_modules(symbols, symbol.line_start, symbol.col_start) == parts
            && !(symbol.kind == SymbolKind::Module
                && rows.iter().any(|reference| {
                    reference.to_name == format!("./{}", local.replace("::", "/"))
                }))
    }) || rows.iter().any(|binding| {
        matches!(
            binding.kind,
            ReferenceKind::Import | ReferenceKind::ImportBinding
        ) && (binding.line, binding.col) != (row.line, row.col)
            && evidence
                .bindings
                .get(&(binding.line, binding.col))
                .map(String::as_str)
                == Some(tail)
            && enclosing_modules(symbols, binding.line, binding.col) == parts
            && !binding.to_name.starts_with("self::")
            && !binding.to_name.starts_with("super::")
    })
}
