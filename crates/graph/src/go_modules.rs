use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use codesage_parser::discover::{content_hash, discover_go_manifests};
use codesage_parser::go_imports::{decode_import_path, package_header_excluded};
use codesage_parser::parse::parse_file_tolerant;
use codesage_parser::position::{node_end_utf8, node_start_utf8};
use codesage_parser::references::simple_go_selector_parts;
use codesage_parser::source::{read_indexable_file_bounded, read_indexable_file_prefix};
use codesage_protocol::work::checkpoint;
use codesage_protocol::{Language, Reference, ReferenceKind, Symbol, SymbolKind};
use codesage_storage::Database;
use tree_sitter::Node;

#[derive(Debug, Default)]
pub(crate) struct GoModules {
    packages: HashMap<String, Package>,
    pub(crate) capped: bool,
}

#[derive(Debug)]
struct Package {
    root: Arc<str>,
    module: Arc<str>,
    files: Vec<String>,
}

impl GoModules {
    pub(crate) fn load<'a>(
        db: &Database,
        files: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self> {
        Ok(Self::load_until(db, files, None)?.unwrap_or(Self {
            capped: true,
            ..Self::default()
        }))
    }

    fn load_until<'a>(
        db: &Database,
        files: impl IntoIterator<Item = &'a str>,
        deadline: Option<Instant>,
    ) -> Result<Option<Self>> {
        let Some(root) = crate::constructor_imports::project_root(db) else {
            return Ok(Some(Self::default()));
        };
        Self::from_indexed(root, files, deadline)
    }

    fn from_indexed<'a>(
        root: &Path,
        files: impl IntoIterator<Item = &'a str>,
        deadline: Option<Instant>,
    ) -> Result<Option<Self>> {
        let mut dirs: HashMap<&str, Vec<String>> = HashMap::new();
        for file in files {
            checkpoint()?;
            if expired(deadline) {
                return Ok(None);
            }
            if let Some(dir) = package_dir(file) {
                let members = dirs.entry(dir).or_default();
                if !file.ends_with("_test.go") {
                    members.push(file.to_owned());
                }
            }
        }
        let mut manifests = HashMap::<&str, Manifest>::new();
        let mut primary = HashMap::<Arc<str>, String>::new();
        let mut packages = HashMap::new();
        for (dir, mut files) in dirs {
            checkpoint()?;
            let mut found_boundary = false;
            for ancestor in ancestors(dir) {
                checkpoint()?;
                if expired(deadline) {
                    return Ok(None);
                }
                let manifest = manifests
                    .entry(ancestor)
                    .or_insert_with(|| read_manifest(root, ancestor));
                let module = match manifest {
                    Manifest::Missing => continue,
                    Manifest::Indeterminate => return Ok(None),
                    Manifest::Boundary => {
                        found_boundary = true;
                        continue;
                    }
                    Manifest::Module(module) => module,
                };
                select_manifest(&mut primary, module, ancestor);
                if !found_boundary {
                    found_boundary = true;
                    files.sort_unstable();
                    packages.insert(
                        dir.to_owned(),
                        Package {
                            root: Arc::from(ancestor),
                            module: Arc::clone(module),
                            files: std::mem::take(&mut files),
                        },
                    );
                }
            }
        }
        if let Some(max_depth) = primary.values().map(|root| depth(root)).max()
            && max_depth > 0
        {
            let filter = crate::drift::IndexableFilter::for_project(root);
            let Some(paths) = discover_go_manifests(root, filter.excludes(), max_depth, deadline)?
            else {
                return Ok(None);
            };
            for path in paths {
                checkpoint()?;
                if expired(deadline) {
                    return Ok(None);
                }
                let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
                if !manifests.contains_key(dir) {
                    match read_manifest(root, dir) {
                        Manifest::Indeterminate => return Ok(None),
                        Manifest::Module(module) if primary.contains_key(&module) => {
                            select_manifest(&mut primary, &module, dir);
                        }
                        _ => {}
                    }
                }
            }
        }
        packages.retain(|_, package| primary[&package.module] == package.root.as_ref());
        Ok((!expired(deadline)).then_some(Self {
            packages,
            ..Self::default()
        }))
    }

    pub(crate) fn targets(&self, caller: &str, import: &str) -> &[String] {
        let Some(source) = package_dir(caller).and_then(|dir| self.packages.get(dir)) else {
            return &[];
        };
        let target_dir = if import == source.module.as_ref() {
            source.root.to_string()
        } else if let Some(path) = import
            .strip_prefix(source.module.as_ref())
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|path| valid_import_path(path))
        {
            if source.root.is_empty() {
                path.to_owned()
            } else {
                format!("{}/{path}", source.root)
            }
        } else {
            return &[];
        };
        self.packages
            .get(&target_dir)
            .filter(|package| package.root == source.root && package.module == source.module)
            .map_or(&[], |package| package.files.as_slice())
    }
}

pub(crate) fn package_dir(file: &str) -> Option<&str> {
    let (dir, basename) = file.rsplit_once('/').unwrap_or(("", file));
    (basename.ends_with(".go")
        && !basename.starts_with(['.', '_'])
        && (dir.is_empty()
            || dir
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".." | "vendor"))))
    .then_some(dir)
}

fn ancestors(dir: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(dir), |dir| {
        (!dir.is_empty()).then(|| dir.rsplit_once('/').map_or("", |(parent, _)| parent))
    })
}

fn depth(dir: &str) -> usize {
    dir.split('/').filter(|part| !part.is_empty()).count()
}

fn select_manifest(primary: &mut HashMap<Arc<str>, String>, module: &Arc<str>, root: &str) {
    primary
        .entry(Arc::clone(module))
        .and_modify(|selected| {
            if (depth(root), root) < (depth(selected), selected.as_str()) {
                root.clone_into(selected);
            }
        })
        .or_insert_with(|| root.to_owned());
}

enum Manifest {
    Missing,
    Boundary,
    Indeterminate,
    Module(Arc<str>),
}

fn read_manifest(root: &Path, dir: &str) -> Manifest {
    let relative = Path::new(dir).join("go.mod");
    match read_indexable_file_bounded(root, &relative, MAX_MANIFEST_BYTES) {
        Ok(Some(source)) => {
            let module = std::str::from_utf8(&source.bytes)
                .ok()
                .and_then(manifest_module);
            module
                .filter(|module| valid_import_path(module))
                .map_or(Manifest::Boundary, |module| {
                    Manifest::Module(Arc::from(module.as_ref()))
                })
        }
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|err| err.kind() == io::ErrorKind::NotFound) =>
        {
            Manifest::Missing
        }
        _ => Manifest::Indeterminate,
    }
}

fn manifest_module(text: &str) -> Option<Cow<'_, str>> {
    enum Block {
        Module,
        Other,
    }
    let mut module = None;
    let mut block = None;
    for line in text.lines() {
        let line = manifest_line(line)?.trim();
        if line.is_empty() {
            continue;
        }
        match block {
            Some(Block::Module) if line == ")" && module.is_some() => block = None,
            Some(Block::Module) if module.is_none() => {
                module = Some(manifest_path(line)?);
            }
            Some(Block::Module) => return None,
            Some(Block::Other) if line == ")" => block = None,
            Some(Block::Other) => {}
            None => {
                let directive = line.strip_prefix("module").filter(|value| {
                    value.is_empty()
                        || value.starts_with(char::is_whitespace)
                        || value.starts_with('(')
                });
                if let Some(value) = directive {
                    if module.is_some() {
                        return None;
                    }
                    let value = value.trim_start();
                    if value == "(" {
                        block = Some(Block::Module);
                    } else {
                        module = Some(manifest_path(value)?);
                    }
                } else if line.ends_with('(') {
                    block = Some(Block::Other);
                }
            }
        }
    }
    if block.is_some() { None } else { module }
}

fn manifest_path(value: &str) -> Option<Cow<'_, str>> {
    let path = if value.starts_with('"') {
        decode_import_path(value)?
    } else {
        if value.contains(['(', ')', '[', ']', '{', '}', ',', '\'']) {
            return None;
        }
        Cow::Borrowed(value)
    };
    valid_import_path(&path).then_some(path)
}

fn manifest_line(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut quoted = false;
    while let Some(&byte) = bytes.get(i) {
        match (byte, quoted) {
            (b'"', _) => quoted = !quoted,
            (b'\\', true) => i += 1,
            (b'/', false) if bytes.get(i + 1) == Some(&b'/') => return Some(&line[..i]),
            (b'/', false) if bytes.get(i + 1) == Some(&b'*') => return None,
            _ => {}
        }
        i += 1;
    }
    (!quoted).then_some(line)
}

fn valid_import_path(path: &str) -> bool {
    !path.contains(['\\', '"', '`'])
        && !path.chars().any(char::is_whitespace)
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

pub(crate) fn selector_call(row: &Reference) -> bool {
    call_selector(row).is_some()
}

pub(crate) fn selector_may_target(row: &Reference, symbol: &Symbol) -> bool {
    let Some((_, member)) = call_selector(row) else {
        return true;
    };
    // Go's generic resolver can only match the dotted spelling exactly.
    // Package selection can additionally match an exported free function.
    (row.to_name == symbol.qualified_name && same_package_directory(row, symbol))
        || (symbol.kind == SymbolKind::Function
            && symbol.file_path.ends_with(".go")
            && symbol.name == member
            && member.starts_with(char::is_uppercase))
}

fn same_package_directory(row: &Reference, symbol: &Symbol) -> bool {
    package_dir(&row.from_file).is_some_and(|dir| package_dir(&symbol.file_path) == Some(dir))
}

fn call_selector(row: &Reference) -> Option<(&str, &str)> {
    if row.kind != ReferenceKind::Call || !row.from_file.ends_with(".go") {
        return None;
    }
    selector(&row.to_name)
}

fn selector(name: &str) -> Option<(&str, &str)> {
    let (receiver, member) = name.split_once('.')?;
    let (receiver, member) = (receiver.trim(), member.trim());
    (identifier(receiver) && identifier(member)).then_some((receiver, member))
}

fn identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCES: usize = 128;
const MAX_NODES: usize = 100_000;
const MAX_MEMBER_ENTRIES: usize = 256;
const MAX_MEMBER_BYTES: usize = 512 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_HEADER_FILES: usize = 4096;

#[derive(Default)]
pub(crate) struct GoFileHeaders {
    ignored: HashMap<String, Option<bool>>,
    pub(crate) capped: bool,
}

impl GoFileHeaders {
    pub(crate) fn excludes(&mut self, db: &Database, path: &str) -> Result<bool> {
        if let Some(ignored) = self.ignored.get(path) {
            return Ok(*ignored == Some(true));
        }
        if self.ignored.len() == MAX_HEADER_FILES {
            self.capped = true;
            return Ok(false);
        }
        checkpoint()?;
        let ignored = match db.overlay_source(path)? {
            Some(Some(source)) => package_header_excluded(
                &source.as_bytes()[..source.len().min(MAX_HEADER_BYTES)],
                source.len() <= MAX_HEADER_BYTES,
            ),
            Some(None) => None,
            None => crate::constructor_imports::project_root(db)
                .and_then(|root| {
                    read_indexable_file_prefix(root, Path::new(path), MAX_HEADER_BYTES)
                        .ok()
                        .flatten()
                })
                .and_then(|prefix| {
                    package_header_excluded(&prefix, prefix.len() < MAX_HEADER_BYTES)
                }),
        };
        self.ignored.insert(path.to_owned(), ignored);
        Ok(ignored == Some(true))
    }
}

#[derive(Default)]
pub(crate) struct GoCalls {
    modules: Option<Arc<GoModules>>,
    sources: HashMap<String, Option<Arc<Surface>>>,
    members: HashMap<String, HashMap<String, Option<Symbol>>>,
    member_entries: usize,
    member_bytes: usize,
    bytes: usize,
    capped: bool,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct Surface {
    package: String,
    imports: Vec<(Option<String>, String)>,
    shadows: HashMap<String, Vec<BindingScope>>,
    normalized_selectors: HashSet<Position>,
}

pub(crate) enum GoResolution {
    Unrelated,
    Proven(Vec<Symbol>),
    Unknown,
}

impl GoResolution {
    pub(crate) fn into_targets(self) -> Option<Vec<Symbol>> {
        match self {
            Self::Unrelated => None,
            Self::Proven(targets) => Some(targets),
            Self::Unknown => Some(Vec::new()),
        }
    }
}

impl GoCalls {
    pub(crate) fn with_deadline(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            ..Self::default()
        }
    }

    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    pub(crate) fn use_modules(&mut self, modules: &Arc<GoModules>) {
        self.modules.get_or_insert_with(|| Arc::clone(modules));
        self.capped |= modules.capped;
    }

    pub(crate) fn resolve(&mut self, db: &Database, row: &Reference) -> Result<GoResolution> {
        use GoResolution::{Proven, Unknown, Unrelated};
        let Some((receiver, member)) = call_selector(row) else {
            return Ok(Unrelated);
        };
        let caller = &row.from_file;
        let Some(source) = self.surface(db, caller)? else {
            return Ok(Unknown);
        };
        let without_package = if source.normalized_selectors.contains(&(row.line, row.col)) {
            Unknown
        } else {
            Unrelated
        };
        if source.imports.is_empty() {
            return Ok(without_package);
        }
        if self.modules.is_none() {
            let paths = db.all_file_paths()?;
            let modules =
                GoModules::load_until(db, paths.iter().map(String::as_str), self.deadline)?
                    .unwrap_or(GoModules {
                        capped: true,
                        ..GoModules::default()
                    });
            self.use_modules(&Arc::new(modules));
        }
        if self.modules.as_ref().expect("modules loaded").capped {
            return Ok(Unknown);
        }
        let mut matched = None;
        for (alias, import) in &source.imports {
            checkpoint()?;
            let matches = if let Some(alias) = alias {
                alias == receiver
            } else if let Some(target) = self.package_surface(db, caller, import)? {
                target.package == receiver
            } else {
                false
            };
            if matches && matched.replace(import.as_str()).is_some() {
                return Ok(Proven(Vec::new()));
            }
        }
        let Some(import) = matched else {
            return Ok(if self.capped {
                Unknown
            } else {
                without_package
            });
        };
        if source.shadows.get(receiver).is_some_and(|scopes| {
            let position = (row.line, row.col);
            scopes
                .iter()
                .any(|&(start, end)| start <= position && position < end)
        }) || !member.starts_with(char::is_uppercase)
        {
            return Ok(Proven(Vec::new()));
        }
        let Some(package) = self.package_surface(db, caller, import)? else {
            return Ok(if self.capped {
                Unknown
            } else {
                Proven(Vec::new())
            });
        };
        self.resolve_member(db, caller, import, &package.package, member)
    }

    pub(crate) fn retain_fallback_targets(
        &mut self,
        db: &Database,
        row: &Reference,
        targets: &mut Vec<Symbol>,
    ) -> Result<()> {
        if targets.is_empty() || call_selector(row).is_none() {
            return Ok(());
        }
        let Some(caller) = self.surface(db, &row.from_file)? else {
            targets.clear();
            return Ok(());
        };
        // Without a verified package import, exact method spellings are only
        // evidence in the caller's own package. An external package's name
        // need not resemble its import path.
        for target in std::mem::take(targets) {
            checkpoint()?;
            if same_package_directory(row, &target)
                && (target.file_path == row.from_file
                    || self
                        .surface(db, &target.file_path)?
                        .is_some_and(|file| file.package == caller.package))
            {
                targets.push(target);
            }
        }
        Ok(())
    }

    fn resolve_member(
        &mut self,
        db: &Database,
        caller: &str,
        import: &str,
        package: &str,
        member: &str,
    ) -> Result<GoResolution> {
        if self.capped || expired(self.deadline) {
            self.capped = true;
            return Ok(GoResolution::Unknown);
        }
        let files = self
            .modules
            .as_ref()
            .expect("modules loaded")
            .targets(caller, import);
        let Some(dir) = files.first().and_then(|file| package_dir(file)) else {
            return Ok(GoResolution::Proven(Vec::new()));
        };
        if let Some(cached) = self
            .members
            .get(dir)
            .and_then(|members| members.get(member))
        {
            return Ok(GoResolution::Proven(cached.iter().cloned().collect()));
        }
        let dir = dir.to_owned();
        let candidates = db
            .find_symbols(member, Some(SymbolKind::Function))?
            .into_iter()
            .filter(|symbol| files.binary_search(&symbol.file_path).is_ok())
            .collect::<Vec<_>>();
        let mut found = None;
        for symbol in candidates {
            checkpoint()?;
            if self
                .surface(db, &symbol.file_path)?
                .is_none_or(|file| file.package != package)
            {
                continue;
            }
            if found.is_some() {
                found = None;
                break;
            }
            found = Some(symbol);
        }
        self.capped |= expired(self.deadline);
        if self.capped {
            return Ok(GoResolution::Unknown);
        }
        let bytes = found
            .as_ref()
            .map_or(0, Symbol::heap_bytes)
            .saturating_add(std::mem::size_of::<(String, Option<Symbol>)>())
            .saturating_add(dir.len())
            .saturating_add(member.len());
        if self.member_entries < MAX_MEMBER_ENTRIES
            && bytes <= MAX_MEMBER_BYTES.saturating_sub(self.member_bytes)
        {
            self.member_bytes += bytes;
            self.member_entries += 1;
            self.members
                .entry(dir)
                .or_default()
                .insert(member.to_owned(), found.clone());
        }
        Ok(GoResolution::Proven(found.into_iter().collect()))
    }

    fn package_surface(
        &mut self,
        db: &Database,
        caller: &str,
        import: &str,
    ) -> Result<Option<Arc<Surface>>> {
        for index in 0.. {
            checkpoint()?;
            let file = self
                .modules
                .as_ref()
                .expect("modules loaded")
                .targets(caller, import)
                .get(index)
                .cloned();
            let Some(file) = file else {
                break;
            };
            if let Some(surface) = self.surface(db, &file)? {
                return Ok(Some(surface));
            }
            if self.capped {
                break;
            }
        }
        Ok(None)
    }

    fn surface(&mut self, db: &Database, file: &str) -> Result<Option<Arc<Surface>>> {
        if expired(self.deadline) {
            self.capped = true;
            return Ok(None);
        }
        if let Some(known) = self.sources.get(file) {
            return Ok(known.clone());
        }
        if self.sources.len() >= MAX_SOURCES || self.bytes >= MAX_SOURCE_BYTES {
            self.capped = true;
            return Ok(None);
        }
        let parsed = self.read_surface(db, file)?.map(Arc::new);
        self.sources.insert(file.to_owned(), parsed.clone());
        Ok(parsed)
    }

    fn read_surface(&mut self, db: &Database, file: &str) -> Result<Option<Surface>> {
        let Some(root) = crate::constructor_imports::project_root(db) else {
            return Ok(None);
        };
        checkpoint()?;
        let (bytes, hash) = match db.overlay_source(file)? {
            Some(Some(text)) => {
                let bytes = text.into_bytes();
                let hash = content_hash(&bytes);
                (bytes, hash)
            }
            Some(None) => return Ok(None),
            None => {
                let limit = (MAX_SOURCE_BYTES - self.bytes) as u64;
                let source = match read_indexable_file_bounded(root, Path::new(file), limit) {
                    Ok(Some(source)) => source,
                    Ok(None) => {
                        self.capped = true;
                        return Ok(None);
                    }
                    Err(_) => return Ok(None),
                };
                (source.bytes, source.content_hash)
            }
        };
        self.bytes += bytes.len();
        if self.bytes > MAX_SOURCE_BYTES {
            self.capped = true;
            return Ok(None);
        }
        if db.get_file_hash(file)?.as_deref() != Some(hash.as_str()) {
            return Ok(None);
        }
        let parsed = parse_file_tolerant(&bytes, Language::Go)?;
        if parsed.degraded {
            return Ok(None);
        }
        let root = parsed.tree.root_node();
        let Some(package) = ordinary_package(root, &bytes) else {
            return Ok(None);
        };
        let mut result = Surface {
            package: package.to_owned(),
            ..Surface::default()
        };
        let mut stack = vec![root];
        let mut count = 0;
        while let Some(node) = stack.pop() {
            if count == MAX_NODES {
                self.capped = true;
                return Ok(None);
            }
            if count % 256 == 0 {
                checkpoint()?;
                if expired(self.deadline) {
                    self.capped = true;
                    return Ok(None);
                }
            }
            count += 1;
            match node.kind() {
                "call_expression" => {
                    if let Some(callee) = node.child_by_field_name("function")
                        && let Some((operand, field)) = simple_go_selector_parts(callee)
                        && (callee.start_byte() != operand.start_byte()
                            || callee.end_byte() != field.end_byte()
                            || bytes.get(operand.end_byte()..field.start_byte()) != Some(b"."))
                    {
                        result.normalized_selectors.insert(start(callee, &bytes));
                    }
                }
                "import_spec" => {
                    if let Some(path) = node
                        .child_by_field_name("path")
                        .and_then(|p| text(p, &bytes))
                        .and_then(decode_import_path)
                        .filter(|path| valid_import_path(path))
                    {
                        let alias = node
                            .child_by_field_name("name")
                            .and_then(|name| text(name, &bytes));
                        if !matches!(alias, Some("_" | ".")) {
                            result
                                .imports
                                .push((alias.map(str::to_owned), path.into_owned()));
                        }
                    }
                }
                "type_switch_statement" => {
                    record_type_switch_alias(node, &bytes, &mut result.shadows);
                }
                "method_declaration" => {
                    record_receiver_type_parameters(node, &bytes, &mut result.shadows);
                }
                "identifier" | "type_identifier" => {
                    if let Some(name) = text(node, &bytes)
                        && let Some(scope) = binding_scope(node, &bytes)
                    {
                        result
                            .shadows
                            .entry(name.to_owned())
                            .or_default()
                            .push(scope);
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        Ok(Some(result))
    }
}

fn text<'a>(node: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(bytes).ok()
}

fn ordinary_package<'a>(root: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    if package_header_excluded(bytes, true)? {
        return None;
    }
    let package = root
        .named_children(&mut root.walk())
        .find(|node| node.kind() == "package_clause")?;
    let name = package
        .named_children(&mut package.walk())
        .find(|node| node.kind() == "package_identifier")?;
    text(name, bytes)
}

type Position = (u32, u32);
type BindingScope = (Position, Position);

fn start(node: Node<'_>, bytes: &[u8]) -> Position {
    let (line, col) = node_start_utf8(&node, bytes);
    (line + 1, col)
}

fn end(node: Node<'_>, bytes: &[u8]) -> Position {
    let (line, col) = node_end_utf8(&node, bytes);
    (line + 1, col)
}

fn record_type_switch_alias(
    node: Node<'_>,
    bytes: &[u8],
    shadows: &mut HashMap<String, Vec<BindingScope>>,
) {
    let Some(alias) = node
        .child_by_field_name("alias")
        .and_then(|alias| alias.named_child(0))
        .and_then(|name| text(name, bytes))
    else {
        return;
    };
    let scopes = shadows.entry(alias.to_owned()).or_default();
    for case in node.named_children(&mut node.walk()) {
        if matches!(case.kind(), "type_case" | "default_case")
            && let Some(colon) = case.children(&mut case.walk()).find(|n| n.kind() == ":")
        {
            scopes.push((end(colon, bytes), end(case, bytes)));
        }
    }
}

fn record_receiver_type_parameters(
    node: Node<'_>,
    bytes: &[u8],
    shadows: &mut HashMap<String, Vec<BindingScope>>,
) {
    let Some(receiver) = node
        .child_by_field_name("receiver")
        .and_then(|receiver| receiver.named_child(0))
        .and_then(|parameter| parameter.child_by_field_name("type"))
    else {
        return;
    };
    let receiver = if receiver.kind() == "pointer_type" {
        receiver.named_child(0).unwrap_or(receiver)
    } else {
        receiver
    };
    let Some(arguments) = receiver.child_by_field_name("type_arguments") else {
        return;
    };
    for argument in arguments.named_children(&mut arguments.walk()) {
        if let Some(name) = argument
            .named_child(0)
            .filter(|n| n.kind() == "type_identifier")
            && let Some(spelling) = text(name, bytes)
        {
            shadows
                .entry(spelling.to_owned())
                .or_default()
                .push((end(name, bytes), end(node, bytes)));
        }
    }
}

fn binding_scope(node: Node<'_>, bytes: &[u8]) -> Option<BindingScope> {
    let parent = node.parent()?;
    let end = |n| end(n, bytes);
    match parent.kind() {
        "parameter_declaration"
        | "variadic_parameter_declaration"
        | "type_parameter_declaration"
            if named_binding(parent, node) =>
        {
            let owner = parent.parent()?.parent()?;
            if parent.kind() == "type_parameter_declaration" {
                return Some((end(node), end(owner)));
            }
            let body = owner.child_by_field_name("body")?;
            Some((start(body, bytes), end(body)))
        }
        "type_spec" | "type_alias" if named_binding(parent, node) => {
            Some((end(node), end(local_scope(parent)?)))
        }
        "const_spec" | "var_spec" if named_binding(parent, node) => {
            Some((end(parent), end(local_scope(parent)?)))
        }
        "expression_list" => {
            let declaration = parent.parent()?;
            if declaration.child_by_field_name("left") != Some(parent) {
                return None;
            }
            match declaration.kind() {
                "short_var_declaration" => {}
                "range_clause" | "receive_statement" if short_declaration(declaration) => {}
                _ => return None,
            }
            Some((end(declaration), end(local_scope(declaration)?)))
        }
        _ => None,
    }
}

fn named_binding(parent: Node<'_>, node: Node<'_>) -> bool {
    parent
        .children_by_field_name("name", &mut parent.walk())
        .any(|name| name == node)
}

fn local_scope(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "block"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "expression_case"
                | "type_case"
                | "communication_case"
                | "default_case"
        ) {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn short_declaration(node: Node<'_>) -> bool {
    node.children(&mut node.walk())
        .any(|child| child.kind() == ":=")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_manifest_has_one_top_level_module_path() {
        for manifest in [
            "module example.com/app",
            r#"module "example.com/\x61pp""#,
            "require (\n  example.com/lib v1.0.0\n)\nmodule example.com/app\n",
            "module( // opener\n  // path\n  \"example.com/\\x61pp\" // suffix\n) // closer\n",
        ] {
            assert_eq!(
                manifest_module(manifest).as_deref(),
                Some("example.com/app")
            );
        }
        for manifest in [
            "module\n",
            "module example.com/app\nmodule example.com/again\n",
            "module (\n example.com/app\n example.com/again\n)\n",
            "module (\n example.com/app\n",
            "module (\n)\n",
            "module (\n example.com/app\n)\nmodule example.com/app",
            "module (\n example.com/app extra\n)\n",
            "module example.com/app extra\n",
        ] {
            assert!(manifest_module(manifest).is_none(), "{manifest}");
        }
    }

    #[test]
    fn nearest_duplicate_manifest_wins_in_every_discovery_order() {
        let dir = tempfile::tempdir().unwrap();
        for root in ["", "deep", "other"] {
            let path = dir.path().join(root);
            std::fs::create_dir_all(&path).unwrap();
            let name = if root == "other" {
                "example.com/other"
            } else {
                "example.com/app"
            };
            std::fs::write(path.join("go.mod"), format!("module {name}\n")).unwrap();
        }
        let root_files = ["cmd/main.go", "pkg/util.go"];
        let other_files = [
            "deep/cmd/main.go",
            "deep/pkg/util.go",
            "other/run.go",
            "other/pkg/inner.go",
        ];
        let mut files: Vec<_> = root_files.into_iter().chain(other_files).collect();
        for _ in 0..files.len() {
            let modules = GoModules::from_indexed(dir.path(), files.iter().copied(), None)
                .unwrap()
                .unwrap();
            assert_eq!(
                modules.targets("cmd/main.go", "example.com/app/pkg"),
                ["pkg/util.go"]
            );
            assert!(
                modules
                    .targets("cmd/main.go", "example.com/app/other/pkg")
                    .is_empty()
            );
            assert!(
                modules
                    .targets("deep/cmd/main.go", "example.com/app/pkg")
                    .is_empty()
            );
            assert_eq!(
                modules.targets("other/run.go", "example.com/other/pkg"),
                ["other/pkg/inner.go"]
            );
            files.rotate_left(1);
        }
        let modules = GoModules::from_indexed(dir.path(), other_files, None)
            .unwrap()
            .unwrap();
        assert!(
            modules
                .targets("deep/cmd/main.go", "example.com/app/pkg")
                .is_empty()
        );
    }

    #[test]
    fn a_shallower_manifest_without_go_files_wins_over_indexed_siblings() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["nested/code", "shallow", "ignored"] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("go.mod"), "module example.com/duplicate\n").unwrap();
        }
        let config = dir.path().join(".codesage");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            "[index]\nexclude_patterns = [\"ignored/**\"]\n",
        )
        .unwrap();
        let files = ["nested/code/cmd/main.go", "nested/code/pkg/util.go"];
        let targets = |files| {
            GoModules::from_indexed(dir.path(), files, None)
                .unwrap()
                .unwrap()
        };
        assert!(
            targets(files)
                .targets(files[0], "example.com/duplicate/pkg")
                .is_empty()
        );
        std::fs::remove_file(dir.path().join("shallow/go.mod")).unwrap();
        assert_eq!(
            targets([files[1], files[0]]).targets(files[0], "example.com/duplicate/pkg"),
            [files[1]]
        );
    }
}
