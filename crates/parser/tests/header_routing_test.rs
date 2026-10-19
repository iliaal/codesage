use std::fs;

use codesage_parser::discover::discover_files_with_excludes;
use codesage_protocol::Language;

fn write(root: &std::path::Path, path: &str, source: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

#[test]
fn cpp_evidence_is_local_and_literal_includes_resolve_unambiguously() {
    let root = tempfile::tempdir().unwrap();
    for path in [
        "c/core.h",
        "cpp/local.h",
        "include/shared.h",
        "one/ambiguous.h",
        "two/ambiguous.h",
        "include/dead.h",
        "include/string.h",
        "include/nested.h",
    ] {
        write(root.path(), path, "struct item { int value; };\n");
    }
    write(
        root.path(),
        "cpp/main.cpp",
        "#include <shared.h>\n#include <ambiguous.h>\n#if 0\n#include <dead.h>\n#endif\nconst char *text = \"#include <string.h>\";\n",
    );
    write(
        root.path(),
        "include/shared.h",
        "#include <nested.h>\nstruct shared { int value; };\n",
    );
    let files = discover_files_with_excludes(root.path(), &[]).unwrap();
    for path in ["cpp/local.h", "include/shared.h"] {
        assert_eq!(
            files.iter().find(|f| f.path == path).unwrap().language,
            Language::Cpp,
            "{path}"
        );
    }
    for path in [
        "c/core.h",
        "one/ambiguous.h",
        "two/ambiguous.h",
        "include/dead.h",
        "include/string.h",
        "include/nested.h",
    ] {
        assert_eq!(
            files.iter().find(|f| f.path == path).unwrap().language,
            Language::C,
            "{path}"
        );
    }
}

#[test]
fn quoted_includes_use_the_source_directory_and_excluded_sources_supply_no_evidence() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "src/cpp/main.cpp",
        "#include \"../../api/local.h\"\n#include \"../api.h\"\n",
    );
    for path in [
        "api/local.h",
        "src/api.h",
        "other/api.h",
        "excluded/api.h",
        "cuda/api.h",
    ] {
        write(root.path(), path, "struct Api {};\n");
    }
    write(root.path(), "excluded/main.cpp", "void excluded() {}\n");
    write(
        root.path(),
        "cuda/kernel.cu",
        "__global__ void kernel() {}\n",
    );
    let files = discover_files_with_excludes(root.path(), &["excluded/main.cpp".into()]).unwrap();
    for path in ["api/local.h", "src/api.h"] {
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .language,
            Language::Cpp,
            "{path}"
        );
    }
    for path in ["other/api.h", "excluded/api.h", "cuda/api.h"] {
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .language,
            Language::C,
            "{path}"
        );
    }
}

#[test]
fn include_evidence_ignores_raw_strings_comments_and_macro_bodies() {
    let root = tempfile::tempdir().unwrap();
    for path in [
        "include/real.h",
        "include/continued.h",
        "include/comment.h",
        "include/raw.h",
        "include/macro.h",
        "include/spliced.h",
        "include/dead.h",
        "include/not_directive.h",
    ] {
        write(root.path(), path, "struct Item { int value; };\n");
    }
    write(
        root.path(),
        "cpp/main.cpp",
        concat!(
            "\u{feff}#include <real.h>\n",
            "const char *raw = u8R\"tag(\n#include <raw.h>\n)tag\";\n",
            "/* comment\n#include <comment.h>\n*/\n",
            "// continued comment \\\n#include <spliced.h>\n",
            "#define BODY \\\n#include <macro.h>\n",
            "#if 1\n#include \\\n<continued.h>\n#else\n#include <dead.h>\n#endif\n",
            "int value; /* no logical newline \\\n */ #include <not_directive.h>\n",
        ),
    );
    let files = discover_files_with_excludes(root.path(), &[]).unwrap();
    for path in ["include/real.h", "include/continued.h"] {
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .language,
            Language::Cpp,
            "{path}"
        );
    }
    for path in [
        "include/comment.h",
        "include/raw.h",
        "include/macro.h",
        "include/spliced.h",
        "include/dead.h",
        "include/not_directive.h",
    ] {
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .language,
            Language::C,
            "{path}"
        );
    }
}

#[test]
fn cached_header_decisions_match_fresh_discovery_after_context_changes() {
    use codesage_parser::discover::discover_files_report_with_context;
    use std::collections::HashMap;

    let root = tempfile::tempdir().unwrap();
    write(root.path(), "include/api.h", "struct Api { int value; };\n");
    write(root.path(), "other/keep.h", "struct Keep { int value; };\n");
    write(root.path(), "cpp/main.cpp", "void run() {}\n");
    let mut previous =
        discover_files_report_with_context(root.path(), &[], &HashMap::new(), &[]).unwrap();
    assert!(
        previous
            .files
            .iter()
            .any(|file| file.path == "cpp/main.cpp")
    );
    for (path, source, expected) in [
        ("include/api.h", "int broken( {\n", Language::Cpp),
        ("include/api.h", "struct Api { int value; };\n", Language::C),
        (
            "cpp/main.cpp",
            "#include <api.h>\nvoid run() {}\n",
            Language::Cpp,
        ),
        ("other/api.h", "struct Other { int value; };\n", Language::C),
    ] {
        write(root.path(), path, source);
        if source.starts_with("int broken") {
            assert!(
                codesage_parser::parse::parse_file_tolerant(source.as_bytes(), Language::C)
                    .unwrap()
                    .degraded
            );
        }
        let current = discover_files_report_with_context(
            root.path(),
            &[],
            &previous.hash_cache,
            &previous.files,
        )
        .unwrap();
        assert_eq!(
            current.files,
            discover_files_with_excludes(root.path(), &[]).unwrap()
        );
        assert_eq!(
            current
                .files
                .iter()
                .find(|file| file.path == "include/api.h")
                .unwrap()
                .language,
            expected,
            "after writing {path}: {source}"
        );
        previous = current;
    }
    fs::remove_file(root.path().join("other/api.h")).unwrap();
    let current =
        discover_files_report_with_context(root.path(), &[], &previous.hash_cache, &previous.files)
            .unwrap();
    assert_eq!(
        current.files,
        discover_files_with_excludes(root.path(), &[]).unwrap()
    );
    assert_eq!(
        current
            .files
            .iter()
            .find(|file| file.path == "include/api.h")
            .unwrap()
            .language,
        Language::Cpp
    );
    fs::remove_file(root.path().join("cpp/main.cpp")).unwrap();
    let removed =
        discover_files_report_with_context(root.path(), &[], &current.hash_cache, &current.files)
            .unwrap();
    assert_eq!(
        removed.files,
        discover_files_with_excludes(root.path(), &[]).unwrap()
    );
    assert_eq!(
        removed
            .files
            .iter()
            .find(|file| file.path == "include/api.h")
            .unwrap()
            .language,
        Language::C
    );
}

#[test]
fn cached_local_headers_preserve_direct_include_peers() {
    use codesage_parser::discover::discover_files_report_with_context;
    use std::collections::HashMap;

    let root = tempfile::tempdir().unwrap();
    write(root.path(), "cpp/local.h", "struct Local { int value; };\n");
    write(
        root.path(),
        "include/shared.h",
        "struct Shared { int value; };\n",
    );
    write(
        root.path(),
        "include/other.h",
        "struct Other { int value; };\n",
    );
    write(root.path(), "cpp/main.cpp", "#include <shared.h>\n");
    let previous =
        discover_files_report_with_context(root.path(), &[], &HashMap::new(), &[]).unwrap();
    for (path, source, expected_shared, expected_other) in [
        (
            "cpp/local.h",
            "struct Local { long value; };\n",
            Language::Cpp,
            Language::C,
        ),
        (
            "cpp/main.cpp",
            "#include <other.h>\n",
            Language::C,
            Language::Cpp,
        ),
    ] {
        write(root.path(), path, source);
        let current = discover_files_report_with_context(
            root.path(),
            &[],
            &previous.hash_cache,
            &previous.files,
        )
        .unwrap();
        assert_eq!(
            current.files,
            discover_files_with_excludes(root.path(), &[]).unwrap()
        );
        for (path, language) in [
            ("cpp/local.h", Language::Cpp),
            ("include/shared.h", expected_shared),
            ("include/other.h", expected_other),
        ] {
            assert_eq!(
                current
                    .files
                    .iter()
                    .find(|file| file.path == path)
                    .unwrap()
                    .language,
                language,
                "{path}"
            );
        }
    }
}

#[test]
fn local_and_reusable_header_decisions_still_validate_every_cpp_snapshot() {
    use codesage_parser::header::cpp_headers_with_context;

    let root = tempfile::tempdir().unwrap();
    write(root.path(), "cpp/local.h", "struct Local { int value; };\n");
    write(
        root.path(),
        "other/clean.h",
        "struct Clean { int value; };\n",
    );
    for path in ["cpp/first.cpp", "cpp/last.cpp"] {
        write(root.path(), path, "void original() {}\n");
    }
    let files = discover_files_with_excludes(root.path(), &[]).unwrap();
    assert_eq!(
        cpp_headers_with_context(root.path(), &files, &files).unwrap(),
        ["cpp/local.h".to_owned()].into_iter().collect()
    );
    for path in ["cpp/first.cpp", "cpp/last.cpp"] {
        write(root.path(), path, "void changed() {}\n");
        let error = cpp_headers_with_context(root.path(), &files, &files).unwrap_err();
        assert!(error.to_string().contains(path), "{error}");
        assert!(
            error
                .to_string()
                .contains("changed while establishing header routing"),
            "{error}"
        );
        write(root.path(), path, "void original() {}\n");
    }
}

#[test]
fn clean_but_ambiguous_macro_declarators_keep_each_projects_legacy_dialect() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "c/rules.h",
        "RULE(EQ(A, B))\nRULE(EQ(C, D))\nRULE(EQ(E, F))\n{ emit(); }\n",
    );
    let files = discover_files_with_excludes(root.path(), &[]).unwrap();
    assert_eq!(
        files
            .iter()
            .find(|file| file.path == "c/rules.h")
            .unwrap()
            .language,
        Language::C
    );
    write(root.path(), "cpp/main.cpp", "void marker() {}\n");
    let files = discover_files_with_excludes(root.path(), &[]).unwrap();
    assert_eq!(
        files
            .iter()
            .find(|file| file.path == "c/rules.h")
            .unwrap()
            .language,
        Language::Cpp
    );
    fs::remove_file(root.path().join("cpp/main.cpp")).unwrap();
    let files = discover_files_with_excludes(root.path(), &[]).unwrap();
    assert_eq!(
        files
            .iter()
            .find(|file| file.path == "c/rules.h")
            .unwrap()
            .language,
        Language::C
    );
}

#[test]
fn header_membership_caps_routing_candidates_and_directories() {
    use codesage_protocol::FileInfo;
    let root = tempfile::tempdir().unwrap();
    let files: Vec<_> = (0..16_384)
        .map(|number| {
            let path = format!("candidate{number}.h");
            write(root.path(), &path, "");
            FileInfo {
                path,
                language: Language::C,
                content_hash: String::new(),
                is_test: false,
            }
        })
        .collect();
    let error = codesage_parser::header::verify_membership(root.path(), &files, &[]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("16384 routing candidates and directories"),
        "{error:#}"
    );
}
