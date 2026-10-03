use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use codesage_parser::{parse::parse_file_tolerant, source::read_indexable_file};
use codesage_protocol::{FeatureKind, Language, TrustBoundary};
use tree_sitter::{Node, Tree};

pub(crate) const JAVA_SOURCE_CAP: usize = 1_000_000;

pub(crate) fn read_source(root: &Path, path: &str) -> Result<Vec<u8>> {
    let source = read_indexable_file(root, Path::new(path))
        .with_context(|| format!("reading Java source: {path}"))?
        .with_context(|| format!("Java source exceeds the structural source limit: {path}"))?
        .bytes;
    std::str::from_utf8(&source).with_context(|| format!("Java source is not UTF-8: {path}"))?;
    Ok(source)
}

#[derive(Default)]
pub struct JavaTypeContext {
    packages: BTreeMap<(String, String), BTreeSet<String>>,
    types: BTreeMap<(String, String), TypeInfo>,
    static_types: BTreeSet<(String, String)>,
    static_values: BTreeSet<(String, String)>,
}

#[derive(Clone)]
struct TypeInfo {
    package: String,
    public: bool,
    package_accessible: bool,
}

impl JavaTypeContext {
    pub fn from_files(root: &Path, paths: &[String]) -> Result<Self> {
        ensure!(
            paths.len() < 50_000,
            "Java declaration discovery reached its 50000-file limit"
        );
        let mut context = Self::default();
        for path in paths {
            let source = read_source(root, path)?;
            let parsed = parse_file_tolerant(&source, Language::Java)?;
            context.insert(path, &parsed.tree, &source);
        }
        Ok(context)
    }

    fn insert(&mut self, path: &str, tree: &Tree, source: &[u8]) {
        let package = package_name(tree, source);
        let root = source_root(path);
        let mut cursor = tree.root_node().walk();
        for declaration in tree.root_node().named_children(&mut cursor) {
            if is_type_declaration(declaration)
                && let Some(name) = declaration.child_by_field_name("name")
            {
                let name = canonical_name(name, source);
                self.packages
                    .entry((root.clone(), package.clone()))
                    .or_default()
                    .insert(name.clone());
                let qualified = if package.is_empty() {
                    name
                } else {
                    format!("{package}.{name}")
                };
                let info = TypeInfo {
                    package: package.clone(),
                    public: has_modifier(declaration, "public"),
                    package_accessible: !has_modifier(declaration, "private"),
                };
                self.types
                    .insert((root.clone(), qualified.clone()), info.clone());
                self.insert_member_types(&root, &qualified, declaration, source, &info);
            }
        }
    }

    fn insert_member_types(
        &mut self,
        root: &str,
        owner: &str,
        declaration: Node<'_>,
        source: &[u8],
        info: &TypeInfo,
    ) {
        let Some(body) = declaration.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        let mut members: Vec<_> = body.named_children(&mut cursor).collect();
        while let Some(member) = members.pop() {
            if member.kind() == "enum_body_declarations" {
                let mut cursor = member.walk();
                members.extend(member.named_children(&mut cursor));
                continue;
            }
            let explicit = has_modifier(member, "static");
            let field_implicit = matches!(
                declaration.kind(),
                "interface_declaration" | "annotation_type_declaration"
            );
            if matches!(member.kind(), "field_declaration" | "constant_declaration")
                && (explicit || field_implicit)
            {
                let mut cursor = member.walk();
                for variable in member
                    .named_children(&mut cursor)
                    .filter(|node| node.kind() == "variable_declarator")
                {
                    if let Some(name) = variable.child_by_field_name("name") {
                        self.static_values.insert((
                            root.to_string(),
                            format!("{owner}.{}", canonical_name(name, source)),
                        ));
                    }
                }
            }
            if ((member.kind() == "method_declaration" && explicit)
                || member.kind() == "enum_constant")
                && let Some(name) = member.child_by_field_name("name")
            {
                self.static_values.insert((
                    root.to_string(),
                    format!("{owner}.{}", canonical_name(name, source)),
                ));
            }
            if !is_type_declaration(member) {
                continue;
            }
            let Some(name) = member.child_by_field_name("name") else {
                continue;
            };
            let qualified = format!("{owner}.{}", canonical_name(name, source));
            let member_info = TypeInfo {
                package: info.package.clone(),
                public: info.public && (field_implicit || has_modifier(member, "public")),
                package_accessible: info.package_accessible && !has_modifier(member, "private"),
            };
            self.types
                .insert((root.to_string(), qualified.clone()), member_info.clone());
            let implicit = matches!(
                declaration.kind(),
                "interface_declaration" | "annotation_type_declaration"
            ) || matches!(
                member.kind(),
                "interface_declaration"
                    | "annotation_type_declaration"
                    | "enum_declaration"
                    | "record_declaration"
            );
            if implicit || explicit {
                self.static_types
                    .insert((root.to_string(), qualified.clone()));
            }
            self.insert_member_types(root, &qualified, member, source, &member_info);
        }
    }

    fn imports_static_type(&self, path: &str, package: &str, qualified: &str) -> bool {
        self.declaration_root(path, qualified).is_some_and(|root| {
            self.static_types.contains(&(root, qualified.to_string()))
                && self.imports_type(path, package, qualified)
        })
    }

    fn imports_type(&self, path: &str, package: &str, qualified: &str) -> bool {
        self.declaration_root(path, qualified)
            .and_then(|root| self.types.get(&(root, qualified.to_string())))
            .is_some_and(|info| info.public || (info.package_accessible && info.package == package))
    }

    fn imports_static_value(&self, path: &str, qualified: &str) -> bool {
        self.declaration_root(path, qualified)
            .is_some_and(|root| self.static_values.contains(&(root, qualified.to_string())))
    }

    fn declares(&self, path: &str, package: &str, name: &str) -> bool {
        visible_source_roots(path).any(|root| {
            self.packages
                .get(&(root, package.to_string()))
                .is_some_and(|names| names.contains(name))
        })
    }

    fn declaration_root(&self, path: &str, qualified: &str) -> Option<String> {
        visible_source_roots(path).find(|root| {
            let mut candidate = qualified;
            loop {
                if self
                    .types
                    .contains_key(&(root.clone(), candidate.to_string()))
                {
                    return true;
                }
                let Some((owner, _)) = candidate.rsplit_once('.') else {
                    return false;
                };
                candidate = owner;
            }
        })
    }
}

fn has_modifier(node: Node<'_>, modifier: &str) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
        .is_some_and(|modifiers| {
            let mut cursor = modifiers.walk();
            modifiers
                .children(&mut cursor)
                .any(|child| child.kind() == modifier)
        })
}

fn source_root(path: &str) -> String {
    for root in ["src/main/java/", "src/test/java/"] {
        if path.starts_with(root) {
            return root.to_string();
        }
        if let Some((module, _)) = path.rsplit_once(&format!("/{root}")) {
            return format!("{module}/{root}");
        }
    }
    String::new()
}

fn visible_source_roots(path: &str) -> impl Iterator<Item = String> {
    let root = source_root(path);
    let main = root
        .strip_suffix("src/test/java/")
        .map(|module| format!("{module}src/main/java/"));
    std::iter::once(root).chain(main)
}

fn package_name(tree: &Tree, source: &[u8]) -> String {
    let mut cursor = tree.root_node().walk();
    tree.root_node()
        .named_children(&mut cursor)
        .find(|node| node.kind() == "package_declaration")
        .and_then(|node| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))
        })
        .map(|node| canonical_name(node, source))
        .unwrap_or_default()
}

fn is_type_declaration(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

struct Shadow {
    name: String,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum JavaRole {
    WebEntrypoint,
    ApplicationService,
    PersistenceBoundary,
    ExternalClient,
    Configuration,
    FrameworkComponent,
}

impl JavaRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WebEntrypoint => "web-entrypoint",
            Self::ApplicationService => "application-service",
            Self::PersistenceBoundary => "persistence-boundary",
            Self::ExternalClient => "external-client",
            Self::Configuration => "configuration",
            Self::FrameworkComponent => "framework-component",
        }
    }

    pub(crate) fn kind(self) -> FeatureKind {
        match self {
            Self::WebEntrypoint => FeatureKind::Route,
            Self::ApplicationService => FeatureKind::Service,
            Self::Configuration => FeatureKind::Config,
            _ => FeatureKind::Library,
        }
    }

    pub(crate) fn boundaries(self) -> &'static [TrustBoundary] {
        match self {
            Self::WebEntrypoint => &[
                TrustBoundary::Network,
                TrustBoundary::UserInput,
                TrustBoundary::Serialization,
            ],
            Self::PersistenceBoundary => &[TrustBoundary::Database, TrustBoundary::Serialization],
            Self::ExternalClient => &[
                TrustBoundary::Network,
                TrustBoundary::ExternalApi,
                TrustBoundary::Serialization,
            ],
            _ => &[],
        }
    }
}

struct Names<'a> {
    imports: BTreeMap<String, String>,
    wildcards: BTreeSet<String>,
    static_wildcards: BTreeSet<String>,
    unknown_static_imports: BTreeSet<String>,
    shadows: Vec<Shadow>,
    context: &'a JavaTypeContext,
    path: &'a str,
    package: String,
}

impl Names<'_> {
    fn on_demand_types(&self, name: &str) -> BTreeSet<String> {
        let ordinary = self.wildcards.iter().filter_map(|owner| {
            let qualified = format!("{owner}.{name}");
            self.context
                .imports_type(self.path, &self.package, &qualified)
                .then_some(qualified)
        });
        let statics = self.static_wildcards.iter().filter_map(|owner| {
            let qualified = format!("{owner}.{name}");
            (self
                .context
                .imports_static_type(self.path, &self.package, &qualified)
                || (name == "Builder" && SUPPORTED_STATIC_OWNERS.contains(&owner.as_str())))
            .then_some(qualified)
        });
        ordinary.chain(statics).collect()
    }

    fn shadowed(&self, name: &str, at: Node<'_>) -> bool {
        self.shadows.iter().any(|shadow| {
            shadow.name == name && shadow.start <= at.start_byte() && at.start_byte() < shadow.end
        })
    }

    fn matches(&self, spelling: &str, qualified: &str, at: Node<'_>) -> bool {
        let owner = spelling.split('.').next().unwrap_or(spelling);
        if self.shadowed(owner, at) {
            return false;
        }
        if let Some((owner, suffix)) = spelling.split_once('.') {
            let qualified_owner = qualified.strip_suffix(&format!(".{suffix}"));
            if let Some(imported) = self.imports.get(owner) {
                return qualified_owner == Some(imported.as_str());
            }
            if self.unknown_static_imports.contains(owner)
                || self.context.declares(self.path, &self.package, owner)
            {
                return false;
            }
            let candidates = self.on_demand_types(owner);
            if !candidates.is_empty() {
                return candidates.len() == 1
                    && qualified_owner == candidates.first().map(String::as_str);
            }
            return spelling == qualified
                || qualified_owner
                    .is_some_and(|qualified_owner| self.matches(owner, qualified_owner, at));
        }
        if let Some(imported) = self.imports.get(spelling) {
            return imported == qualified;
        }
        if self.unknown_static_imports.contains(spelling) {
            return false;
        }
        if self.context.declares(self.path, &self.package, spelling) {
            return false;
        }
        let candidates = self.on_demand_types(spelling);
        if !candidates.is_empty() {
            return candidates.len() == 1 && candidates.contains(qualified);
        }
        qualified
            .rsplit_once('.')
            .is_some_and(|(package, name)| spelling == name && self.wildcards.contains(package))
    }
}

fn canonical_name(node: Node<'_>, source: &[u8]) -> String {
    let mut components = Vec::new();
    let mut pending = vec![(node, 0)];
    let mut visited = 0;
    while let Some((node, depth)) = pending.pop() {
        if depth >= 16 || visited >= 64 || node.is_error() || node.is_missing() || node.has_error()
        {
            return String::new();
        }
        visited += 1;
        match node.kind() {
            "identifier" | "type_identifier" => {
                let Ok(name) = node.utf8_text(source) else {
                    return String::new();
                };
                components.push(name);
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node
                    .named_children(&mut cursor)
                    .filter(|child| !is_name_decoration(*child))
                    .collect();
                if children.len() != 2 {
                    return String::new();
                }
                pending.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
            }
            "array_type" | "generic_type" | "annotated_type" => {
                let mut cursor = node.walk();
                let child = if node.kind() == "array_type" {
                    node.child_by_field_name("element")
                } else {
                    node.named_children(&mut cursor)
                        .find(|child| !is_name_decoration(*child))
                };
                let Some(child) = child else {
                    return String::new();
                };
                pending.push((child, depth + 1));
            }
            _ => return String::new(),
        }
    }
    components.join(".")
}

fn is_name_decoration(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "annotation" | "marker_annotation" | "line_comment" | "block_comment"
    )
}

#[cfg(test)]
fn roles(tree: &Tree, source: &[u8]) -> BTreeSet<JavaRole> {
    roles_with_context(tree, source, "", &JavaTypeContext::default())
}

pub(crate) fn roles_with_context(
    tree: &Tree,
    source: &[u8],
    path: &str,
    context: &JavaTypeContext,
) -> BTreeSet<JavaRole> {
    if source.len() > JAVA_SOURCE_CAP {
        return BTreeSet::new();
    }
    let mut names = Names {
        imports: BTreeMap::new(),
        wildcards: BTreeSet::new(),
        static_wildcards: BTreeSet::new(),
        unknown_static_imports: BTreeSet::new(),
        shadows: Vec::new(),
        context,
        path,
        package: package_name(tree, source),
    };
    let mut nodes = Vec::new();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if node.is_error() || node.is_missing() {
            continue;
        }
        if node.kind() == "import_declaration" {
            let mut cursor = node.walk();
            let is_static = node
                .children(&mut cursor)
                .any(|child| child.kind() == "static");
            {
                let mut cursor = node.walk();
                let wildcard = node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "asterisk");
                let mut cursor = node.walk();
                if let Some(path) = node
                    .named_children(&mut cursor)
                    .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))
                {
                    let qualified = canonical_name(path, source);
                    if qualified.is_empty() {
                        continue;
                    }
                    if is_static && wildcard {
                        names.static_wildcards.insert(qualified);
                    } else if is_static {
                        if qualified
                            .strip_suffix(".Builder")
                            .is_some_and(|owner| SUPPORTED_STATIC_OWNERS.contains(&owner))
                            || context.imports_static_type(names.path, &names.package, &qualified)
                        {
                            if let Some(simple) = qualified.rsplit('.').next() {
                                names.imports.insert(simple.to_string(), qualified);
                            }
                        } else if !context.imports_static_value(names.path, &qualified)
                            && let Some(simple) = qualified.rsplit('.').next()
                        {
                            names.unknown_static_imports.insert(simple.to_string());
                        }
                    } else if wildcard {
                        names.wildcards.insert(qualified);
                    } else if let Some(simple) = qualified.rsplit('.').next() {
                        names.imports.insert(simple.to_string(), qualified);
                    }
                }
            }
        }
        if is_type_declaration(node)
            && let Some(name) = node.child_by_field_name("name")
            && let Some(parent) = node.parent()
        {
            let start = if matches!(parent.kind(), "block" | "switch_block_statement_group") {
                node.start_byte()
            } else {
                parent.start_byte()
            };
            names.shadows.push(Shadow {
                name: canonical_name(name, source),
                start,
                end: parent.end_byte(),
            });
        }
        if node.kind() == "type_parameter"
            && let Some(parameters) = node.parent()
            && let Some(owner) = parameters.parent()
        {
            let mut cursor = node.walk();
            if let Some(name) = node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "type_identifier")
            {
                names.shadows.push(Shadow {
                    name: canonical_name(name, source),
                    start: parameters.start_byte(),
                    end: owner.end_byte(),
                });
            }
        }
        nodes.push(node);
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }

    let mut result = BTreeSet::new();
    for node in nodes {
        match node.kind() {
            "annotation" | "marker_annotation" => {
                let Some(declaration) = node
                    .parent()
                    .filter(|parent| parent.kind() == "modifiers")
                    .and_then(|parent| parent.parent())
                else {
                    continue;
                };
                if !matches!(
                    declaration.kind(),
                    "class_declaration"
                        | "interface_declaration"
                        | "record_declaration"
                        | "enum_declaration"
                        | "method_declaration"
                ) {
                    continue;
                }
                if let Some(name) = node.child_by_field_name("name")
                    && let Some(role) = annotation_role(&names, &canonical_name(name, source), name)
                {
                    result.insert(role);
                }
            }
            "superclass" | "super_interfaces" | "extends_interfaces" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "type_list" {
                        let mut cursor = child.walk();
                        for typ in child.named_children(&mut cursor) {
                            add_type_role(&names, typ, source, &mut result);
                        }
                    } else {
                        add_type_role(&names, child, source, &mut result);
                    }
                }
            }
            "field_declaration" | "constant_declaration" => {
                if let Some(typ) = node.child_by_field_name("type") {
                    let spelling = canonical_name(typ, source);
                    if is_client_type(&names, &spelling, typ) {
                        result.insert(JavaRole::ExternalClient);
                    }
                }
            }
            _ => {}
        }
    }
    result
}

fn add_type_role(names: &Names<'_>, node: Node<'_>, source: &[u8], roles: &mut BTreeSet<JavaRole>) {
    let spelling = canonical_name(node, source);
    if [
        "org.springframework.data.repository.Repository",
        "org.springframework.data.repository.CrudRepository",
        "org.springframework.data.repository.ListCrudRepository",
        "org.springframework.data.repository.PagingAndSortingRepository",
        "org.springframework.data.jpa.repository.JpaRepository",
        "org.springframework.data.mongodb.repository.MongoRepository",
    ]
    .iter()
    .any(|qualified| names.matches(&spelling, qualified, node))
    {
        roles.insert(JavaRole::PersistenceBoundary);
    }
    if [
        "javax.servlet.http.HttpServlet",
        "jakarta.servlet.http.HttpServlet",
    ]
    .iter()
    .any(|qualified| names.matches(&spelling, qualified, node))
    {
        roles.insert(JavaRole::WebEntrypoint);
    }
}

const SUPPORTED_STATIC_OWNERS: &[&str] = &[
    "org.springframework.web.client.RestClient",
    "org.springframework.web.reactive.function.client.WebClient",
    "java.net.http.HttpClient",
    "okhttp3.OkHttpClient",
];

fn is_client_type(names: &Names<'_>, spelling: &str, at: Node<'_>) -> bool {
    [
        "org.springframework.web.client.RestTemplate",
        "org.springframework.web.client.RestClient",
        "org.springframework.web.client.RestClient.Builder",
        "org.springframework.web.reactive.function.client.WebClient",
        "org.springframework.web.reactive.function.client.WebClient.Builder",
        "java.net.http.HttpClient",
        "java.net.http.HttpClient.Builder",
        "okhttp3.OkHttpClient",
        "okhttp3.OkHttpClient.Builder",
    ]
    .iter()
    .any(|qualified| names.matches(spelling, qualified, at))
}

fn annotation_role(names: &Names<'_>, spelling: &str, at: Node<'_>) -> Option<JavaRole> {
    const ANNOTATIONS: &[(&str, JavaRole)] = &[
        (
            "org.springframework.stereotype.Controller",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.RestController",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.RequestMapping",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.GetMapping",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.PostMapping",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.PutMapping",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.DeleteMapping",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.web.bind.annotation.PatchMapping",
            JavaRole::WebEntrypoint,
        ),
        ("javax.ws.rs.Path", JavaRole::WebEntrypoint),
        ("jakarta.ws.rs.Path", JavaRole::WebEntrypoint),
        (
            "javax.servlet.annotation.WebServlet",
            JavaRole::WebEntrypoint,
        ),
        (
            "jakarta.servlet.annotation.WebServlet",
            JavaRole::WebEntrypoint,
        ),
        (
            "org.springframework.stereotype.Service",
            JavaRole::ApplicationService,
        ),
        (
            "org.springframework.stereotype.Repository",
            JavaRole::PersistenceBoundary,
        ),
        ("javax.persistence.Entity", JavaRole::PersistenceBoundary),
        ("jakarta.persistence.Entity", JavaRole::PersistenceBoundary),
        (
            "org.springframework.cloud.openfeign.FeignClient",
            JavaRole::ExternalClient,
        ),
        (
            "org.springframework.context.annotation.Configuration",
            JavaRole::Configuration,
        ),
        (
            "org.springframework.context.annotation.Bean",
            JavaRole::Configuration,
        ),
        (
            "org.springframework.boot.autoconfigure.SpringBootApplication",
            JavaRole::Configuration,
        ),
        (
            "org.springframework.stereotype.Component",
            JavaRole::FrameworkComponent,
        ),
    ];
    ANNOTATIONS
        .iter()
        .find_map(|(qualified, role)| names.matches(spelling, qualified, at).then_some(*role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_parser::parse::parse_file;
    use codesage_protocol::Language;

    #[test]
    fn source_reader_preserves_utf8_empty_sources_and_structural_size_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Source.java");
        std::fs::write(&path, "// café\nclass Source {}\n").unwrap();
        assert_eq!(
            read_source(root.path(), "Source.java").unwrap(),
            "// café\nclass Source {}\n".as_bytes()
        );
        std::fs::write(&path, []).unwrap();
        assert!(read_source(root.path(), "Source.java").unwrap().is_empty());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(
            read_source(root.path(), "Source.java")
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
        std::fs::File::create(&path)
            .unwrap()
            .set_len(codesage_parser::discover::MAX_INDEXABLE_FILE_BYTES + 1)
            .unwrap();
        assert!(
            read_source(root.path(), "Source.java")
                .unwrap_err()
                .to_string()
                .contains("structural source limit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_reader_refuses_leaf_and_ancestor_links() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = "class Outside {}\n";
        std::fs::write(outside.path().join("Source.java"), source).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("Source.java"),
            root.path().join("Source.java"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(read_source(root.path(), "Source.java").is_err());
        assert!(read_source(root.path(), "linked/Source.java").is_err());
        assert_eq!(
            std::fs::read(outside.path().join("Source.java")).unwrap(),
            source.as_bytes()
        );
    }

    fn classify(source: &str) -> BTreeSet<JavaRole> {
        roles(
            &parse_file(source.as_bytes(), Language::Java).unwrap(),
            source.as_bytes(),
        )
    }

    #[test]
    fn qualified_type_names_ignore_annotations_and_comments_without_reading_arguments() {
        for source in [
            "class Client { java.net.http.@A HttpClient field; }",
            "class Client { java.net.http.@example.A(\"java.util.List\") HttpClient field; }",
            "class Client { java.net.http.@A HttpClient @A [][] field; }",
            "class Client { java.net.http.HttpClient.@A Builder field; }",
            "class Client { java.net./* comment */http.HttpClient field; }",
            "class Client { java.net.// comment\nhttp.HttpClient field; }",
            "import java.net./* comment */http.HttpClient; class Client { @A HttpClient field; }",
            "import java.net./* comment */http.*; class Client { @A HttpClient[] field; }",
            "import static java.net.http./* comment */HttpClient.Builder; interface Client { @A Builder[] FIELD = null; }",
            "import java.net./* comment */http.HttpClient; class Client { HttpClient.@A Builder[] field; }",
        ] {
            assert_eq!(
                classify(source),
                BTreeSet::from([JavaRole::ExternalClient]),
                "{source}"
            );
        }
        for source in [
            "class Container { java.util.@A List<java.net.http.@A HttpClient>[] field; }",
            "class Container { java.util.List<java.net.http.HttpClient.@A Builder> field; }",
            "class Container { java.util.@A(\"java.net.http.HttpClient\") List<Object> field; }",
            "class Client<HttpClient> { @A HttpClient[] field; }",
            "class Client { class HttpClient {} @A HttpClient[] field; }",
            "class java { static class net { static class http { static class HttpClient {} } } } class Client { java.net.http.@A HttpClient field; }",
            "import example./* comment */java; class Client { java.net.http.@A HttpClient field; }",
            "import static example./* comment */Heads.Builder; import static java.net.http.HttpClient.*; class Client { @A Builder[] field; }",
        ] {
            assert!(classify(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn normalized_names_keep_annotation_heritage_and_package_precedence() {
        for (source, expected) in [
            (
                "@org.springframework.web./* comment */bind.annotation.RestController class Web {}",
                JavaRole::WebEntrypoint,
            ),
            (
                "import org.springframework.web./* comment */bind.annotation.RestController; @RestController class Web {}",
                JavaRole::WebEntrypoint,
            ),
            (
                "@org.springframework./* comment */stereotype.Service class Service {}",
                JavaRole::ApplicationService,
            ),
            (
                "@org.springframework./* comment */stereotype.Repository class Repository {}",
                JavaRole::PersistenceBoundary,
            ),
            (
                "@org.springframework.context./* comment */annotation.Configuration class Config {}",
                JavaRole::Configuration,
            ),
            (
                "@org.springframework./* comment */stereotype.Component class Component {}",
                JavaRole::FrameworkComponent,
            ),
            (
                "interface Store extends org.springframework.data.jpa.repository.@A JpaRepository<String, Integer> {}",
                JavaRole::PersistenceBoundary,
            ),
            (
                "import org.springframework.data.jpa./* comment */repository.JpaRepository; interface Store extends @A JpaRepository<String, Integer> {}",
                JavaRole::PersistenceBoundary,
            ),
            (
                "class Servlet extends javax.servlet./* comment */http.@A HttpServlet {}",
                JavaRole::WebEntrypoint,
            ),
        ] {
            assert_eq!(classify(source), BTreeSet::from([expected]), "{source}");
        }
        let mut context = JavaTypeContext::default();
        let declaration = "package example./* comment */nested; public class java { public static class net { public static class http { public static class HttpClient {} } } }";
        let tree = parse_file(declaration.as_bytes(), Language::Java).unwrap();
        context.insert(
            "src/main/java/example/nested/java.java",
            &tree,
            declaration.as_bytes(),
        );
        let source =
            "package example.nested; class Client { java.net.http.@probe.A HttpClient field; }";
        let tree = parse_file(source.as_bytes(), Language::Java).unwrap();
        assert!(
            roles_with_context(
                &tree,
                source.as_bytes(),
                "src/main/java/example/nested/Client.java",
                &context
            )
            .is_empty()
        );
    }

    #[test]
    fn canonical_names_reject_error_nodes_and_excessive_qualified_depth() {
        for source in [
            "class Bad { java.net..http.HttpClient field; }".to_string(),
            format!("class Deep {{ {}HttpClient field; }}", "owner.".repeat(32)),
        ] {
            let parsed = parse_file_tolerant(source.as_bytes(), Language::Java).unwrap();
            let mut pending = vec![parsed.tree.root_node()];
            let mut field_type = None;
            while let Some(node) = pending.pop() {
                if node.kind() == "field_declaration" {
                    field_type = node.child_by_field_name("type");
                    break;
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            assert!(
                canonical_name(field_type.expect("field type"), source.as_bytes()).is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn test_source_visibility_is_directional_and_uses_the_nearest_declared_owner() {
        let mut context = JavaTypeContext::default();
        for (path, source) in [
            (
                "src/main/java/example/java.java",
                "package example; public class java { public static class net { public static class http { public static class HttpClient {} } } }",
            ),
            (
                "src/main/java/example/Heads.java",
                "package example; public class Heads { public static class java {} public static class Builder {} }",
            ),
            (
                "src/main/java/example/Values.java",
                "package example; public class Values { public static Object Builder; }",
            ),
            (
                "src/test/java/example/Heads.java",
                "package example; public class Heads {}",
            ),
            (
                "src/test/java/testonly/java.java",
                "package testonly; public class java {}",
            ),
        ] {
            let tree = parse_file(source.as_bytes(), Language::Java).unwrap();
            context.insert(path, &tree, source.as_bytes());
        }
        for (path, source, expected) in [
            (
                "src/test/java/example/Same.java",
                "package example; class Same { java.net.http.HttpClient field; }",
                BTreeSet::new(),
            ),
            (
                "src/test/java/other/Imported.java",
                "package other; import example.*; class Imported { java.net.http.HttpClient field; }",
                BTreeSet::new(),
            ),
            (
                "src/test/java/other/Static.java",
                "package other; import static example.Heads.*; class Static { java.net.http.HttpClient field; }",
                BTreeSet::from([JavaRole::ExternalClient]),
            ),
            (
                "src/test/java/other/Value.java",
                "package other; import static example.Values.Builder; import static java.net.http.HttpClient.*; class Value { Builder field; }",
                BTreeSet::from([JavaRole::ExternalClient]),
            ),
            (
                "src/main/java/testonly/Main.java",
                "package testonly; class Main { java.net.http.HttpClient field; }",
                BTreeSet::from([JavaRole::ExternalClient]),
            ),
            (
                "child/src/test/java/example/Child.java",
                "package example; class Child { java.net.http.HttpClient field; }",
                BTreeSet::from([JavaRole::ExternalClient]),
            ),
        ] {
            let tree = parse_file(source.as_bytes(), Language::Java).unwrap();
            assert_eq!(
                roles_with_context(&tree, source.as_bytes(), path, &context),
                expected,
                "{path}: {source}"
            );
        }
        assert!(!context.imports_static_type(
            "src/test/java/other/Test.java",
            "other",
            "example.Heads.Builder"
        ));
        assert!(context.imports_static_type(
            "src/main/java/other/Main.java",
            "other",
            "example.Heads.Builder"
        ));
    }

    #[test]
    fn on_demand_type_namespaces_use_source_members_and_ignore_values() {
        let mut context = JavaTypeContext::default();
        for source in [
            "package example; public class Heads { public static class java { public static class net { public static class http { public static class HttpClient {} } } } public static class org { public static class springframework { public static class web { public static class bind { public static class annotation { public @interface RestController {} } } } } } public static class Builder {} }",
            "package example; public class Values { public static Object java; public static void org() {} public static Object Builder; }",
            "package example; public class InnerHeads { public class java { public class net { public class http { public class HttpClient {} } } } }",
            "package example; public class PrivateHeads { private static class java { public static class net { public static class http { public static class HttpClient {} } } } }",
            "package hidden; class java { public static class net { public static class http { public static class HttpClient {} } } }",
        ] {
            let tree = parse_file(source.as_bytes(), Language::Java).unwrap();
            context.insert(
                "src/main/java/example/Declarations.java",
                &tree,
                source.as_bytes(),
            );
        }
        for source in [
            "package bad; import static example.Heads.*; class Client { java.net.http.HttpClient field; }",
            "package bad; import static example.Heads.*; @org.springframework.web.bind.annotation.RestController class Web {}",
            "package bad; import example.Heads.*; class Client { java.net.http.HttpClient field; }",
            "package bad; import example.InnerHeads.*; class Client { java.net.http.HttpClient field; }",
            "package bad; import static example.Heads.*; import static java.net.http.HttpClient.*; class Client { Builder field; }",
            "package hidden; class Client { java.net.http.HttpClient field; }",
        ] {
            let tree = parse_file(source.as_bytes(), Language::Java).unwrap();
            assert!(
                roles_with_context(
                    &tree,
                    source.as_bytes(),
                    "src/main/java/bad/Subject.java",
                    &context
                )
                .is_empty(),
                "{source}"
            );
        }
        for (source, expected) in [
            (
                "package good; import static example.Values.*; class Client { java.net.http.HttpClient field; }",
                JavaRole::ExternalClient,
            ),
            (
                "package good; import static example.Values.*; @org.springframework.web.bind.annotation.RestController class Web {}",
                JavaRole::WebEntrypoint,
            ),
            (
                "package good; import static example.InnerHeads.*; class Client { java.net.http.HttpClient field; }",
                JavaRole::ExternalClient,
            ),
            (
                "package good; import java.net.http.HttpClient.Builder; import static example.Heads.*; class Client { Builder field; }",
                JavaRole::ExternalClient,
            ),
            (
                "package good; import static example.Values.*; import static java.net.http.HttpClient.*; class Client { Builder field; }",
                JavaRole::ExternalClient,
            ),
            (
                "package good; import static example.PrivateHeads.*; class Client { java.net.http.HttpClient field; }",
                JavaRole::ExternalClient,
            ),
            (
                "package good; import example.PrivateHeads.*; class Client { java.net.http.HttpClient field; }",
                JavaRole::ExternalClient,
            ),
            (
                "package good; import hidden.*; class Client { java.net.http.HttpClient field; }",
                JavaRole::ExternalClient,
            ),
        ] {
            let tree = parse_file(source.as_bytes(), Language::Java).unwrap();
            assert_eq!(
                roles_with_context(
                    &tree,
                    source.as_bytes(),
                    "src/main/java/good/Subject.java",
                    &context
                ),
                BTreeSet::from([expected]),
                "{source}"
            );
        }
    }

    #[test]
    fn type_names_respect_lexical_scopes_and_type_parameters() {
        for source in [
            "import java.net.http.HttpClient; class Generic<HttpClient> { HttpClient field; }",
            "import java.net.http.HttpClient; class Outer { class HttpClient {} class Inner { HttpClient field; } }",
            "import java.net.http.HttpClient; class Outer { interface HttpClient {} HttpClient field; }",
            "import java.net.http.HttpClient; class Generic { <HttpClient> void f() { class Holder { HttpClient field; } } }",
            "import java.net.http.HttpClient; class Generic { <HttpClient> Generic() { class Holder { HttpClient field; } } }",
            "import org.springframework.web.bind.annotation.RestController; class Outer { @RestController class Inner {} @interface RestController {} }",
        ] {
            assert!(classify(source).is_empty(), "{source}");
        }
        for source in [
            "import org.springframework.web.bind.annotation.RestController; @RestController class Scope {} class Other { class RestController {} }",
            "import java.net.http.HttpClient; class Client { HttpClient field; } class Other<HttpClient> {}",
            "import org.springframework.web.bind.annotation.RestController; class Outer { void f() { class RestController {} } @RestController class Inner {} }",
            "import java.net.http.HttpClient; class Outer { void f() { class Holder { HttpClient field; } class HttpClient {} } }",
        ] {
            assert_eq!(classify(source).len(), 1, "{source}");
        }
    }

    #[test]
    fn static_imports_resolve_supported_nested_client_types() {
        for source in [
            "import static java.net.http.HttpClient.Builder; class Client { Builder field; }",
            "import static java.net.http.HttpClient.*; class Client { Builder field; }",
            "import static org.springframework.web.reactive.function.client.WebClient.Builder; class Client { Builder field; }",
            "import java.net.http.*; class Client { HttpClient.Builder field; }",
        ] {
            assert_eq!(
                classify(source),
                BTreeSet::from([JavaRole::ExternalClient]),
                "{source}"
            );
        }
        for source in [
            "import static example.Client.Builder; class Client { Builder field; }",
            "import static java.net.http.HttpClient.newBuilder; class Client { Object field; }",
            "import static java.net.http.HttpClient.*; class Client<Builder> { Builder field; }",
        ] {
            assert!(classify(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn array_fields_use_the_principal_type_without_searching_generic_arguments() {
        for source in [
            "class Client { java.net.http.HttpClient[] field; }",
            "class Client { java.net.http.HttpClient[][] field; }",
            "class Client { java.net.http.HttpClient field[]; }",
            "class Client { java.net.http.HttpClient[] field[]; }",
            "import java.net.http.HttpClient; class Client { HttpClient[] field; }",
            "import java.net.http.HttpClient; class Client { HttpClient.Builder[] field; }",
            "import static java.net.http.HttpClient.Builder; class Client { Builder[][] field; }",
        ] {
            assert_eq!(
                classify(source),
                BTreeSet::from([JavaRole::ExternalClient]),
                "{source}"
            );
        }
        for source in [
            "class Container { java.util.List<java.net.http.HttpClient>[] field; }",
            "import java.net.http.HttpClient; class Container { List<HttpClient>[][] field; }",
            "import java.net.http.HttpClient; class Generic<HttpClient> { HttpClient[] field; }",
            "class Generic<java> { java.net.http.HttpClient[] field; }",
            "class Local { class java { class net { class http { class HttpClient {} } } } java.net.http.HttpClient[] field; }",
            "class Local { class HttpClient {} HttpClient[][] field; }",
            "class Unrelated { example.HttpClient[] field; }",
        ] {
            assert!(classify(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn interface_constants_share_class_field_type_evidence() {
        for source in [
            "interface Client { java.net.http.HttpClient FIELD = null; }",
            "import java.net.http.HttpClient; interface Client { HttpClient FIELD = null; }",
            "interface Client { java.net.http.HttpClient[][] FIELD = null; }",
            "interface Client { java.net.http.HttpClient FIELD[][] = null; }",
            "import static java.net.http.HttpClient.Builder; interface Client { Builder[] FIELD = null; }",
            "@interface Client { java.net.http.HttpClient FIELD = null; }",
        ] {
            assert_eq!(
                classify(source),
                BTreeSet::from([JavaRole::ExternalClient]),
                "{source}"
            );
        }
        for source in [
            "interface Container { java.util.List<java.net.http.HttpClient>[] FIELD = null; }",
            "import java.net.http.HttpClient; interface Container { List<HttpClient> FIELD = null; }",
            "interface Local { class HttpClient {} HttpClient[] FIELD = null; }",
            "interface Plain { String FIELD = null; }",
        ] {
            assert!(classify(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn annotations_require_qualified_framework_evidence() {
        for source in [
            "import org.springframework.web.bind.annotation.RestController; @RestController class Orders {}",
            "import org.springframework.web.bind.annotation.*; @RestController class Orders {}",
            "@org.springframework.web.bind.annotation.RestController class Orders {}",
            "import org.springframework.web.bind.annotation.GetMapping; class Orders { @GetMapping public void get() {} }",
        ] {
            assert_eq!(
                classify(source),
                BTreeSet::from([JavaRole::WebEntrypoint]),
                "{source}"
            );
        }
        for source in [
            "@RestController class Orders {}",
            "import example.RestController; @RestController class Orders {}",
            "import org.springframework.web.bind.annotation.*; @interface RestController {} @RestController class Orders {}",
            "import org.springframework.web.bind.annotation.RestController; class Orders { String hint = \"@RestController\"; /* @RestController */ }",
            "import org.springframework.stereotype.Repository; class Repository {} class Orders extends Repository {}",
        ] {
            assert!(classify(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn repository_heritage_and_client_fields_have_roles_without_annotations() {
        assert_eq!(
            classify(
                "import org.springframework.data.jpa.repository.JpaRepository; interface Orders extends JpaRepository<Order, Long> {}"
            ),
            BTreeSet::from([JavaRole::PersistenceBoundary])
        );
        assert_eq!(
            classify("class Client { org.springframework.web.client.RestTemplate http; }"),
            BTreeSet::from([JavaRole::ExternalClient])
        );
        assert_eq!(
            classify(
                "import org.springframework.web.reactive.function.client.WebClient; class Client { WebClient.Builder http; }"
            ),
            BTreeSet::from([JavaRole::ExternalClient])
        );
        assert!(classify("import org.springframework.data.jpa.repository.JpaRepository; class Container extends List<JpaRepository> {}").is_empty());
        assert!(
            classify("import org.springframework.web.client.RestTemplate; class Plain {}")
                .is_empty()
        );
    }
}
