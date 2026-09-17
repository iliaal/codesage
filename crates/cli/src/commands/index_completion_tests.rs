use super::*;
use std::process::Command;

struct Fixture {
    dir: tempfile::TempDir,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(PROJECT_DIR)).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let head = codesage_graph::drift::git_head_sha(dir.path()).unwrap();
        let fixture = Self { dir, head };
        fixture.write(
            "Cargo.toml",
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\n",
        );
        fixture.write("src/lib.rs", "pub fn healthy() {}\n");
        fixture
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.root().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    fn db(&self) -> Database {
        open_db_for_model(
            self.root(),
            &EmbeddingConfig::default().model,
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
        )
        .unwrap()
    }

    fn run(
        &self,
        full: bool,
        mapper: Option<FeatureMapper>,
        embedder: Option<TestEmbedder>,
    ) -> Result<()> {
        run_index_passes(
            self.root().to_path_buf(),
            self.db(),
            vec![],
            full,
            mapper,
            false,
            embedder.map(|embedder| (Box::new(embedder) as Box<dyn TextEmbedder>, fingerprint())),
        )
    }
}

fn fingerprint() -> SemanticFingerprint {
    SemanticFingerprint::with_artifact_digest(
        &EmbeddingConfig::default(),
        codesage_storage::db::DEFAULT_EMBEDDING_DIM,
        "index-completion-test",
    )
}

struct TestEmbedder {
    fail_read: Option<PathBuf>,
}

impl TextEmbedder for TestEmbedder {
    fn prepare(&mut self, _: usize) -> Result<()> {
        // Discovery already hashed this file. Model preparation is the external
        // boundary at which a transient read failure can occur before chunking.
        if let Some(path) = self.fail_read.take() {
            std::fs::remove_file(&path)?;
            std::fs::create_dir(&path)?;
        }
        Ok(())
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }
}

#[test]
fn partial_structural_pass_retains_writes_without_advancing_state_and_retries() {
    let fixture = Fixture::new();
    fixture.write("guard.rs", "fn blocked() {}\n");
    fixture.write("retry.rs", "fn original() {}\n");
    fixture.run(false, None, None).unwrap();
    fixture
        .db()
        .set_structural_index_state("previous-head")
        .unwrap();
    fixture.write("retry.rs", "fn blocked() {}\n");
    fixture.write("src/lib.rs", "pub fn updated() {}\n");
    let conn = rusqlite::Connection::open(db_path(fixture.root())).unwrap();
    conn.execute_batch(
        "CREATE UNIQUE INDEX transient_rejection ON symbols(name) WHERE name = 'blocked'",
    )
    .unwrap();

    let result = fixture.run(
        false,
        Some(codesage_features::map_features_detailed),
        Some(TestEmbedder { fail_read: None }),
    );
    assert_eq!(crate::exit_code_for(&result), 1);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("structural: 1 failed files")
    );
    let db = fixture.db();
    assert_eq!(
        db.get_structural_index_state().unwrap().unwrap().0,
        "previous-head"
    );
    assert_eq!(db.symbols_for_file("retry.rs").unwrap()[0].name, "original");
    assert_eq!(
        db.symbols_for_file("src/lib.rs").unwrap()[0].name,
        "updated"
    );
    assert!(
        db.all_semantic_file_hashes()
            .unwrap()
            .contains_key("src/lib.rs")
    );
    assert!(read_feature_map_state(fixture.root()).is_some());
    drop(db);

    // Clear the failure without changing source content. The failed file must
    // not have acquired a fresh structural hash during the partial pass.
    conn.execute_batch("DROP INDEX transient_rejection")
        .unwrap();
    fixture
        .run(false, Some(codesage_features::map_features_detailed), None)
        .unwrap();
    let db = fixture.db();
    assert_eq!(db.symbols_for_file("retry.rs").unwrap()[0].name, "blocked");
    assert_eq!(
        db.get_structural_index_state().unwrap().unwrap().0,
        fixture.head
    );
}

#[test]
fn partial_semantic_pass_returns_failure_and_retries_identical_bytes() {
    let fixture = Fixture::new();
    let source = "fn retry() {}\n";
    fixture.write("retry.rs", source);
    let result = fixture.run(
        true,
        Some(codesage_features::map_features_detailed),
        Some(TestEmbedder {
            fail_read: Some(fixture.root().join("retry.rs")),
        }),
    );
    assert_eq!(crate::exit_code_for(&result), 1);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("semantic: 1 failed files")
    );
    let db = fixture.db();
    assert_eq!(
        db.get_structural_index_state().unwrap().unwrap().0,
        fixture.head
    );
    assert_eq!(db.symbols_for_file("retry.rs").unwrap()[0].name, "retry");
    let hashes = db.all_semantic_file_hashes().unwrap();
    assert!(hashes.contains_key("src/lib.rs"));
    assert!(!hashes.contains_key("retry.rs"));
    drop(db);

    std::fs::remove_dir(fixture.root().join("retry.rs")).unwrap();
    fixture.write("retry.rs", source);
    fixture
        .run(
            false,
            Some(codesage_features::map_features_detailed),
            Some(TestEmbedder { fail_read: None }),
        )
        .unwrap();
    let db = fixture.db();
    assert_eq!(
        db.all_semantic_file_hashes().unwrap()["retry.rs"],
        codesage_parser::discover::content_hash(source.as_bytes())
    );
    assert!(!db.chunk_embeddings_for_file("retry.rs").unwrap().is_empty());
}

fn partial_mapper(
    root: &Path,
    db: &Database,
    excludes: &[String],
) -> Result<codesage_features::FeatureMapOutcome> {
    let mut outcome = codesage_features::map_features_detailed(root, db, excludes)?;
    // Simulate one failed mapper at the orchestration boundary, retaining the
    // real successful mappers' writes. No production failure switch is needed.
    if root.join(PROJECT_DIR).join("mapper-failure").exists() {
        outcome
            .mapper_errors
            .push("transient mapper failure".into());
    }
    Ok(outcome)
}

#[test]
fn partial_mapper_pass_retains_independent_work_and_retries_without_source_changes() {
    let fixture = Fixture::new();
    fixture.run(false, Some(partial_mapper), None).unwrap();
    assert!(read_feature_map_state(fixture.root()).is_some());
    fixture.write(".codesage/mapper-failure", "");
    let result = fixture.run(
        true,
        Some(partial_mapper),
        Some(TestEmbedder { fail_read: None }),
    );
    assert_eq!(crate::exit_code_for(&result), 1);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("feature mapping: 1 failed mappers")
    );
    assert_eq!(read_feature_map_state(fixture.root()), None);
    let db = fixture.db();
    assert_eq!(
        db.get_structural_index_state().unwrap().unwrap().0,
        fixture.head
    );
    assert_eq!(
        db.symbols_for_file("src/lib.rs").unwrap()[0].name,
        "healthy"
    );
    assert!(
        db.all_semantic_file_hashes()
            .unwrap()
            .contains_key("src/lib.rs")
    );
    assert!(
        db.list_features(None, None, None, 0)
            .unwrap()
            .iter()
            .any(|feature| feature.entry_path == "src/lib.rs")
    );
    drop(db);

    std::fs::remove_file(fixture.root().join(PROJECT_DIR).join("mapper-failure")).unwrap();
    let result = fixture.run(
        false,
        Some(partial_mapper),
        Some(TestEmbedder { fail_read: None }),
    );
    assert_eq!(crate::exit_code_for(&result), 0);
    result.unwrap();
    assert_eq!(
        read_feature_map_state(fixture.root()),
        feature_map_fingerprint(&fixture.db(), fixture.root(), &[])
    );
}

#[cfg(unix)]
#[test]
fn hook_retries_unchanged_content_after_incomplete_index_exit() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let fixture = Fixture::new();
    let root = fixture.root();
    let stub = root.join(PROJECT_DIR).join("index-stub");
    fixture.write(".codesage/index-stub", "#!/bin/sh\necho \"$1\" >> .codesage/passes\nif [ \"$1\" = index ] && [ -f .codesage/failure ]; then exit 1; fi\nexit 0\n");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hook = root.join(".git/hooks/post-commit");
    std::fs::write(
        &hook,
        crate::commands::hooks::generate_post_commit_hook_body(stub.to_str().unwrap()),
    )
    .unwrap();
    fixture.write(".codesage/failure", "");
    let fire = |runs: usize| {
        assert!(
            Command::new("sh")
                .arg(&hook)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = std::fs::read_to_string(root.join(".codesage/hooks.log")).unwrap_or_default();
            if log.matches("hook exit=").count() == runs
                && !root.join(".codesage/hook-index.lock").exists()
            {
                return log;
            }
            assert!(Instant::now() < deadline, "hook failed to finish: {log}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    let failed_log = fire(1);
    assert!(failed_log.contains("] index exit=1"));
    assert!(failed_log.contains("] git-index exit=0"));
    assert!(!root.join(".codesage/hook-state").exists());
    // The hook excludes .codesage from its digest, so clearing this failure
    // leaves HEAD and content digest identical across both invocations.
    std::fs::remove_file(root.join(".codesage/failure")).unwrap();
    fire(2);
    assert!(root.join(".codesage/hook-state").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join(".codesage/passes")).unwrap(),
        "index\ngit-index\nindex\ngit-index\n"
    );
    assert!(
        Command::new("sh")
            .arg(&hook)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".codesage/passes")).unwrap(),
        "index\ngit-index\nindex\ngit-index\n"
    );
}
