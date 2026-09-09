use std::{path::Path, process::Command};

fn codesage(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codesage"));
    cmd.current_dir(root);
    cmd
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [&["init"][..], &["index", "--no-semantic"][..]] {
        let output = codesage(dir.path()).args(args).output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    dir
}

#[test]
fn strict_fails_when_no_documents_are_checked() {
    for selection in [
        "defaults",
        "empty-directory",
        "non-markdown",
        "skip-file",
        "excluded",
    ] {
        let dir = project();
        let root = dir.path();
        let mut args = vec!["doctor", "--docs", "--strict"];
        match selection {
            "empty-directory" => {
                std::fs::create_dir(root.join("empty")).unwrap();
                args.push("empty");
            }
            "non-markdown" => {
                std::fs::write(root.join("notes.txt"), "Plain text\n").unwrap();
                args.push("notes.txt");
            }
            "skip-file" => {
                std::fs::write(
                    root.join("README.md"),
                    "<!-- codesage-docs: skip-file -->\n",
                )
                .unwrap();
            }
            "excluded" => {
                std::fs::write(root.join("README.md"), "# Introduction\n").unwrap();
                let config = root.join(".codesage/config.toml");
                let mut text = std::fs::read_to_string(&config).unwrap();
                text.push_str("\n[docs]\nexclude_patterns = [\"README.md\"]\n");
                std::fs::write(config, text).unwrap();
            }
            _ => {}
        }
        let output = codesage(root).args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{selection}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("no documents checked"),
            "{selection}: {output:?}"
        );
        let output = codesage(root).args(&args).arg("--json").output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{selection}: {output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["files"], serde_json::json!([]));
    }
}

#[test]
fn strict_succeeds_for_a_document_without_checkable_claims() {
    let dir = project();
    std::fs::write(
        dir.path().join("README.md"),
        "# Introduction\n\nA helpful project.\n",
    )
    .unwrap();
    let output = codesage(dir.path())
        .args(["doctor", "--docs", "--strict", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["files"], serde_json::json!(["README.md"]));
    assert_eq!(report["claims_checked"], 0);
}

#[test]
fn generated_and_inherited_methods_are_not_reported_as_missing() {
    let dir = project();
    let root = dir.path();
    std::fs::write(root.join("types.rs"), "#[derive(Default)]\npub struct Widget {}\npub struct Plain;\npub struct Custom;\nimpl crate::traits::Factory for Custom {}\n").unwrap();
    std::fs::write(
        root.join("traits.rs"),
        "pub trait Factory: Sized { fn make() -> Option<Self> { None } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("parent.php"),
        "<?php\nclass ParentClass { public static function inherited() {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("types.php"),
        "<?php\nclass ChildClass extends ParentClass {}\nclass PlainClass {}\n",
    )
    .unwrap();
    let output = codesage(root)
        .args(["index", "--no-semantic"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    std::fs::write(root.join("README.md"), "`Widget::default()`, `Custom::make()`, and `ChildClass::inherited()` exist.\n`Plain::absent_method()` and `PlainClass::absent_method()` do not.\n").unwrap();
    let output = codesage(root)
        .args(["doctor", "--docs", "--strict", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let claims: Vec<_> = report["drifted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["claim"].as_str().unwrap())
        .collect();
    assert_eq!(
        claims,
        vec!["Plain::absent_method()", "PlainClass::absent_method()"]
    );
}

#[test]
fn non_strict_succeeds_and_discloses_no_documents_checked() {
    let dir = project();
    let output = codesage(dir.path())
        .args(["doctor", "--docs"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no documents checked"),
        "{output:?}"
    );
}

#[test]
fn comments_between_derive_and_type_preserve_attribute_association() {
    let dir = project();
    let root = dir.path();
    std::fs::write(
        root.join("types.rs"),
        r###"
#[derive(Default)]
/// Represents `{}`; its default is empty.
pub struct LineDocumented {}
#[derive(Default)]
/** Represents `{}`; its default is empty. */
pub struct BlockDocumented {}
#[derive(Default)]
/* Outer {; /* Nested }; */ still a comment }; */
pub struct NestedDocumented {}
#[derive(Default)]
#[doc = "Represents `{};`."]
pub struct AttributeDocumented {}
/// An undecorated type; `{}` does not imply generated methods.
pub struct Plain {}
/** Another plain type; `{}` is only documentation. */
pub struct PlainBlock {}
"###,
    )
    .unwrap();
    let output = codesage(root)
        .args(["index", "--no-semantic"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    std::fs::write(root.join("README.md"), "`LineDocumented::default()`, `BlockDocumented::default()`, `NestedDocumented::default()`, and `AttributeDocumented::default()` exist.\n`Plain::absent_method()` and `PlainBlock::absent_method()` do not.\n").unwrap();
    let output = codesage(root)
        .args(["doctor", "--docs", "--strict", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let claims: Vec<_> = report["drifted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["claim"].as_str().unwrap())
        .collect();
    assert_eq!(
        claims,
        vec!["Plain::absent_method()", "PlainBlock::absent_method()"]
    );
}

#[cfg(unix)]
#[test]
fn strict_reports_unreadable_directories_alongside_checked_documents() {
    use std::os::unix::{
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    };
    let dir = project();
    let root = dir.path();
    std::fs::write(root.join("README.md"), "# Introduction\n").unwrap();
    let secret = root.join("docs/secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    let root_user = root.metadata().unwrap().uid() == 0;
    let binary = root.join("codesage-test");
    if root_user {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_codesage"), &binary).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        assert!(std::fs::read_dir(&secret).is_err());
    }
    for paths in [&["README.md", "docs"][..], &[][..]] {
        let mut cmd = if root_user {
            let mut cmd = Command::new(&binary);
            cmd.current_dir(root).uid(65534).gid(65534);
            cmd
        } else {
            codesage(root)
        };
        let output = cmd
            .args(["doctor", "--docs", "--strict", "--json"])
            .args(paths)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["files"], serde_json::json!(["README.md"]));
        assert_eq!(report["files_failed"][0]["path"], "docs/secret");
        assert!(
            report["files_failed"][0]["error"]
                .as_str()
                .unwrap()
                .contains("Permission denied")
        );
    }
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn cpp_constexpr_constant_values_are_checked_through_the_indexer() {
    let dir = project();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/retry.cc"),
        "namespace net {\nconstexpr int kMaxRetries = 3;\n}\n",
    )
    .unwrap();
    let output = codesage(root)
        .args(["index", "--no-semantic"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    for (documented, drifted) in [("5", 1usize), ("3", 0)] {
        std::fs::write(
            root.join("README.md"),
            format!("`kMaxRetries` defaults to {documented}.\n"),
        )
        .unwrap();
        let output = codesage(root)
            .args(["doctor", "--docs", "--strict", "--json"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(drifted == 1)),
            "{documented}: {output:?}"
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["claims_checked"], 1, "{documented}: {report}");
        let findings = report["drifted"].as_array().unwrap();
        assert_eq!(findings.len(), drifted, "{documented}: {report}");
        if drifted == 1 {
            assert_eq!(findings[0]["class"], "constant");
            assert_eq!(findings[0]["claim"], "kMaxRetries = 5");
            let reason = findings[0]["reason"].as_str().unwrap();
            assert!(reason.contains("doc says 5"), "{reason}");
            assert!(reason.contains("source says 3"), "{reason}");
        }
    }
}
