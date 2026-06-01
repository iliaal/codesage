//! Trust-boundary derivation engine.
//!
//! Given a file's already-extracted [`Reference`] list (or, equivalently, the
//! same rows read back from the `refs` table) plus the file's language, return
//! a sorted-deduped [`Vec<TrustBoundary>`] by matching every reference against
//! the per-language rule table.
//!
//! The engine is **purely in-memory** for the derivation step — it takes
//! `&[Reference]` directly so the indexer can derive boundaries from
//! freshly-parsed refs without paying a round-trip to the DB. The
//! [`store_for_file`] helper persists the result; [`derive_for_index`] runs
//! the whole pipeline against an indexed DB and is the path used after
//! incremental refresh hooks.

use std::collections::BTreeSet;

use anyhow::Result;
use codesage_protocol::{Language, Reference, ReferenceKind, TrustBoundary};
use codesage_storage::Database;

use crate::trust_boundary_rules::{TrustBoundaryRule, rule_matches, rules_for};

/// Derive the set of trust boundaries crossed by a file from its parsed
/// references. Sorted, deduped, language-aware (C++ inherits C rules).
///
/// Only references with kinds that *can* signal a boundary contribute:
/// `Import`, `Include`, `Call`, and `TypeHint`. `Inheritance`, `TraitUse`, and
/// `Instantiation` produce no boundary signal in practice (Rust trait derives
/// surface as `TraitUse` on totally innocuous traits like `Debug` and would
/// pollute boundaries with no benefit).
pub fn derive_from_refs(refs: &[Reference], language: Language) -> Vec<TrustBoundary> {
    let tables = rules_for(language);
    if tables.is_empty() {
        return Vec::new();
    }
    let mut acc: BTreeSet<TrustBoundary> = BTreeSet::new();
    for r in refs {
        if !ref_kind_signals_boundary(r.kind) {
            continue;
        }
        let name = normalize_ref_name(&r.to_name, r.kind);
        for table in tables {
            apply_rules(table, &name, &mut acc);
        }
    }
    acc.into_iter().collect()
}

/// Strip the angle-bracket / quote framing that the C parser preserves
/// around `#include` directives. The parser records `<sys/socket.h>` and
/// `"local.h"` verbatim; the rule patterns are written against the bare
/// path, so without this normalization every C include-shaped rule
/// silently misses on real source.
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

/// Derive *and* persist boundaries for one file's parsed refs. Replaces
/// whatever rows were stored previously for `file_id` (idempotent on
/// re-index). Callers inside an `execute_batch` closure should use this.
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

/// Walk every indexed file, re-derive boundaries from the `refs` rows the
/// parser stored, and replace each file's boundary set. Use after a schema
/// migration that introduces this table, or after rule-table changes that
/// invalidate previously-computed boundaries. O(refs_total).
pub fn derive_for_index(db: &Database) -> Result<usize> {
    let files = db.all_files_with_id_and_language()?;
    derive_for_files(db, &files)
}

/// Targeted version of `derive_for_index`: derive boundaries only for
/// the given `(file_id, path, language)` tuples. Pair with
/// `Database::files_pending_boundary_derivation` to backfill exactly
/// the files that need work without reprocessing already-stamped ones.
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
            let refs = db.refs_outgoing_for_file_id(*file_id)?;
            let in_memory: Vec<Reference> = refs
                .into_iter()
                .map(|(to_name, kind)| Reference {
                    from_file: String::new(),
                    from_symbol: None,
                    to_name,
                    kind,
                    line: 0,
                    col: 0,
                })
                .collect();
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
        }
    }

    fn inc(to: &str) -> Reference {
        Reference {
            kind: ReferenceKind::Include,
            ..imp(to)
        }
    }

    #[test]
    fn c_include_strips_angle_brackets_and_quotes() {
        // Regression: the C parser preserves `<...>` / `"..."` framing around
        // includes; rules are written against the bare path. Without the
        // strip, every C include rule misses silently.
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
        // A Rust file `impl Default for Foo` would record a TraitUse on
        // `Default`. That mustn't count toward boundaries; only Import /
        // Include / Call / TypeHint do.
        let r = Reference {
            from_file: String::new(),
            from_symbol: None,
            to_name: "reqwest::Client".to_string(),
            kind: ReferenceKind::Inheritance,
            line: 0,
            col: 0,
        };
        let b = derive_from_refs(&[r], Language::Rust);
        assert!(
            b.is_empty(),
            "Inheritance refs must not signal a boundary, got {:?}",
            b
        );
    }
}
