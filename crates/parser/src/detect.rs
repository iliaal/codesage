use std::path::Path;

use codesage_protocol::Language;

/// Pure path-based language detection. `.h` and `.c` always map to C here.
/// For project-aware `.h`-as-C++ routing (when the same project also contains
/// `.cpp`/`.hpp`/etc.), use [`detect_language_with_dialect`] from the discovery
/// layer.
pub fn detect_language(path: &Path) -> Option<Language> {
    detect_language_with_dialect(path, false)
}

/// Path-based language detection with header-dialect override. When
/// `header_is_cpp` is true, bare `.h` files map to [`Language::Cpp`] instead of
/// [`Language::C`]. `.c` always stays C — a `.c` file inside a C++ project is
/// still C by convention, and the C grammar parses it correctly.
pub fn detect_language_with_dialect(path: &Path, header_is_cpp: bool) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "php" => Some(Language::Php),
        "py" | "pyi" => Some(Language::Python),
        "c" => Some(Language::C),
        "h" => Some(if header_is_cpp {
            Language::Cpp
        } else {
            Language::C
        }),
        // Unambiguous C++ source / header / module extensions.
        "cpp" | "cc" | "cxx" | "c++" | "cppm" | "ixx" | "hpp" | "hh" | "hxx" | "h++" | "tpp"
        | "ipp" => Some(Language::Cpp),
        "rs" => Some(Language::Rust),
        "js" | "mjs" | "cjs" | "jsx" => Some(Language::JavaScript),
        "ts" | "tsx" => Some(Language::TypeScript),
        "go" => Some(Language::Go),
        _ => None,
    }
}

/// True for any extension that proves a project is using C++ (i.e. its `.h`
/// files should be parsed as C++ rather than C). The discovery layer scans the
/// file list once, sets a project-wide flag, then re-routes `.h` files.
pub fn is_unambiguous_cpp_extension(ext: &str) -> bool {
    matches!(
        ext,
        "cpp"
            | "cc"
            | "cxx"
            | "c++"
            | "cppm"
            | "ixx"
            | "hpp"
            | "hh"
            | "hxx"
            | "h++"
            | "tpp"
            | "ipp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_extension() {
        assert_eq!(detect_language(Path::new("foo.php")), Some(Language::Php));
    }

    #[test]
    fn python_extensions() {
        assert_eq!(detect_language(Path::new("bar.py")), Some(Language::Python));
        assert_eq!(
            detect_language(Path::new("types.pyi")),
            Some(Language::Python)
        );
    }

    #[test]
    fn c_extensions_default_to_c() {
        assert_eq!(detect_language(Path::new("main.c")), Some(Language::C));
        assert_eq!(detect_language(Path::new("header.h")), Some(Language::C));
    }

    #[test]
    fn h_routes_to_cpp_when_dialect_is_cpp() {
        assert_eq!(
            detect_language_with_dialect(Path::new("header.h"), true),
            Some(Language::Cpp)
        );
        // .c never flips, even when the project is C++.
        assert_eq!(
            detect_language_with_dialect(Path::new("main.c"), true),
            Some(Language::C)
        );
    }

    #[test]
    fn cpp_source_extensions() {
        for ext in [
            "main.cpp",
            "main.cc",
            "main.cxx",
            "main.c++",
            "module.cppm",
            "module.ixx",
        ] {
            assert_eq!(
                detect_language(Path::new(ext)),
                Some(Language::Cpp),
                "{ext} should be C++"
            );
        }
    }

    #[test]
    fn cpp_header_extensions() {
        for ext in ["h.hpp", "h.hh", "h.hxx", "h.h++", "tpl.tpp", "tpl.ipp"] {
            assert_eq!(
                detect_language(Path::new(ext)),
                Some(Language::Cpp),
                "{ext} should be C++"
            );
        }
    }

    #[test]
    fn rust_extension() {
        assert_eq!(detect_language(Path::new("lib.rs")), Some(Language::Rust));
        assert_eq!(detect_language(Path::new("main.rs")), Some(Language::Rust));
    }

    #[test]
    fn javascript_extensions() {
        assert_eq!(
            detect_language(Path::new("app.js")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("index.mjs")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("lib.cjs")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("App.jsx")),
            Some(Language::JavaScript)
        );
    }

    #[test]
    fn typescript_extensions() {
        assert_eq!(
            detect_language(Path::new("app.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            detect_language(Path::new("App.tsx")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn go_extension() {
        assert_eq!(detect_language(Path::new("main.go")), Some(Language::Go));
        assert_eq!(
            detect_language(Path::new("handler_test.go")),
            Some(Language::Go)
        );
    }

    #[test]
    fn unrecognized_extensions() {
        assert_eq!(detect_language(Path::new("readme.txt")), None);
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }

    #[test]
    fn unambiguous_cpp_extension_classifier() {
        assert!(is_unambiguous_cpp_extension("cpp"));
        assert!(is_unambiguous_cpp_extension("hpp"));
        assert!(is_unambiguous_cpp_extension("h++"));
        assert!(!is_unambiguous_cpp_extension("h"));
        assert!(!is_unambiguous_cpp_extension("c"));
        assert!(!is_unambiguous_cpp_extension("rs"));
    }
}
