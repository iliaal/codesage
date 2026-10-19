use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use codesage_parser::{
    discover::discover_files_with_excludes, extract::extract_symbols, parse::parse_file_tolerant,
    references::extract_references,
};
use codesage_protocol::Language;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let root = arguments
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: header_dialects ROOT [--current-only]"))?;
    let current_only = match arguments.next().as_deref() {
        None => false,
        Some("--current-only") => true,
        Some(other) => anyhow::bail!("unknown option: {other}"),
    };
    anyhow::ensure!(arguments.next().is_none(), "unexpected argument");
    let started = std::time::Instant::now();
    let files = discover_files_with_excludes(Path::new(&root), &[])?;
    eprintln!("discovery_elapsed_ms={}", started.elapsed().as_millis());
    let mut totals = BTreeMap::<(String, String), (usize, usize, usize, usize)>::new();
    for file in files
        .into_iter()
        .filter(|f| matches!(f.language, Language::C | Language::Cpp))
    {
        let source = std::fs::read(Path::new(&root).join(&file.path))?;
        let mut variants = Vec::new();
        for (mode, language) in [
            ("current", file.language),
            ("forced_c", Language::C),
            ("forced_cpp", Language::Cpp),
        ] {
            if current_only && mode != "current" {
                continue;
            }
            let language = if file.path.ends_with(".h") {
                language
            } else {
                file.language
            };
            let parsed = parse_file_tolerant(&source, language)?;
            let symbols: BTreeSet<_> =
                extract_symbols(&parsed.tree, &source, language, &file.path)?
                    .into_iter()
                    .map(|s| {
                        format!(
                            "{}:{:?}:{}:{}",
                            s.qualified_name, s.kind, s.line_start, s.line_end
                        )
                    })
                    .collect();
            let refs: BTreeSet<_> =
                extract_references(&parsed.tree, &source, language, &file.path)?
                    .into_iter()
                    .map(|r| format!("{:?}", r))
                    .collect();
            let count = totals
                .entry((mode.to_owned(), language.to_string()))
                .or_default();
            count.0 += 1;
            count.1 += usize::from(parsed.degraded);
            count.2 += symbols.len();
            count.3 += refs.len();
            println!(
                "FILE\t{mode}\t{}\t{language}\t{}\t{}\t{}",
                file.path,
                parsed.degraded,
                symbols.len(),
                refs.len()
            );
            variants.push((symbols, refs));
        }
        if !current_only && file.path.ends_with(".h") {
            for (label, a, b) in [
                ("SYMBOL", &variants[1].0, &variants[2].0),
                ("REFERENCE", &variants[1].1, &variants[2].1),
            ] {
                for row in a.difference(b) {
                    println!("{label}_C_ONLY\t{}\t{row}", file.path);
                }
                for row in b.difference(a) {
                    println!("{label}_CPP_ONLY\t{}\t{row}", file.path);
                }
            }
        }
    }
    for ((mode, language), (files, degraded, symbols, refs)) in totals {
        println!("TOTAL\t{mode}\t{language}\t{files}\t{degraded}\t{symbols}\t{refs}");
    }
    eprintln!("total_elapsed_ms={}", started.elapsed().as_millis());
    Ok(())
}
