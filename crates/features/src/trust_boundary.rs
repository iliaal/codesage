//! Derive trust boundaries from parsed references and Java framework roles.
//! Persistence helpers support indexed references and current source.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use codesage_parser::{parse::parse_file, references::extract_references};
use codesage_protocol::{Language, Reference, ReferenceKind, TrustBoundary};
use codesage_storage::Database;
use tree_sitter::Tree;

use crate::java_roles;
pub use crate::java_roles::JavaTypeContext;
use crate::trust_boundary_rules::{TrustBoundaryRule, rule_matches, rules_for};

/// Derive the set of trust boundaries crossed by a file from its parsed
/// references. Sorted, deduped, language-aware (C++ inherits C rules).
///
/// Uses `Import`, `Include`, `Call`, `TypeHint`, and Python `ImportBinding`
/// references. Excludes inheritance and trait uses to avoid treating ordinary
/// traits such as Rust's `Debug` as boundary signals.
pub fn derive_from_refs(refs: &[Reference], language: Language) -> Vec<TrustBoundary> {
    let tables = rules_for(language);
    if tables.is_empty() {
        return Vec::new();
    }
    let mut acc: BTreeSet<TrustBoundary> = BTreeSet::new();
    for r in refs {
        if !(ref_kind_signals_boundary(r.kind)
            || language == Language::Python && r.kind == ReferenceKind::ImportBinding)
        {
            continue;
        }
        let name = normalize_ref_name(&r.to_name, r.kind);
        for table in tables {
            apply_rules(table, &name, &mut acc);
        }
    }
    acc.into_iter().collect()
}

pub fn derive_from_tree(
    refs: &[Reference],
    language: Language,
    tree: &Tree,
    source: &[u8],
) -> Vec<TrustBoundary> {
    derive_from_tree_with_context(
        refs,
        language,
        tree,
        source,
        "",
        &JavaTypeContext::default(),
    )
}

pub fn derive_from_tree_with_context(
    refs: &[Reference],
    language: Language,
    tree: &Tree,
    source: &[u8],
    path: &str,
    context: &JavaTypeContext,
) -> Vec<TrustBoundary> {
    let mut boundaries: BTreeSet<_> = derive_from_refs(refs, language).into_iter().collect();
    if language == Language::Java {
        for role in java_roles::roles_with_context(tree, source, path, context) {
            boundaries.extend(role.boundaries());
        }
    }
    boundaries.into_iter().collect()
}

pub(crate) fn boundary_updates_from_source(
    root: &Path,
    db: &Database,
    files: &[(i64, String, Language)],
) -> Result<Vec<(i64, Vec<TrustBoundary>)>> {
    let paths = db
        .all_files_with_id_and_language()?
        .into_iter()
        .filter(|(_, _, language)| *language == Language::Java)
        .map(|(_, path, _)| path)
        .collect::<Vec<_>>();
    boundary_updates_from_source_in_context(root, db, files, &paths)
}

pub(crate) fn boundary_updates_from_source_in_context(
    root: &Path,
    db: &Database,
    files: &[(i64, String, Language)],
    paths: &[String],
) -> Result<Vec<(i64, Vec<TrustBoundary>)>> {
    let context = JavaTypeContext::from_files(root, paths)?;
    let mut updates = Vec::with_capacity(files.len());
    for (id, path, language) in files {
        let boundaries = if *language == Language::Java {
            let source = java_roles::read_source(root, path)?;
            let tree = parse_file(&source, *language)?;
            let refs = extract_references(&tree, &source, *language, path)?;
            derive_from_tree_with_context(&refs, *language, &tree, &source, path, &context)
        } else {
            derive_from_refs(&indexed_references(db, *id)?, *language)
        };
        updates.push((*id, boundaries));
    }
    Ok(updates)
}

pub fn derive_for_files_with_source(
    root: &Path,
    db: &Database,
    files: &[(i64, String, Language)],
) -> Result<usize> {
    let updates = boundary_updates_from_source(root, db, files)?;
    db.execute_batch(|db| {
        for (id, boundaries) in &updates {
            db.replace_file_trust_boundaries(*id, boundaries)?;
        }
        Ok(())
    })?;
    Ok(updates.len())
}

fn indexed_references(db: &Database, id: i64) -> Result<Vec<Reference>> {
    Ok(db
        .refs_outgoing_for_file_id(id)?
        .into_iter()
        .map(|(to_name, kind)| Reference {
            from_file: String::new(),
            from_symbol: None,
            to_name,
            kind,
            line: 0,
            col: 0,
            lazy: false,
            to: None,
            from_line: None,
            is_test: false,
        })
        .collect())
}

/// C includes retain angle brackets or quotes; rules match the bare path.
fn normalize_ref_name(name: &str, kind: ReferenceKind) -> String {
    if kind != ReferenceKind::Include {
        return name.to_string();
    }
    let trimmed = name.trim();
    let inner = trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);
    inner.to_string()
}

fn ref_kind_signals_boundary(kind: ReferenceKind) -> bool {
    matches!(
        kind,
        ReferenceKind::Import
            | ReferenceKind::Include
            | ReferenceKind::Call
            | ReferenceKind::TypeHint
    )
}

fn apply_rules(table: &[TrustBoundaryRule], name: &str, acc: &mut BTreeSet<TrustBoundary>) {
    for rule in table {
        if rule_matches(rule, name) {
            for b in rule.boundaries {
                acc.insert(*b);
            }
        }
    }
}

/// Replace a file's reference-derived boundaries. Java framework roles
/// require `derive_from_tree` or `derive_for_files_with_source` instead.
pub fn derive_for_file(
    db: &Database,
    file_id: i64,
    language: Language,
    refs: &[Reference],
) -> Result<Vec<TrustBoundary>> {
    let boundaries = derive_from_refs(refs, language);
    db.replace_file_trust_boundaries(file_id, &boundaries)?;
    Ok(boundaries)
}

/// Replace every file's reference-derived boundaries from indexed rows.
/// Java framework roles require `derive_for_files_with_source` instead.
pub fn derive_for_index(db: &Database) -> Result<usize> {
    let files = db.all_files_with_id_and_language()?;
    derive_for_files(db, &files)
}

/// Targeted reference-only version of `derive_for_index`. Java framework
/// roles require `derive_for_files_with_source` instead.
pub fn derive_for_files(
    db: &Database,
    files: &[(i64, String, codesage_protocol::Language)],
) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }
    let mut updated = 0usize;
    db.execute_batch(|db| {
        for (file_id, _path, language) in files {
            let in_memory = indexed_references(db, *file_id)?;
            let boundaries = derive_from_refs(&in_memory, *language);
            db.replace_file_trust_boundaries(*file_id, &boundaries)?;
            updated += 1;
        }
        Ok(())
    })?;
    Ok(updated)
}

/// Persist a pre-computed boundary list. Thin wrapper used when the caller
/// already has the derived set (e.g. inside the indexer that wants one
/// `execute_batch` for symbols + refs + boundaries).
pub fn store_for_file(db: &Database, file_id: i64, boundaries: &[TrustBoundary]) -> Result<()> {
    db.replace_file_trust_boundaries(file_id, boundaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{Reference, ReferenceKind};

    fn imp(to: &str) -> Reference {
        Reference {
            from_file: String::new(),
            from_symbol: None,
            to_name: to.to_string(),
            kind: ReferenceKind::Import,
            line: 0,
            col: 0,
            lazy: false,
            to: None,
            from_line: None,
            is_test: false,
        }
    }

    fn inc(to: &str) -> Reference {
        Reference {
            kind: ReferenceKind::Include,
            ..imp(to)
        }
    }

    #[test]
    fn python_import_bindings_retain_boundary_signals() {
        let reference = Reference {
            kind: ReferenceKind::ImportBinding,
            ..imp("socket")
        };
        assert!(derive_from_refs(&[reference], Language::Python).contains(&TrustBoundary::Network));
    }

    #[test]
    fn c_include_strips_angle_brackets_and_quotes() {
        let refs = vec![
            inc("<sys/socket.h>"),
            inc("<curl/curl.h>"),
            inc("\"local.h\""),
        ];
        let b = derive_from_refs(&refs, Language::C);
        assert!(
            b.contains(&TrustBoundary::Network),
            "bracketed sys/socket.h must still match the C rule, got {:?}",
            b
        );
        assert!(
            b.contains(&TrustBoundary::ExternalApi),
            "bracketed curl/curl.h must yield ExternalApi, got {:?}",
            b
        );
    }

    fn call(to: &str) -> Reference {
        Reference {
            kind: ReferenceKind::Call,
            ..imp(to)
        }
    }

    #[test]
    fn rust_reqwest_yields_network_and_external_api() {
        let refs = vec![imp("reqwest::Client")];
        let b = derive_from_refs(&refs, Language::Rust);
        assert!(b.contains(&TrustBoundary::Network));
        assert!(b.contains(&TrustBoundary::ExternalApi));
        assert_eq!(b.len(), 2, "got {:?}", b);
    }

    #[test]
    fn rust_serde_json_yields_serialization() {
        let refs = vec![imp("serde_json::from_str")];
        let b = derive_from_refs(&refs, Language::Rust);
        assert_eq!(b, vec![TrustBoundary::Serialization]);
    }

    #[test]
    fn rust_dedupes_when_multiple_imports_share_boundary() {
        let refs = vec![imp("std::fs"), imp("std::fs::File"), imp("tokio::fs::read")];
        let b = derive_from_refs(&refs, Language::Rust);
        assert_eq!(b, vec![TrustBoundary::Filesystem]);
    }

    #[test]
    fn php_curl_call_yields_network_and_external_api() {
        let refs = vec![call("curl_exec"), call("curl_init")];
        let b = derive_from_refs(&refs, Language::Php);
        assert!(b.contains(&TrustBoundary::Network));
        assert!(b.contains(&TrustBoundary::ExternalApi));
    }

    #[test]
    fn php_exec_yields_process_exec() {
        let refs = vec![call("exec"), call("shell_exec")];
        let b = derive_from_refs(&refs, Language::Php);
        assert_eq!(b, vec![TrustBoundary::ProcessExec]);
    }

    #[test]
    fn c_socket_include_yields_network() {
        let refs = vec![inc("sys/socket.h")];
        let b = derive_from_refs(&refs, Language::C);
        assert_eq!(b, vec![TrustBoundary::Network]);
    }

    #[test]
    fn c_unistd_yields_both_filesystem_and_process_exec() {
        let refs = vec![inc("unistd.h")];
        let b = derive_from_refs(&refs, Language::C);
        // Ordering is by enum discriminant: Filesystem before ProcessExec.
        assert_eq!(
            b,
            vec![TrustBoundary::Filesystem, TrustBoundary::ProcessExec]
        );
    }

    #[test]
    fn cpp_inherits_c_rules_plus_filesystem_header() {
        let refs = vec![inc("filesystem"), inc("sys/socket.h")];
        let b = derive_from_refs(&refs, Language::Cpp);
        assert!(b.contains(&TrustBoundary::Filesystem));
        assert!(b.contains(&TrustBoundary::Network));
    }

    #[test]
    fn cuda_include_yields_concurrency() {
        // `.cu`/`.cuh` files are parsed as C++; the CUDA headers live in the
        // C rule table (inherited by C++) and map to a concurrency boundary.
        for header in [
            "cuda.h",
            "cuda_runtime.h",
            "cuda_runtime_api.h",
            "device_launch_parameters.h",
        ] {
            let b = derive_from_refs(&[inc(header)], Language::Cpp);
            assert_eq!(
                b,
                vec![TrustBoundary::Concurrency],
                "{header} should yield concurrency"
            );
        }
    }

    #[test]
    fn python_requests_yields_network_external_api() {
        let refs = vec![imp("requests.get")];
        let b = derive_from_refs(&refs, Language::Python);
        assert!(b.contains(&TrustBoundary::Network));
        assert!(b.contains(&TrustBoundary::ExternalApi));
    }

    #[test]
    fn python_os_environ_yields_secrets() {
        let refs = vec![imp("os.environ"), imp("os.environ.get")];
        let b = derive_from_refs(&refs, Language::Python);
        assert!(b.contains(&TrustBoundary::Secrets));
    }

    #[test]
    fn go_net_http_yields_network() {
        let refs = vec![imp("net/http"), imp("net/http/httptest")];
        let b = derive_from_refs(&refs, Language::Go);
        assert_eq!(b, vec![TrustBoundary::Network]);
    }

    #[test]
    fn js_child_process_yields_process_exec() {
        let refs = vec![imp("node:child_process"), imp("child_process")];
        let b = derive_from_refs(&refs, Language::JavaScript);
        assert_eq!(b, vec![TrustBoundary::ProcessExec]);
    }

    #[test]
    fn empty_refs_yield_no_boundaries() {
        let b = derive_from_refs(&[], Language::Rust);
        assert!(b.is_empty());
    }

    #[test]
    fn inheritance_ref_kind_does_not_signal_boundary() {
        let r = Reference {
            from_file: String::new(),
            from_symbol: None,
            to_name: "reqwest::Client".to_string(),
            kind: ReferenceKind::Inheritance,
            line: 0,
            col: 0,
            lazy: false,
            to: None,
            from_line: None,
            is_test: false,
        };
        let b = derive_from_refs(&[r], Language::Rust);
        assert!(
            b.is_empty(),
            "Inheritance refs must not signal a boundary, got {:?}",
            b
        );
    }
}
