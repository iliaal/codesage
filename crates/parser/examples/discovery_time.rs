use codesage_parser::discover::discover_files_report_with_context;
use std::{collections::HashMap, path::Path, time::Instant};
fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).unwrap();
    let start = Instant::now();
    let first = discover_files_report_with_context(Path::new(&root), &[], &HashMap::new(), &[])?;
    println!(
        "cold_ms={} files={} bytes_hashed={}",
        start.elapsed().as_millis(),
        first.files.len(),
        first.bytes_hashed
    );
    for file in &first.files {
        if file.path.ends_with(".h") {
            println!("HEADER\t{}\t{}", file.path, file.language);
        }
    }
    let start = Instant::now();
    let second =
        discover_files_report_with_context(Path::new(&root), &[], &first.hash_cache, &first.files)?;
    println!(
        "warm_ms={} files={} bytes_hashed={} hashes_reused={}",
        start.elapsed().as_millis(),
        second.files.len(),
        second.bytes_hashed,
        second.hashes_reused
    );
    assert_eq!(first.files, second.files);
    Ok(())
}
