//! Read-side query commands: `search`, `find-symbol`, `find-references`,
//! `dependencies`, `impact`, `export`, `similar`.

use anyhow::Result;
use codesage_graph::{
    export_context, export_context_for_symbol, find_references, find_similar, find_symbol,
    impact_analysis_report, list_dependencies, search_page,
};
use codesage_protocol::{
    ContextBundle, ExportRequest, FileCategory, FindReferencesRequest, FindSymbolRequest,
    ImpactOptions, ImpactRequest, ImpactTarget, Language, ReferenceKind, SearchRequest, SymbolKind,
};

use crate::{
    find_project_root, load_query_stack, load_symbol_context_db, open_db, open_db_read_only,
};

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
    let report = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: name.to_string(),
            kind,
        },
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if report.results.is_empty() {
        println!("No references found for '{name}'");
    } else {
        for r in &report.results {
            let ctx = r.from_symbol.as_deref().unwrap_or("top-level");
            println!(
                "{} {} -- {}:{} (in {})",
                r.kind, r.to_name, r.from_file, r.line, ctx
            );
        }
    }
    if let Some(note) = &report.note {
        println!("Note: {note}");
    }
    Ok(())
}

fn parse_search_language(language: Option<&str>) -> Result<Option<Language>> {
    language
        .map(|l| Language::parse(l).ok_or_else(|| anyhow::anyhow!("unknown language: {l}")))
        .transpose()
}

/// Match the graph's normalization so CLI output reports the applied threshold.
fn normalize_min_jaccard(min_jaccard: f32) -> f32 {
    if min_jaccard.is_finite() {
        min_jaccard.clamp(0.0, 1.0)
    } else {
        0.85
    }
}

pub(crate) fn cmd_search(
    query: &str,
    limit: usize,
    offset: usize,
    language: Option<&str>,
    paths: Option<Vec<String>>,
    adaptive_limit: bool,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    let (db, mut embedder, mut reranker) = load_query_stack(&root)?;

    let languages = parse_search_language(language)?.map(|lang| vec![lang]);

    let req = SearchRequest {
        query: query.to_string(),
        limit: Some(limit),
        offset: Some(offset),
        languages,
        paths,
        adaptive_limit,
    };

    let query_embedding = embedder.embed_one(&req.query)?;
    let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker.as_mut().map(|r| {
        Box::new(move |q: &str, docs: &[&str]| r.score_pairs(q, docs))
            as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
    });
    let page = search_page(&db, &query_embedding, rerank_fn, &req)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else if page.results.is_empty() {
        println!("No results found for '{query}'");
    } else {
        let results = &page.results;
        for r in results {
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
        // Suppress a natural single-row cliff, but explain rows removed by --adaptive-limit.
        if results.len() < 2 && page.margin_pct == Some(0) {
            return Ok(());
        }
        if let (Some(confidence), Some(margin), Some(cliff_at)) =
            (page.confidence, page.margin_pct, page.cliff_at)
        {
            let label = match confidence {
                codesage_protocol::SearchConfidence::High => "high",
                codesage_protocol::SearchConfidence::Low => "low",
            };
            println!("confidence: {label} (largest drop {margin}%, cliff after row {cliff_at})");
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
    let min_jaccard = normalize_min_jaccard(min_jaccard);
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

pub(crate) fn cmd_trace(from: &str, to: &str, max_depth: usize, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let report = codesage_graph::trace_call_path(
        &db,
        &codesage_protocol::CallPathRequest {
            from: from.to_string(),
            to: to.to_string(),
            max_depth,
        },
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if !report.found {
        println!(
            "No call chain from '{}' to '{}'{}.",
            from,
            to,
            if report.bounded {
                " within the search bound"
            } else {
                ""
            }
        );
        if let Some(note) = &report.note {
            println!("  {note}");
        }
        return Ok(());
    }

    println!(
        "Call chain from '{}' to '{}' ({} hop{}):",
        from,
        to,
        report.length,
        if report.length == 1 { "" } else { "s" }
    );
    for (i, step) in report.steps.iter().enumerate() {
        let arrow = if i == 0 { " " } else { "\u{2192}" };
        match step.call_line {
            Some(line) => println!(
                "  {arrow} {} ({}:{}) called at {}:{}",
                step.qualified_name,
                step.file_path,
                step.line_start,
                report.steps[i - 1].file_path,
                line
            ),
            None => println!(
                "  {arrow} {} ({}:{})",
                step.qualified_name, step.file_path, step.line_start
            ),
        }
    }
    Ok(())
}

pub(crate) fn cmd_from_trace(
    file: Option<&std::path::Path>,
    limit: Option<usize>,
    json: bool,
) -> Result<()> {
    use anyhow::{Context, bail};
    use std::io::{IsTerminal, Read};

    let trace = match file {
        Some(p) if p.as_os_str() != "-" => std::fs::read_to_string(p)
            .with_context(|| format!("failed to read trace from {}", p.display()))?,
        _ => {
            if std::io::stdin().is_terminal() {
                bail!("no trace provided (pass a file path or pipe the trace via stdin)");
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    if trace.trim().is_empty() {
        bail!("the trace is empty");
    }

    let root = find_project_root()?;
    let db = open_db(&root)?;
    let report = codesage_graph::from_trace(
        &db,
        &root,
        &codesage_protocol::FromTraceRequest { trace, limit },
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!(
        "format: {} ({} parsed, {} resolved ({} with symbol), {} ambiguous, {} unresolved; {} stack{}; {})",
        report.format,
        report.parsed,
        report.resolved,
        report.with_symbol,
        report.ambiguous,
        report.unresolved,
        report.stacks,
        if report.stacks == 1 { "" } else { "s" },
        report.order
    );
    if let Some(note) = &report.note {
        println!("  {note}");
    }
    let mut current_stack: Option<u32> = None;
    for f in &report.frames {
        if report.stacks > 1 && current_stack != Some(f.stack) {
            current_stack = Some(f.stack);
            println!(
                "-- stack {}{}",
                f.stack,
                if f.stack == 0 && report.root_cause_first {
                    " (root cause)"
                } else {
                    ""
                }
            );
        }
        let location = match (&f.file, f.line) {
            (Some(p), Some(l)) => format!("{p}:{l}"),
            (Some(p), None) => p.clone(),
            (None, _) => "(no file)".to_string(),
        };
        let symbol = match &f.symbol {
            Some(s) => format!(
                "{} ({}, {}:{}-{})",
                s.qualified_name, s.kind, s.path, s.line_start, s.line_end
            ),
            None => f.function.clone().unwrap_or_default(),
        };
        println!("#{:<3} {:<10} {location}  {symbol}", f.index, f.status);
        for c in &f.candidates {
            println!("        candidate: {c}");
        }
        if f.candidates_total > f.candidates.len() {
            println!(
                "        … {} more candidates ({} total)",
                f.candidates_total - f.candidates.len(),
                f.candidates_total
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

    // Neither flag must remain None to preserve heuristic target classification.
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

/// Portable context bundle; token counts are approximate.
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

/// Hook failures return success without a payload to avoid agent-context noise.
/// Record failures in the fire log; CODESAGE_BRIEF_DEBUG exposes causes on stderr.
pub(crate) fn cmd_brief(file: &str, json: bool, session: Option<&str>) -> Result<()> {
    let Ok(root) = std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| crate::evidence_root(&cwd))
    else {
        debug_brief("no project root found from cwd");
        return Ok(());
    };
    let rel = file.trim_start_matches("./");
    let indexed_brief = if crate::db_path(&root).try_exists().unwrap_or(true) {
        let on_disk = std::fs::read(root.join(rel))
            .ok()
            .map(|b| codesage_parser::discover::content_hash(&b));
        open_db_read_only(&root)
            .and_then(|db| codesage_graph::build_edit_brief(&db, rel, on_disk.as_deref()))
    } else {
        Ok(codesage_protocol::EditBrief {
            file_path: rel.to_string(),
            empty: true,
            ..Default::default()
        })
    };
    let mut brief = match indexed_brief {
        Ok(brief) => brief,
        Err(e) => {
            debug_brief(&format!("building the brief for {rel} failed: {e:#}"));
            // Failed fires belong in the denominator of efficacy measurements.
            if let Some(session) = session {
                let ledger = crate::brief_gate::ledger_dir(&crate::daemon::default_runtime_dir());
                crate::brief_gate::log_fire(
                    &ledger,
                    session,
                    &root,
                    rel,
                    crate::brief_gate::Decision::Error,
                    "",
                );
            }
            return Ok(());
        }
    };

    let overlap = codesage_graph::branch_overlap::branch_overlap(
        &root,
        &[rel.to_string()],
        std::time::Duration::from_millis(100),
    );
    brief.empty &= overlap.branches.is_empty();
    brief.branch_overlap = Some(overlap);

    let rendered = render_brief(&brief);

    if let Some(session) = session {
        // Session gating applies to both formats; hash rendered text so format changes
        // cannot re-arm an already served payload.
        let dir = crate::daemon::default_runtime_dir();
        let decision = crate::brief_gate::evaluate(&dir, session, rel, &rendered);
        // Log suppressed fires before returning; keep the ledger outside volatile runtime storage.
        let ledger = crate::brief_gate::ledger_dir(&dir);
        crate::brief_gate::log_fire(&ledger, session, &root, rel, decision, &rendered);
        if decision != crate::brief_gate::Decision::Served {
            return Ok(());
        }
    }

    if json {
        // Explicit JSON queries include empty results unless session gating suppresses them.
        if let Ok(s) = serde_json::to_string(&brief) {
            println!("{s}");
        }
        return Ok(());
    }

    print!("{rendered}");
    Ok(())
}

/// Optional diagnostics stay on stderr, separate from the agent payload.
fn debug_brief(msg: &str) {
    if std::env::var_os("CODESAGE_BRIEF_DEBUG").is_some_and(|v| !v.is_empty()) {
        eprintln!("brief: {msg}");
    }
}

/// Empty briefs must render empty: the gate charges only nonempty payloads.
fn render_brief(brief: &codesage_protocol::EditBrief) -> String {
    let mut out = String::new();
    if let Some(p) = brief.churn_percentile.filter(|_| brief.hotspot) {
        out.push_str(&format!("hotspot: churn percentile {:.0}%", p * 100.0));
        // Zero recorded fixes do not establish safety.
        if let (Some(f), Some(c)) = (brief.fix_count, brief.commits)
            && f > 0
        {
            out.push_str(&format!(", {f} of {c} commits were fixes"));
        }
        out.push('\n');
    }
    if !brief.tests.is_empty() {
        out.push_str(&format!("tests: {}\n", brief.tests.join(", ")));
    }
    if !brief.coupled.is_empty() {
        out.push_str(&format!("changes with: {}\n", brief.coupled.join(", ")));
    }
    if let Some(overlap) = &brief.branch_overlap
        && !overlap.branches.is_empty()
    {
        for branch in &overlap.branches {
            out.push_str(&format!(
                "branch overlap: {:?} edits {} (same-file, HEAD...branch; merge base {})\n",
                branch.branch,
                codesage_graph::branch_overlap::quoted_paths(&branch.files),
                branch.merge_base
            ));
        }
        out.push_str(&format!(
            "{}\n",
            codesage_graph::branch_overlap::branch_overlap_summary(overlap)
        ));
    }
    out
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
        assert_eq!(export_format("md", true), export_format("json", false));
        assert_eq!(export_format("md", false), "md");
        assert_eq!(export_format("ingest", false), "ingest");
    }

    #[test]
    fn search_rejects_unknown_language_instead_of_unfiltering() {
        let err = parse_search_language(Some("cobol"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown language"), "got: {err}");
        assert!(parse_search_language(None).unwrap().is_none());
        assert_eq!(
            parse_search_language(Some("rust")).unwrap(),
            Some(Language::Rust)
        );
    }

    #[test]
    fn similar_threshold_normalizes_to_contract_range() {
        assert_eq!(normalize_min_jaccard(0.7), 0.7);
        assert_eq!(normalize_min_jaccard(1.5), 1.0);
        assert_eq!(normalize_min_jaccard(-0.2), 0.0);
        assert_eq!(normalize_min_jaccard(f32::NAN), 0.85);
        assert_eq!(normalize_min_jaccard(f32::INFINITY), 0.85);
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
