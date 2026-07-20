//! Feature-slice query commands: `features-list`, `feature-show`,
//! `feature-for`, `feature-bundle`, `trust-boundaries`.

use anyhow::{Result, bail};

use crate::{find_project_root, load_symbol_context_db, open_db};

pub(crate) fn cmd_features_list(
    kind: Option<&str>,
    lang: Option<&str>,
    tag: Option<&str>,
    since: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    use codesage_protocol::{FeatureKind, Language};
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let kind = match kind {
        None => None,
        Some(k) => Some(
            FeatureKind::parse(k).ok_or_else(|| anyhow::anyhow!("unknown feature kind: {k}"))?,
        ),
    };
    let language = match lang {
        None => None,
        Some(l) => {
            Some(Language::parse(l).ok_or_else(|| anyhow::anyhow!("unknown language: {l}"))?)
        }
    };
    // With `--since`, fetch unbounded then cap after the changed-file
    // intersection — the SQL LIMIT runs before our filter, so a default
    // limit would truncate candidates the diff filter hasn't seen yet.
    let query_limit = if since.is_some() { 0 } else { limit };
    let mut features = db.list_features(kind, language, tag, query_limit)?;
    if let Some(git_ref) = since {
        let changed = codesage_graph::changed_files_since(&root, git_ref)?;
        features.retain(|f| codesage_graph::feature_touched_since(&f.files, &changed));
        if limit > 0 && features.len() > limit {
            features.truncate(limit);
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FeatureListResults {
                results: features,
            })?
        );
    } else if features.is_empty() {
        println!("No features matched.");
    } else {
        println!("Features ({}):", features.len());
        for f in &features {
            let disc = f
                .entry_command
                .as_deref()
                .or(f.entry_route.as_deref())
                .or(f.entry_symbol.as_deref())
                .unwrap_or("");
            println!(
                "  {:<22} {:<14} {:<10} {:<12} {}",
                f.feature_id,
                f.kind.as_str(),
                f.language.as_str(),
                disc,
                f.title
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_feature_show(id: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let feature = match db.load_feature(id)? {
        Some(f) => f,
        None => bail!("no feature with id `{id}` in this project"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&feature)?);
    } else {
        println!("{} ({})", feature.title, feature.feature_id);
        println!("  kind: {}", feature.kind.as_str());
        println!("  source: {}", feature.source);
        println!("  language: {}", feature.language.as_str());
        println!("  confidence: {}", feature.confidence.as_str());
        println!("  entry: {}", feature.entry_path);
        if let Some(s) = &feature.entry_symbol {
            println!("  entry_symbol: {s}");
        }
        if let Some(r) = &feature.entry_route {
            println!("  entry_route: {r}");
        }
        if let Some(c) = &feature.entry_command {
            println!("  entry_command: {c}");
        }
        if let Some(c) = &feature.test_command {
            println!("  test_command: {c}");
        }
        if !feature.tags.is_empty() {
            println!("  tags: {}", feature.tags.join(", "));
        }
        if !feature.trust_boundaries.is_empty() {
            let names: Vec<&str> = feature
                .trust_boundaries
                .iter()
                .map(|b| b.as_str())
                .collect();
            println!("  trust_boundaries: {}", names.join(", "));
        }
        println!("  files ({}):", feature.files.len());
        for f in &feature.files {
            let reason = f.reason.as_deref().unwrap_or("");
            println!("    {:<8} {} ({reason})", f.role.as_str(), f.path);
        }
    }
    Ok(())
}

pub(crate) fn cmd_feature_bundle(
    id: &str,
    include_callers: bool,
    include_callees: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    // Open against the configured embedding model so `primary` / `related`
    // resolve real chunks. The default-model `open_db` points at the
    // MiniLM 384-dim chunk table and returns empty content on projects
    // configured for a different model (e.g. php-src uses jina v2 768-dim).
    let db = load_symbol_context_db(&root)?;
    let bundle = codesage_graph::feature_bundle(&db, id, include_callers, include_callees, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
    } else {
        println!("{}", bundle.target_description);
        println!("  primary ({}):", bundle.primary.len());
        for r in &bundle.primary {
            println!(
                "    {}:{}-{} ({:.0} chars)",
                r.file_path,
                r.start_line,
                r.end_line,
                r.content.chars().count()
            );
        }
        if !bundle.related.is_empty() {
            println!("  related ({}):", bundle.related.len());
            for r in &bundle.related {
                println!(
                    "    {}:{}-{} ({:.0} chars)",
                    r.file_path,
                    r.start_line,
                    r.end_line,
                    r.content.chars().count()
                );
            }
        }
        if !bundle.symbol_definitions.is_empty() {
            println!("  symbols ({}):", bundle.symbol_definitions.len());
            for s in &bundle.symbol_definitions {
                println!(
                    "    {} ({}) @ {}:{}",
                    s.qualified_name,
                    s.kind.as_str(),
                    s.file_path,
                    s.line_start
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_feature_for(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let features = db.features_for_file(file)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FeatureListResults {
                results: features,
            })?
        );
    } else if features.is_empty() {
        println!("No mapped feature owns or contexts `{file}`.");
    } else {
        println!("Features for {file}:");
        for f in &features {
            println!("  {} {} {}", f.feature_id, f.kind.as_str(), f.title);
        }
    }
    Ok(())
}

pub(crate) fn cmd_trust_boundaries(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let tags = db.trust_boundaries_for_file_path(file)?;
    if json {
        let names: Vec<&str> = tags.iter().map(|b| b.as_str()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "file": file,
                "trust_boundaries": names,
            }))?
        );
    } else if tags.is_empty() {
        println!(
            "Trust boundaries: {file} -> (none; file may not be indexed or has no recognized boundary signal)"
        );
    } else {
        println!("Trust boundaries: {file}");
        for t in &tags {
            println!("  - {}", t.as_str());
        }
    }
    Ok(())
}
