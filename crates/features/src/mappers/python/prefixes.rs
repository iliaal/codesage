use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use regex::Regex;

use super::{PythonFramework, find_balanced_close, split_top_level_args};

pub(super) struct Receiver {
    pub name: String,
    pub prefixes: BTreeSet<String>,
}

pub(super) fn receivers(
    source: &str,
    constructors: &Regex,
    framework: PythonFramework,
) -> Result<Vec<Receiver>> {
    let mut found = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for cap in constructors.captures_iter(source) {
        let name = cap[1].to_string();
        let kind = &cap[2];
        let open = cap.get(0).expect("complete constructor match").end() - 1;
        let Some(close) = find_balanced_close(source.as_bytes(), open) else {
            continue;
        };
        let args = split_top_level_args(&source[open + 1..close]);
        let prefix = match kind {
            "Blueprint" => keyword_prefix(&args, "url_prefix", true),
            "APIRouter" => keyword_prefix(&args, "prefix", false)
                .filter(|p| p.is_empty() || (p.starts_with('/') && !p.ends_with('/'))),
            _ => Some(String::new()),
        };
        if found
            .insert(name.clone(), (kind.to_string(), prefix))
            .is_some()
        {
            ambiguous.insert(name);
        }
    }
    let mut mounted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let assignments = Regex::new(
        r"(?m)^[ \t]*([A-Za-z_]\w*)(?:[ \t]*\.[ \t]*(prefix|url_prefix))?[ \t]*(?::[^=\n]+)?(?:\*\*|//|<<|>>|[+\-*/%@&|^])?=",
    )?;
    let mut assigned = BTreeSet::new();
    for cap in assignments.captures_iter(source) {
        let name = cap[1].to_string();
        if cap.get(2).is_some() || !assigned.insert(name.clone()) {
            ambiguous.insert(name);
        }
    }
    if matches!(framework, PythonFramework::Flask) {
        let registration = Regex::new(r"(?m)^[ \t]*([A-Za-z_]\w*)\s*\.\s*register_blueprint\s*\(")?;
        for cap in registration.captures_iter(source) {
            let open = cap.get(0).expect("complete registration match").end() - 1;
            let Some(close) = find_balanced_close(source.as_bytes(), open) else {
                continue;
            };
            let args = split_top_level_args(&source[open + 1..close]);
            let Some(target) =
                keyword(&args, "blueprint").or_else(|| args.first().map(String::as_str))
            else {
                continue;
            };
            let Some((kind, default)) = found.get(target) else {
                continue;
            };
            if kind != "Blueprint" {
                continue;
            }
            let prefixes = mounted.entry(target.to_string()).or_default();
            if found.get(&cap[1]).is_none_or(|(kind, _)| kind != "Flask")
                || ambiguous.contains(&cap[1])
            {
                continue;
            }
            let prefix = match keyword(&args, "url_prefix") {
                Some(value) if value != "None" => string_literal(value),
                _ if args.iter().any(|arg| arg.starts_with("**")) => None,
                _ => default.clone(),
            };
            if let Some(prefix) = prefix {
                prefixes.insert(prefix);
            }
        }
    }
    Ok(found
        .into_iter()
        .filter_map(|(name, (_, prefix))| {
            if ambiguous.contains(&name) {
                return None;
            }
            let prefixes = mounted
                .remove(&name)
                .unwrap_or_else(|| prefix.into_iter().collect());
            Some(Receiver { name, prefixes })
        })
        .collect())
}

fn keyword<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().find_map(|arg| {
        let (key, value) = arg.split_once('=')?;
        (key.trim() == name).then(|| value.trim())
    })
}

fn keyword_prefix(args: &[String], name: &str, none_is_empty: bool) -> Option<String> {
    match keyword(args, name) {
        Some("None") if none_is_empty => Some(String::new()),
        Some(value) => string_literal(value),
        None if args.iter().any(|arg| arg.starts_with("**")) => None,
        None => Some(String::new()),
    }
}

pub(super) fn join_route(framework: PythonFramework, prefix: &str, route: &str) -> String {
    match framework {
        PythonFramework::FastApi => format!("{prefix}{route}"),
        PythonFramework::Flask if prefix.is_empty() => route.to_string(),
        PythonFramework::Flask if route.is_empty() => prefix.to_string(),
        PythonFramework::Flask => format!(
            "{}/{}",
            prefix.trim_end_matches('/'),
            route.trim_start_matches('/')
        ),
    }
}

pub(super) fn string_literal(input: &str) -> Option<String> {
    let input = input.trim();
    let (raw, value) = match input.as_bytes().first()? {
        b'r' | b'R' => (true, &input[1..]),
        b'u' | b'U' => (false, &input[1..]),
        _ => (false, input),
    };
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let mut chars = value[1..].chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == quote {
            return chars.as_str().is_empty().then_some(out);
        }
        if matches!(c, '\n' | '\r') {
            return None;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        let escape = chars.next()?;
        if raw {
            out.push('\\');
            out.push(escape);
            continue;
        }
        match escape {
            '\\' | '\'' | '"' => out.push(escape),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\n' => {}
            'x' | 'u' | 'U' => {
                let count = match escape {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let mut code = 0;
                for _ in 0..count {
                    code = code * 16 + chars.next()?.to_digit(16)?;
                }
                out.push(char::from_u32(code)?);
            }
            '0'..='7' | 'N' => return None,
            _ => {
                out.push('\\');
                out.push(escape);
            }
        }
    }
    None
}

pub(super) fn route_source(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
        } else if matches!(bytes[i], b'\'' | b'"') {
            let quote = bytes[i];
            let triple = bytes.get(i..i + 3).is_some_and(|s| s == [quote; 3]);
            let start = i;
            i += if triple { 3 } else { 1 };
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == quote
                    && (!triple || bytes.get(i..i + 3).is_some_and(|s| s == [quote; 3]))
                {
                    i += if triple { 3 } else { 1 };
                    break;
                } else {
                    i += 1;
                }
            }
            if triple {
                for b in &mut out[start..i] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).expect("only complete UTF-8 spans are blanked")
}

#[cfg(test)]
mod tests {
    use super::super::{FeatureMapper, MapperContext, PythonMapper};
    use tempfile::tempdir;

    fn routes(source: &str) -> Vec<String> {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("app.py"), source).unwrap();
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let mut routes: Vec<_> = seeds.into_iter().filter_map(|s| s.entry_route).collect();
        routes.sort_unstable();
        routes
    }

    #[test]
    fn fastapi_router_prefix_preserves_exact_concatenation() {
        assert_eq!(
            routes(
                r#"
from fastapi import APIRouter
router = APIRouter(
    tags=["demo"],
    prefix='/api',
)
@router.get("")
def root(): pass
@router.post("//users")
def users(): pass
@router.get("users")
def bare(): pass
"#
            ),
            ["GET /api", "GET /apiusers", "POST /api//users"]
        );
    }

    #[test]
    fn flask_none_registration_retains_constructor_while_empty_overrides() {
        assert_eq!(
            routes(
                r#"
from flask import Flask, Blueprint
app = Flask(__name__)
bp = Blueprint('api', __name__, url_prefix='/base/')
@bp.route('/users')
def users(): pass
app.register_blueprint(bp, url_prefix=None, name='fallback')
app.register_blueprint(bp, url_prefix='', name='empty')
"#
            ),
            ["GET /base/users", "GET /users"]
        );
    }

    #[test]
    fn flask_unmounted_constructor_prefix_preserves_empty_route() {
        assert_eq!(
            routes(
                r#"
from flask import Blueprint
bp = Blueprint('api', __name__, url_prefix='/base/')
@bp.route('')
def root(): pass
"#
            ),
            ["GET /base/"]
        );
    }

    #[test]
    fn unresolved_constructor_prefixes_do_not_publish_unprefixed_routes() {
        for value in [
            "PREFIX",
            "f'/v{version}'",
            "'/v1' + SUFFIX",
            "b'/bytes'",
            "None",
        ] {
            let source = format!(
                "from fastapi import APIRouter\nrouter = APIRouter(prefix={value})\n@router.get('/leak')\ndef handler(): pass\n"
            );
            assert!(routes(&source).is_empty(), "{value}");
        }
        for value in ["PREFIX", "f'/v{version}'", "'/v1' + SUFFIX", "b'/bytes'"] {
            let source = format!(
                "from flask import Blueprint\nbp = Blueprint('api', __name__, url_prefix={value})\n@bp.route('/leak')\ndef handler(): pass\n"
            );
            assert!(routes(&source).is_empty(), "{value}");
        }
    }

    #[test]
    fn dynamic_registration_does_not_fall_back_to_constructor_prefix() {
        assert_eq!(
            routes(
                r#"
from flask import Flask, Blueprint
app = Flask(__name__)
bp = Blueprint('api', __name__, url_prefix='/default')
@bp.route('/users')
def users(): pass
app.register_blueprint(bp, url_prefix=PREFIX, name='dynamic')
app.register_blueprint(bp, url_prefix='/known', name='literal')
"#
            ),
            ["GET /known/users"]
        );
    }

    #[test]
    fn prefix_scanner_ignores_docstring_declarations_and_preserves_hash_literals() {
        assert_eq!(
            routes(
                r#"
"""
from flask import Blueprint
decoy = Blueprint('decoy', __name__, url_prefix='/fake')
@decoy.route('/fake')
def fake(): pass
"""
from fastapi import APIRouter
router = APIRouter(prefix='/hash#tag') # prefix='/fake'
@router.get('/users')
def users(): pass
"#
            ),
            ["GET /hash#tag/users"]
        );
    }

    #[test]
    fn strict_literal_rejects_route_expressions() {
        for value in ["f'/v{version}'", "'/v1' + SUFFIX", "'/v1' '/v2'"] {
            assert!(routes(&format!("from fastapi import APIRouter\nrouter = APIRouter(prefix='/api')\n@router.get({value})\ndef handler(): pass\n")).is_empty());
        }
    }

    #[test]
    fn ambiguous_same_named_receivers_do_not_share_prefixes() {
        assert!(
            routes(
                r#"
from fastapi import APIRouter
def one():
    router = APIRouter(prefix='/one')
    @router.get('/users')
    def users(): pass
def two():
    router = APIRouter(prefix='/two')
    @router.get('/users')
    def users(): pass
"#
            )
            .is_empty()
        );
    }

    #[test]
    fn reassigned_receiver_does_not_reuse_an_obsolete_prefix() {
        assert!(
            routes(
                r#"
from fastapi import APIRouter
router = APIRouter(prefix='/obsolete')
router = get_runtime_router()
@router.get('/users')
def users(): pass
"#
            )
            .is_empty()
        );
    }

    #[test]
    fn prefix_writes_invalidate_both_frameworks() {
        for (import, constructor, attribute, decorator) in [
            (
                "from fastapi import APIRouter",
                "APIRouter(prefix='/api')",
                "prefix",
                "get",
            ),
            (
                "from flask import Blueprint",
                "Blueprint('api', __name__, url_prefix='/api')",
                "url_prefix",
                "route",
            ),
        ] {
            let base = format!("{import}\nreceiver = {constructor}\n");
            let handler = format!("@receiver.{decorator}('/users')\ndef users(): pass\n");
            assert_eq!(routes(&format!("{base}{handler}")), ["GET /api/users"]);
            assert_eq!(
                routes(&format!("{base}receiver.{attribute}: str\n{handler}")),
                ["GET /api/users"]
            );
            for assignment in ["= '/v2'", "+= '/v2'", ": str = '/v2'"] {
                let actual = routes(&format!(
                    "{base}receiver.{attribute} {assignment}\n{handler}"
                ));
                assert!(
                    actual.is_empty(),
                    "{constructor}: {attribute} {assignment}: {actual:?}"
                );
            }
            for assignment in [
                "= get_receiver()",
                ": object = get_receiver()",
                "|= other_receiver",
            ] {
                let actual = routes(&format!("{base}receiver {assignment}\n{handler}"));
                assert!(
                    actual.is_empty(),
                    "{constructor}: receiver {assignment}: {actual:?}"
                );
            }
        }
    }

    #[test]
    fn flask_registration_keyword_order_does_not_change_the_mount() {
        for arguments in [
            "bp, url_prefix='/mounted'",
            "blueprint=bp, url_prefix='/mounted'",
            "url_prefix='/mounted', blueprint=bp",
            "name='mounted', url_prefix='/mounted', blueprint=bp",
        ] {
            let actual = routes(&format!(
                "from flask import Flask, Blueprint\napp = Flask(__name__)\nbp = Blueprint('api', __name__, url_prefix='/default')\n@bp.route('/users')\ndef users(): pass\napp.register_blueprint({arguments})\n"
            ));
            assert_eq!(actual, ["GET /mounted/users"], "{arguments}");
        }
    }

    #[test]
    fn flask_constructor_prefix_is_overridden_per_registration() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.py"),
            r#"
from flask import Flask, Blueprint
app = Flask(__name__)
bp = Blueprint("api", __name__, url_prefix="/default")
@bp.route("/users")
def users(): pass
app.register_blueprint(bp, url_prefix="/v1", name="one")
app.register_blueprint(bp, url_prefix="/v2", name="two")
"#,
        )
        .unwrap();
        let seeds = PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let mut routes: Vec<_> = seeds
            .iter()
            .filter(|s| s.source == "flask-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        routes.sort_unstable();
        assert_eq!(routes, ["GET /v1/users", "GET /v2/users"]);
    }
}
