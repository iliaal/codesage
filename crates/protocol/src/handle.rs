//! Legible entity handles carried on every emitted row.
//!
//! ```text
//! sym:<path>#<qualified>            a symbol, line-independent
//! sym:<path>#<qualified>@<line>     one of several definitions sharing a
//!                                   qualified name inside one file
//! file:<path>
//! dir:<path>
//! chunk:<path>:<start>-<end>
//! feat_<hex16>                      feature ids keep their derivation
//! ```
//!
//! Escaping rule: inside the `path` and `qualified` components, `%`, `#`,
//! and `@` are written as `%25`, `%23`, and `%40`. Nothing else is escaped:
//! `#` is then the only delimiter a `sym:` handle needs, `@` can only be the
//! overload separator, and `chunk:` takes its range from the last `:`, so a
//! `:` inside a path never needs escaping. Decoding leaves any other `%`
//! sequence untouched.
//!
//! Paths are repository-relative: an absolute path, a `..` segment, or a
//! backslash makes a handle unparseable.

use std::collections::HashMap;
use std::fmt;

use crate::Symbol;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Handle {
    Symbol {
        path: String,
        qualified: String,
        /// `line_start`, present only when several definitions in `path`
        /// share `qualified`.
        line: Option<u32>,
    },
    File {
        path: String,
    },
    Dir {
        path: String,
    },
    Chunk {
        path: String,
        start: u32,
        end: u32,
    },
    Feature {
        id: String,
    },
}

const FEATURE_PREFIX: &str = "feat_";
const FEATURE_HEX_LEN: usize = 16;

fn escape(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for c in component.chars() {
        match c {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '@' => out.push_str("%40"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    let mut rest = component;
    while let Some(pos) = rest.find('%') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let decoded = match after.get(..3) {
            Some("%25") => Some('%'),
            Some("%23") => Some('#'),
            Some("%40") => Some('@'),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[3..];
            }
            None => {
                out.push('%');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A repository-relative path that cannot escape the repository.
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|segment| segment == "..")
}

fn parse_line(digits: &str) -> Option<u32> {
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

impl Handle {
    pub fn symbol(
        path: impl Into<String>,
        qualified: impl Into<String>,
        line: Option<u32>,
    ) -> Self {
        Handle::Symbol {
            path: path.into(),
            qualified: qualified.into(),
            line,
        }
    }

    pub fn file(path: impl Into<String>) -> Self {
        Handle::File { path: path.into() }
    }

    pub fn dir(path: impl Into<String>) -> Self {
        Handle::Dir { path: path.into() }
    }

    pub fn chunk(path: impl Into<String>, start: u32, end: u32) -> Self {
        Handle::Chunk {
            path: path.into(),
            start,
            end,
        }
    }

    /// The repository-relative path a handle names; `None` for a feature.
    pub fn path(&self) -> Option<&str> {
        match self {
            Handle::Symbol { path, .. }
            | Handle::File { path }
            | Handle::Dir { path }
            | Handle::Chunk { path, .. } => Some(path),
            Handle::Feature { .. } => None,
        }
    }

    /// Parse a handle; `None` for anything that is not one, including a
    /// handle whose path is absolute or carries a `..` segment.
    pub fn parse(input: &str) -> Option<Handle> {
        if let Some(hex) = input.strip_prefix(FEATURE_PREFIX) {
            let well_formed = hex.len() == FEATURE_HEX_LEN
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            return well_formed.then(|| Handle::Feature {
                id: input.to_string(),
            });
        }
        if let Some(rest) = input.strip_prefix("sym:") {
            let (path, tail) = rest.split_once('#')?;
            let (qualified, line) = match tail.rsplit_once('@') {
                Some((qualified, digits)) => (qualified, Some(parse_line(digits)?)),
                None => (tail, None),
            };
            // A raw `@` is escaped inside components, so a second one is a
            // malformed suffix rather than part of the name.
            if qualified.contains('@') {
                return None;
            }
            let path = unescape(path);
            let qualified = unescape(qualified);
            return (valid_path(&path) && !qualified.is_empty()).then_some(Handle::Symbol {
                path,
                qualified,
                line,
            });
        }
        if let Some(rest) = input.strip_prefix("file:") {
            let path = unescape(rest);
            return valid_path(&path).then_some(Handle::File { path });
        }
        if let Some(rest) = input.strip_prefix("dir:") {
            let trimmed = rest.strip_suffix('/').unwrap_or(rest);
            let path = unescape(trimmed);
            return valid_path(&path).then_some(Handle::Dir { path });
        }
        if let Some(rest) = input.strip_prefix("chunk:") {
            let (path, range) = rest.rsplit_once(':')?;
            let (start, end) = range.split_once('-')?;
            let start = parse_line(start)?;
            let end = parse_line(end)?;
            let path = unescape(path);
            return (valid_path(&path) && start <= end).then_some(Handle::Chunk {
                path,
                start,
                end,
            });
        }
        None
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Handle::Symbol {
                path,
                qualified,
                line,
            } => {
                write!(f, "sym:{}#{}", escape(path), escape(qualified))?;
                if let Some(line) = line {
                    write!(f, "@{line}")?;
                }
                Ok(())
            }
            Handle::File { path } => write!(f, "file:{}", escape(path)),
            Handle::Dir { path } => write!(f, "dir:{}", escape(path)),
            Handle::Chunk { path, start, end } => {
                write!(f, "chunk:{}:{start}-{end}", escape(path))
            }
            Handle::Feature { id } => f.write_str(id),
        }
    }
}

/// Flag every definition that shares its `(file_path, qualified_name)` with
/// another one in `symbols`, so its handle carries `@line_start`. Exact only
/// when `symbols` holds every definition of each name it contains for each
/// file it contains, which is what the storage read paths return.
pub fn mark_overloads(symbols: &mut [Symbol]) {
    let mut counts: HashMap<(&str, &str), usize> = HashMap::with_capacity(symbols.len());
    for s in symbols.iter() {
        *counts
            .entry((s.file_path.as_str(), s.qualified_name.as_str()))
            .or_insert(0) += 1;
    }
    let overloaded: Vec<bool> = symbols
        .iter()
        .map(|s| counts[&(s.file_path.as_str(), s.qualified_name.as_str())] > 1)
        .collect();
    for (s, flag) in symbols.iter_mut().zip(overloaded) {
        s.overloaded = flag;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SymbolKind;

    fn round_trip(handle: Handle) {
        let text = handle.to_string();
        assert_eq!(Handle::parse(&text), Some(handle), "{text}");
    }

    #[test]
    fn symbol_handles_round_trip() {
        round_trip(Handle::symbol(
            "crates/graph/src/search.rs",
            "search_page",
            None,
        ));
        round_trip(Handle::symbol("src/Db.php", "App\\Db::open", None));
        round_trip(Handle::symbol("src/db.rs", "Database::open", Some(120)));
        round_trip(Handle::symbol("pkg/mod.py", "Class.method", None));
        assert_eq!(
            Handle::symbol("src/db.rs", "Database::open", Some(120)).to_string(),
            "sym:src/db.rs#Database::open@120"
        );
    }

    #[test]
    fn file_dir_chunk_feature_round_trip() {
        round_trip(Handle::file("src/lib.rs"));
        round_trip(Handle::dir("crates/graph/src"));
        round_trip(Handle::chunk("src/lib.rs", 10, 42));
        round_trip(Handle::Feature {
            id: "feat_0123456789abcdef".to_string(),
        });
        assert_eq!(
            Handle::chunk("src/lib.rs", 10, 42).to_string(),
            "chunk:src/lib.rs:10-42"
        );
        assert_eq!(
            Handle::parse("dir:crates/graph/"),
            Some(Handle::dir("crates/graph"))
        );
    }

    #[test]
    fn reserved_characters_in_components_are_escaped() {
        let awkward = Handle::symbol("odd/a#b@c:d%e.rs", "Ns::f#g@h%", Some(7));
        let text = awkward.to_string();
        assert_eq!(text, "sym:odd/a%23b%40c:d%25e.rs#Ns::f%23g%40h%25@7");
        assert_eq!(Handle::parse(&text), Some(awkward));

        round_trip(Handle::file("with:colon/and#hash@at.rs"));
        round_trip(Handle::chunk("with:colon/and#hash.rs", 1, 3));
        round_trip(Handle::dir("weird%dir/x@y"));
        assert_eq!(
            Handle::parse("chunk:with:colon/a.rs:1-3"),
            Some(Handle::chunk("with:colon/a.rs", 1, 3))
        );
    }

    #[test]
    fn unknown_percent_sequences_stay_literal() {
        assert_eq!(
            Handle::parse("file:src/100%done.rs"),
            Some(Handle::file("src/100%done.rs"))
        );
        assert_eq!(Handle::parse("file:a%"), Some(Handle::file("a%")));
    }

    #[test]
    fn path_escapes_are_rejected() {
        for bad in [
            "file:/etc/passwd",
            "file:../secret",
            "file:src/../../x.rs",
            "sym:/abs.rs#f",
            "sym:a/..#f",
            "dir:..",
            "dir:/",
            "chunk:../a.rs:1-2",
            "file:src\\x.rs",
            "file:",
            "sym:#f",
            "file:a\nb",
        ] {
            assert_eq!(Handle::parse(bad), None, "{bad}");
        }
        assert_eq!(
            Handle::parse("file:src/..hidden/x..rs"),
            Some(Handle::file("src/..hidden/x..rs")),
            "`..` is rejected only as a whole segment"
        );
    }

    #[test]
    fn malformed_handles_are_rejected() {
        for bad in [
            "sym:a.rs",
            "sym:a.rs#",
            "sym:a.rs#f@",
            "sym:a.rs#f@x",
            "sym:a.rs#f@1@2",
            "chunk:a.rs",
            "chunk:a.rs:1",
            "chunk:a.rs:x-2",
            "chunk:a.rs:5-2",
            "feat_",
            "feat_0123",
            "feat_0123456789ABCDEF",
            "feat_0123456789abcdefg",
            "symbol:a.rs#f",
            "",
            "a.rs",
        ] {
            assert_eq!(Handle::parse(bad), None, "{bad}");
        }
    }

    fn symbol(file_path: &str, qualified: &str, line_start: u32) -> Symbol {
        Symbol {
            name: qualified.rsplit("::").next().unwrap().to_string(),
            qualified_name: qualified.to_string(),
            kind: SymbolKind::Function,
            file_path: file_path.to_string(),
            line_start,
            line_end: line_start + 1,
            col_start: 0,
            col_end: 0,
            rationale: Vec::new(),
            overloaded: false,
        }
    }

    #[test]
    fn overloads_get_line_handles_only_within_one_file_and_name() {
        let mut symbols = vec![
            symbol("a.cpp", "Foo::run", 10),
            symbol("a.cpp", "Foo::run", 30),
            symbol("a.cpp", "Foo::stop", 50),
            symbol("b.cpp", "Foo::run", 10),
        ];
        mark_overloads(&mut symbols);
        assert_eq!(
            symbols
                .iter()
                .map(|s| s.handle().to_string())
                .collect::<Vec<_>>(),
            [
                "sym:a.cpp#Foo::run@10",
                "sym:a.cpp#Foo::run@30",
                "sym:a.cpp#Foo::stop",
                "sym:b.cpp#Foo::run",
            ]
        );
    }

    #[test]
    fn symbol_handle_falls_back_to_name_without_qualified_name() {
        let mut s = symbol("a.rs", "run", 1);
        s.qualified_name.clear();
        assert_eq!(s.handle().to_string(), "sym:a.rs#run");
    }
}
