use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use tree_sitter::Node;

use super::{cargo_toml_context, rel_path};
use crate::mappers::shared::{is_safe_file, read_to_string_bounded, walk_files};
use crate::mappers::types::{FeatureSeed, MapperContext};

pub(super) fn seed_mcp_servers(
    ctx: &MapperContext,
    pkg_dir: &Path,
    pkg_name: &str,
    seeds: &mut Vec<FeatureSeed>,
) {
    let manifest = read_manifest(ctx, &pkg_dir.join("Cargo.toml"));
    let modern = manifest
        .as_ref()
        .and_then(|manifest| package_modern(ctx, pkg_dir, manifest));
    let roots = manifest
        .as_ref()
        .and_then(|manifest| cargo_roots(ctx, pkg_dir, manifest, modern));
    let origin = Origin {
        modern: modern == Some(true),
        wrong_extern: roots
            .as_ref()
            .is_none_or(|roots| roots.iter().any(|path| root_namespace_uncertain(ctx, path))),
    };
    for entry in walk_files(ctx.root, &pkg_dir.join("src"), 10_000, ctx.excludes) {
        if !entry.ends_with(".rs")
            || !ctx.allowed(&entry)
            || codesage_parser::discover::is_test_like_path(&entry)
        {
            continue;
        }
        let Ok(Some(source)) = read_to_string_bounded(&ctx.root.join(&entry)) else {
            continue;
        };
        if !source.contains("ServerHandler") {
            continue;
        }
        let Ok(tree) = codesage_parser::parse::parse_file(source.as_bytes(), Language::Rust) else {
            continue;
        };
        if codesage_parser::extract::file_is_test_by_syntax(
            &tree,
            source.as_bytes(),
            Language::Rust,
        ) {
            continue;
        }
        let mut servers = BTreeSet::new();
        let path = ctx.root.join(&entry);
        let parent = path.parent().unwrap_or(ctx.root);
        let is_root = roots
            .as_ref()
            .is_some_and(|roots| path.canonicalize().is_ok_and(|path| roots.contains(&path)));
        let directory_known = is_root || path.file_name().is_some_and(|file| file == "mod.rs");
        collect_servers(
            ctx,
            tree.root_node(),
            &source,
            directory_known.then_some(parent),
            &[],
            origin,
            &mut servers,
        );
        for server in servers {
            seeds.push(FeatureSeed {
                summary: format!("MCP ServerHandler implementation at {entry}"),
                source: "rust-mcp-server",
                confidence: FeatureConfidence::High,
                entry_symbol: Some(server.clone()),
                tags: vec!["rust".to_string(), "mcp".to_string(), "service".to_string()],
                context_files: cargo_toml_context(ctx, pkg_dir),
                test_prefixes: vec![rel_path(ctx.root, &pkg_dir.join("tests"))],
                ..FeatureSeed::new(
                    FeatureKind::Service,
                    Language::Rust,
                    format!("MCP server `{server}` ({pkg_name})"),
                    entry.clone(),
                )
            });
        }
    }
}

fn read_manifest(ctx: &MapperContext, path: &Path) -> Option<toml::Value> {
    if !is_safe_file(ctx.root, path) || !ctx.allowed(&rel_path(ctx.root, path)) {
        return None;
    }
    let source = read_to_string_bounded(path).ok()??;
    toml::from_str(&source).ok()
}

fn modern_edition(value: &toml::Value) -> Option<bool> {
    match value.as_str()? {
        "2015" => Some(false),
        "2018" | "2021" | "2024" => Some(true),
        _ => None,
    }
}

fn package_modern(ctx: &MapperContext, pkg_dir: &Path, manifest: &toml::Value) -> Option<bool> {
    let package = manifest.get("package")?;
    let Some(edition) = package.get("edition") else {
        return Some(false);
    };
    if edition.as_str().is_some() {
        return modern_edition(edition);
    }
    if edition.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
        return None;
    }
    if let Some(workspace) = manifest.get("workspace") {
        if package.get("workspace").is_some() {
            return None;
        }
        return modern_edition(workspace.get("package")?.get("edition")?);
    }
    let root = ctx.root.canonicalize().ok()?;
    let package_dir = pkg_dir.canonicalize().ok()?;
    let explicit = match package.get("workspace") {
        Some(value) => Some(value.as_str()?),
        None => None,
    };
    let candidates: Vec<_> = if let Some(path) = explicit {
        vec![pkg_dir.join(path).canonicalize().ok()?]
    } else {
        package_dir
            .ancestors()
            .skip(1)
            .take(32)
            .map(Path::to_path_buf)
            .collect()
    };
    for directory in candidates {
        if !directory.starts_with(&root) {
            return None;
        }
        let path = directory.join("Cargo.toml");
        if !path.exists() {
            continue;
        }
        let owner = read_manifest(ctx, &path)?;
        let Some(workspace) = owner.get("workspace") else {
            continue;
        };
        let relative = rel_path(&directory, &package_dir);
        let matches = |value: &toml::Value| -> Option<bool> {
            let mut matched = false;
            for pattern in value.as_array()? {
                let glob = globset::GlobBuilder::new(pattern.as_str()?)
                    .literal_separator(true)
                    .build()
                    .ok()?
                    .compile_matcher();
                matched |= glob.is_match(&relative);
            }
            Some(matched)
        };
        let excluded = match workspace.get("exclude") {
            Some(value) => matches(value)?,
            None => false,
        };
        if excluded || !matches(workspace.get("members")?)? {
            return None;
        }
        return modern_edition(workspace.get("package")?.get("edition")?);
    }
    None
}

fn cargo_roots(
    ctx: &MapperContext,
    pkg_dir: &Path,
    manifest: &toml::Value,
    modern: Option<bool>,
) -> Option<BTreeSet<PathBuf>> {
    let package = manifest.get("package")?.as_table()?;
    let binaries = match manifest.get("bin") {
        Some(value) => Some(value.as_array()?),
        None => None,
    };
    let explicit_names: BTreeSet<_> = binaries
        .into_iter()
        .flatten()
        .map(|binary| binary.get("name")?.as_str())
        .collect::<Option<_>>()?;
    let configured_targets = ["lib", "bin", "example", "test", "bench"]
        .iter()
        .any(|target| manifest.get(target).is_some());
    let automatic = |name: &str| {
        if let Some(value) = package.get(name) {
            return value.as_bool();
        }
        if name != "autobins" || !configured_targets {
            return Some(true);
        }
        modern
    };
    let mut paths = BTreeSet::new();
    if let Some(lib) = manifest.get("lib") {
        let lib = lib.as_table()?;
        paths.insert(pkg_dir.join(match lib.get("path") {
            Some(path) => path.as_str()?,
            None => "src/lib.rs",
        }));
    } else if automatic("autolib")? && pkg_dir.join("src/lib.rs").exists() {
        paths.insert(pkg_dir.join("src/lib.rs"));
    }
    if automatic("autobins")? {
        if pkg_dir.join("src/main.rs").exists()
            && !package
                .get("name")
                .and_then(toml::Value::as_str)
                .is_some_and(|name| explicit_names.contains(name))
        {
            paths.insert(pkg_dir.join("src/main.rs"));
        }
        let bin_dir = pkg_dir.join("src/bin");
        let mut names = BTreeSet::new();
        for entry in walk_files(ctx.root, &bin_dir, 10_000, ctx.excludes) {
            let path = ctx.root.join(&entry);
            let relative = path.strip_prefix(&bin_dir).ok()?;
            let parts: Vec<_> = relative.components().collect();
            let name = match parts.as_slice() {
                [_] if path.extension().is_some_and(|extension| extension == "rs") => {
                    path.file_stem()?
                }
                [name, _] if path.file_name().is_some_and(|file| file == "main.rs") => {
                    name.as_os_str()
                }
                _ => continue,
            };
            if name
                .to_str()
                .is_some_and(|name| explicit_names.contains(name))
            {
                continue;
            }
            if !names.insert(name.to_os_string()) {
                return None;
            }
            paths.insert(path);
        }
    }
    if let Some(binaries) = manifest.get("bin") {
        for binary in binaries.as_array()? {
            let binary = binary.as_table()?;
            if let Some(path) = binary.get("path") {
                paths.insert(pkg_dir.join(path.as_str()?));
                continue;
            }
            let name = binary.get("name")?.as_str()?;
            let mut candidates = vec![
                pkg_dir.join(format!("src/bin/{name}.rs")),
                pkg_dir.join(format!("src/bin/{name}/main.rs")),
            ];
            if package.get("name").and_then(toml::Value::as_str) == Some(name) {
                candidates.push(pkg_dir.join("src/main.rs"));
            }
            candidates.retain(|path| path.exists());
            let [path] = candidates.as_slice() else {
                return None;
            };
            paths.insert(path.clone());
        }
    }
    let root = ctx.root.canonicalize().ok()?;
    paths
        .into_iter()
        .map(|path| {
            if !is_safe_file(ctx.root, &path) || !ctx.allowed(&rel_path(ctx.root, &path)) {
                return None;
            }
            let path = path.canonicalize().ok()?;
            path.starts_with(&root).then_some(path)
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Origin {
    modern: bool,
    wrong_extern: bool,
}

const EXTERNAL_RMCP: &str = "$external_rmcp";

fn root_namespace_uncertain(ctx: &MapperContext, path: &Path) -> bool {
    if !is_safe_file(ctx.root, path) || !ctx.allowed(&rel_path(ctx.root, path)) {
        return true;
    }
    let Ok(Some(source)) = read_to_string_bounded(path) else {
        return true;
    };
    let Ok(tree) = codesage_parser::parse::parse_file(source.as_bytes(), Language::Rust) else {
        return true;
    };
    let mut cursor = tree.root_node().walk();
    tree.root_node().has_error()
        || tree.root_node().named_children(&mut cursor).any(|item| {
            if test_gated(item, &source) {
                return false;
            }
            if item_macro(item) {
                return true;
            }
            item.kind() == "extern_crate_declaration"
                && item.child_by_field_name("name").is_some_and(|name| {
                    let alias = item.child_by_field_name("alias").unwrap_or(name);
                    text(alias, &source) == "rmcp" && text(name, &source) != "rmcp"
                })
        })
}

fn collect_servers(
    ctx: &MapperContext,
    scope: Node<'_>,
    source: &str,
    module_dir: Option<&Path>,
    module_path: &[String],
    mut origin: Origin,
    servers: &mut BTreeSet<String>,
) {
    let mut cursor = scope.walk();
    if scope
        .named_children(&mut cursor)
        .any(|child| child.kind() == "inner_attribute_item" && test_attribute(child, source))
    {
        return;
    }
    let items: Vec<_> = scope
        .named_children(&mut cursor)
        .filter(|item| !test_gated(*item, source))
        .collect();
    let mut bindings = BTreeMap::<String, Option<Vec<String>>>::new();
    let mut globs = Vec::new();
    for item in &items {
        match item.kind() {
            "use_declaration" => {
                if let Some(argument) = item.child_by_field_name("argument") {
                    let mut imports = Vec::new();
                    collect_imports(argument, source, &[], &mut imports, &mut globs);
                    for (name, path) in imports {
                        match bindings.entry(name) {
                            std::collections::btree_map::Entry::Vacant(entry) => {
                                entry.insert(Some(path));
                            }
                            std::collections::btree_map::Entry::Occupied(mut entry) => {
                                entry.insert(None);
                            }
                        }
                    }
                }
            }
            "mod_item" | "trait_item" | "struct_item" | "enum_item" | "type_item" => {
                if let Some(name) = item.child_by_field_name("name") {
                    bindings.insert(text(name, source).to_string(), None);
                }
            }
            "extern_crate_declaration" => {
                if let Some(name) = item.child_by_field_name("name") {
                    let alias = item.child_by_field_name("alias").unwrap_or(name);
                    let real = text(name, source) == "rmcp";
                    origin.wrong_extern |= text(alias, source) == "rmcp" && !real;
                    let path = real.then(|| vec![EXTERNAL_RMCP.to_string()]);
                    bindings
                        .entry(text(alias, source).to_string())
                        .and_modify(|binding| *binding = None)
                        .or_insert(path);
                }
            }
            _ => {}
        }
    }
    let mut remaining = 32;
    let namespace_known = !items.iter().any(|item| item_macro(*item))
        && globs
            .iter()
            .all(|path| glob_preserves_rmcp(ctx, scope, source, module_dir, path, &mut remaining));
    if !namespace_known
        && !bindings
            .get("rmcp")
            .and_then(Option::as_ref)
            .is_some_and(|path| {
                path.first()
                    .is_some_and(|first| first.is_empty() || first == EXTERNAL_RMCP)
            })
    {
        bindings.insert("rmcp".to_string(), None);
    }
    for item in items {
        if item.kind() == "mod_item" {
            if let (Some(body), Some(name)) = (
                item.child_by_field_name("body"),
                item.child_by_field_name("name"),
            ) {
                let mut nested_path = module_path.to_vec();
                nested_path.push(text(name, source).to_string());
                let nested_dir = module_dir
                    .filter(|_| !path_attribute(item, source))
                    .map(|dir| dir.join(text(name, source)));
                collect_servers(
                    ctx,
                    body,
                    source,
                    nested_dir.as_deref(),
                    &nested_path,
                    origin,
                    servers,
                );
            }
            continue;
        }
        if item.kind() != "impl_item" || item.has_error() {
            continue;
        }
        let mut cursor = item.walk();
        if item.children(&mut cursor).any(|child| child.kind() == "!") {
            continue;
        }
        let Some(trait_node) = item.child_by_field_name("trait") else {
            continue;
        };
        let Some(trait_path) = path_parts(trait_node, source) else {
            continue;
        };
        let mut impl_bindings = bindings.clone();
        if let Some(parameters) = item.child_by_field_name("type_parameters") {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                if let Some(name) = parameter.child_by_field_name("name") {
                    impl_bindings.insert(text(name, source).to_string(), None);
                }
            }
        }
        if resolve_path(&trait_path, &impl_bindings, origin, 0).as_deref()
            != Some(["rmcp".to_string(), "ServerHandler".to_string()].as_slice())
        {
            continue;
        }
        if let Some(ty) = item.child_by_field_name("type")
            && let Some(name) = type_name(ty, source, module_path)
        {
            servers.insert(name);
        }
    }
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source[node.byte_range()].trim_start_matches("r#")
}

fn type_name(node: Node<'_>, source: &str, module_path: &[String]) -> Option<String> {
    if node.kind() == "generic_type" {
        return type_name(node.child_by_field_name("type")?, source, module_path);
    }
    let parts = path_parts(node, source)?;
    let mut qualified = module_path.to_vec();
    match parts.first()?.as_str() {
        "self" => qualified.extend_from_slice(&parts[1..]),
        "super" => {
            let mut offset = 0;
            while parts.get(offset).is_some_and(|part| part == "super") {
                if qualified.pop().is_none() {
                    return Some(parts.join("::"));
                }
                offset += 1;
            }
            qualified.extend_from_slice(&parts[offset..]);
        }
        _ if parts.len() == 1 => qualified.extend(parts),
        _ => qualified = parts,
    }
    Some(qualified.join("::"))
}

fn path_parts(node: Node<'_>, source: &str) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" | "type_identifier" | "self" | "super" | "crate" => {
            Some(vec![text(node, source).to_string()])
        }
        "scoped_identifier" | "scoped_type_identifier" => {
            let mut parts = if let Some(path) = node.child_by_field_name("path") {
                path_parts(path, source)?
            } else {
                vec![String::new()]
            };
            parts.push(text(node.child_by_field_name("name")?, source).to_string());
            Some(parts)
        }
        _ => None,
    }
}

fn collect_imports(
    node: Node<'_>,
    source: &str,
    prefix: &[String],
    imports: &mut Vec<(String, Vec<String>)>,
    globs: &mut Vec<Vec<String>>,
) {
    match node.kind() {
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_imports(child, source, prefix, imports, globs);
            }
        }
        "scoped_use_list" => {
            let mut path = prefix.to_vec();
            if let Some(scope) = node.child_by_field_name("path") {
                let Some(parts) = path_parts(scope, source) else {
                    return;
                };
                path.extend(parts);
            }
            if let Some(list) = node.child_by_field_name("list") {
                collect_imports(list, source, &path, imports, globs);
            }
        }
        "use_wildcard" => {
            let mut path = prefix.to_vec();
            if let Some(scope) = node.named_child(0) {
                let Some(parts) = path_parts(scope, source) else {
                    return;
                };
                path.extend(parts);
            }
            globs.push(path);
        }
        _ => {
            let (path, alias) = if node.kind() == "use_as_clause" {
                let (Some(path), Some(alias)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("alias"),
                ) else {
                    return;
                };
                (path, Some(text(alias, source)))
            } else {
                (node, None)
            };
            let Some(parts) = path_parts(path, source) else {
                return;
            };
            let mut path = prefix.to_vec();
            if parts != ["self"] || prefix.is_empty() {
                path.extend(parts);
            }
            if let Some(name) = alias.or_else(|| path.last().map(String::as_str))
                && name != "_"
            {
                imports.push((name.to_string(), path));
            }
        }
    }
}

// Glob evidence is limited to conventional local modules. Unknown exports cannot
// establish that an unqualified `rmcp` names the external crate.
fn glob_preserves_rmcp(
    ctx: &MapperContext,
    scope: Node<'_>,
    source: &str,
    module_dir: Option<&Path>,
    path: &[String],
    remaining: &mut usize,
) -> bool {
    let Some(next) = remaining.checked_sub(1) else {
        return false;
    };
    *remaining = next;
    let path = path.strip_prefix(&["self".to_string()]).unwrap_or(path);
    let Some((name, rest)) = path.split_first() else {
        return false;
    };
    let mut cursor = scope.walk();
    let modules: Vec<_> = scope
        .named_children(&mut cursor)
        .filter(|item| {
            item.kind() == "mod_item"
                && !test_gated(*item, source)
                && item
                    .child_by_field_name("name")
                    .is_some_and(|node| text(node, source) == name)
        })
        .collect();
    let [module] = modules.as_slice() else {
        return false;
    };
    if path_attribute(*module, source) {
        return false;
    }
    let nested_dir = module_dir.map(|directory| directory.join(name));
    if let Some(body) = module.child_by_field_name("body") {
        return if rest.is_empty() {
            exports_preserve_rmcp(body, source)
        } else {
            glob_preserves_rmcp(ctx, body, source, nested_dir.as_deref(), rest, remaining)
        };
    }
    let Some(module_dir) = module_dir else {
        return false;
    };
    let nested_dir = module_dir.join(name);
    let candidates = [
        module_dir.join(format!("{name}.rs")),
        nested_dir.join("mod.rs"),
    ];
    let existing: Vec<_> = candidates.iter().filter(|path| path.exists()).collect();
    let [file] = existing.as_slice() else {
        return false;
    };
    if !ctx.allowed(&rel_path(ctx.root, file))
        || !std::fs::symlink_metadata(file).is_ok_and(|meta| meta.file_type().is_file())
        || !file.canonicalize().ok().is_some_and(|resolved| {
            ctx.root
                .canonicalize()
                .is_ok_and(|root| resolved.starts_with(root))
        })
    {
        return false;
    }
    let Ok(Some(source)) = read_to_string_bounded(file) else {
        return false;
    };
    let Ok(tree) = codesage_parser::parse::parse_file(source.as_bytes(), Language::Rust) else {
        return false;
    };
    if tree.root_node().has_error() {
        return false;
    }
    if rest.is_empty() {
        exports_preserve_rmcp(tree.root_node(), &source)
    } else {
        glob_preserves_rmcp(
            ctx,
            tree.root_node(),
            &source,
            Some(&nested_dir),
            rest,
            remaining,
        )
    }
}

fn path_attribute(module: Node<'_>, source: &str) -> bool {
    let mut previous = module.prev_named_sibling();
    while let Some(node) = previous {
        match node.kind() {
            "attribute_item" if text(node, source).contains("path") => return true,
            "attribute_item" | "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = node.prev_named_sibling();
    }
    false
}

fn exports_preserve_rmcp(scope: Node<'_>, source: &str) -> bool {
    if scope.has_error() {
        return false;
    }
    let mut cursor = scope.walk();
    scope.named_children(&mut cursor).all(|item| {
        if test_gated(item, source) {
            return true;
        }
        if item_macro(item) {
            return false;
        }
        let mut cursor = item.walk();
        if !item
            .named_children(&mut cursor)
            .any(|child| child.kind() == "visibility_modifier")
        {
            return true;
        }
        if item
            .child_by_field_name("alias")
            .or_else(|| item.child_by_field_name("name"))
            .is_some_and(|name| text(name, source) == "rmcp")
        {
            return false;
        }
        if item.kind() == "use_declaration"
            && let Some(argument) = item.child_by_field_name("argument")
        {
            let mut imports = Vec::new();
            let mut globs = Vec::new();
            collect_imports(argument, source, &[], &mut imports, &mut globs);
            return globs.is_empty() && imports.iter().all(|(name, _)| name != "rmcp");
        }
        true
    })
}

fn item_macro(item: Node<'_>) -> bool {
    item.kind() == "macro_invocation"
        || item
            .named_child(0)
            .is_some_and(|child| child.kind() == "macro_invocation")
}

fn resolve_path(
    path: &[String],
    bindings: &BTreeMap<String, Option<Vec<String>>>,
    origin: Origin,
    depth: usize,
) -> Option<Vec<String>> {
    let first = path.first()?;
    if depth > 16 {
        return None;
    }
    if first == EXTERNAL_RMCP {
        let mut resolved = vec!["rmcp".to_string()];
        resolved.extend_from_slice(&path[1..]);
        return Some(resolved);
    }
    if first.is_empty() {
        return (origin.modern
            && !origin.wrong_extern
            && path.get(1).is_some_and(|part| part == "rmcp"))
        .then(|| path[1..].to_vec());
    }
    if let Some(binding) = bindings.get(first) {
        let binding = binding.as_ref()?;
        let mut resolved = if binding.len() == 1 && binding[0] == "rmcp" && first == "rmcp" {
            (origin.modern && !origin.wrong_extern).then(|| binding.clone())?
        } else {
            resolve_path(binding, bindings, origin, depth + 1)?
        };
        resolved.extend_from_slice(&path[1..]);
        Some(resolved)
    } else {
        (origin.modern && !origin.wrong_extern && first == "rmcp").then(|| path.to_vec())
    }
}

fn test_gated(item: Node<'_>, source: &str) -> bool {
    let mut previous = item.prev_named_sibling();
    while let Some(node) = previous {
        match node.kind() {
            "attribute_item" => {
                if test_attribute(node, source) {
                    return true;
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = node.prev_named_sibling();
    }
    false
}

fn test_attribute(node: Node<'_>, source: &str) -> bool {
    let compact: String = source[node.byte_range()]
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    compact
        .strip_prefix("#[cfg(")
        .or_else(|| compact.strip_prefix("#![cfg("))
        .and_then(|rest| rest.strip_suffix(")]"))
        .is_some_and(requires_test)
}

fn requires_test(predicate: &str) -> bool {
    if predicate == "test" {
        return true;
    }
    let Some(args) = predicate
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let mut depth = 0usize;
    let mut start = 0;
    for (index, byte) in args.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                if requires_test(&args[start..index]) {
                    return true;
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    requires_test(&args[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mappers::rust::RustMapper;
    use crate::mappers::types::FeatureMapper;

    fn write(root: &Path, relative: &str, source: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    fn map_source(source: &str) -> Vec<FeatureSeed> {
        map_source_files(source, &[])
    }

    fn map_source_files(source: &str, files: &[(&str, &str)]) -> Vec<FeatureSeed> {
        map_source_at("src/transport/bridge.rs", source, files)
    }

    fn map_source_at(entry: &str, source: &str, files: &[(&str, &str)]) -> Vec<FeatureSeed> {
        let directory = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"tools\"\nedition = \"2024\"\n",
        );
        write(directory.path(), entry, source);
        for (path, source) in files {
            write(directory.path(), path, source);
        }
        RustMapper
            .map(&MapperContext::for_root(directory.path()))
            .unwrap()
            .into_iter()
            .filter(|seed| seed.kind == FeatureKind::Service)
            .collect()
    }

    #[test]
    fn mcp_service_uses_implementation_type_and_source_path() {
        let services = map_source(
            "use rmcp::{ServiceExt, ServerHandler};\n\
             struct Inventory;\n\
             impl ServerHandler for Inventory {}",
        );
        assert_eq!(services.len(), 1);
        let service = &services[0];
        assert_eq!(service.entry_symbol.as_deref(), Some("Inventory"));
        assert_eq!(service.entry_path, "src/transport/bridge.rs");
        assert_eq!(service.title, "MCP server `Inventory` (tools)");
        assert_eq!(service.source, "rust-mcp-server");
        assert_eq!(service.language, Language::Rust);
        assert_eq!(service.confidence, FeatureConfidence::High);
        assert_eq!(service.context_files[0].path, "Cargo.toml");
    }

    #[test]
    fn mcp_service_resolves_explicit_trait_and_crate_aliases() {
        for declaration in [
            "impl rmcp::ServerHandler for transport::Bridge {}",
            "impl ::rmcp::ServerHandler for transport::Bridge {}",
            "use rmcp::ServerHandler as Handler; impl Handler for transport::Bridge {}",
            "use {rmcp::{ServerHandler as Handler}}; impl Handler for transport::Bridge {}",
            "use rmcp as sdk; impl sdk::ServerHandler for transport::Bridge {}",
            "use rmcp::{self as sdk}; use sdk::ServerHandler as Handler; \
             impl Handler for transport::Bridge {}",
            "extern crate rmcp as sdk; impl sdk::ServerHandler for transport::Bridge {}",
            "extern crate rmcp; impl rmcp::ServerHandler for transport::Bridge {}",
            "mod endpoint { use rmcp::ServerHandler; \
             impl ServerHandler for transport::Bridge {} }",
        ] {
            let services = map_source(declaration);
            assert_eq!(services.len(), 1, "{declaration}");
            assert_eq!(
                services[0].entry_symbol.as_deref(),
                Some("transport::Bridge"),
                "{declaration}",
            );
        }
    }

    #[test]
    fn mcp_service_child_module_keeps_external_prelude_despite_parent_shadow() {
        let services = map_source(
            "mod rmcp { pub trait ServerHandler {} }\n\
             impl rmcp::ServerHandler for Local {}\n\
             mod inner { struct Server; impl rmcp::ServerHandler for Server {} }",
        );
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].entry_symbol.as_deref(), Some("inner::Server"));
    }

    #[test]
    fn mcp_service_rejects_local_glob_namespace_collisions() {
        for import in ["use local::*;", "use self::local::{*};"] {
            for implementation in [
                "impl rmcp::ServerHandler for Local {}",
                "use rmcp::ServerHandler; impl ServerHandler for Local {}",
                "use rmcp as sdk; impl sdk::ServerHandler for Local {}",
            ] {
                let source = format!(
                    "mod local {{ pub mod rmcp {{ pub trait ServerHandler {{}} }} }}\n\
                     {import} struct Local; {implementation}"
                );
                assert!(map_source(&source).is_empty(), "{source}");
            }
        }
        for file in [
            "src/transport/bridge/local.rs",
            "src/transport/bridge/local/mod.rs",
        ] {
            for implementation in [
                "impl rmcp::ServerHandler for Local {}",
                "use rmcp::ServerHandler; impl ServerHandler for Local {}",
            ] {
                let source = format!("mod local; use local::*; struct Local; {implementation}");
                assert!(
                    map_source_at(
                        "src/transport/bridge/mod.rs",
                        &source,
                        &[(file, "pub mod rmcp { pub trait ServerHandler {} }")],
                    )
                    .is_empty(),
                    "{file}: {source}",
                );
            }
        }
    }

    #[test]
    fn mcp_service_keeps_proven_external_paths_and_unrelated_globs() {
        for implementation in [
            "impl ::rmcp::ServerHandler for External {}",
            "use ::rmcp::ServerHandler; impl ServerHandler for External {}",
            "use ::rmcp as sdk; impl sdk::ServerHandler for External {}",
            "extern crate rmcp; impl rmcp::ServerHandler for External {}",
        ] {
            let source = format!(
                "mod local {{ pub mod rmcp {{ pub trait ServerHandler {{}} }} }}\n\
                 use local::*; struct External; {implementation}"
            );
            assert_eq!(map_source(&source).len(), 1, "{source}");
        }
        let implementation = "use rmcp::ServerHandler; impl ServerHandler for External {}";
        assert_eq!(
            map_source(&format!(
                "mod local {{ pub struct Other; }} use local::*; {implementation}"
            ))
            .len(),
            1,
        );
        assert_eq!(
            map_source_at(
                "src/transport/bridge/mod.rs",
                &format!("mod local; use local::*; {implementation}"),
                &[("src/transport/bridge/local.rs", "pub struct Other;")],
            )
            .len(),
            1,
        );
    }

    #[test]
    fn mcp_service_keeps_unknown_glob_exports_unproven() {
        for declaration in [
            "mod local;",
            "mod local { pub use other::*; }",
            "mod local { pub use other as rmcp; }",
            "mod local { make_modules!(); }",
            "#[path = \"elsewhere.rs\"] mod local;",
        ] {
            let source = format!(
                "{declaration} use local::*; use rmcp::ServerHandler; \
                 impl ServerHandler for Unproven {{}}"
            );
            assert!(map_source(&source).is_empty(), "{source}");
        }
        assert!(
            map_source_files(
                "mod local; use local::*; impl rmcp::ServerHandler for Ambiguous {}",
                &[
                    ("src/transport/bridge/local.rs", "pub struct Other;"),
                    ("src/transport/bridge/local/mod.rs", "pub struct Other;"),
                ],
            )
            .is_empty(),
        );
    }

    #[test]
    fn mcp_service_distinguishes_crate_roots_from_reserved_module_stems() {
        for stem in ["main", "lib"] {
            let directory = tempfile::tempdir().unwrap();
            write(
                directory.path(),
                "Cargo.toml",
                "[package]\nname = \"tools\"\nedition = \"2024\"\n",
            );
            write(directory.path(), "src/lib.rs", "mod transport;");
            write(
                directory.path(),
                "src/transport.rs",
                &format!("mod {stem};"),
            );
            write(
                directory.path(),
                &format!("src/transport/{stem}.rs"),
                "mod local; use self::local::*; struct Local; impl rmcp::ServerHandler for Local {}",
            );
            write(
                directory.path(),
                "src/transport/local.rs",
                "pub struct Other;",
            );
            write(
                directory.path(),
                &format!("src/transport/{stem}/local.rs"),
                "pub mod rmcp { pub trait ServerHandler {} }",
            );
            assert!(
                RustMapper
                    .map(&MapperContext::for_root(directory.path()))
                    .unwrap()
                    .iter()
                    .all(|seed| seed.kind != FeatureKind::Service),
                "{stem}",
            );
        }
        for (target, extra_manifest, child) in [
            ("src/lib.rs", "", "src/local.rs"),
            ("src/main.rs", "", "src/local.rs"),
            ("src/bin/tool.rs", "", "src/bin/local.rs"),
            ("src/bin/tool/main.rs", "", "src/bin/tool/local.rs"),
            (
                "src/custom.rs",
                "[lib]\npath = \"src/custom.rs\"\n",
                "src/local.rs",
            ),
            (
                "src/custom.rs",
                "[[bin]]\nname = \"tool\"\npath = \"src/custom.rs\"\n",
                "src/local.rs",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            write(
                directory.path(),
                "Cargo.toml",
                &format!("[package]\nname = \"tools\"\nedition = \"2024\"\n{extra_manifest}"),
            );
            write(
                directory.path(),
                target,
                "mod local; use local::*; use rmcp::ServerHandler; struct Server; impl ServerHandler for Server {}",
            );
            write(directory.path(), child, "pub struct Other;");
            let map = || {
                RustMapper
                    .map(&MapperContext::for_root(directory.path()))
                    .unwrap()
            };
            assert_eq!(
                map()
                    .iter()
                    .filter(|seed| seed.kind == FeatureKind::Service)
                    .count(),
                1,
                "{target}"
            );
            write(
                directory.path(),
                child,
                "pub mod rmcp { pub trait ServerHandler {} }",
            );
            assert!(
                map().iter().all(|seed| seed.kind != FeatureKind::Service),
                "{target}"
            );
        }
    }

    #[test]
    fn mcp_service_respects_disabled_auto_targets_and_unknown_manifests() {
        let directory = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"tools\"\nautobins = false\n",
        );
        write(
            directory.path(),
            "src/bin/tool.rs",
            "mod local; use local::*; impl rmcp::ServerHandler for Local {}",
        );
        write(directory.path(), "src/bin/local.rs", "pub struct Other;");
        write(
            directory.path(),
            "src/bin/tool/local.rs",
            "pub mod rmcp { pub trait ServerHandler {} }",
        );
        let map = || {
            RustMapper
                .map(&MapperContext::for_root(directory.path()))
                .unwrap()
        };
        assert!(map().iter().all(|seed| seed.kind != FeatureKind::Service));
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"tools\"\nautobins = \"invalid\"\n",
        );
        assert!(map().iter().all(|seed| seed.kind != FeatureKind::Service));
    }

    #[test]
    fn mcp_service_honors_explicit_bin_overrides_and_edition_auto_defaults() {
        for (edition, binary_name) in [
            ("", "custom"),
            ("edition = \"2015\"\n", "custom"),
            ("edition = \"2024\"\n", "tool"),
            ("edition.workspace = true\n", "tool"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            write(
                directory.path(),
                "Cargo.toml",
                &format!(
                    "[package]\nname = \"tools\"\n{edition}\n\
                 [[bin]]\nname = \"{binary_name}\"\npath = \"src/custom.rs\"\n\
                 [workspace.package]\nedition = \"2024\"\n"
                ),
            );
            write(directory.path(), "src/lib.rs", "mod bin;");
            write(directory.path(), "src/bin.rs", "mod tool;");
            write(directory.path(), "src/custom.rs", "fn main() {}");
            write(
                directory.path(),
                "src/bin/tool.rs",
                "mod local; use self::local::*; struct Local; impl rmcp::ServerHandler for Local {}",
            );
            write(
                directory.path(),
                "src/bin/local.rs",
                "pub struct Other; fn main() {}",
            );
            write(
                directory.path(),
                "src/bin/tool/local.rs",
                "pub mod rmcp { pub trait ServerHandler {} }",
            );
            assert!(
                RustMapper
                    .map(&MapperContext::for_root(directory.path()))
                    .unwrap()
                    .iter()
                    .all(|seed| seed.kind != FeatureKind::Service),
                "{edition}: {binary_name}"
            );
        }
    }

    #[test]
    fn mcp_service_does_not_assume_the_mapper_root_owns_inherited_editions() {
        let directory = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "Cargo.toml",
            "[workspace]\nexclude = [\"crates/local\"]\n[workspace.package]\nedition = \"2024\"\n",
        );
        write(
            directory.path(),
            "crates/local/Cargo.toml",
            "[package]\nname = \"local\"\nedition.workspace = true\n\
             [workspace.package]\nedition = \"2015\"\n\
             [[bin]]\nname = \"custom\"\npath = \"src/custom.rs\"\n",
        );
        write(
            directory.path(),
            "crates/local/src/lib.rs",
            "mod bin; mod service;",
        );
        write(directory.path(), "crates/local/src/bin.rs", "mod tool;");
        write(
            directory.path(),
            "crates/local/src/custom.rs",
            "fn main() {}",
        );
        write(
            directory.path(),
            "crates/local/src/bin/tool.rs",
            "mod local; use self::local::*; struct Local; impl rmcp::ServerHandler for Local {}",
        );
        write(
            directory.path(),
            "crates/local/src/bin/local.rs",
            "pub struct Other;",
        );
        write(
            directory.path(),
            "crates/local/src/bin/tool/local.rs",
            "pub mod rmcp { pub trait ServerHandler {} }",
        );
        let map = || {
            RustMapper
                .map(&MapperContext::for_root(directory.path()))
                .unwrap()
        };
        assert!(map().iter().all(|seed| seed.kind != FeatureKind::Service));
        write(
            directory.path(),
            "crates/local/Cargo.toml",
            "[package]\nname = \"local\"\nedition.workspace = true\nworkspace = \"../elsewhere\"\n\
             [[bin]]\nname = \"custom\"\npath = \"src/custom.rs\"\n",
        );
        assert!(map().iter().all(|seed| seed.kind != FeatureKind::Service));
        write(
            directory.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/local\"]\n[workspace.package]\nedition = \"2024\"\n",
        );
        write(
            directory.path(),
            "crates/local/Cargo.toml",
            "[package]\nname = \"local\"\nedition.workspace = true\nautobins = false\n\
             [[bin]]\nname = \"custom\"\npath = \"src/custom.rs\"\n",
        );
        write(
            directory.path(),
            "crates/local/src/service/mod.rs",
            "mod params; use params::*; use rmcp::ServerHandler; impl ServerHandler for External {}",
        );
        write(
            directory.path(),
            "crates/local/src/service/params.rs",
            "pub struct Params;",
        );
        let services: Vec<_> = map()
            .into_iter()
            .filter(|seed| seed.kind == FeatureKind::Service)
            .collect();
        assert_eq!(services.len(), 2);
        assert!(
            services
                .iter()
                .all(|seed| seed.entry_symbol.as_deref() == Some("External"))
        );
    }

    #[test]
    fn mcp_service_requires_modern_absolute_origin_or_explicit_real_extern() {
        let directory = tempfile::tempdir().unwrap();
        let map = || {
            RustMapper
                .map(&MapperContext::for_root(directory.path()))
                .unwrap()
                .into_iter()
                .filter(|seed| seed.kind == FeatureKind::Service)
                .collect::<Vec<_>>()
        };
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"legacy\"\nedition = \"2015\"\n",
        );
        write(
            directory.path(),
            "src/lib.rs",
            "mod rmcp { pub trait ServerHandler {} } struct Local; impl ::rmcp::ServerHandler for Local {}",
        );
        assert!(map().is_empty());
        write(
            directory.path(),
            "src/lib.rs",
            "extern crate rmcp as sdk; struct External; impl sdk::ServerHandler for External {}",
        );
        assert_eq!(map().len(), 1);
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"modern\"\nedition = \"2024\"\n",
        );
        for declaration in ["extern crate self as rmcp;", "extern crate other as rmcp;"] {
            write(
                directory.path(),
                "src/lib.rs",
                &format!(
                    "{declaration} pub trait ServerHandler {{}} struct Local; impl ::rmcp::ServerHandler for Local {{}}"
                ),
            );
            assert!(map().is_empty(), "{declaration}");
            write(
                directory.path(),
                "src/lib.rs",
                &format!("{declaration} pub trait ServerHandler {{}} mod endpoint;"),
            );
            write(
                directory.path(),
                "src/endpoint.rs",
                "struct Local; impl ::rmcp::ServerHandler for Local {}",
            );
            assert!(map().is_empty(), "child of {declaration}");
        }
    }

    #[test]
    fn mcp_service_does_not_guess_attributed_module_directories() {
        let directory = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"paths\"\nedition = \"2024\"\n",
        );
        write(
            directory.path(),
            "src/lib.rs",
            "#[path = \"renamed.rs\"] mod child;\n\
             #[path = \"actual\"] mod endpoint { mod local; use local::*; struct Local; impl rmcp::ServerHandler for Local {} }",
        );
        write(
            directory.path(),
            "src/renamed.rs",
            "mod local; use local::*; struct Local; impl rmcp::ServerHandler for Local {}",
        );
        write(
            directory.path(),
            "src/local.rs",
            "pub mod rmcp { pub trait ServerHandler {} }",
        );
        write(
            directory.path(),
            "src/renamed/local.rs",
            "pub struct Other;",
        );
        write(
            directory.path(),
            "src/actual/local.rs",
            "pub mod rmcp { pub trait ServerHandler {} }",
        );
        write(
            directory.path(),
            "src/endpoint/local.rs",
            "pub struct Other;",
        );
        assert!(
            RustMapper
                .map(&MapperContext::for_root(directory.path()))
                .unwrap()
                .iter()
                .all(|seed| seed.kind != FeatureKind::Service)
        );
    }

    #[test]
    fn mcp_service_treats_item_macros_as_unknown_namespace_expansions() {
        for implementation in [
            "impl rmcp::ServerHandler for Local {}",
            "use rmcp::ServerHandler; impl ServerHandler for Local {}",
        ] {
            assert!(map_source(&format!(
                "macro_rules! make_local {{ () => {{ mod rmcp {{ pub trait ServerHandler {{}} }} }} }} \
                 make_local!(); struct Local; {implementation}"
            )).is_empty());
        }
        assert_eq!(
            map_source("make_items!(); impl ::rmcp::ServerHandler for External {}").len(),
            1
        );
        assert_eq!(
            map_source(
                "fn helper() { println!(\"hello\"); } impl rmcp::ServerHandler for External {}"
            )
            .len(),
            1
        );
    }

    #[test]
    fn mcp_service_keeps_same_named_inline_types_distinct() {
        let services = map_source(
            r###"
            mod alpha {
                mod nested {
                    struct Server;
                    impl rmcp::ServerHandler for Server {}
                }
            }
            mod beta {
                mod nested {
                    struct Server;
                    impl rmcp::ServerHandler for Server {}
                }
            }
            "###,
        );
        let names: Vec<_> = services
            .iter()
            .map(|seed| seed.entry_symbol.as_deref().unwrap())
            .collect();
        assert_eq!(names, ["alpha::nested::Server", "beta::nested::Server"]);
        assert!(
            services
                .iter()
                .all(|seed| seed.entry_path == "src/transport/bridge.rs")
        );
    }

    #[test]
    fn mcp_service_normalizes_self_and_super_but_preserves_root_paths() {
        let services = map_source(
            r###"
            struct RootServer;
            impl rmcp::ServerHandler for crate::RootServer {}
            impl rmcp::ServerHandler for super::OuterServer {}
            mod nested {
                struct SelfServer;
                impl rmcp::ServerHandler for self::SelfServer {}
                struct Parent;
                mod deeper {
                    impl rmcp::ServerHandler for super::Parent {}
                }
            }
            "###,
        );
        let names: Vec<_> = services
            .iter()
            .map(|seed| seed.entry_symbol.as_deref().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "crate::RootServer",
                "nested::Parent",
                "nested::SelfServer",
                "super::OuterServer"
            ]
        );
    }

    #[test]
    fn mcp_service_rejects_lexical_decoys_and_ambiguous_bindings() {
        let services = map_source(
            r###"
            // impl rmcp::ServerHandler for CommentDecoy {}
            const DECOY: &str = "impl rmcp::ServerHandler for StringDecoy {}";
            trait ServerHandler {}
            impl ServerHandler for LocalTraitDecoy {}
            mod unrelated { use other::ServerHandler; impl ServerHandler for OtherDecoy {} }
            mod wildcard { use rmcp::*; impl ServerHandler for WildcardDecoy {} }
            mod shadow {
                mod rmcp { pub trait ServerHandler {} }
                impl rmcp::ServerHandler for ShadowDecoy {}
            }
            mod alias_shadow {
                use rmcp as sdk;
                mod sdk { pub trait ServerHandler {} }
                impl sdk::ServerHandler for AliasShadowDecoy {}
            }
            mod conflicting {
                use rmcp::ServerHandler as Handler;
                use other::ServerHandler as Handler;
                impl Handler for ConflictingDecoy {}
            }
            mod parent_import { impl ServerHandler for ParentImportDecoy {} }
            impl rmcp::ClientHandler for ClientDecoy {}
            impl !rmcp::ServerHandler for NegativeDecoy {}
            impl<T> rmcp::ServerHandler for RealServer<T> {}
            "###,
        );
        let names: Vec<_> = services
            .iter()
            .map(|seed| seed.entry_symbol.as_deref().unwrap())
            .collect();
        assert_eq!(names, ["RealServer"]);
    }

    #[test]
    fn mcp_service_skips_test_gated_implementations() {
        let services = map_source(
            r###"
            use rmcp::ServerHandler;
            #[cfg(test)]
            impl ServerHandler for TestOnly {}
            #[cfg(all(feature = "x", all(test)))]
            mod nested {
                use rmcp::ServerHandler;
                impl ServerHandler for NestedTestOnly {}
            }
            #[cfg(test)]
            // A comment must not separate the cfg from its item.
            mod commented { impl rmcp::ServerHandler for CommentedTestOnly {} }
            mod inner_attribute {
                #![cfg(test)]
                impl rmcp::ServerHandler for InnerTestOnly {}
            }
            #[cfg(not(test))]
            impl ServerHandler for Product {}
            #[cfg(any(test, feature = "x"))]
            impl ServerHandler for OptionalProduct {}
            "###,
        );
        let names: Vec<_> = services
            .iter()
            .map(|seed| seed.entry_symbol.as_deref().unwrap())
            .collect();
        assert_eq!(names, ["OptionalProduct", "Product"]);
        assert!(map_source("#![cfg(test)]\nimpl rmcp::ServerHandler for TestFile {}").is_empty());
    }

    #[test]
    fn mcp_service_does_not_read_nonregular_or_excluded_roots() {
        let directory = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"bridge\"\nedition = \"2024\"\n[lib]\npath = \"root.rs\"\n",
        );
        write(
            directory.path(),
            "src/service.rs",
            "impl rmcp::ServerHandler for Unproven {}",
        );
        std::fs::create_dir(directory.path().join("root.rs")).unwrap();
        assert!(
            RustMapper
                .map(&MapperContext::for_root(directory.path()))
                .unwrap()
                .iter()
                .all(|seed| seed.kind != FeatureKind::Service)
        );
        std::fs::remove_dir(directory.path().join("root.rs")).unwrap();
        write(directory.path(), "root.rs", "mod service;");
        let excludes = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("root.rs").unwrap())
            .build()
            .unwrap();
        assert!(
            RustMapper
                .map(&MapperContext {
                    root: directory.path(),
                    excludes: Some(&excludes),
                })
                .unwrap()
                .iter()
                .all(|seed| seed.kind != FeatureKind::Service)
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(directory.path().join("root.rs")).unwrap();
            write(directory.path(), "actual.rs", "mod service;");
            std::os::unix::fs::symlink("actual.rs", directory.path().join("root.rs")).unwrap();
            assert!(
                RustMapper
                    .map(&MapperContext::for_root(directory.path()))
                    .unwrap()
                    .iter()
                    .all(|seed| seed.kind != FeatureKind::Service)
            );
        }
    }

    #[test]
    fn mcp_service_honors_excludes_and_test_paths() {
        let directory = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "Cargo.toml",
            "[package]\nname = \"bridge\"\nedition = \"2024\"\n",
        );
        for (path, name) in [
            ("src/real.rs", "Real"),
            ("src/hidden/server.rs", "Excluded"),
            ("src/tests/server.rs", "InTests"),
            ("src/bridge_test.rs", "TestSuffix"),
        ] {
            write(
                directory.path(),
                path,
                &format!("impl rmcp::ServerHandler for {name} {{}}"),
            );
        }
        let excludes = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("src/hidden").unwrap())
            .build()
            .unwrap();
        let seeds = RustMapper
            .map(&MapperContext {
                root: directory.path(),
                excludes: Some(&excludes),
            })
            .unwrap();
        let names: Vec<_> = seeds
            .iter()
            .filter(|seed| seed.kind == FeatureKind::Service)
            .map(|seed| seed.entry_symbol.as_deref().unwrap())
            .collect();
        assert_eq!(names, ["Real"]);
    }
}
