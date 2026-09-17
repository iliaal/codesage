use codesage_graph::search;
use codesage_protocol::{Language, SearchRequest};
use codesage_storage::{Database, db::DEFAULT_EMBEDDING_DIM};

#[test]
fn search_stem_candidates_respect_restrictions_and_pagination() {
    let db = Database::open_in_memory().unwrap();
    let near = vec![0.0; DEFAULT_EMBEDDING_DIM];
    let mut far = near.clone();
    far[0] = 1.0;
    for (path, language, content, embedding) in [
        ("src/a.rs", "rust", "fn entry_a() {}", &near),
        ("src/b.rs", "rust", "fn entry_b() {}", &near),
        ("src/foo_bar.rs", "rust", "struct FooBar;", &far),
        (
            "src/nested/foo-bar.ts",
            "typescript",
            "class FooBar {}",
            &far,
        ),
        ("vendor/FooBar.rs", "rust", "struct FooBar;", &far),
        ("foreign/FooBar.py", "python", "class FooBar: pass", &far),
    ] {
        db.insert_chunks(path, language, &[(content, 1, 1, embedding.as_slice())])
            .unwrap();
    }
    // A short symbol avoids the rare-token BM25 gate. With limit=1 only a
    // near entry is retrieved, so finding a definition proves stem recovery.
    let mut request = SearchRequest {
        query: "FooBar".into(),
        limit: Some(1),
        offset: None,
        languages: None,
        paths: None,
        adaptive_limit: false,
        explain: true,
    };
    let unrestricted = search(&db, &near, None, &request).unwrap();
    assert_eq!(unrestricted.len(), 1);
    assert!(unrestricted[0].content.contains("FooBar"));
    assert_eq!(
        unrestricted[0].trace.as_ref().unwrap()[0].stage,
        "stem_scan"
    );

    for (filter_language, filter_path, expected_count) in
        [(true, false, 4), (false, true, 4), (true, true, 3)]
    {
        request.languages = filter_language.then(|| vec![Language::Rust]);
        // '*' must retain the existing separator-crossing glob semantics.
        request.paths = filter_path.then(|| vec!["src/*".into()]);
        for (limit, offset) in [(1, 0), (1, 1), (10, 0)] {
            request.limit = Some(limit);
            request.offset = Some(offset);
            let results = search(&db, &near, None, &request).unwrap();
            assert_eq!(results.len(), if limit == 10 { expected_count } else { 1 });
            for result in results {
                assert!(
                    !filter_language || result.language == Language::Rust,
                    "excluded language in {result:?}"
                );
                assert!(
                    !filter_path || result.file_path.starts_with("src/"),
                    "excluded path in {result:?}"
                );
            }
        }
    }

    request.limit = Some(10);
    request.offset = None;
    request.languages = None;
    request.paths = Some(vec![]);
    assert!(search(&db, &near, None, &request).unwrap().is_empty());
    request.paths = None;
    request.languages = Some(vec![]);
    assert!(search(&db, &near, None, &request).unwrap().is_empty());
}
