#[path = "search.rs"]
mod production;

use codesage_embed::{config::EmbeddingConfig, model::Embedder, reranker::Reranker};
use codesage_protocol::SearchRequest;
use codesage_storage::{Database, embedding_to_bytes};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, time::Instant};

fn main() -> anyhow::Result<()> {
    codesage_embed::model::init_for_main();
    let args: Vec<_> = std::env::args().collect();
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&args[1])?)?;
    let paths: HashMap<String, [String; 2]> =
        serde_json::from_str(&std::fs::read_to_string(&args[2])?)?;
    let config: EmbeddingConfig = serde_json::from_value(json!({
        "model": "jinaai/jina-embeddings-v2-base-code", "device": "gpu"
    }))?;
    let mut embedder = Embedder::new(&config)?;
    anyhow::ensure!(embedder.execution_provider() == "cuda", "CUDA required");
    eprintln!("Embedding execution provider: cuda");
    let mut reranker = Reranker::new("cross-encoder/ms-marco-MiniLM-L6-v2", "gpu")?;
    let mut dbs = HashMap::new();
    for (name, pair) in paths {
        dbs.insert(
            name,
            [
                Database::open_for_model_existing(
                    Path::new(&pair[0]),
                    &config.model,
                    embedder.dim(),
                )?,
                Database::open_for_model_existing(
                    Path::new(&pair[1]),
                    &config.model,
                    embedder.dim(),
                )?,
            ],
        );
    }
    for (i, case) in cases.into_iter().enumerate() {
        let query = case["query"].as_str().unwrap();
        let db = &dbs[case["project"].as_str().unwrap()];
        let req: SearchRequest = serde_json::from_value(json!({"query": query, "limit": 10}))?;
        let embedding = embedder.embed_one(query)?;
        let bytes = embedding_to_bytes(&embedding);
        anyhow::ensure!(
            format!("{:?}", db[0].search_knn(&bytes, 50, None)?)
                == format!("{:?}", db[1].search_knn(&bytes, 50, None)?),
            "semantic candidates changed"
        );
        let mut results = [Value::Null, Value::Null];
        let mut millis = [0.0, 0.0];
        for arm in [i % 2, 1 - i % 2] {
            let started = Instant::now();
            results[arm] = serde_json::to_value(production::search(
                &db[arm],
                &embedding,
                Some(Box::new(|q, docs| reranker.score_pairs(q, docs))),
                &req,
            )?)?;
            millis[arm] = started.elapsed().as_secs_f64() * 1000.0;
        }
        println!(
            "{}",
            json!({"case": case, "baseline": results[0],
            "candidate": results[1], "search_ms": millis, "knn_identical": true})
        );
    }
    Ok(())
}
