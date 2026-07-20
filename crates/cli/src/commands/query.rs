//! Read-side query commands: `search`, `find-symbol`, `find-references`,
//! `dependencies`, `impact`, `export`, `similar`.

use anyhow::Result;
use codesage_graph::{
    export_context, export_context_for_symbol, find_references, find_similar, find_symbol,
    impact_analysis_report, list_dependencies, search,
};
use codesage_protocol::{
    ContextBundle, ExportRequest, FileCategory, FindReferencesRequest, FindSymbolRequest,
    ImpactOptions, ImpactRequest, ImpactTarget, Language, ReferenceKind, SearchRequest, SymbolKind,
};

use crate::{find_project_root, load_query_stack, load_symbol_context_db, open_db};

pub(crate) fn cmd_find_symbol(name: &str, kind_str: Option<&str>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let kind = kind_str
        .map(|kind| {
            SymbolKind::parse(kind).ok_or_else(|| anyhow::anyhow!("unknown symbol kind: {kind}"))
        })
        .transpose()?;
    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: name.to_string(),
            kind,
        },
    )?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FindSymbolResults { results })?
        );
    } else if results.is_empty() {
        println!("No symbols found for '{name}'");
    } else {
        for s in &results {
            println!(
                "{} {} -- {}:{}",
                s.kind, s.qualified_name, s.file_path, s.line_start
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_find_references(name: &str, kind_str: Option<&str>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let kind = kind_str
        .map(|kind| {
            ReferenceKind::parse(kind)
                .ok_or_else(|| anyhow::anyhow!("unknown reference kind: {kind}"))
        })
        .transpose()?;
    let results = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: name.to_string(),
            kind,
        },
    )?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FindReferencesResults { results })?
        );
    } else if results.is_empty() {
        println!("No references found for '{name}'");
    } else {
        for r in &results {
            let ctx = r.from_symbol.as_deref().unwrap_or("top-level");
            println!(
                "{} {} -- {}:{} (in {})",
                r.kind, r.to_name, r.from_file, r.line, ctx
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_search(
    query: &str,
    limit: usize,
    offset: usize,
    language: Option<&str>,
    paths: Option<Vec<String>>,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    let (db, mut embedder, mut reranker) = load_query_stack(&root)?;

    let languages = language.and_then(|l| Language::parse(l).map(|lang| vec![lang]));

    let req = SearchRequest {
        query: query.to_string(),
        limit: Some(limit),
        offset: Some(offset),
        languages,
        paths,
    };

    let query_embedding = embedder.embed_one(&req.query)?;
    let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker.as_mut().map(|r| {
        Box::new(move |q: &str, docs: &[&str]| r.score_pairs(q, docs))
            as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
    });
    let results = search(&db, &query_embedding, rerank_fn, &req)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::SearchResults { results })?
        );
    } else if results.is_empty() {
        println!("No results found for '{query}'");
    } else {
        for r in &results {
            let preview: String = r.content.chars().take(120).collect();
            let preview = preview.replace('\n', " ");
            println!(
                "{:.1}% {}:{}-{} ({}) {}",
                r.score * 100.0,
                r.file_path,
                r.start_line,
                r.end_line,
                r.language,
                preview
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_dependencies(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let deps = list_dependencies(&db, file)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&deps)?);
    } else {
        println!("File: {}", deps.file_path);
        if deps.imports.is_empty() {
            println!("\nImports: (none)");
        } else {
            println!("\nImports:");
            for imp in &deps.imports {
                println!("  {imp}");
            }
        }
        if deps.imported_by.is_empty() {
            println!("\nImported by: (none)");
        } else {
            println!("\nImported by:");
            for by in &deps.imported_by {
                println!("  {by}");
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_similar(symbol: &str, min_jaccard: f32, limit: usize, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let hits = find_similar(&db, symbol, min_jaccard, limit)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FindSimilarResults { results: hits })?
        );
    } else if hits.is_empty() {
        println!("No clones of '{symbol}' at Jaccard >= {min_jaccard:.2}");
    } else {
        println!("Clones of '{symbol}' (Jaccard >= {min_jaccard:.2}):");
        for h in &hits {
            println!(
                "  {:.3}  {}:{}-{}  {}()",
                h.jaccard, h.file_path, h.line_start, h.line_end, h.name
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_impact(
    target: &str,
    is_file: bool,
    is_symbol: bool,
    depth: usize,
    source_only: bool,
    forward: bool,
    siblings: bool,
    limit: Option<usize>,
    summary_only: bool,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    // Pass Some(true) only when the user explicitly set --file; an unset false
    // would force Symbol classification and break the heuristic fallback.
    let hint = if is_file {
        Some(true)
    } else if is_symbol {
        Some(false)
    } else {
        None
    };
    let req = ImpactRequest {
        target: ImpactTarget::from_hint(target.to_string(), hint),
        depth,
        source_only,
    };
    let opts = ImpactOptions {
        include_forward: forward,
        include_siblings: siblings,
        limit,
        summary_only,
    };

    let report = impact_analysis_report(&db, &req, &opts)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.results.is_empty()
        && report.forward_dependencies.is_empty()
        && report.sibling_symbols.is_empty()
    {
        println!("No impact detected for '{target}'.");
        return Ok(());
    }

    println!(
        "Impact of '{}' (depth={}, {} files affected{}):",
        target,
        depth,
        report.results.len(),
        if report.truncated { ", truncated" } else { "" }
    );
    for e in &report.results {
        let cat = match e.category {
            FileCategory::Source => "src",
            FileCategory::Test => "test",
            FileCategory::Config => "cfg",
        };
        println!(
            "  [{cat}] d={} {} ({} refs)",
            e.distance,
            e.file_path,
            e.reasons.len()
        );
        for r in e.reasons.iter().take(3) {
            println!("    via {} @ line {} ({})", r.via_symbol, r.line, r.kind);
        }
    }

    if let Some(summary) = &report.summary {
        let dist: Vec<String> = summary
            .by_distance
            .iter()
            .map(|d| format!("d{}={}", d.distance, d.count))
            .collect();
        println!(
            "Summary: {} affected ({})",
            summary.total_affected,
            dist.join(" ")
        );
    }

    if !report.forward_dependencies.is_empty() {
        println!(
            "Forward dependencies ({}):",
            report.forward_dependencies.len()
        );
        for f in &report.forward_dependencies {
            println!("  {f}");
        }
    }

    if !report.sibling_symbols.is_empty() {
        println!("Sibling symbols ({}):", report.sibling_symbols.len());
        for s in &report.sibling_symbols {
            println!("  {} ({}) @ line {}", s.name, s.kind.as_str(), s.line);
        }
    }
    Ok(())
}

/// Resolve the effective export format: `--json` is a pure shorthand for
/// `--format json` (clap rejects passing both together).
pub(crate) fn export_format(format: &str, json: bool) -> &str {
    if json { "json" } else { format }
}

pub(crate) fn cmd_export(
    target: &str,
    is_symbol: bool,
    limit: usize,
    callers: bool,
    callees: bool,
    format: &str,
) -> Result<()> {
    let root = find_project_root()?;
    let req = ExportRequest::from_target(target.to_string(), is_symbol, limit, callers, callees);

    let bundle = if is_symbol {
        let db = load_symbol_context_db(&root)?;
        export_context_for_symbol(&db, target, &req)?
    } else {
        let (db, mut embedder, mut reranker) = load_query_stack(&root)?;
        let query_embedding = embedder.embed_one(req.query.as_deref().unwrap_or_default())?;
        let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker.as_mut().map(|r| {
            Box::new(move |q: &str, docs: &[&str]| r.score_pairs(q, docs))
                as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
        });
        export_context(&db, &query_embedding, rerank_fn, &req)?
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&bundle)?),
        "ingest" => print_bundle_ingest(&bundle, target, is_symbol),
        _ => print_bundle_markdown(&bundle),
    }
    Ok(())
}

/// Flat-text envelope inspired by gitingest: one self-contained artifact agents can paste
/// into another LLM session without re-templating. Token count is a chars/4 approximation.
fn print_bundle_ingest(bundle: &ContextBundle, target: &str, is_symbol: bool) {
    let target_label = if is_symbol {
        format!("symbol={target}")
    } else {
        format!("query=\"{target}\"")
    };

    let mut all_results: Vec<&codesage_protocol::SearchResult> = bundle.primary.iter().collect();
    all_results.extend(bundle.related.iter());
    let total_chars: usize = all_results.iter().map(|r| r.content.len()).sum();
    let approx_tokens = total_chars / 4;

    let unique_files: Vec<&String> = {
        let mut seen = std::collections::BTreeSet::new();
        let mut order = Vec::new();
        for r in &all_results {
            if seen.insert(r.file_path.as_str()) {
                order.push(&r.file_path);
            }
        }
        order
    };

    println!("=== CodeSage context bundle ===");
    println!("Target: {target_label}");
    println!("Description: {}", bundle.target_description);
    println!(
        "Counts: {} chunks across {} files ({} primary, {} related)",
        all_results.len(),
        unique_files.len(),
        bundle.primary.len(),
        bundle.related.len()
    );
    println!(
        "Approx tokens: ~{} (chars/4 estimate; replace with real tokenizer for billing)",
        approx_tokens
    );
    if !bundle.symbol_definitions.is_empty() {
        println!("Symbol definitions: {}", bundle.symbol_definitions.len());
    }
    println!();

    println!("=== File tree ===");
    for line in render_file_tree(&unique_files) {
        println!("{line}");
    }
    println!();

    if !bundle.symbol_definitions.is_empty() {
        println!("=== Symbol definitions ===");
        for s in &bundle.symbol_definitions {
            println!(
                "- {} ({}): {}:{} qualified={}",
                s.name,
                s.kind.as_str(),
                s.file_path,
                s.line_start,
                s.qualified_name
            );
        }
        println!();
    }

    println!("=== Files ===");
    println!();
    for r in &all_results {
        let symbols = if r.symbols.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = r
                .symbols
                .iter()
                .map(|s| format!("{}({})", s.name, s.kind))
                .collect();
            format!(" symbols=[{}]", names.join(", "))
        };
        println!(
            "=== {}:{}-{} lang={}{} ===",
            r.file_path, r.start_line, r.end_line, r.language, symbols
        );
        println!("{}", r.content.trim_end());
        println!();
    }
}

/// Render a list of file paths as an ASCII tree. Files appear in sorted order under each dir.
fn render_file_tree(paths: &[&String]) -> Vec<String> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        is_file: bool,
    }

    let mut root = Node::default();
    for p in paths {
        let mut cur = &mut root;
        let parts: Vec<&str> = p.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            cur = cur.children.entry(part.to_string()).or_default();
            if i == parts.len() - 1 {
                cur.is_file = true;
            }
        }
    }

    let mut out = Vec::new();
    fn walk(node: &Node, prefix: &str, out: &mut Vec<String>) {
        let entries: Vec<(&String, &Node)> = node.children.iter().collect();
        let n = entries.len();
        for (i, (name, child)) in entries.iter().enumerate() {
            let last = i == n - 1;
            let connector = if last { "└── " } else { "├── " };
            let label = if child.is_file && child.children.is_empty() {
                name.to_string()
            } else {
                format!("{name}/")
            };
            out.push(format!("{prefix}{connector}{label}"));
            let next_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            walk(child, &next_prefix, out);
        }
    }
    walk(&root, "", &mut out);
    out
}

fn print_bundle_markdown(bundle: &ContextBundle) {
    println!("# Context: {}", bundle.target_description);
    println!();

    if !bundle.primary.is_empty() {
        println!("## Primary matches ({})\n", bundle.primary.len());
        for r in &bundle.primary {
            print_result_block(r);
        }
    }

    if !bundle.related.is_empty() {
        println!("## Related code ({})\n", bundle.related.len());
        for r in &bundle.related {
            print_result_block(r);
        }
    }

    if !bundle.symbol_definitions.is_empty() {
        println!(
            "## Symbol definitions ({})\n",
            bundle.symbol_definitions.len()
        );
        for s in &bundle.symbol_definitions {
            println!(
                "- **{}** ({}) — `{}:{}` ({})",
                s.name,
                s.kind.as_str(),
                s.file_path,
                s.line_start,
                s.qualified_name
            );
        }
        println!();
    }
}

fn print_result_block(r: &codesage_protocol::SearchResult) {
    println!(
        "### `{}:{}-{}` ({})",
        r.file_path, r.start_line, r.end_line, r.language
    );
    if !r.symbols.is_empty() {
        let syms: Vec<String> = r
            .symbols
            .iter()
            .map(|s| format!("{} ({})", s.name, s.kind))
            .collect();
        println!("**Symbols:** {}", syms.join(", "));
    }
    println!();
    println!("```{}", r.language);
    println!("{}", r.content);
    println!("```");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    fn paths_refs(owned: &[String]) -> Vec<&String> {
        owned.iter().collect()
    }

    #[test]
    fn export_json_flag_is_exact_alias_for_format_json() {
        // `--json` must behave identically to `--format json`: same resolved
        // format string, so the same match arm runs in cmd_export.
        assert_eq!(export_format("md", true), export_format("json", false));
        assert_eq!(export_format("md", false), "md");
        assert_eq!(export_format("ingest", false), "ingest");
    }

    #[test]
    fn render_file_tree_empty() {
        let out = render_file_tree(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn render_file_tree_single_file() {
        let owned = paths_owned(&["foo.rs"]);
        let out = render_file_tree(&paths_refs(&owned));
        assert_eq!(out, vec!["└── foo.rs"]);
    }

    #[test]
    fn render_file_tree_nested() {
        let owned = paths_owned(&[
            "src/auth/login.php",
            "src/auth/session.php",
            "src/handlers/webhook.php",
        ]);
        let out = render_file_tree(&paths_refs(&owned));
        assert_eq!(
            out,
            vec![
                "└── src/",
                "    ├── auth/",
                "    │   ├── login.php",
                "    │   └── session.php",
                "    └── handlers/",
                "        └── webhook.php",
            ]
        );
    }

    #[test]
    fn render_file_tree_multiple_top_level() {
        let owned = paths_owned(&["a.rs", "b.rs", "c.rs"]);
        let out = render_file_tree(&paths_refs(&owned));
        assert_eq!(out, vec!["├── a.rs", "├── b.rs", "└── c.rs"]);
    }
}
