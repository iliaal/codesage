use codesage_features::mappers::c::CCppMapper;
use codesage_features::mappers::types::{FeatureMapper, MapperContext};
use tempfile::tempdir;

#[test]
fn cmake_literal_brackets_preserve_source_paths_and_argument_forms() {
    let dir = tempdir().unwrap();
    let sources = [
        "file[1].c",
        "[literal].c",
        "[=literal].c",
        "plain.c",
        "quoted [path];name.c",
        "bracket [path];name.c",
    ];
    for source in sources {
        std::fs::write(dir.path().join(source), "int value;\n").unwrap();
    }
    std::fs::write(
        dir.path().join("CMakeLists.txt"),
        r#"add_library(core STATIC file[1].c [literal].c [=literal].c;plain.c
            "quoted [path];name.c" [=[bracket [path];name.c]=])
"#,
    )
    .unwrap();

    let seeds = CCppMapper
        .map(&MapperContext::for_root(dir.path()))
        .unwrap();
    let library = seeds
        .iter()
        .find(|seed| seed.source == "cmake-lib")
        .expect("CMake library with literal bracket source paths");
    let owned: Vec<_> = library
        .owned_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(owned, sources);
}
