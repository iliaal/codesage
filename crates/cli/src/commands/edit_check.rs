use std::io::{IsTerminal, Read};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

const MAX_REPLACEMENT_BYTES: usize = 1_048_576;

pub(crate) fn run(
    file: &str,
    target: &str,
    replacement: Option<&Path>,
    line: Option<usize>,
    json: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let root = cwd
        .ancestors()
        .find(|path| path.join(".git").exists())
        .context("no Git worktree found; run edit-check inside a Git worktree")?;
    let replacement = match replacement {
        Some(path) if path.as_os_str() != "-" => {
            let input = std::fs::File::open(path)
                .with_context(|| format!("opening replacement from {}", path.display()))?;
            read_replacement(input)
                .with_context(|| format!("reading replacement from {}", path.display()))?
        }
        _ => {
            if std::io::stdin().is_terminal() {
                bail!(
                    "no replacement provided (pass a file path or pipe the declaration via stdin)"
                );
            }
            read_replacement(std::io::stdin().lock())?
        }
    };
    let report = codesage_graph::edit_check::edit_check(root, file, target, line, &replacement)?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
    println!(
        "{}:{} {} at HEAD {}",
        report.file, report.line, report.symbol, report.head
    );
    println!(
        "working file matches HEAD: {}",
        report.worktree_matches_head
    );
    println!("before: {}", serde_json::to_string(&report.before)?);
    println!("after: {}", serde_json::to_string(&report.after)?);
    println!(
        "changed: arity={}, visibility={}, overloads={}",
        report.arity_changed, report.visibility_changed, report.overloads_changed
    );
    if report.overloads_changed {
        println!(
            "overloads before: {}",
            serde_json::to_string(&report.overloads_before)?
        );
        println!(
            "overloads after: {}",
            serde_json::to_string(&report.overloads_after)?
        );
    }
    println!(
        "proven incompatible callers: {}{}",
        report.incompatible_callers.len(),
        if report.callers_truncated {
            "+ (truncated)"
        } else {
            ""
        }
    );
    for caller in &report.incompatible_callers {
        println!(
            "  {}:{} {}: {}",
            caller.file, caller.line, caller.expression, caller.reason
        );
    }
    if report.counts_floor {
        println!("caller counts are a lower bound");
    }
    for unknown in &report.unknown {
        println!("unknown: {unknown}");
    }
    println!("This is not a compilation or safety verdict.");
    Ok(())
}

fn read_replacement(input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .take((MAX_REPLACEMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("reading replacement")?;
    ensure!(
        bytes.len() <= MAX_REPLACEMENT_BYTES,
        "replacement exceeds 1 MiB"
    );
    let replacement = String::from_utf8(bytes).context("replacement must be UTF-8")?;
    ensure!(!replacement.trim().is_empty(), "replacement is empty");
    Ok(replacement)
}
