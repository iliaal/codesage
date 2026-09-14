use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use regex::Regex;

use super::prefixes::{route_source, string_literal as literal};
use super::{FeatureSeed, PyFile, SeedFile};

const MAX_INCLUDE_DEPTH: usize = 32;
const MAX_EXPANSIONS: usize = 10_000;
const MAX_ROUTE_BYTES: usize = 4096;

struct Route {
    fragment: Option<String>,
    target: Target,
}

enum Target {
    View(Option<String>),
    Include(String),
}

pub(super) fn routes(files: &[PyFile]) -> Result<Vec<FeatureSeed>> {
    let import_re = Regex::new(
        r"(?m)^\s*(?:from\s+django\.(?:urls|conf\.urls)\s+import\b|import\s+django\.(?:urls|conf\.urls)\b)",
    )?;
    let call_re = Regex::new(r"^(path|re_path|url)\s*\(")?;
    let mut declarations = BTreeMap::new();
    let mut modules: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for file in files {
        for rel in [
            file.rel.as_str(),
            file.rel.strip_prefix("src/").unwrap_or(&file.rel),
        ] {
            let module = rel
                .strip_suffix("/__init__.py")
                .or_else(|| rel.strip_suffix(".py"));
            if let Some(module) = module {
                modules
                    .entry(module.replace('/', "."))
                    .or_default()
                    .insert(&file.rel);
            }
        }
        let source = route_source(&file.contents);
        if !import_re.is_match(&source) {
            continue;
        }
        let mut parsed = Vec::new();
        for body in super::django_urlpatterns_bodies(&source) {
            for entry in super::split_top_level_args(&body) {
                let Some(cap) = call_re.captures(entry.trim()) else {
                    continue;
                };
                let helper = cap.get(1).expect("helper group").as_str();
                let open = cap.get(0).expect("whole match").end() - 1;
                let Some(close) = super::find_balanced_close(entry.as_bytes(), open) else {
                    continue;
                };
                if !entry[close + 1..].trim().is_empty() {
                    continue;
                }
                if let Some(route) = parse_route(helper, &entry[open + 1..close]) {
                    parsed.push(route);
                }
            }
        }
        if parsed.is_empty() {
            continue;
        }
        declarations.insert(file.rel.as_str(), parsed);
    }
    let mut included = BTreeSet::new();
    for routes in declarations.values() {
        for route in routes {
            if let Target::Include(module) = &route.target
                && let Some(targets) = modules.get(module)
            {
                included.extend(targets.iter().copied());
            }
        }
    }
    let mut expansion = Expansion {
        declarations: &declarations,
        modules: &modules,
        remaining: MAX_EXPANSIONS,
        limited: false,
        out: BTreeMap::new(),
    };
    for root in declarations
        .keys()
        .filter(|path| !included.contains(**path))
    {
        expansion.walk(root, "", &mut Vec::new());
    }
    if expansion.limited {
        tracing::warn!(
            "Django route mapping reached an include expansion, depth, or route length limit; some routes were omitted"
        );
    }
    Ok(expansion.out.into_values().collect())
}

struct Expansion<'a> {
    declarations: &'a BTreeMap<&'a str, Vec<Route>>,
    modules: &'a BTreeMap<String, BTreeSet<&'a str>>,
    remaining: usize,
    limited: bool,
    out: BTreeMap<(String, String), FeatureSeed>,
}

impl Expansion<'_> {
    fn walk(&mut self, file: &str, prefix: &str, stack: &mut Vec<String>) {
        if stack.len() >= MAX_INCLUDE_DEPTH {
            self.limited = true;
            return;
        }
        if stack.iter().any(|path| path == file) {
            return;
        }
        let Some(routes) = self.declarations.get(file) else {
            return;
        };
        stack.push(file.to_string());
        for route in routes {
            if self.remaining == 0 {
                self.limited = true;
                break;
            }
            self.remaining -= 1;
            let Some(fragment) = &route.fragment else {
                continue;
            };
            if prefix.len() + fragment.len() > MAX_ROUTE_BYTES {
                self.limited = true;
                continue;
            }
            let mounted = format!("{prefix}{fragment}");
            if mounted.len() + usize::from(!mounted.starts_with('/')) > MAX_ROUTE_BYTES {
                self.limited = true;
                continue;
            }
            match &route.target {
                Target::Include(module) => {
                    if let Some(targets) = self.modules.get(module)
                        && targets.len() == 1
                        && let Some(target) = targets.first()
                    {
                        self.walk(target, &mounted, stack);
                    }
                }
                Target::View(symbol) => {
                    let route = super::ensure_leading_slash(&mounted);
                    let key = (file.to_string(), route.clone());
                    let seed = self.out.entry(key).or_insert_with(|| {
                        let mut seeds = Vec::new();
                        super::push_django_route_seed(&mut seeds, file, &route, symbol.as_deref());
                        seeds.pop().expect("route helper emits one seed")
                    });
                    for path in &stack[..stack.len() - 1] {
                        if !seed.context_files.iter().any(|f| f.path == *path) {
                            seed.context_files.push(SeedFile {
                                path: path.clone(),
                                reason: "Django URL include mount".to_string(),
                            });
                        }
                    }
                }
            }
        }
        stack.pop();
    }
}

fn parse_route(helper: &str, args: &str) -> Option<Route> {
    let parts = super::split_top_level_args(args);
    let view = parts.get(1)?.trim();
    let include_args = view.strip_prefix("include").and_then(|s| {
        let s = s.trim();
        let close = super::find_balanced_close(s.as_bytes(), 0)?;
        (s.starts_with('(') && close + 1 == s.len()).then(|| &s[1..close])
    });
    let target = if let Some(args) = include_args {
        let args = super::split_top_level_args(args);
        let first = args.first()?.trim();
        let module = if first.starts_with('(') && first.ends_with(')') {
            let tuple = super::split_top_level_args(&first[1..first.len() - 1]);
            if tuple.len() != 2 {
                return None;
            }
            literal(tuple.first()?)?
        } else {
            literal(first)?
        };
        if !module.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            return None;
        }
        Target::Include(module)
    } else {
        if view.starts_with("include") {
            return None;
        }
        Target::View(super::django_view_symbol(view))
    };
    let fragment = literal(parts.first()?).and_then(|raw| {
        if helper == "path" {
            Some(super::strip_django_converters(&raw))
        } else if matches!(target, Target::Include(_)) {
            include_regex_fragment(&raw)
        } else {
            let normalized = super::normalize_django_regex_route(&raw)?;
            let fragment = if raw.trim_start_matches('^').starts_with('/') {
                normalized
            } else {
                normalized
                    .strip_prefix('/')
                    .unwrap_or(&normalized)
                    .to_string()
            };
            Some(fragment)
        }
    });
    Some(Route { fragment, target })
}

fn include_regex_fragment(raw: &str) -> Option<String> {
    let raw = raw.strip_prefix('^').unwrap_or(raw);
    let mut out = String::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with("(?P<") {
            let gt = i + 4 + raw[i + 4..].find('>')?;
            let close = super::find_balanced_close(raw.as_bytes(), i)?;
            if gt >= close {
                return None;
            }
            out.push('<');
            out.push_str(&raw[i + 4..gt]);
            out.push('>');
            i = close + 1;
            continue;
        }
        let c = raw[i..].chars().next()?;
        i += c.len_utf8();
        if c == '\\' {
            let escaped = raw[i..].chars().next()?;
            if !"\\/.+*?()[]{}|^$".contains(escaped) {
                return None;
            }
            out.push(escaped);
            i += escaped.len_utf8();
        } else if ".+*?()[]{}|^$".contains(c) {
            return None;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mappers::types::{FeatureMapper, MapperContext};

    fn map(files: &[(&str, &str)]) -> Vec<FeatureSeed> {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        super::super::PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap()
            .into_iter()
            .filter(|seed| seed.source == "django-route")
            .collect()
    }

    #[test]
    fn include_chains_preserve_multiple_mounts_and_context() {
        let seeds = map(&[
            (
                "urls.py",
                "from django.urls import path, include\nurlpatterns = [path('api/', include('pkg.urls')), path('legacy/', include('pkg.urls'))]\n",
            ),
            (
                "src/pkg/urls.py",
                "from django.urls import path, include\nurlpatterns = [path('v1/', include('pkg.accounts'))]\n",
            ),
            (
                "src/pkg/accounts/__init__.py",
                "from django.urls import path\nurlpatterns = [path('users/<int:id>/', views.detail)]\n",
            ),
        ]);
        assert_eq!(seeds.len(), 2, "{seeds:?}");
        let paths: Vec<_> = seeds
            .iter()
            .map(|s| s.entry_route.as_deref().unwrap())
            .collect();
        assert_eq!(paths, ["/api/v1/users/<id>/", "/legacy/v1/users/<id>/"]);
        for seed in seeds {
            assert_eq!(seed.entry_path, "src/pkg/accounts/__init__.py");
            assert_eq!(seed.entry_symbol.as_deref(), Some("detail"));
            let context: Vec<_> = seed.context_files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(context, ["urls.py", "src/pkg/urls.py"]);
        }
    }

    #[test]
    fn include_concatenates_without_inserting_or_removing_slashes() {
        let seeds = map(&[
            (
                "urls.py",
                "from django.urls import path, include\nurlpatterns = [path('api', include(('child', 'app'), namespace='v1')), path('double/', include('child')), path('', include('empty'))]\n",
            ),
            (
                "child.py",
                "from django.urls import path\nurlpatterns = [path('users/', views.users), path('/leading/', views.leading), path('', views.empty)]\n",
            ),
            (
                "empty.py",
                "from django.urls import path\nurlpatterns = [path('', views.root)]\n",
            ),
        ]);
        let paths: BTreeSet<_> = seeds
            .iter()
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert_eq!(
            paths,
            BTreeSet::from([
                "/api",
                "/api/leading/",
                "/apiusers/",
                "/double/",
                "/double//leading/",
                "/double/users/",
                "/"
            ])
        );
    }

    #[test]
    fn dynamic_and_ambiguous_mounts_do_not_emit_unmounted_children() {
        let seeds = map(&[
            (
                "urls.py",
                "from django.urls import path, re_path, include\nurlpatterns = [path(PREFIX, include('dynamic')), path(f'{prefix}/', include('formatted')), path('api/' + suffix, include('joined')), re_path(r'^api/$', include('anchored')), path('ambiguous/', include('duplicate')), path('safe/', views.safe)]\n",
            ),
            (
                "dynamic.py",
                "from django.urls import path\nurlpatterns = [path('dynamic/', views.bad)]\n",
            ),
            (
                "formatted.py",
                "from django.urls import path\nurlpatterns = [path('formatted/', views.bad)]\n",
            ),
            (
                "joined.py",
                "from django.urls import path\nurlpatterns = [path('joined/', views.bad)]\n",
            ),
            (
                "anchored.py",
                "from django.urls import path\nurlpatterns = [path('anchored/', views.bad)]\n",
            ),
            (
                "duplicate.py",
                "from django.urls import path\nurlpatterns = [path('root/', views.bad)]\n",
            ),
            (
                "src/duplicate.py",
                "from django.urls import path\nurlpatterns = [path('src/', views.bad)]\n",
            ),
        ]);
        assert_eq!(seeds.len(), 1, "{seeds:?}");
        assert_eq!(seeds[0].entry_route.as_deref(), Some("/safe/"));
    }

    #[test]
    fn include_cycles_stop_at_the_cycle_without_losing_other_mounts() {
        let seeds = map(&[
            (
                "urls.py",
                "from django.urls import path, include\nurlpatterns = [path('a/', include('child')), path('b/', include('child'))]\n",
            ),
            (
                "child.py",
                "from django.urls import path, include\nurlpatterns = [path('loop/', include('child')), path('ok/', views.ok)]\n",
            ),
            (
                "cycle_a.py",
                "from django.urls import path, include\nurlpatterns = [path('a/', include('cycle_b')), path('unmounted/', views.bad)]\n",
            ),
            (
                "cycle_b.py",
                "from django.urls import path, include\nurlpatterns = [path('b/', include('cycle_a'))]\n",
            ),
        ]);
        let paths: Vec<_> = seeds
            .iter()
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert_eq!(paths, ["/a/ok/", "/b/ok/"]);
    }

    #[test]
    fn comments_and_documented_urlpatterns_do_not_create_mounts() {
        let seeds = map(&[
            (
                "urls.py",
                "'''\nfrom django.urls import path, include\nurlpatterns = [path('fake/', include('child'))]\n'''\nfrom django.urls import path, include\nurlpatterns = [\n path('hash#/', include('child')), # path('comment/', include('child'))\n]\n",
            ),
            (
                "child.py",
                "from django.urls import path\nurlpatterns = [path('real/', views.real)]\n",
            ),
        ]);
        assert_eq!(seeds.len(), 1, "{seeds:?}");
        assert_eq!(seeds[0].entry_route.as_deref(), Some("/hash#/real/"));
    }

    #[test]
    fn excessive_include_depth_does_not_fall_back_to_unmounted_routes() {
        let dir = tempfile::tempdir().unwrap();
        for depth in 0..MAX_INCLUDE_DEPTH + 1 {
            let body = if depth == MAX_INCLUDE_DEPTH {
                "path('leaf/', views.leaf)".to_string()
            } else {
                format!("path('p/', include('urls{}'))", depth + 1)
            };
            std::fs::write(
                dir.path().join(format!("urls{depth}.py")),
                format!("from django.urls import path, include\nurlpatterns = [{body}]\n"),
            )
            .unwrap();
        }
        let seeds = super::super::PythonMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(!seeds.iter().any(|s| s.source == "django-route"));
    }

    #[test]
    fn persisted_mounts_have_distinct_stable_ids_context_and_stale_removal() {
        use codesage_protocol::{FeatureFileRole, FeatureKind};
        use codesage_storage::Database;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mounts = "from django.urls import path, include\nurlpatterns = [path('a/', include('child')), path('b/', include('child'))]\n";
        std::fs::write(root.join("urls.py"), mounts).unwrap();
        std::fs::write(
            root.join("child.py"),
            "from django.urls import path\nurlpatterns = [path('users/', views.users)]\n",
        )
        .unwrap();
        let db = Database::open_in_memory().unwrap();
        crate::map_features(root, &db, &[]).unwrap();
        let first = db
            .list_features(Some(FeatureKind::Route), None, None, 100)
            .unwrap();
        assert_eq!(first.len(), 2);
        let ids: BTreeSet<_> = first.iter().map(|f| f.feature_id.clone()).collect();
        assert_eq!(ids.len(), 2);
        for feature in &first {
            assert!(
                feature
                    .files
                    .iter()
                    .any(|f| f.path == "urls.py" && f.role == FeatureFileRole::Context)
            );
        }
        crate::map_features(root, &db, &[]).unwrap();
        let second = db
            .list_features(Some(FeatureKind::Route), None, None, 100)
            .unwrap();
        assert_eq!(ids, second.iter().map(|f| f.feature_id.clone()).collect());
        std::fs::write(
            root.join("urls.py"),
            "from django.urls import path, include\nurlpatterns = [path('a/', include('child'))]\n",
        )
        .unwrap();
        let stats = crate::map_features(root, &db, &[]).unwrap();
        assert_eq!(stats.removed, 1);
        let final_routes = db
            .list_features(Some(FeatureKind::Route), None, None, 100)
            .unwrap();
        assert_eq!(final_routes.len(), 1);
        assert_eq!(final_routes[0].entry_route.as_deref(), Some("/a/users/"));
        assert!(ids.contains(&final_routes[0].feature_id));
    }

    #[test]
    fn regex_mounts_preserve_literal_escapes_and_named_parameters() {
        let seeds = map(&[
            (
                "urls.py",
                "from django.urls import re_path, include\nurlpatterns = [re_path(r'^v1\\./(?P<id>\\d+)/', include('child')), re_path(r'^wild./', include('wild')), re_path(r'^word\\w/', include('word'))]\n",
            ),
            (
                "child.py",
                "from django.urls import path\nurlpatterns = [path('users/', views.users)]\n",
            ),
            (
                "wild.py",
                "from django.urls import path\nurlpatterns = [path('bad/', views.bad)]\n",
            ),
            (
                "word.py",
                "from django.urls import path\nurlpatterns = [path('bad/', views.bad)]\n",
            ),
        ]);
        assert_eq!(seeds.len(), 1, "{seeds:?}");
        assert_eq!(seeds[0].entry_route.as_deref(), Some("/v1./<id>/users/"));
    }

    #[test]
    fn expansion_budget_bounds_emitted_routes() {
        let mut source = "from django.urls import path\nurlpatterns = [\n".to_string();
        for number in 0..=MAX_EXPANSIONS {
            source.push_str(&format!("path('route{number}/', views.route),\n"));
        }
        source.push_str("]\n");
        let seeds = map(&[("urls.py", &source)]);
        assert_eq!(seeds.len(), MAX_EXPANSIONS);
        assert!(
            seeds
                .iter()
                .any(|s| s.entry_route.as_deref() == Some("/route0/"))
        );
        assert!(
            !seeds
                .iter()
                .any(|s| s.entry_route.as_deref() == Some(&format!("/route{MAX_EXPANSIONS}/")))
        );
    }

    #[test]
    fn composed_route_length_is_bounded_without_unmounted_fallback() {
        let prefix = "p".repeat(MAX_ROUTE_BYTES - 2);
        let root = format!(
            "from django.urls import path, include\nurlpatterns = [path('{prefix}', include('child'))]\n"
        );
        let seeds = map(&[
            ("urls.py", &root),
            (
                "child.py",
                "from django.urls import path\nurlpatterns = [path('x', views.within), path('xx', views.over)]\n",
            ),
        ]);
        assert_eq!(seeds.len(), 1, "{seeds:?}");
        assert_eq!(
            seeds[0].entry_route.as_ref().unwrap().len(),
            MAX_ROUTE_BYTES
        );
        assert_eq!(seeds[0].entry_symbol.as_deref(), Some("within"));
    }
}
