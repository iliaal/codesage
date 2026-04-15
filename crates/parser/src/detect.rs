use std::path::Path;

use codesage_protocol::Language;

pub fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "php" => Some(Language::Php),
        "py" | "pyi" => Some(Language::Python),
        "c" | "h" => Some(Language::C),
        "rs" => Some(Language::Rust),
        "js" | "mjs" | "cjs" | "jsx" => Some(Language::JavaScript),
        "ts" | "tsx" => Some(Language::TypeScript),
        _ => None,
    }
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
    fn c_extensions() {
        assert_eq!(detect_language(Path::new("main.c")), Some(Language::C));
        assert_eq!(detect_language(Path::new("header.h")), Some(Language::C));
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
    fn unrecognized_extensions() {
        assert_eq!(detect_language(Path::new("readme.txt")), None);
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }
}
