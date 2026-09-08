use std::path::Path;

use codesage_protocol::Language;

/// Path-based detection; `.h` and `.c` map to C. Discovery uses
/// [`detect_language_with_dialect`] for project-aware header routing.
pub fn detect_language(path: &Path) -> Option<Language> {
    detect_language_with_dialect(path, false)
}

/// With `header_is_cpp`, `.h` maps to C++; `.c` always maps to C.
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
        // C++ error recovery tolerates CUDA qualifiers. CUDA alone must not
        // switch the project's C headers to C++.
        "cu" | "cuh" => Some(Language::Cpp),
        _ if is_unambiguous_cpp_extension(ext) => Some(Language::Cpp),
        "java" => Some(Language::Java),
        "rs" => Some(Language::Rust),
        "js" | "mjs" | "cjs" | "jsx" => Some(Language::JavaScript),
        "ts" | "tsx" | "mts" | "cts" => Some(Language::TypeScript),
        "go" => Some(Language::Go),
        _ => None,
    }
}

/// Extensions that switch project-wide `.h` parsing to C++.
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
    fn cuda_extensions_are_cpp() {
        assert_eq!(detect_language(Path::new("kernel.cu")), Some(Language::Cpp));
        assert_eq!(
            detect_language(Path::new("kernel.cuh")),
            Some(Language::Cpp)
        );
    }

    #[test]
    fn cuda_extension_does_not_route_c_headers_to_cpp() {
        assert!(!is_unambiguous_cpp_extension("cu"));
        assert!(!is_unambiguous_cpp_extension("cuh"));
    }

    #[test]
    fn rust_extension() {
        assert_eq!(detect_language(Path::new("lib.rs")), Some(Language::Rust));
        assert_eq!(detect_language(Path::new("main.rs")), Some(Language::Rust));
    }

    #[test]
    fn java_extension() {
        assert_eq!(
            detect_language(Path::new("Example.java")),
            Some(Language::Java)
        );
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
        assert_eq!(
            detect_language(Path::new("mod.mts")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            detect_language(Path::new("mod.cts")),
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
