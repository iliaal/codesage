use codesage_graph::{full_index, list_dependencies_batch};
use codesage_storage::Database;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn prerequisites(depinfo: &str) -> Result<Vec<String>, String> {
    let joined = depinfo.replace("\\\r\n", "").replace("\\\n", "");
    let rule = joined
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .ok_or("missing dependency rule")?;
    let (_, rhs) = rule
        .split_once(": ")
        .ok_or("missing prerequisite separator")?;
    let mut paths = Vec::new();
    let mut path = String::new();
    let mut chars = rhs.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => path.push(chars.next().ok_or("unfinished escape")?),
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                path.push('$');
            }
            c if c.is_whitespace() => {
                if !path.is_empty() {
                    paths.push(std::mem::take(&mut path));
                }
            }
            c => path.push(c),
        }
    }
    if !path.is_empty() {
        paths.push(path);
    }
    if paths.is_empty() {
        return Err("empty compiler source oracle".into());
    }
    Ok(paths)
}

fn compiler_sources(root: &Path) -> Option<BTreeSet<String>> {
    let output = match Command::new("cargo")
        .args(["check", "--offline", "--quiet", "--lib", "--target-dir"])
        .arg(root.join("target"))
        .current_dir(root)
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP Rust dep-info oracle: cargo is absent");
            return None;
        }
        Err(error) => panic!("cannot execute cargo check: {error}"),
    };
    assert!(
        output.status.success(),
        "cargo check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let depfiles: Vec<_> = std::fs::read_dir(root.join("target/debug/deps"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "d"))
        .collect();
    assert_eq!(
        depfiles.len(),
        1,
        "dependency-free fixture needs one oracle"
    );
    let root = root.canonicalize().unwrap();
    let mut sources = BTreeSet::new();
    for source in prerequisites(&std::fs::read_to_string(&depfiles[0]).unwrap()).unwrap() {
        let source = root.join(source).canonicalize().unwrap();
        assert_eq!(source.extension().unwrap(), "rs");
        let local = source.strip_prefix(&root).expect("nonlocal compiler input");
        sources.insert(local.to_str().unwrap().replace('\\', "/"));
    }
    assert!(
        sources.contains("src/lib.rs"),
        "missing crate root: {sources:?}"
    );
    assert!(
        sources.len() > 1,
        "oracle must exercise another source file"
    );
    Some(sources)
}

fn reachable_sources(db: &Database) -> BTreeSet<String> {
    let files = db.indexed_files_with_prefix("").unwrap();
    let paths: Vec<_> = files.iter().map(String::as_str).collect();
    let entries = list_dependencies_batch(db, &paths).unwrap();
    let mut reachable = BTreeSet::from(["src/lib.rs".to_string()]);
    loop {
        let before = reachable.len();
        for entry in &entries {
            if entry
                .imported_by
                .iter()
                .any(|from| reachable.contains(from))
            {
                reachable.insert(entry.file_path.clone());
            }
        }
        if reachable.len() == before {
            return reachable;
        }
    }
}

fn compare(files: &[(&str, &str)], edition: &str, missing: &[&str], extra: &[&str]) {
    let dir = tempfile::Builder::new()
        .prefix("codesage depinfo ")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"depinfo_oracle\"\nversion = \"0.0.0\"\nedition = \"{edition}\"\n[workspace]\n"),
    )
    .unwrap();
    for (path, source) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let Some(compiler) = compiler_sources(root) else {
        return;
    };
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    let graph = reachable_sources(&db);
    if missing.is_empty() && extra.is_empty() {
        assert_eq!(graph, compiler, "graph and compiler source sets differ");
    } else {
        let observed_missing: BTreeSet<_> =
            compiler.difference(&graph).map(String::as_str).collect();
        let observed_extra: BTreeSet<_> = graph.difference(&compiler).map(String::as_str).collect();
        eprintln!("Unsupported extraction: missing={observed_missing:?}, extra={observed_extra:?}");
        assert_eq!(observed_missing, missing.iter().copied().collect());
        assert_eq!(observed_extra, extra.iter().copied().collect());
    }
}

#[test]
fn depinfo_parser_preserves_escaped_paths_and_rejects_empty_oracles() {
    assert_eq!(
        prerequisites(concat!(
            "out: src/lib.rs src/a\\ b.rs \\\n",
            " src/c\\#d.rs src/$$e.rs\n\nsrc/lib.rs:\n"
        ))
        .unwrap(),
        ["src/lib.rs", "src/a b.rs", "src/c#d.rs", "src/$e.rs"]
    );
    assert!(prerequisites("").is_err());
    assert!(prerequisites("out: ").is_err());
    assert!(prerequisites("out: src/trailing\\").is_err());
}

#[test]
fn pub_use_and_local_env_module_match_compiler_transitively() {
    compare(
        &[
            (
                "src/lib.rs",
                "pub mod bridge; pub mod env; pub use crate::bridge::value;",
            ),
            ("src/bridge.rs", "pub use crate::env::value;"),
            ("src/env.rs", "pub fn value() {}"),
            ("src/unopened.rs", "pub fn decoy() {}"),
        ],
        "2024",
        &[],
        &[],
    );
}

#[test]
fn bare_pub_mod_is_classified_as_missing_declaration_edge() {
    compare(
        &[
            ("src/lib.rs", "pub mod env;"),
            ("src/env.rs", "pub fn value() {}"),
        ],
        "2024",
        &["src/env.rs"],
        &[],
    );
}

#[test]
fn macro_body_mod_is_classified_as_unexpanded() {
    compare(
        &[
            (
                "src/lib.rs",
                "macro_rules! modules { () => { pub mod generated; } } modules!();",
            ),
            ("src/generated.rs", "pub fn value() {}"),
        ],
        "2024",
        &["src/generated.rs"],
        &[],
    );
}

#[test]
fn cfg_attr_relocation_exposes_both_missing_and_spurious_sources() {
    compare(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(not(any()), path = \"selected file.rs\")] pub mod relocated; pub use crate::relocated::value;",
            ),
            ("src/selected file.rs", "pub fn value() {}"),
            ("src/relocated.rs", "pub fn decoy() {}"),
        ],
        "2024",
        &["src/selected file.rs"],
        &["src/relocated.rs"],
    );
}

#[test]
fn edition_2015_leading_colons_are_classified_as_unresolved() {
    compare(
        &[
            ("src/lib.rs", "pub mod env; pub use ::env::value;"),
            ("src/env.rs", "pub fn value() {}"),
        ],
        "2015",
        &["src/env.rs"],
        &[],
    );
}
