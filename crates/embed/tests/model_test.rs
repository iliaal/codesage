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

#[test]
#[ignore]
fn reranker_run_stops_when_its_work_control_is_cancelled() {
    use codesage_embed::reranker::Reranker;
    use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};
    use std::time::{Duration, Instant};

    let mut reranker = Reranker::new("cross-encoder/ms-marco-MiniLM-L6-v2", "cpu")
        .expect("failed to create reranker");
    let query = "where is the session token validated";
    let short = [
        "fn validate_session_token(token: &str) -> Result<Claims>",
        "fn open_connection_pool(url: &str) -> Pool",
    ];
    let plain = reranker.score_pairs(query, &short).unwrap();

    let control = WorkControl::new(None);
    let scoped = {
        let _scope = control.enter();
        reranker.score_pairs(query, &short).unwrap()
    };
    assert_eq!(
        plain, scoped,
        "an uncancelled control must not change scores"
    );

    // One 32-row batch truncated to 512 tokens: a single long native run.
    let long_doc = "fn handle(request: &Request) -> Response { route(request) } ".repeat(120);
    let docs = vec![long_doc.as_str(); 32];
    let started = Instant::now();
    reranker.score_pairs(query, &docs).unwrap();
    let full = started.elapsed();

    let control = WorkControl::new(None);
    let canceller = control.clone();
    let half = full / 2;
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(half);
        canceller.cancel(StopReason::ClientCancelled);
    });
    let started = Instant::now();
    let error = {
        let _scope = control.enter();
        reranker.score_pairs(query, &docs).unwrap_err()
    };
    let stopped_after = started.elapsed();
    cancel.join().unwrap();
    eprintln!("full run {full:?}, cancelled at {half:?}, returned after {stopped_after:?}");
    assert_eq!(
        error.downcast_ref::<WorkStopped>().unwrap().reason,
        StopReason::ClientCancelled,
        "{error:#}"
    );
    assert!(
        stopped_after < half + (full - half) / 2 && full > Duration::from_millis(200),
        "a terminated run must return well before a full run: {stopped_after:?} of {full:?}"
    );

    let after = reranker.score_pairs(query, &short).unwrap();
    assert_eq!(
        plain, after,
        "the session must score normally after a terminated run"
    );
}
