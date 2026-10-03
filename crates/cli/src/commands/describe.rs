use anyhow::{Context, Result};
use codesage_graph::DescribeOptions;
use codesage_protocol::DescribeDetail;

pub(crate) fn run(
    target: &str,
    detail: &str,
    sections: Option<Vec<String>>,
    json: bool,
) -> Result<()> {
    let root = crate::find_project_root()?;
    let db = crate::open_db_read_only(&root)?;
    let _snapshot = db.read_snapshot()?;
    let options = DescribeOptions {
        detail: DescribeDetail::parse(detail)
            .context("detail must be compact, standard, or full")?,
        sections,
        ..DescribeOptions::default()
    };
    let result = codesage_graph::describe(&root, &db, target, &options)?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else if let Some(card) = &result.card {
        println!("{} ({:?})", card.handle, card.kind);
        for (name, section) in &card.sections {
            println!("{name}: {}", serde_json::to_string(&section.data)?);
            if let Some(incomplete) = &section.completeness {
                println!(
                    "  {}: {}; recover {} {}",
                    incomplete.kind,
                    incomplete.reason,
                    incomplete.recover.tool,
                    serde_json::to_string(&incomplete.recover.arguments)?
                );
            }
        }
        println!("{} ms, {} bytes", result.cost.ms, result.cost.bytes);
    } else if let Some(target) = &result.target {
        println!(
            "{} candidates for {:?}",
            target.candidates_total, target.input
        );
        for candidate in &target.resolved {
            println!("{}", candidate.handle);
        }
    }
    Ok(())
}
