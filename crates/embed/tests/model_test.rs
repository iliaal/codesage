use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
#[ignore]
fn embedding_similarity_ordering() {
    let config = EmbeddingConfig::default();
    let mut embedder = Embedder::new(&config).expect("failed to create embedder");

    let e1 = embedder
        .embed_one("function that handles user authentication")
        .unwrap();
    let e2 = embedder
        .embed_one("login and password verification logic")
        .unwrap();
    let e3 = embedder
        .embed_one("database connection pooling configuration")
        .unwrap();

    assert_eq!(e1.len(), 384);

    let sim_related = cosine_sim(&e1, &e2);
    let sim_unrelated = cosine_sim(&e1, &e3);

    eprintln!("auth vs login: {sim_related:.4}");
    eprintln!("auth vs database: {sim_unrelated:.4}");

    assert!(
        sim_related > sim_unrelated,
        "expected similar sentences to have higher similarity: {sim_related} vs {sim_unrelated}"
    );
}

#[test]
#[ignore]
fn batch_embedding() {
    let config = EmbeddingConfig::default();
    let mut embedder = Embedder::new(&config).expect("failed to create embedder");

    let texts = vec!["hello world", "foo bar baz", "test embedding"];
    let results = embedder.embed_batch(&texts).unwrap();

    assert_eq!(results.len(), 3);
    for emb in &results {
        assert_eq!(emb.len(), 384);
        let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "expected unit vector, got norm={norm}"
        );
    }
}
