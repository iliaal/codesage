//! Session gate + project orientation commands: `session-start`,
//! `session-end`, `overview`.

use anyhow::Result;

use crate::{PROJECT_DIR, find_project_root, flush_stdio, open_db};

pub(crate) fn cmd_session_start(session_id: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let snap = codesage_graph::session_start(&root, &db, session_id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
    } else {
        let snapshot_path = root
            .join(PROJECT_DIR)
            .join("sessions")
            .join(format!("{session_id}.json"));
        println!("Session baseline saved: {}", snapshot_path.display());
        println!(
            "  files={}  symbols={}  cycles={}  top_risk_files={}",
            snap.file_count,
            snap.symbol_count,
            snap.cycles.len(),
            snap.top_risk_files.len()
        );
        if let Some(head) = &snap.git_head {
            println!("  git HEAD: {head}");
        }
    }
    Ok(())
}

pub(crate) fn cmd_session_end(session_id: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let diff = codesage_graph::session_end(&root, &db, session_id)?;
    let pass = diff.pass;
    if json {
        println!("{}", serde_json::to_string_pretty(&diff)?);
    } else {
        let verdict = if diff.pass { "PASS" } else { "FAIL" };
        println!(
            "Session {}: {} ({}s)",
            diff.session_id, verdict, diff.duration_seconds
        );
        println!(
            "  files: {} → {}  ({} added, {} removed)",
            diff.file_count_before,
            diff.file_count_after,
            diff.new_files.len(),
            diff.removed_files.len(),
        );
        println!(
            "  symbols: {} → {}",
            diff.symbol_count_before, diff.symbol_count_after
        );
        if !diff.new_cycles.is_empty() {
            println!("  NEW cycles ({}):", diff.new_cycles.len());
            for c in diff.new_cycles.iter().take(5) {
                println!("    - {} files: {}", c.len(), c.join(", "));
            }
            if diff.new_cycles.len() > 5 {
                println!("    (+{} more)", diff.new_cycles.len() - 5);
            }
        }
        if !diff.resolved_cycles.is_empty() {
            println!("  resolved cycles: {}", diff.resolved_cycles.len());
        }
        if !diff.risk_regressions.is_empty() {
            println!(
                "  risk regressions ({}, max delta {:.2}):",
                diff.risk_regressions.len(),
                diff.max_risk_regression
            );
            for r in diff.risk_regressions.iter().take(10) {
                println!(
                    "    {:>5.2} → {:>5.2}  (Δ{:+.2})  {}",
                    r.before, r.after, r.delta, r.file
                );
            }
        }
        if !diff.summary_notes.is_empty() {
            println!("  Notes:");
            for n in &diff.summary_notes {
                println!("    - {n}");
            }
        }
    }
    if !pass {
        flush_stdio();
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn cmd_overview(json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let overview = codesage_graph::build_project_overview(&root, &db)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&overview)?);
        return Ok(());
    }

    println!("Project: {}", overview.project_root);
    println!(
        "Index: {} files, {} symbols | {}",
        overview.file_count, overview.symbol_count, overview.freshness.structural_summary
    );
    if overview.freshness.semantic_indexed {
        println!(
            "Semantic: {} files with chunks",
            overview.freshness.semantic_indexed_files
        );
    } else {
        println!("Semantic: not indexed");
    }

    if !overview.languages.is_empty() {
        let langs: Vec<String> = overview
            .languages
            .iter()
            .map(|l| format!("{} ({})", l.language.as_str(), l.file_count))
            .collect();
        println!("Languages: {}", langs.join(", "));
    }

    if overview.feature_count > 0 {
        let kinds: Vec<String> = overview
            .feature_summary
            .iter()
            .map(|k| format!("{} {}", k.count, k.kind.as_str()))
            .collect();
        println!(
            "Features: {} total ({})",
            overview.feature_count,
            kinds.join(", ")
        );
    }

    if !overview.top_risk_files.is_empty() {
        println!("Top risk:");
        for r in &overview.top_risk_files {
            println!("  {:.3}  {}", r.score, r.file);
        }
    }

    if !overview.trust_boundary_clusters.is_empty() {
        let tb: Vec<String> = overview
            .trust_boundary_clusters
            .iter()
            .map(|c| format!("{} ({})", c.boundary.as_str(), c.file_count))
            .collect();
        println!("Trust boundaries: {}", tb.join(", "));
    }

    if !overview.entrypoints.is_empty() {
        println!("Entrypoints (sample):");
        for e in &overview.entrypoints {
            println!("  [{}] {} — {}", e.kind.as_str(), e.title, e.entry_path);
        }
    }

    if !overview.suggested_next_calls.is_empty() {
        println!("Suggested next calls:");
        for c in &overview.suggested_next_calls {
            println!("  {} → {}  ({})", c.intent, c.tool, c.why);
        }
    }

    Ok(())
}
