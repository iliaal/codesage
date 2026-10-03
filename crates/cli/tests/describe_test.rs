use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use codesage_protocol::{
    FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, Handle,
    Language,
};
use codesage_storage::Database;
use serde_json::{Value, json};

struct Server {
    child: Child,
    responses: Receiver<String>,
    id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start() -> Self {
        Self::start_with_envelope(None)
    }

    fn start_with_envelope(envelope: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
        command
            .args(["mcp", "--direct"])
            .env("CODESAGE_WATCH", "0")
            .env_remove("CODESAGE_MCP_TEST_QUERY_EMBEDDING")
            .env_remove("CODESAGE_ENVELOPE");
        if let Some(envelope) = envelope {
            command.env("CODESAGE_ENVELOPE", envelope);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            responses,
            id: 0,
        };
        let initialized = server.request("initialize", json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "describe-test", "version": "1"}}));
        assert_eq!(initialized["serverInfo"]["name"], "codesage");
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        )
        .unwrap();
        loop {
            let response: Value = serde_json::from_str(
                &self
                    .responses
                    .recv_timeout(Duration::from_secs(30))
                    .expect("MCP response within 30 seconds"),
            )
            .unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

fn cli(root: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(arguments)
        .current_dir(root)
        .env("CODESAGE_WATCH", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "Cargo.toml",
            "[package]\nname = \"describe_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod helper;\nuse crate::helper::leaf;\npub fn caller() { leaf(); }\n",
        ),
        (
            "src/helper.rs",
            "// WHY: keep fixture rationale\npub fn leaf() { target(); }\npub fn target() {}\n#[cfg(test)]\nmod tests { use super::leaf; #[test] fn test_leaf() { leaf(); } }\n",
        ),
        ("src/a.rs", "pub fn duplicate() {}\n"),
        ("src/b.rs", "pub fn duplicate() {}\n"),
        ("single/one.rs", "pub fn only() {}\n"),
        ("single/tests/only_test.rs", "#[test] fn only_test() {}\n"),
    ] {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    cli(root.path(), &["init"]);
    cli(root.path(), &["index", "--no-semantic"]);
    let db = Database::open(&root.path().join(".codesage/index.db")).unwrap();
    db.upsert_feature(&FeatureRecord {
        feature_id: "feat_1111111111111111".into(),
        title: "Fixture".into(),
        summary: "Fixture functions".into(),
        kind: FeatureKind::Library,
        source: "cargo-lib".into(),
        confidence: FeatureConfidence::High,
        entry_path: "src/lib.rs".into(),
        entry_symbol: None,
        entry_route: None,
        entry_command: None,
        test_command: Some("cargo test -p describe_fixture".into()),
        language: Language::Rust,
        tags: vec![],
        trust_boundaries: vec![],
        files: vec![
            FeatureFileRef {
                path: "src/lib.rs".into(),
                role: FeatureFileRole::Entry,
                reason: None,
            },
            FeatureFileRef {
                path: "src/helper.rs".into(),
                role: FeatureFileRole::Owned,
                reason: None,
            },
        ],
    })
    .unwrap();
    root
}

#[test]
fn describe_stdio_clones_disclose_actual_seeds_for_an_exact_symbol_handle() {
    let first = "pub fn subject(a: i32) -> i32 { if a > 0 { a + 1 } else { a - 1 } }\n";
    let fingerprinted_first = "pub fn subject(a: i32) -> i32 { let doubled = a * 2; let adjusted = doubled + 17; let squared = adjusted * adjusted; if squared > 100 { squared - 21 } else if squared > 50 { squared + 22 } else { squared % 23 } }\n";
    let second = "pub fn subject(a: &[i32]) -> i32 {\n    let mut sum = 0;\n    for value in a {\n        match value {\n            0 => sum += 100,\n            1 => sum += 200,\n            _ => sum += value * value,\n        }\n    }\n    while sum > 10000 { sum /= 2; }\n    sum\n}\n";
    let mut server = Server::start();
    for (first, target_seeded, include_other) in [
        (first, false, true),
        (first, false, false),
        (fingerprinted_first, true, true),
        (fingerprinted_first, true, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), first).unwrap();
        std::fs::write(
            root.path().join("c.rs"),
            second.replace("fn subject", "fn second_clone"),
        )
        .unwrap();
        if include_other {
            std::fs::write(root.path().join("b.rs"), second).unwrap();
        }
        cli(root.path(), &["init"]);
        cli(root.path(), &["index", "--no-semantic"]);
        let target = "sym:a.rs#subject";
        let definition = server.call(
            "find_symbol",
            json!({"project": root.path(), "target": target}),
        );
        assert_ne!(definition["isError"], true, "{definition}");
        assert_eq!(
            definition["structuredContent"]["target"]["ambiguous"],
            false
        );
        assert_eq!(
            definition["structuredContent"]["target"]["candidates_total"],
            1
        );
        let mut expected_seeds = Vec::new();
        if target_seeded {
            expected_seeds.push("sym:a.rs#subject");
        }
        if include_other {
            expected_seeds.push("sym:b.rs#subject");
        }
        let db = Database::open_read_only(&root.path().join(".codesage/index.db")).unwrap();
        assert_eq!(
            db.fingerprints_named("subject").unwrap().len(),
            expected_seeds.len()
        );
        for detail in ["compact", "full"] {
            let response = server.call("describe", json!({"project": root.path(), "target": target, "detail": detail, "sections": ["clones"]}));
            assert_ne!(response["isError"], true, "{response}");
            let card = &response["structuredContent"]["card"];
            let section = &card["sections"]["clones"];
            assert!(section.get("completeness").is_none(), "{section}");
            let data = &section["data"];
            assert_eq!(data["name_union"], include_other);
            assert_eq!(data["target_seeded"], target_seeded);
            assert_eq!(data["seed_scope"], "bare_name");
            assert_eq!(data["seed_total"], expected_seeds.len());
            let sampled = if detail == "compact" {
                &expected_seeds[..expected_seeds.len().min(1)]
            } else {
                &expected_seeds[..]
            };
            assert_eq!(
                data["seeds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["handle"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                sampled
            );
            for row in data["seeds"].as_array().unwrap() {
                assert_eq!(row["line"], 1);
                assert!(row["file"].as_str().unwrap().starts_with("file:"));
            }
            let top = data["top"].as_array().unwrap();
            if include_other {
                assert_eq!(top.len(), 1);
                assert_eq!(top[0]["handle"], "sym:c.rs#second_clone");
                assert_eq!(top[0]["jaccard"], 1.0);
            } else {
                assert!(top.is_empty(), "{top:?}");
            }
            let command: Value = serde_json::from_slice(&cli(
                root.path(),
                &[
                    "describe",
                    target,
                    "--detail",
                    detail,
                    "--sections",
                    "clones",
                    "--json",
                ],
            ))
            .unwrap();
            assert_eq!(command["card"], *card);
            let expand = &section["expand"];
            let expanded = server.call(
                expand["tool"].as_str().unwrap(),
                expand["arguments"].clone(),
            );
            assert_ne!(expanded["isError"], true, "{expanded}");
            let payload = &expanded["structuredContent"];
            assert_eq!(payload["target"]["ambiguous"], false);
            assert_eq!(payload["target"]["candidates_total"], 1);
            assert_eq!(payload["results"].as_array().unwrap().len(), top.len());
            if include_other {
                assert_eq!(payload["results"][0]["file_path"], "c.rs");
                assert_eq!(payload["results"][0]["name"], "second_clone");
                assert_eq!(payload["results"][0]["jaccard"], top[0]["jaccard"]);
            }
        }
    }
}

#[test]
fn describe_stdio_wrong_case_directory_is_a_miss_and_exact_retry_expands() {
    let root = fixture();
    let mut server = Server::start();
    for target in ["dir:SRC", "dir:absent", "dir:src-other"] {
        let response = server.call(
            "describe",
            json!({"project": root.path(), "target": target}),
        );
        assert_eq!(response["isError"], true, "{response}");
        let failure = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["text"].as_str())
            .filter_map(|text| serde_json::from_str::<Value>(text).ok())
            .find(|value| value.get("error").is_some())
            .unwrap();
        assert_eq!(failure["error"]["code"], "E_NOT_FOUND");
        assert!(response["structuredContent"].get("card").is_none());
        let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["describe", target, "--json"])
            .current_dir(root.path())
            .env("CODESAGE_WATCH", "0")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(target));
    }
    let response = server.call("describe", json!({"project": root.path(), "target": "dir:src", "detail": "full", "sections": ["module_map"]}));
    assert_ne!(response["isError"], true, "{response}");
    let card = &response["structuredContent"]["card"];
    assert_eq!(card["sections"]["module_map"]["data"]["files"], 4);
    let command: Value = serde_json::from_slice(&cli(
        root.path(),
        &[
            "describe",
            "dir:src",
            "--detail",
            "full",
            "--sections",
            "module_map",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(command["card"], *card);
    let expand = &card["sections"]["module_map"]["expand"];
    let expanded = server.call(
        expand["tool"].as_str().unwrap(),
        expand["arguments"].clone(),
    );
    assert_ne!(expanded["isError"], true, "{expanded}");
    assert_eq!(expanded["structuredContent"]["card"], *card);
}

#[test]
fn describe_stdio_directory_consumers_use_literal_component_ancestry() {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("src/a.rs", "pub fn inside() {}\n"),
        ("src/nested/c.rs", "pub fn nested() {}\n"),
        (
            "src-other/b.rs",
            "pub fn outside() { outside(); outside(); outside(); }\n",
        ),
        ("SRC/case.rs", "pub fn uppercase() {}\n"),
        ("literal_/a.rs", "pub fn under() {}\n"),
        ("literal_-other/b.rs", "pub fn under_sibling() {}\n"),
        ("literalX/b.rs", "pub fn under_wildcard() {}\n"),
        ("literal%/a.rs", "pub fn percent() {}\n"),
        ("literal%-other/b.rs", "pub fn percent_sibling() {}\n"),
        ("literalXYZ/b.rs", "pub fn percent_wildcard() {}\n"),
    ] {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    cli(root.path(), &["init"]);
    cli(root.path(), &["index", "--no-semantic"]);
    let db = Database::open(&root.path().join(".codesage/index.db")).unwrap();
    for path in db.all_file_paths().unwrap() {
        let outside = path.ends_with("b.rs") || path.starts_with("SRC/");
        db.upsert_git_file(
            &path,
            if outside { 100.0 } else { 1.0 },
            u32::from(outside) * 5,
            10,
            Some(1_700_000_000),
        )
        .unwrap();
    }
    let mut feature = FeatureRecord {
        feature_id: "feat_ffffffffffffffff".into(),
        title: "Directory boundary fixture".into(),
        summary: "Directory-owned functions".into(),
        kind: FeatureKind::Library,
        source: "fixture".into(),
        confidence: FeatureConfidence::High,
        entry_path: "src-other".into(),
        entry_symbol: None,
        entry_route: None,
        entry_command: None,
        test_command: None,
        language: Language::Rust,
        tags: vec![],
        trust_boundaries: vec![],
        files: vec![FeatureFileRef {
            path: "src-other".into(),
            role: FeatureFileRole::Entry,
            reason: None,
        }],
    };
    db.upsert_feature(&feature).unwrap();
    let outside: Value =
        serde_json::from_slice(&cli(root.path(), &["risk", "src-other/b.rs", "--json"])).unwrap();
    let mut server = Server::start();
    for (index, (scope, paths, modules)) in [
        (
            "src",
            vec!["src/a.rs", "src/nested/c.rs"],
            vec!["src", "src/nested"],
        ),
        ("literal_", vec!["literal_/a.rs"], vec!["literal_"]),
        ("literal%", vec!["literal%/a.rs"], vec!["literal%"]),
    ]
    .into_iter()
    .enumerate()
    {
        feature.feature_id = format!("feat_{:016x}", index + 1);
        feature.entry_path = scope.into();
        feature.files[0].path = scope.into();
        db.upsert_feature(&feature).unwrap();
        let scores = paths
            .iter()
            .map(|path| {
                let risk: Value =
                    serde_json::from_slice(&cli(root.path(), &["risk", path, "--json"])).unwrap();
                assert_ne!(risk["unscored"], true, "{risk}");
                risk["score"].as_f64().unwrap()
            })
            .collect::<Vec<_>>();
        let max = scores.iter().copied().fold(0.0, f64::max);
        let mean = scores.iter().sum::<f64>() / scores.len() as f64;
        assert!(outside["score"].as_f64().unwrap() > max, "{outside}");
        let target = Handle::dir(scope).unwrap().to_string();
        let response = server.call("describe", json!({"project": root.path(), "target": target, "detail": "full", "sections": ["module_map", "fan_in", "features", "risk"]}));
        assert_ne!(response["isError"], true, "{response}");
        let card = &response["structuredContent"]["card"];
        let command: Value = serde_json::from_slice(&cli(
            root.path(),
            &[
                "describe",
                &target,
                "--detail",
                "full",
                "--sections",
                "module_map,fan_in,features,risk",
                "--json",
            ],
        ))
        .unwrap();
        assert_eq!(command["card"], *card);
        let sections = &card["sections"];
        let module_map = &sections["module_map"]["data"];
        assert_eq!(module_map["files"], paths.len());
        assert_eq!(module_map["lines"], paths.len());
        assert_eq!(module_map["symbols"]["total"], paths.len());
        assert_eq!(module_map["modules_total"], modules.len());
        let actual_modules = module_map["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["handle"].as_str().unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            actual_modules,
            modules
                .iter()
                .map(|path| Handle::dir(*path).unwrap().to_string())
                .collect()
        );
        let fan_in = sections["fan_in"]["data"]["top"].as_array().unwrap();
        assert_eq!(
            fan_in
                .iter()
                .map(|row| row["handle"].as_str().unwrap().to_string())
                .collect::<std::collections::BTreeSet<_>>(),
            paths
                .iter()
                .map(|path| Handle::file(*path).unwrap().to_string())
                .collect()
        );
        assert!(fan_in.iter().all(|row| row["fan_in"] == 0));
        assert_eq!(sections["features"]["data"]["total"], 1);
        assert_eq!(
            sections["features"]["data"]["top"],
            json!([feature.feature_id])
        );
        let expected_risk = json!({"files": paths.len(), "scored": paths.len(), "unmeasured_files": 0, "max": max, "mean": mean});
        for (key, value) in expected_risk.as_object().unwrap() {
            assert_eq!(sections["risk"]["data"][key], *value);
        }
        for section in sections.as_object().unwrap().values() {
            assert!(section.get("completeness").is_none(), "{section}");
            let expand = &section["expand"];
            let expanded = server.call(
                expand["tool"].as_str().unwrap(),
                expand["arguments"].clone(),
            );
            assert_ne!(expanded["isError"], true, "{expanded}");
            let name = expand["arguments"]["sections"][0].as_str().unwrap();
            assert_eq!(
                expanded["structuredContent"]["card"]["sections"][name]["data"],
                section["data"]
            );
        }
        let owned = server.call("describe", json!({"project": root.path(), "target": feature.feature_id, "detail": "full", "sections": ["risk"]}));
        assert_ne!(owned["isError"], true, "{owned}");
        let owned_risk = &owned["structuredContent"]["card"]["sections"]["risk"];
        assert!(owned_risk.get("completeness").is_none(), "{owned_risk}");
        assert_eq!(owned_risk["data"], sections["risk"]["data"]);
        let command: Value = serde_json::from_slice(&cli(
            root.path(),
            &[
                "describe",
                &feature.feature_id,
                "--detail",
                "full",
                "--sections",
                "risk",
                "--json",
            ],
        ))
        .unwrap();
        assert_eq!(command["card"], owned["structuredContent"]["card"]);
    }
}

#[test]
fn describe_stdio_single_file_rollups_and_directory_expansion_preserve_scope() {
    let root = fixture();
    let path = root.path().join(".codesage/index.db");
    let db = Database::open(&path).unwrap();
    let mut feature = db.load_feature("feat_1111111111111111").unwrap().unwrap();
    feature.feature_id = "feat_2222222222222222".into();
    feature.entry_path = "single/one.rs".into();
    feature.files[0].path = "single/one.rs".into();
    feature.files[1].role = FeatureFileRole::Test;
    db.upsert_feature(&feature).unwrap();
    let mut server = Server::start();
    for target in [&feature.feature_id, "dir:single"] {
        let result = server.call("describe", json!({"project": root.path(), "target": target, "detail": "full", "sections": ["risk"]}));
        assert_ne!(result["isError"], true, "{result}");
        let risk = &result["structuredContent"]["card"]["sections"]["risk"]["data"];
        assert_eq!(risk["files"], 1);
        assert_eq!(risk["scored"], 0);
        assert_eq!(risk["unmeasured_files"], 1);
        assert!(risk["max"].is_null());
        assert!(risk["mean"].is_null());
        assert!(risk.get("score").is_none());
        let command: Value = serde_json::from_slice(&cli(
            root.path(),
            &[
                "describe",
                target,
                "--detail",
                "full",
                "--sections",
                "risk",
                "--json",
            ],
        ))
        .unwrap();
        assert_eq!(command["card"], result["structuredContent"]["card"]);
    }
    db.upsert_git_file("single/one.rs", 8.0, 3, 10, Some(1_700_000_000))
        .unwrap();
    let expected: Value =
        serde_json::from_slice(&cli(root.path(), &["risk", "single/one.rs", "--json"])).unwrap();
    assert!(expected["score"].as_f64().unwrap() > 0.0, "{expected}");
    assert_ne!(expected["unscored"], true);
    for target in [&feature.feature_id, "dir:single"] {
        let result = server.call("describe", json!({"project": root.path(), "target": target, "detail": "full", "sections": ["risk"]}));
        assert_ne!(result["isError"], true, "{result}");
        let section = &result["structuredContent"]["card"]["sections"]["risk"];
        assert!(section.get("completeness").is_none(), "{section}");
        let risk = &section["data"];
        assert_eq!(risk["files"], 1);
        assert_eq!(risk["scored"], 1);
        assert_eq!(risk["unmeasured_files"], 0);
        assert_eq!(risk["max"], expected["score"]);
        assert_eq!(risk["mean"], expected["score"]);
        assert!(risk.get("score").is_none());
        let command: Value = serde_json::from_slice(&cli(
            root.path(),
            &[
                "describe",
                target,
                "--detail",
                "full",
                "--sections",
                "risk",
                "--json",
            ],
        ))
        .unwrap();
        assert_eq!(command["card"], result["structuredContent"]["card"]);
    }
    for (id, entry) in [
        ("feat_3333333333333333", "single"),
        ("feat_4444444444444444", "single/tests/only_test.rs"),
        ("feat_5555555555555555", "single-other"),
        ("feat_6666666666666666", "src"),
    ] {
        let mut record = feature.clone();
        record.feature_id = id.into();
        record.entry_path = entry.into();
        db.upsert_feature(&record).unwrap();
    }
    let compact = server.call(
        "describe",
        json!({"project": root.path(), "target": "dir:single", "sections": ["features"]}),
    );
    assert_ne!(compact["isError"], true, "{compact}");
    let section = &compact["structuredContent"]["card"]["sections"]["features"];
    assert_eq!(section["data"]["total"], 3);
    assert_eq!(section["data"]["top"], json!(["feat_3333333333333333"]));
    let expand = &section["expand"];
    assert_eq!(expand["tool"], "describe");
    assert_eq!(expand["arguments"]["target"], "dir:single");
    assert_eq!(expand["arguments"]["detail"], "full");
    assert_eq!(expand["arguments"]["sections"], json!(["features"]));
    let expanded = server.call(
        expand["tool"].as_str().unwrap(),
        expand["arguments"].clone(),
    );
    assert_ne!(expanded["isError"], true, "{expanded}");
    let card = &expanded["structuredContent"]["card"];
    assert_eq!(card["sections"].as_object().unwrap().len(), 1);
    assert_eq!(card["sections"]["features"]["data"]["total"], 3);
    assert_eq!(
        card["sections"]["features"]["data"]["top"],
        json!([
            "feat_3333333333333333",
            "feat_2222222222222222",
            "feat_4444444444444444"
        ])
    );
    let command: Value = serde_json::from_slice(&cli(
        root.path(),
        &[
            "describe",
            "dir:single",
            "--detail",
            "full",
            "--sections",
            "features",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(command["card"], *card);
}

#[test]
fn describe_old_schema_preserves_database_wal_and_available_facts() {
    let root = fixture();
    let path = root.path().join(".codesage/index.db");
    drop(
        Database::open_for_model_existing(
            &path,
            "jinaai/jina-embeddings-v2-base-code",
            codesage_protocol::DEFAULT_EMBEDDING_DIM,
        )
        .unwrap(),
    );
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL;
         DROP TRIGGER files_clear_interpretation;
         ALTER TABLE files DROP COLUMN interpretation;
         ALTER TABLE files DROP COLUMN is_test;
         ALTER TABLE symbols DROP COLUMN visibility;
         ALTER TABLE symbols DROP COLUMN is_test;
         ALTER TABLE refs DROP COLUMN lazy;
         DROP TABLE semantic_files;
         CREATE TABLE semantic_files(chunk_table TEXT, content_hash TEXT, indexed_at INTEGER);
         DELETE FROM schema_migrations WHERE name IN (
             '0020_file_interpretation', '0022_refs_lazy',
             '0023_symbols_visibility', '0024_is_test'
         );",
        )
        .unwrap();
    std::fs::write(root.path().join("src/helper.rs"), "pub fn edited() {}\n").unwrap();
    let schema = || {
        writer
            .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let migrations = || {
        writer
            .prepare("SELECT name FROM schema_migrations ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before_schema = schema();
    let before_migrations = migrations();
    let before_db = std::fs::read(&path).unwrap();
    let wal = root.path().join(".codesage/index.db-wal");
    let before_wal = std::fs::read(&wal).unwrap();
    assert!(!before_wal.is_empty());
    let mut server = Server::start();
    let result = server.call(
        "describe",
        json!({"project": root.path(), "target": "file:src/helper.rs"}),
    );
    assert_ne!(result["isError"], true, "{result}");
    let content = &result["structuredContent"];
    let identity = &content["card"]["sections"]["identity"]["data"];
    assert_eq!(identity["language"], "rust");
    assert_eq!(identity["lines"], 1);
    assert_eq!(identity["indexed"]["dirty"], true);
    assert!(identity["is_test"].is_null());
    assert_eq!(
        content["card"]["sections"]["identity"]["completeness"]["reason"],
        "optional_schema_unavailable"
    );
    assert_eq!(
        identity["unavailable"],
        json!(["is_test", "interpretation_current"])
    );
    assert_eq!(
        content["card"]["sections"]["symbols"]["data"]["unscored"],
        true
    );
    assert_eq!(content["index"]["semantic"], "unknown");
    assert_eq!(content["index"]["dirty_paths"], json!(["src/helper.rs"]));
    assert_eq!(
        content["card"]["sections"]["symbols"]["completeness"]["recover"]["tool"],
        "describe"
    );
    let command: Value = serde_json::from_slice(&cli(
        root.path(),
        &["describe", "file:src/helper.rs", "--json"],
    ))
    .unwrap();
    assert_eq!(command["card"], content["card"]);
    assert_eq!(schema(), before_schema);
    assert_eq!(migrations(), before_migrations);
    assert_eq!(std::fs::read(&path).unwrap(), before_db);
    assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
}

#[test]
fn describe_preserves_legacy_python_facts_until_index_upgrades_bindings() {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("api.py", "def patch(value):\n    return value\n"),
        (
            "caller.py",
            "from api import patch\ndef entry():\n    return patch('x')\n",
        ),
        ("view.py", "def patch(value):\n    return None\n"),
    ] {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    cli(root.path(), &["init"]);
    cli(root.path(), &["index", "--no-semantic", "--no-features"]);
    let path = root.path().join(".codesage/index.db");
    let request = codesage_protocol::FindReferencesRequest {
        symbol_name: "patch".into(),
        kind: Some(codesage_protocol::ReferenceKind::Call),
    };
    let target = |db: &Database| {
        let references = codesage_graph::find_references(db, &request).unwrap();
        let callers: Vec<_> = references
            .results
            .into_iter()
            .filter(|row| row.from_file == "caller.py")
            .collect();
        assert_eq!(callers.len(), 1);
        callers[0].to.clone()
    };
    let reader = Database::open_read_only(&path).unwrap();
    assert_eq!(target(&reader).as_deref(), Some("sym:api.py#patch"));
    drop(reader);
    let writer = rusqlite::Connection::open(&path).unwrap();
    let hashes = || {
        writer
            .prepare("SELECT path, content_hash FROM files ORDER BY path")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before_hashes = hashes();
    writer
        .execute_batch(
            "DROP TABLE python_bindings;
             DELETE FROM schema_migrations WHERE name = '0026_python_bindings';
             UPDATE files SET interpretation = 'codesage/structural/v1;parser-queries=4;extraction=7;trust-boundaries=1';",
        )
        .unwrap();
    let reader = Database::open_read_only(&path).unwrap();
    assert!(
        reader
            .python_export_binding("api.py", "patch")
            .unwrap()
            .is_none()
    );
    assert!(target(&reader).is_none());
    drop(reader);
    let before_db = std::fs::read(&path).unwrap();
    let wal = root.path().join(".codesage/index.db-wal");
    let before_wal = std::fs::read(&wal).unwrap();
    assert!(!before_wal.is_empty());
    for envelope in [None, Some("legacy")] {
        let mut server = Server::start_with_envelope(envelope);
        for handle in ["file:caller.py", "sym:api.py#patch"] {
            let result = server.call(
                "describe",
                json!({"project": root.path(), "target": handle, "detail": "full"}),
            );
            assert_ne!(result["isError"], true, "{result}");
            let card = &result["structuredContent"]["card"];
            assert_eq!(card["handle"], handle);
            if handle.starts_with("file:") {
                assert_eq!(card["sections"]["identity"]["data"]["language"], "python");
                assert_eq!(
                    card["sections"]["identity"]["data"]["indexed"]["interpretation_current"],
                    false
                );
                assert_eq!(
                    card["sections"]["dependencies"]["data"]["imports"]["top_internal"],
                    json!([])
                );
            }
            let command: Value = serde_json::from_slice(&cli(
                root.path(),
                &["describe", handle, "--detail", "full", "--json"],
            ))
            .unwrap();
            assert_eq!(command["card"], *card);
        }
    }
    assert_eq!(hashes(), before_hashes);
    assert_eq!(std::fs::read(&path).unwrap(), before_db);
    assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
    let missing: i64 = writer
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'python_bindings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);
    drop(writer);
    cli(root.path(), &["index", "--no-semantic", "--no-features"]);
    let reader = Database::open_read_only(&path).unwrap();
    assert_eq!(target(&reader).as_deref(), Some("sym:api.py#patch"));
    assert!(
        reader
            .python_export_binding("api.py", "patch")
            .unwrap()
            .is_some()
    );
    drop(reader);
    let reader =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let hashes_after: Vec<_> = reader
        .prepare("SELECT path, content_hash FROM files ORDER BY path")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(hashes_after, before_hashes);
    let card: Value = serde_json::from_slice(&cli(
        root.path(),
        &["describe", "file:caller.py", "--detail", "full", "--json"],
    ))
    .unwrap();
    assert_eq!(
        card["card"]["sections"]["dependencies"]["data"]["imports"]["top_internal"],
        json!(["file:api.py"])
    );
}

#[test]
fn describe_python_dependencies_use_actual_import_loads() {
    for active in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("pkg")).unwrap();
        std::fs::write(
            root.path().join("caller.py"),
            "from pkg import api\ndef entry():\n    return api.patch('x')\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("pkg/api.py"),
            "def patch(value):\n    return value\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("pkg/__init__.py"),
            if active {
                "from unittest import mock\napi: object\ndef __getattr__(name):\n    return mock\n"
            } else {
                "api: object\n"
            },
        )
        .unwrap();
        cli(root.path(), &["init"]);
        cli(root.path(), &["index", "--no-semantic", "--no-features"]);
        let result: Value = serde_json::from_slice(&cli(
            root.path(),
            &[
                "describe",
                "file:caller.py",
                "--detail",
                "full",
                "--sections",
                "dependencies",
                "--json",
            ],
        ))
        .unwrap();
        let expected = if active {
            json!(["file:pkg/__init__.py"])
        } else {
            json!(["file:pkg/__init__.py", "file:pkg/api.py"])
        };
        assert_eq!(
            result["card"]["sections"]["dependencies"]["data"]["imports"]["top_internal"], expected,
            "{active}: {result}"
        );
        for envelope in [None, Some("legacy")] {
            let mut server = Server::start_with_envelope(envelope);
            let response = server.call("describe", json!({"project": root.path(), "target": "file:caller.py", "detail": "full", "sections": ["dependencies"]}));
            assert_ne!(response["isError"], true, "{response}");
            assert_eq!(response["structuredContent"]["card"], result["card"]);
        }
    }
}

#[test]
fn describe_cost_matches_final_payload_in_default_and_legacy_modes() {
    let root = fixture();
    for envelope in [None, Some("legacy")] {
        let mut server = Server::start_with_envelope(envelope);
        for detail in ["compact", "standard", "full"] {
            for target in [
                "file:src/helper.rs",
                "sym:src/helper.rs#leaf",
                "feat_1111111111111111",
                "dir:src",
                "duplicate",
            ] {
                let result = server.call(
                    "describe",
                    json!({"project": root.path(), "target": target, "detail": detail}),
                );
                assert_ne!(result["isError"], true, "{result}");
                let content = &result["structuredContent"];
                assert_eq!(content["cost"]["detail"], detail);
                assert!(content["cost"]["ms"].as_u64().is_some());
                assert_eq!(
                    content["cost"]["bytes"].as_u64().unwrap(),
                    serde_json::to_vec(content).unwrap().len() as u64,
                    "{envelope:?}, {detail}, {target}: {content}"
                );
                let rendered: Vec<Value> = result["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .filter_map(|text| serde_json::from_str::<Value>(text).ok())
                    .filter(|value| value.get("cost").is_some())
                    .collect();
                assert_eq!(rendered, vec![content.clone()]);
                if target == "duplicate" {
                    assert_eq!(content["target"]["ambiguous"], true);
                    assert!(content["card"].is_null());
                } else {
                    assert_eq!(content["card"]["handle"], target);
                }
                if envelope == Some("legacy") {
                    for key in ["tool", "index", "completeness"] {
                        assert!(content.get(key).is_none(), "{content}");
                    }
                }
            }
        }
    }
}

#[test]
fn describe_stdio_warm_cache_preserves_missing_partial_and_measured_history() {
    let root = fixture();
    let path = root.path().join(".codesage/index.db");
    let db = Database::open_for_model_existing(
        &path,
        "jinaai/jina-embeddings-v2-base-code",
        codesage_protocol::DEFAULT_EMBEDDING_DIM,
    )
    .unwrap();
    let paths = ["src/lib.rs", "src/helper.rs", "src/a.rs", "src/b.rs"];
    let mut feature = db.load_feature("feat_1111111111111111").unwrap().unwrap();
    feature.files = paths
        .iter()
        .enumerate()
        .map(|(index, path)| FeatureFileRef {
            path: (*path).into(),
            role: if index == 0 {
                FeatureFileRole::Entry
            } else {
                FeatureFileRole::Owned
            },
            reason: None,
        })
        .collect();
    db.upsert_feature(&feature).unwrap();
    let writer = rusqlite::Connection::open(&path).unwrap();
    let schema = || {
        writer
            .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let wal = root.path().join(".codesage/index.db-wal");
    for measured in [0, 2, 4] {
        for file in paths.iter().take(measured) {
            db.upsert_git_file(file, 8.0, 3, 10, Some(1_700_000_000))
                .unwrap();
        }
        let before_schema = schema();
        let before_db = std::fs::read(&path).unwrap();
        let before_wal = std::fs::read(&wal).unwrap();
        assert!(!before_wal.is_empty());
        let targets: Vec<String> = paths
            .iter()
            .map(|file| format!("file:{file}"))
            .chain([feature.feature_id.clone(), "dir:src".into()])
            .collect();
        let mut server = Server::start();
        let mut cold_cards = Vec::new();
        let mut measured_scores = Vec::new();
        for (index, target) in targets.iter().enumerate() {
            let command: Value = serde_json::from_slice(&cli(
                root.path(),
                &[
                    "describe",
                    target,
                    "--detail",
                    "full",
                    "--sections",
                    "risk",
                    "--json",
                ],
            ))
            .unwrap();
            let result = server.call("describe", json!({"project": root.path(), "target": target, "detail": "full", "sections": ["risk"]}));
            assert_ne!(result["isError"], true, "{result}");
            let card = &result["structuredContent"]["card"];
            assert_eq!(*card, command["card"], "cold {measured}: {target}");
            let section = &card["sections"]["risk"];
            assert!(section.get("completeness").is_none(), "{section}");
            let data = &section["data"];
            assert!(data.get("cached_scores").is_none());
            if index < paths.len() {
                assert!(
                    !data["notes"].as_array().unwrap().is_empty(),
                    "{target}: {data}"
                );
                if index < measured {
                    assert!(data.get("unscored").is_none());
                    assert!(data.get("history_terms").is_none());
                    measured_scores.push(data["score"].as_f64().unwrap());
                } else {
                    assert_eq!(data["unscored"], true);
                    assert_eq!(data["history_terms"], "unmeasured");
                    assert_eq!(data["recover"]["command"], "codesage git-index");
                    assert!(
                        data["notes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|note| note.as_str().unwrap().contains("no indexed git history")),
                        "{data}"
                    );
                }
            } else {
                assert_eq!(data["files"], paths.len());
                assert_eq!(data["scored"], measured);
                assert_eq!(data["unmeasured_files"], paths.len() - measured);
                assert_eq!(data["unscored"], measured < paths.len());
                if measured == 0 {
                    assert!(data["max"].is_null());
                    assert!(data["mean"].is_null());
                } else {
                    assert_eq!(
                        data["max"],
                        measured_scores
                            .iter()
                            .copied()
                            .max_by(f64::total_cmp)
                            .unwrap()
                    );
                    assert_eq!(
                        data["mean"],
                        measured_scores.iter().sum::<f64>() / measured_scores.len() as f64
                    );
                }
            }
            cold_cards.push(card.clone());
        }
        let overview = server.call("project_overview", json!({"project": root.path()}));
        assert_ne!(overview["isError"], true, "{overview}");
        for (target, cold) in targets.iter().zip(cold_cards) {
            let result = server.call("describe", json!({"project": root.path(), "target": target, "detail": "full", "sections": ["risk"]}));
            assert_ne!(result["isError"], true, "{result}");
            let warm = &result["structuredContent"]["card"];
            assert_eq!(*warm, cold, "warm {measured}: {target}");
            let command: Value = serde_json::from_slice(&cli(
                root.path(),
                &[
                    "describe",
                    target,
                    "--detail",
                    "full",
                    "--sections",
                    "risk",
                    "--json",
                ],
            ))
            .unwrap();
            assert_eq!(command["card"], *warm);
            let expand = &warm["sections"]["risk"]["expand"];
            let mut arguments = expand["arguments"].clone();
            arguments["project"] = json!(root.path());
            let expanded = server.call(expand["tool"].as_str().unwrap(), arguments);
            assert_ne!(expanded["isError"], true, "{expanded}");
            if expand["tool"] == "describe" {
                assert_eq!(expanded["structuredContent"]["card"], *warm);
            } else {
                let assessed = &expanded["structuredContent"];
                assert_eq!(assessed["score"], warm["sections"]["risk"]["data"]["score"]);
                assert_eq!(
                    assessed["unscored"].as_bool().unwrap_or(false),
                    warm["sections"]["risk"]["data"]["unscored"]
                        .as_bool()
                        .unwrap_or(false)
                );
                assert_eq!(assessed["notes"], warm["sections"]["risk"]["data"]["notes"]);
            }
        }
        assert_eq!(schema(), before_schema);
        assert_eq!(std::fs::read(&path).unwrap(), before_db);
        assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
    }
}

#[test]
fn describe_stdio_cards_cli_parity_and_every_expansion_execute() {
    let root = fixture();
    let mut server = Server::start();
    let listing = server.request("tools/list", json!({}));
    let tools = listing["tools"].as_array().unwrap();
    let schema = tools
        .iter()
        .find(|tool| tool["name"] == "describe")
        .unwrap();
    assert_eq!(schema["annotations"]["readOnlyHint"], true);
    assert!(
        schema["inputSchema"]["properties"]
            .get("sections")
            .is_some()
    );
    assert!(schema["outputSchema"]["properties"].get("card").is_some());
    for (target, section_count) in [
        ("file:src/helper.rs", 9),
        ("sym:src/helper.rs#leaf", 7),
        ("feat_1111111111111111", 5),
        ("dir:src", 4),
    ] {
        let arguments = json!({"project": root.path(), "target": target});
        let result = server.call("describe", arguments);
        assert_ne!(result["isError"], true, "{target}: {result}");
        let content = &result["structuredContent"];
        let sections = content["card"]["sections"].as_object().unwrap();
        assert_eq!(sections.len(), section_count);
        assert!(
            serde_json::to_vec(&content["card"]).unwrap().len() < 3000,
            "{target}: {content}"
        );
        assert_eq!(
            content["cost"]["bytes"].as_u64().unwrap(),
            serde_json::to_vec(content).unwrap().len() as u64
        );
        let rendered: Value = serde_json::from_str(
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|item| {
                    item["text"]
                        .as_str()
                        .filter(|text| serde_json::from_str::<Value>(text).is_ok())
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*content, rendered);
        let command: Value =
            serde_json::from_slice(&cli(root.path(), &["describe", target, "--json"])).unwrap();
        assert_eq!(
            command["card"], content["card"],
            "CLI card parity for {target}"
        );
        if let Some(next) = content.get("next").filter(|next| !next.is_null()) {
            let followed = server.call(next["tool"].as_str().unwrap(), next["arguments"].clone());
            assert_ne!(followed["isError"], true, "next for {target}: {followed}");
        }
        for (name, section) in sections {
            let expand = &section["expand"];
            let tool = expand["tool"].as_str().unwrap();
            assert!(
                tools.iter().any(|row| row["name"] == tool),
                "unadvertised {tool}"
            );
            let expanded = server.call(tool, expand["arguments"].clone());
            assert_ne!(expanded["isError"], true, "{target}/{name}: {expanded}");
            if let Some(recover) = section.pointer("/completeness/recover") {
                let recovered = server.call(
                    recover["tool"].as_str().unwrap(),
                    recover["arguments"].clone(),
                );
                assert_ne!(
                    recovered["isError"], true,
                    "recover {target}/{name}: {recovered}"
                );
            }
        }
    }
}

#[test]
fn describe_stdio_ambiguity_and_invalid_sections_preserve_resolution_policy() {
    let root = fixture();
    let mut server = Server::start();
    let result = server.call(
        "describe",
        json!({"project": root.path(), "target": "duplicate"}),
    );
    assert_ne!(result["isError"], true);
    let content = &result["structuredContent"];
    assert!(content.get("card").is_none());
    assert_eq!(content["target"]["ambiguous"], true);
    assert_eq!(content["target"]["candidates_total"], 2);
    let invalid = server.call(
        "describe",
        json!({"project": root.path(), "target": "file:src/lib.rs", "sections": ["cycles"]}),
    );
    assert_eq!(invalid["isError"], true);
    let failure = invalid["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|value| value.get("error").is_some())
        .unwrap();
    assert_eq!(failure["error"]["code"], "E_PARAM");
    let selected = server.call("describe", json!({"project": root.path(), "target": "file:src/lib.rs", "detail": "full", "sections": ["identity"]}));
    assert_ne!(selected["isError"], true);
    assert_eq!(
        selected["structuredContent"]["card"]["sections"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    let output: Value = serde_json::from_slice(&cli(
        root.path(),
        &[
            "describe",
            "file:src/lib.rs",
            "--detail",
            "full",
            "--sections",
            "identity",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(output["card"], selected["structuredContent"]["card"]);
}
