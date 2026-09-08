pub mod index {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum IndexStrategy {
        Full,
        Incremental,
    }
}
#[expect(dead_code)]
mod semantic;

use anyhow::Result;
use codesage_embed::{config::EmbeddingConfig, model::Embedder};
use codesage_parser::discover::discover_files_with_excludes;
use codesage_storage::Database;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Instant};

fn main() -> Result<()> {
    codesage_embed::model::init_for_main();
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 4,
        "usage: probe ROOT full|watcher|compare|growth-bound|failure-bound DATABASE"
    );
    anyhow::ensure!(
        matches!(
            args[2].as_str(),
            "full" | "watcher" | "compare" | "growth-bound" | "failure-bound"
        ),
        "unknown mode: {}",
        args[2]
    );
    let root = Path::new(&args[1]);
    if args[2] == "compare" {
        let model = "jinaai/jina-embeddings-v2-base-code";
        let before = Database::open_for_model_existing(root, model, 768)?;
        let after = Database::open_for_model_existing(Path::new(&args[3]), model, 768)?;
        let mut max_abs = 0f64;
        let mut min_cosine = 1f64;
        let mut changed = 0usize;
        let mut total = 0usize;
        let mut before_paths = before.all_chunk_file_paths()?;
        let mut after_paths = after.all_chunk_file_paths()?;
        before_paths.sort();
        after_paths.sort();
        anyhow::ensure!(before_paths == after_paths, "file path sets differ");
        for path in before_paths {
            let a = before.chunk_embeddings_for_file(&path)?;
            let b = after.chunk_embeddings_for_file(&path)?;
            anyhow::ensure!(a.len() == b.len(), "chunk count differs: {path}");
            for ((text_a, vec_a), (text_b, vec_b)) in a.iter().zip(&b) {
                anyhow::ensure!(
                    text_a == text_b && vec_a.len() == 768 && vec_b.len() == 768,
                    "chunk or dimension differs: {path}"
                );
                total += 1;
                changed += usize::from(vec_a != vec_b);
                let mut dot = 0f64;
                let mut norm_a = 0f64;
                let mut norm_b = 0f64;
                for (a, b) in vec_a.iter().zip(vec_b) {
                    let (a, b) = (f64::from(*a), f64::from(*b));
                    anyhow::ensure!(a.is_finite() && b.is_finite(), "non-finite vector: {path}");
                    max_abs = max_abs.max((a - b).abs());
                    dot += a * b;
                    norm_a += a * a;
                    norm_b += b * b;
                }
                anyhow::ensure!(norm_a > 0.0 && norm_b > 0.0, "zero vector: {path}");
                min_cosine = min_cosine.min(dot / (norm_a * norm_b).sqrt());
            }
        }
        anyhow::ensure!(total > 0, "no vectors compared");
        println!(
            "{}",
            json!({"vectors": total, "changed_vectors": changed,
            "max_abs_difference": max_abs, "min_cosine": min_cosine})
        );
        return Ok(());
    }
    if args[2] == "growth-bound" {
        println!(
            "{}",
            json!({"read_chunk_bytes": semantic::growth_probe(root)?})
        );
        return Ok(());
    }
    if args[2] == "failure-bound" {
        println!("{}", semantic::failure_probe(root, Path::new(&args[3]))?);
        return Ok(());
    }
    let config: EmbeddingConfig = serde_json::from_value(json!({
        "model": "jinaai/jina-embeddings-v2-base-code", "device": "gpu"
    }))?;
    let mut embedder = Embedder::new(&config)?;
    let db = Database::open_for_model(Path::new(&args[3]), &config.model, embedder.dim())?;
    let fingerprint = semantic::resolve_semantic_fingerprint(
        &db,
        &config,
        embedder.dim(),
        semantic::ArtifactLookup::Resolve,
    )?
    .unwrap();
    let started = Instant::now();
    let stats = if args[2] == "full" {
        semantic::semantic_full_index(root, &db, &mut embedder, &[], &fingerprint, false)?
    } else {
        let files = discover_files_with_excludes(root, &[])?;
        semantic::semantic_index_files(root, &db, &mut embedder, &files, &fingerprint, false)?
    };
    let elapsed = started.elapsed().as_secs_f64();
    let mut hash = Sha256::new();
    let mut chunk_hash = Sha256::new();
    let mut vector_hash = Sha256::new();
    let mut paths = db.all_chunk_file_paths()?;
    paths.sort();
    let mut chunks = 0;
    let mut chunk_bytes = 0;
    let mut vector_bytes = 0;
    for path in paths {
        hash.update(path.as_bytes());
        for row in db.chunks_for_file(&path)? {
            hash.update(format!("{row:?}"));
            chunk_hash.update(format!("{row:?}"));
        }
        for (text, vector) in db.chunk_embeddings_for_file(&path)? {
            chunks += 1;
            chunk_bytes += text.len();
            vector_bytes += vector.len() * 4;
            hash.update(text.as_bytes());
            for value in vector {
                hash.update(value.to_le_bytes());
                vector_hash.update(value.to_le_bytes());
            }
        }
    }
    let mut hashes: Vec<_> = db.all_semantic_file_hashes()?.into_iter().collect();
    hashes.sort();
    hash.update(serde_json::to_vec(&hashes)?);
    println!(
        "{}",
        json!({"mode": args[2], "seconds": elapsed,
        "provider": embedder.execution_provider(), "stats": stats,
        "chunks": chunks, "chunk_bytes": chunk_bytes, "vector_bytes": vector_bytes,
        "chunk_sha256": format!("{:x}", chunk_hash.finalize()),
        "vector_sha256": format!("{:x}", vector_hash.finalize()),
        "hashes_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&hashes)?)),
        "output_sha256": format!("{:x}", hash.finalize())})
    );
    Ok(())
}
