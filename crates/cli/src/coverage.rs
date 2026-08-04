//! `codesage coverage` — what this project contains that indexing cannot see.
//!
//! Answers a question no existing surface does. `IndexStats::files_skipped`
//! counts files unchanged since the last pass (freshness), and `files_failed`
//! counts parse errors on files that were at least recognized. A file whose
//! extension maps to no supported language is dropped at discovery and reaches
//! neither counter, so the largest coverage gap is the one nothing reports.

use anyhow::Result;
use codesage_parser::discover::survey_coverage;

use crate::{find_project_root, get_exclude_patterns, load_project_config};

pub fn run(json: bool, top: usize) -> Result<()> {
    let root = find_project_root()?;
    // Same exclude set the indexer uses, so the denominator matches what
    // indexing would actually consider rather than a parallel definition.
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);

    let survey = survey_coverage(&root, &excludes)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&survey)?);
        return Ok(());
    }

    let pct = survey.covered_fraction() * 100.0;
    println!(
        "Coverage: {}/{} files indexable ({:.1}%)",
        survey.covered_total,
        survey.covered_total + survey.uncovered_total,
        pct
    );
    println!("Excluded by config: {}", survey.excluded);

    if !survey.covered_by_language.is_empty() {
        println!("\nIndexed, by language:");
        let mut langs: Vec<_> = survey.covered_by_language.iter().collect();
        langs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (lang, n) in langs {
            println!("  {n:>7}  {lang}");
        }
    }

    if survey.uncovered_by_extension.is_empty() {
        println!("\nNothing uncovered: every walked file maps to a supported language.");
        return Ok(());
    }

    let mut exts: Vec<_> = survey.uncovered_by_extension.iter().collect();
    // Descending by count, then by extension so equal counts are stable.
    exts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let shown = if top == 0 {
        exts.len()
    } else {
        top.min(exts.len())
    };
    println!("\nNOT indexed (no supported language), by extension:");
    for (ext, n) in exts.iter().take(shown) {
        println!("  {n:>7}  {ext}");
    }
    if shown < exts.len() {
        let rest: usize = exts[shown..].iter().map(|(_, n)| **n).sum();
        println!(
            "  {:>7}  ... {} more extensions (--top 0 for all)",
            rest,
            exts.len() - shown
        );
    }
    println!(
        "\nNot every line above is a gap: docs, config and binaries are legitimately\nunindexed. What matters is source in a language CodeSage does not parse. For\nany extension listed, a search returns nothing because the file was never\nindexed, not because the code is absent. Extension-to-language mapping lives\nin crates/parser/src/detect.rs."
    );
    Ok(())
}
