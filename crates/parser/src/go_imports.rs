use std::borrow::Cow;

pub fn decode_import_path(literal: &str) -> Option<Cow<'_, str>> {
    if let Some(raw) = literal
        .strip_prefix('`')
        .and_then(|raw| raw.strip_suffix('`'))
    {
        if raw.contains('`') {
            return None;
        }
        return Some(if raw.contains('\r') {
            Cow::Owned(raw.replace('\r', ""))
        } else {
            Cow::Borrowed(raw)
        });
    }
    decode_interpreted(literal)
}

fn decode_interpreted(literal: &str) -> Option<Cow<'_, str>> {
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    if !inner.contains(['\\', '"', '\n']) {
        return Some(Cow::Borrowed(inner));
    }
    let mut output = Vec::with_capacity(inner.len());
    let mut bytes = inner.bytes();
    while let Some(byte) = bytes.next() {
        let decoded = match byte {
            b'"' | b'\n' => return None,
            b'\\' => match bytes.next()? {
                b'a' => 7,
                b'b' => 8,
                b'f' => 12,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'v' => 11,
                escaped @ (b'\\' | b'"') => escaped,
                b'x' => u8::try_from(digits(&mut bytes, 2, 16)?).ok()?,
                escape @ (b'u' | b'U') => {
                    let count = if escape == b'u' { 4 } else { 8 };
                    let scalar = char::from_u32(digits(&mut bytes, count, 16)?)?;
                    let mut utf8 = [0; 4];
                    output.extend_from_slice(scalar.encode_utf8(&mut utf8).as_bytes());
                    continue;
                }
                first @ b'0'..=b'7' => {
                    let value = u32::from(first - b'0') * 64 + digits(&mut bytes, 2, 8)?;
                    u8::try_from(value).ok()?
                }
                _ => return None,
            },
            ordinary => ordinary,
        };
        output.push(decoded);
    }
    // Go byte escapes can produce non-UTF-8 strings. Imported paths require UTF-8.
    String::from_utf8(output).ok().map(Cow::Owned)
}

fn digits(bytes: &mut impl Iterator<Item = u8>, count: usize, radix: u32) -> Option<u32> {
    (0..count).try_fold(0, |value, _| {
        Some(value * radix + char::from(bytes.next()?).to_digit(radix)?)
    })
}

/// Decide whether the source declares an ignored generator or documentation package.
/// A truncated or unrecognized header leaves the membership unknown.
pub fn package_header_excluded(bytes: &[u8], complete: bool) -> Option<bool> {
    let source = match str::from_utf8(bytes) {
        Ok(source) => source,
        Err(err) => str::from_utf8(&bytes[..err.valid_up_to()]).ok()?,
    };
    let complete = complete && source.len() == bytes.len();
    let mut rest = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut modern = None;
    let mut legacy = false;
    let mut legacy_pending = false;
    let mut legacy_header = true;
    let mut at_line_start = true;
    loop {
        let input = rest.trim_start();
        let newlines = rest[..rest.len() - input.len()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .take(2)
            .count();
        at_line_start |= newlines > 0;
        if legacy_header && newlines == 2 {
            legacy |= legacy_pending;
        }
        if let Some(comment) = input.strip_prefix("//") {
            let end = comment.find('\n')?;
            let line = &comment[..end];
            if at_line_start {
                if let Some(tag) = constraint_suffix(line, "go:build") {
                    if modern.replace(tag == "ignore").is_some() {
                        return None;
                    }
                } else if legacy_header {
                    legacy_pending |=
                        constraint_suffix(line.trim_start(), "+build") == Some("ignore");
                }
            }
            rest = &comment[end..];
        } else if let Some(comment) = input.strip_prefix("/*") {
            rest = comment.split_once("*/")?.1;
            legacy_header = false;
        } else {
            let clause = input.strip_prefix("package")?;
            if clause.starts_with(char::is_whitespace)
                || clause.starts_with("//")
                || clause.starts_with("/*")
            {
                let package = header_package_name(clause, complete)?;
                return Some(modern.unwrap_or(legacy) || package == "documentation");
            }
            return None;
        }
        at_line_start = false;
    }
}

fn header_package_name(mut clause: &str, complete: bool) -> Option<&str> {
    loop {
        let token = clause.trim_start();
        if let Some(comment) = token.strip_prefix("//") {
            clause = comment.split_once('\n')?.1;
        } else if let Some(comment) = token.strip_prefix("/*") {
            clause = comment.split_once("*/")?.1;
        } else {
            if !token.starts_with(|c: char| c == '_' || c.is_alphabetic()) {
                return None;
            }
            let end = token.find(|c: char| c != '_' && !c.is_alphanumeric());
            return end
                .map(|end| &token[..end])
                .or_else(|| complete.then_some(token));
        }
    }
}

fn constraint_suffix<'a>(comment: &'a str, directive: &str) -> Option<&'a str> {
    let suffix = comment.strip_prefix(directive)?;
    let trimmed = suffix.trim_start();
    (suffix.is_empty() || trimmed.len() != suffix.len()).then_some(trimmed.trim_end())
}

#[cfg(test)]
mod tests {
    use super::{decode_import_path, package_header_excluded};

    #[test]
    fn go_path_decoder_uses_the_compiler_string_value() {
        for (literal, path) in [
            (r#""example.com/wire""#, "example.com/wire"),
            (
                r#""example.com/\x77\151\u0072\U00000065""#,
                "example.com/wire",
            ),
            (r#""example.com/\xc3\xa9""#, "example.com/é"),
            (r#""example.com/\u00e9""#, "example.com/é"),
            (r#""example.com/é""#, "example.com/é"),
            ("`example.com/wi\rre`", "example.com/wire"),
            ("`example.com/\\x77ire`", "example.com/\\x77ire"),
        ] {
            assert_eq!(decode_import_path(literal).as_deref(), Some(path));
        }
        for invalid in [
            r#""\xff""#,
            r#""\400""#,
            r#""\uD800""#,
            r#""\x0Q""#,
            r#""\'""#,
            "`in`valid`",
        ] {
            assert!(decode_import_path(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn only_actual_go_ignored_headers_exclude_file_membership() {
        for (source, expected) in [
            ("//go:build\tignore\n\npackage main\n", Some(true)),
            ("//+build ignore\n\npackage main\n", Some(true)),
            ("//+build ignore\npackage app\n", Some(false)),
            (
                "//go:build !ignore\n//+build ignore\n\npackage app\n",
                Some(false),
            ),
            ("/* //go:build ignore */\npackage app\n", Some(false)),
            (
                "/* comment */ //go:build ignore\npackage app\n",
                Some(false),
            ),
            ("package app\n//go:build ignore\n", Some(false)),
            ("package /* comment */ documentation", Some(true)),
            ("package documentation2\n", Some(false)),
            ("//go:build ignore\n/*", None),
            ("//go:build ignore\n\npacka", None),
        ] {
            assert_eq!(
                package_header_excluded(source.as_bytes(), true),
                expected,
                "{source}"
            );
        }
        assert_eq!(
            package_header_excluded(b"package documentation", false),
            None
        );
    }
}
