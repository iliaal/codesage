#[path = "qualified-baseline.rs"]
mod baseline;
#[path = "qualified-candidate.rs"]
mod candidate;

use codesage_embed::{config::EmbeddingConfig, model::Embedder, reranker::Reranker};
use codesage_protocol::SearchRequest;
use codesage_storage::Database;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};

fn main() -> anyhow::Result<()> {
    codesage_embed::model::init_for_main();
    let args: Vec<_> = std::env::args().collect();
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&args[1])?)?;
    let projects: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(&args[2])?)?;
    let config: EmbeddingConfig = serde_json::from_value(json!({
        "model": "jinaai/jina-embeddings-v2-base-code", "device": "gpu"
    }))?;
    let mut embedder = Embedder::new(&config)?;
    eprintln!(
        "Embedding execution provider: {}",
        embedder.execution_provider()
    );
    let mut reranker = Reranker::new("cross-encoder/ms-marco-MiniLM-L6-v2", "gpu")?;
    let mut dbs = HashMap::new();
    for (name, path) in projects {
        dbs.insert(
            name,
            Database::open_for_model_existing(Path::new(&path), &config.model, embedder.dim())?,
        );
    }
    for case in cases {
        let query = case["query"].as_str().unwrap();
        let db = &dbs[case["project"].as_str().unwrap()];
        let req: SearchRequest = serde_json::from_value(json!({"query": query, "limit": 10}))?;
        let embedding = embedder.embed_one(query)?;
        let before = baseline::search(
            db,
            &embedding,
            Some(Box::new(|q, docs| reranker.score_pairs(q, docs))),
            &req,
        )?;
        let after = candidate::search(
            db,
            &embedding,
            Some(Box::new(|q, docs| reranker.score_pairs(q, docs))),
            &req,
        )?;
        println!(
            "{}",
            json!({"case": case, "baseline": before, "candidate": after})
        );
    }
    Ok(())
}
