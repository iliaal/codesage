use codesage_parser::{
    extract::extract_symbols, fingerprint::file_fingerprints, parse::parse_file,
};
use codesage_protocol::{Language, SymbolKind, Visibility};

#[test]
fn c_wrapped_names_preserve_distinct_definitions_and_ranges() {
    let source = "#define FNAME(name) name\n\
static void sink(void) {}\n\
static void ordinary(void) { sink(); }\n\
static void FNAME(first)(int value) {\n    sink();\n}\n\
static void FNAME(second)(void) { sink(); }\n";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "api.c").unwrap();
    let functions: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function)
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.line_start,
                symbol.line_end,
                symbol.col_start,
                symbol.visibility,
            )
        })
        .collect();
    assert_eq!(
        functions,
        [
            ("sink", 2, 2, 0, Some(Visibility::File)),
            ("ordinary", 3, 3, 0, Some(Visibility::File)),
            ("first", 4, 6, 0, Some(Visibility::File)),
            ("second", 7, 7, 0, Some(Visibility::File)),
        ],
        "{}",
        tree.root_node().to_sexp()
    );
    let first = symbols
        .iter()
        .find(|symbol| symbol.name == "first")
        .unwrap();
    assert_eq!(first.col_end, 1);
    let second = symbols
        .iter()
        .find(|symbol| symbol.name == "second")
        .unwrap();
    assert_eq!(
        second.col_end as usize,
        source.lines().nth(6).unwrap().len()
    );
}

#[test]
fn cpp_wrapped_names_keep_qualified_methods_and_declarations() {
    let source = "#define __API_HOOK(name) name\n\
static void helper() {}\n\
class CBasePlayer {\npublic:\n    void ReloadWeapons(int slot);\n};\n\
void CBasePlayer::__API_HOOK(ReloadWeapons)(int slot) { helper(); }\n";
    let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::Cpp, "api.cpp").unwrap();
    let methods: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.qualified_name.as_str(),
                symbol.line_start,
                symbol.line_end,
                symbol.col_start,
            )
        })
        .collect();
    assert_eq!(
        methods,
        [
            ("ReloadWeapons", "CBasePlayer::ReloadWeapons", 5, 5, 4),
            ("ReloadWeapons", "CBasePlayer::ReloadWeapons", 7, 7, 0),
        ],
        "{}",
        tree.root_node().to_sexp()
    );
}

#[test]
fn wrapped_names_reject_nondefinitions_and_unproven_expansions() {
    let cases = [
        "#define FNAME(name) name\nvoid FNAME(forbidden_prototype)(void);\n",
        "#define FNAME(name) name\nvoid ordinary(void) { FNAME(forbidden_call)(); }\n",
        "int ordinary(void) { return factory()(forbidden_argument); }\n",
        "int (*factory(void))(int forbidden_parameter) { return 0; }\n",
        "#define CHOOSE(first, second) first\nvoid CHOOSE(forbidden_first, forbidden_second)(void) {}\n",
        "#define PASTE(name) prefix_##name\nvoid PASTE(forbidden_pasted)(void) {}\n",
        "#define FIXED(name) actual_name\nvoid FIXED(forbidden_replaced)(void) {}\n",
        "#define VARIADIC(name, ...) name\nvoid VARIADIC(forbidden_variadic)(void) {}\n",
        "void UNKNOWN(forbidden_unknown)(void) {}\n",
        "#define FNAME(name) name\nvoid FNAME((forbidden_parenthesized))(void) {}\n",
    ];
    for language in [Language::C, Language::Cpp] {
        for source in cases {
            let tree = parse_file(source.as_bytes(), language).unwrap();
            let symbols = extract_symbols(&tree, source.as_bytes(), language, "api.c").unwrap();
            assert!(
                !symbols
                    .iter()
                    .any(|symbol| symbol.name.starts_with("forbidden_")),
                "{language:?}: {source}\n{symbols:#?}\n{}",
                tree.root_node().to_sexp()
            );
        }
    }
}

#[test]
fn wrapped_names_require_uninterrupted_unconditional_local_identity_evidence() {
    for language in [Language::C, Language::Cpp] {
        for interruption in [
            "#undef FNAME\n",
            "#define FNAME(name) replacement\n",
            "#ifdef OPTION\n#define FNAME(name) replacement\n#endif\n",
            "#include \"unknown.h\"\n",
        ] {
            let source = format!(
                "#define FNAME(name) name\nvoid FNAME(before)(void) {{}}\n{interruption}void FNAME(forbidden_after)(void) {{}}\n#define FNAME(name) name\nvoid FNAME(restored)(void) {{}}\n"
            );
            let tree = parse_file(source.as_bytes(), language).unwrap();
            let symbols = extract_symbols(&tree, source.as_bytes(), language, "api.c").unwrap();
            let functions: Vec<_> = symbols
                .iter()
                .filter(|symbol| symbol.kind == SymbolKind::Function)
                .map(|symbol| symbol.name.as_str())
                .collect();
            assert_eq!(functions, ["before", "restored"], "{language:?}: {source}");
        }
        let source = "#ifdef OPTION\n#define FNAME(name) name\n#endif\nvoid FNAME(forbidden_conditional)(void) {}\n";
        let tree = parse_file(source.as_bytes(), language).unwrap();
        let symbols = extract_symbols(&tree, source.as_bytes(), language, "api.c").unwrap();
        assert!(
            symbols
                .iter()
                .all(|symbol| symbol.name != "forbidden_conditional")
        );
    }
}

#[test]
fn cpp_wrapped_in_class_methods_and_fingerprints_share_identity() {
    let source = "#define FNAME(name) name\n\
namespace game {\nclass Player {\npublic:\n\
    int FNAME(score)(int value) { return helper(1) + helper(2) + helper(3) + helper(4) + helper(5); }\n\
};\n}\n\
int game::Player::FNAME(other)(int value) { return helper(1) + helper(2) + helper(3) + helper(4) + helper(5); }\n";
    let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::Cpp, "api.cpp").unwrap();
    let methods: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.qualified_name.as_str(),
                symbol.line_start,
            )
        })
        .collect();
    assert_eq!(
        methods,
        [
            ("score", "game::Player::score", 5),
            ("other", "game::Player::other", 8)
        ]
    );
    let fingerprints = file_fingerprints(&tree, source.as_bytes(), Language::Cpp);
    assert_eq!(
        fingerprints
            .iter()
            .map(|fingerprint| (
                fingerprint.name.as_str(),
                fingerprint.kind,
                fingerprint.line_start
            ))
            .collect::<Vec<_>>(),
        [
            ("score", SymbolKind::Method, 5),
            ("other", SymbolKind::Method, 8)
        ]
    );
}

#[test]
fn wrapped_name_fingerprints_reject_rewriting_macros() {
    let body = "{ return helper(1) + helper(2) + helper(3) + helper(4) + helper(5); }";
    let source = format!(
        "#define FNAME(name) name\n#define PASTE(name) prefix_##name\nint FNAME(accepted)(void) {body}\nint PASTE(forbidden_fingerprint)(void) {body}\n"
    );
    for language in [Language::C, Language::Cpp] {
        let tree = parse_file(source.as_bytes(), language).unwrap();
        let fingerprints = file_fingerprints(&tree, source.as_bytes(), language);
        assert_eq!(
            fingerprints
                .iter()
                .map(|fingerprint| fingerprint.name.as_str())
                .collect::<Vec<_>>(),
            ["accepted"]
        );
    }
}

#[test]
fn cpp_wrapped_qualification_uses_scope_fields_across_trivia() {
    for wrapper in [
        "example::Owner :: FNAME",
        "example::Owner:: /* why */ FNAME",
        "example::Owner::\\\nFNAME",
        "example /* outer */ :: Owner /* inner */ :: FNAME",
    ] {
        let source = format!(
            "#define FNAME(name) name\nnamespace example {{ class Owner {{ public: int first(); }}; }}\nint {wrapper}(first)() {{ return helper(1) + helper(2) + helper(3) + helper(4) + helper(5); }}\n"
        );
        let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
        let symbols = extract_symbols(&tree, source.as_bytes(), Language::Cpp, "api.cpp").unwrap();
        let definitions: Vec<_> = symbols
            .iter()
            .filter(|symbol| symbol.name == "first")
            .collect();
        assert_eq!(definitions.len(), 2, "{wrapper}: {symbols:#?}");
        assert!(
            definitions
                .iter()
                .all(|symbol| symbol.qualified_name == "example::Owner::first")
        );
        let definition = definitions
            .iter()
            .find(|symbol| symbol.line_start == 3)
            .unwrap();
        assert_eq!(definition.kind, SymbolKind::Method);
        assert_eq!(definition.line_end as usize, source.lines().count());
        let fingerprints = file_fingerprints(&tree, source.as_bytes(), Language::Cpp);
        assert_eq!(fingerprints.len(), 1, "{wrapper}");
        assert_eq!(
            (
                fingerprints[0].name.as_str(),
                fingerprints[0].kind,
                fingerprints[0].line_start,
                fingerprints[0].line_end
            ),
            (
                "first",
                SymbolKind::Method,
                definition.line_start,
                definition.line_end
            )
        );
    }
}
