use std::collections::{BTreeSet, HashMap};

use codesage_protocol::python::{
    PythonBindingCapture, PythonBindingTarget, PythonBindings, PythonClassBinding,
    PythonImportSite, PythonReferenceBinding,
};
use codesage_protocol::{Reference, ReferenceKind, Symbol};
use tree_sitter::{Node, Tree};

use crate::parse::node_text_lossy;

#[derive(Clone)]
enum Value {
    Target(PythonBindingTarget),
    FromImport(PythonBindingTarget),
    Alias(Vec<String>, usize, usize),
    Declaration,
}

struct Binding {
    at: usize,
    value: Value,
    local: bool,
}

struct Scope {
    parent: Option<usize>,
    function: bool,
    class: bool,
    comprehension: bool,
    generator: bool,
    global_scope: usize,
    overlay: bool,
}

#[derive(Clone, Copy)]
enum Directive {
    Global,
    Nonlocal,
}

struct LexicalBindings<'a> {
    file: String,
    source: &'a [u8],
    symbols: &'a [Symbol],
    scopes: Vec<Scope>,
    bindings: HashMap<(usize, String), Vec<Binding>>,
    nodes: HashMap<(u32, u32), (Node<'a>, usize)>,
    directives: HashMap<(usize, String), Directive>,
    imports: Vec<(usize, usize, PythonImportSite)>,
    order: HashMap<usize, (usize, usize)>,
    complete: usize,
    classes: HashMap<usize, (String, u32)>,
}

pub fn extract_python_bindings<'a>(
    tree: &'a Tree,
    source: &'a [u8],
    refs: &[Reference],
    symbols: &'a [Symbol],
) -> PythonBindings {
    let root = tree.root_node();
    let mut lexical = LexicalBindings {
        file: refs
            .first()
            .map(|row| row.from_file.clone())
            .or_else(|| symbols.first().map(|symbol| symbol.file_path.clone()))
            .unwrap_or_default(),
        source,
        symbols,
        scopes: vec![Scope {
            parent: None,
            function: false,
            class: false,
            comprehension: false,
            generator: false,
            global_scope: 0,
            overlay: false,
        }],
        bindings: HashMap::new(),
        nodes: HashMap::new(),
        directives: HashMap::new(),
        imports: Vec::new(),
        order: evaluation_order(root),
        complete: 0,
        classes: HashMap::new(),
    };
    lexical.complete = lexical.end(root);
    lexical.collect(root, 0);
    for row in refs.iter().filter(|row| row.kind == ReferenceKind::Import) {
        if let Some((node, scope)) = lexical.nodes.get(&(row.line, row.col)) {
            let target = lexical.directive_target(row, *node);
            if target.module.is_some() {
                lexical.imports.push((
                    *scope,
                    target.event_order,
                    PythonImportSite {
                        file: row.from_file.clone(),
                        to_name: row.to_name.clone(),
                        line: row.line,
                        col: row.col,
                    },
                ));
            }
        }
    }
    lexical.route_class_bindings();
    let mut references = Vec::new();
    for row in refs {
        let node = lexical.nodes.get(&(row.line, row.col)).copied();
        let mut target = match (row.kind, node) {
            (ReferenceKind::Import, Some((node, _))) => lexical.directive_target(row, node),
            (ReferenceKind::ImportBinding, Some((node, scope))) => {
                let target = lexical.import_target(node);
                if std::iter::successors(Some(node), |node| node.parent())
                    .any(|node| node.kind() == "import_from_statement")
                {
                    lexical.resolve_from_import_value(&target, scope, target.event_order, 0)
                } else {
                    target
                }
            }
            (ReferenceKind::Call | ReferenceKind::Inheritance, Some((node, scope))) => {
                let expression = if node.parent().is_some_and(|p| p.kind() == "attribute") {
                    node.parent().unwrap_or(node)
                } else {
                    node
                };
                let parts = dotted_parts(expression, source);
                lexical.resolve(scope, &parts, lexical.start(node), 0)
            }
            _ => PythonBindingTarget::default(),
        };
        if let Some((node, scope)) = node {
            if row.kind == ReferenceKind::Import
                && target.module.as_ref().is_some_and(|module| {
                    codesage_protocol::python::module_candidates(module, &row.from_file)
                        .contains(&row.from_file)
                })
            {
                let global = lexical.scopes[scope].global_scope;
                let mut names = BTreeSet::from(["*".to_string(), "__getattr__".to_string()]);
                if target.wildcard {
                    names.insert("__all__".into());
                    let all = lexical.resolve(global, &["__all__".into()], lexical.start(node), 0);
                    if let Some(exports) = all.export_names {
                        names.extend(exports);
                    }
                } else if !target.name.is_empty() {
                    names.insert(target.name.clone());
                }
                target.namespace = Some(
                    names
                        .into_iter()
                        .map(|name| {
                            let value = lexical.resolve(
                                global,
                                std::slice::from_ref(&name),
                                lexical.start(node),
                                0,
                            );
                            (name, value)
                        })
                        .collect(),
                );
            }
            let at = if row.kind == ReferenceKind::ImportBinding {
                target.event_order
            } else {
                lexical.start(node)
            };
            let modules = lexical.visible_imports(scope, at);
            if row.kind == ReferenceKind::ImportBinding && !target.module_binding {
                freeze_prefix(&mut target, &modules, symbols);
            }
            set_live_modules(&mut target, &modules);
        }
        references.push(PythonReferenceBinding {
            to_name: row.to_name.clone(),
            kind: row.kind,
            line: row.line,
            col: row.col,
            target,
        });
    }
    let names: BTreeSet<_> = lexical
        .bindings
        .keys()
        .filter(|(scope, _)| *scope == 0)
        .map(|(_, name)| name.clone())
        .collect();
    let exports = names
        .into_iter()
        .map(|name| {
            let mut target = lexical.resolve(0, std::slice::from_ref(&name), lexical.complete, 0);
            set_live_modules(&mut target, &lexical.visible_imports(0, lexical.complete));
            (name, target)
        })
        .collect();
    let mut classes = Vec::new();
    for (scope, (qualified_class, definition_line)) in &lexical.classes {
        for (origin, name) in lexical
            .bindings
            .keys()
            .filter(|(origin, _)| origin == scope)
        {
            let mut target =
                lexical.resolve(*origin, std::slice::from_ref(name), lexical.complete, 0);
            set_live_modules(
                &mut target,
                &lexical.visible_imports(*origin, lexical.complete),
            );
            classes.push(PythonClassBinding {
                qualified_class: qualified_class.clone(),
                definition_line: *definition_line,
                name: name.clone(),
                target,
            });
        }
    }
    PythonBindings {
        references,
        exports,
        classes,
    }
}

fn transparent_expression(mut node: Node<'_>) -> Node<'_> {
    loop {
        if node.named_child_count() != 1 {
            break;
        }
        let mut cursor = node.walk();
        let transparent = node.kind() == "parenthesized_expression"
            || (node.kind() == "tuple_pattern"
                && !node.children(&mut cursor).any(|child| child.kind() == ","));
        if !transparent {
            break;
        }
        node = node.named_child(0).unwrap();
    }
    node
}

fn dotted_parts(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut node = transparent_expression(node);
    let mut suffix = Vec::new();
    while node.kind() == "attribute" {
        let Some(object) = node.child_by_field_name("object") else {
            return Vec::new();
        };
        let Some(attribute) = node.child_by_field_name("attribute") else {
            return Vec::new();
        };
        suffix.push(node_text_lossy(&attribute, source));
        node = transparent_expression(object);
    }
    if !matches!(node.kind(), "identifier" | "dotted_name") {
        return Vec::new();
    }
    let mut parts: Vec<_> = node_text_lossy(&node, source)
        .split('.')
        .map(str::to_string)
        .collect();
    parts.extend(suffix.into_iter().rev());
    parts
}

fn append_suffix(target: &mut PythonBindingTarget, suffix: &str) {
    if !target.name.is_empty() {
        target.name.push('.');
    }
    target.name.push_str(suffix);
    if let Some(fallback) = &mut target.fallback {
        append_suffix(fallback, suffix);
    }
}

fn freeze_prefix(
    target: &mut PythonBindingTarget,
    modules: &[PythonImportSite],
    symbols: &[Symbol],
) {
    let attributes = if target.module.is_some() && !target.name.is_empty() {
        target.name.split('.').count()
    } else {
        symbols
            .iter()
            .find(|symbol| {
                symbol.kind == codesage_protocol::SymbolKind::Class
                    && Some(symbol.line_start) == target.definition_line
            })
            .and_then(|symbol| {
                target
                    .name
                    .strip_prefix(&format!("{}.", symbol.qualified_name))
            })
            .map_or(0, |suffix| suffix.split('.').count())
    };
    if attributes > 0 {
        target.captures.push(PythonBindingCapture {
            attributes,
            import_sites: modules.into(),
        });
    }
    if let Some(fallback) = &mut target.fallback {
        freeze_prefix(fallback, modules, symbols);
    }
}

fn set_live_modules(target: &mut PythonBindingTarget, modules: &[PythonImportSite]) {
    if target.module.is_some() || target.definition_line.is_some() {
        target.import_sites = modules.into();
    }
    if let Some(fallback) = &mut target.fallback {
        set_live_modules(fallback, modules);
    }
}

fn evaluation_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    let mut children: Vec<_> = node.named_children(&mut cursor).collect();
    if matches!(
        node.kind(),
        "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression"
    ) {
        children.sort_by_key(|child| !matches!(child.kind(), "for_in_clause" | "if_clause"));
    } else if matches!(
        node.kind(),
        "assignment" | "named_expression" | "for_in_clause" | "for_statement"
    ) {
        let right = node
            .child_by_field_name("right")
            .or_else(|| node.child_by_field_name("value"));
        children.sort_by_key(|child| right.is_none_or(|right| right.id() != child.id()));
    }
    children
}

fn evaluation_order(root: Node<'_>) -> HashMap<usize, (usize, usize)> {
    let mut order: HashMap<usize, (usize, usize)> = HashMap::new();
    let mut pending = vec![(root, false)];
    let mut rank = 0;
    while let Some((node, exiting)) = pending.pop() {
        rank += 1;
        if exiting {
            order.get_mut(&node.id()).unwrap().1 = rank;
        } else {
            order.insert(node.id(), (rank, 0));
            pending.push((node, true));
            pending.extend(
                evaluation_children(node)
                    .into_iter()
                    .rev()
                    .map(|child| (child, false)),
            );
        }
    }
    order
}

impl<'a> LexicalBindings<'a> {
    fn start(&self, node: Node<'_>) -> usize {
        self.order[&node.id()].0
    }

    fn end(&self, node: Node<'_>) -> usize {
        self.order[&node.id()].1
    }

    fn add(&mut self, scope: usize, name: String, at: usize, mut value: Value) {
        if let Value::Target(target) | Value::FromImport(target) = &mut value {
            target.bound = true;
        }
        self.bindings
            .entry((scope, name))
            .or_default()
            .push(Binding {
                at,
                value,
                local: true,
            });
    }

    fn collect(&mut self, root: Node<'a>, scope: usize) {
        let mut pending = vec![(root, scope)];
        while let Some((node, scope)) = pending.pop() {
            self.collect_node(node, scope, &mut pending);
        }
    }

    fn execution_owner(&self, scope: usize) -> usize {
        let mut parent = self.scopes[scope].parent;
        while let Some(next) = parent {
            if self.scopes[next].function {
                return next;
            }
            parent = self.scopes[next].parent;
        }
        0
    }

    fn directive_scope(&self, scope: usize, name: &str, directive: Directive) -> Option<usize> {
        match directive {
            Directive::Global => Some(self.scopes[scope].global_scope),
            Directive::Nonlocal => {
                let mut parent = self.scopes[scope].parent;
                while let Some(next) = parent {
                    if next != 0
                        && self.scopes[next].function
                        && !self.directives.contains_key(&(next, name.into()))
                        && self
                            .bindings
                            .get(&(next, name.into()))
                            .is_some_and(|events| events.iter().any(|event| event.local))
                    {
                        return Some(next);
                    }
                    parent = self.scopes[next].parent;
                }
                None
            }
        }
    }

    fn route_class_bindings(&mut self) {
        let routes: Vec<_> = self
            .directives
            .iter()
            .filter_map(|((scope, name), directive)| {
                (self.scopes[*scope].class || matches!(directive, Directive::Global))
                    .then(|| {
                        self.directive_scope(*scope, name, *directive)
                            .map(|outer| (*scope, name.clone(), outer, *directive))
                    })
                    .flatten()
            })
            .collect();
        for (scope, name, outer, directive) in routes {
            if let Some(mut events) = self.bindings.remove(&(scope, name.clone())) {
                if matches!(directive, Directive::Global) && outer != 0 {
                    for event in &mut events {
                        event.local = false;
                    }
                }
                self.bindings
                    .entry((outer, name))
                    .or_default()
                    .extend(events);
            }
        }
        let imports: Vec<_> = self
            .imports
            .iter()
            .filter(|(scope, _, _)| self.scopes[*scope].class)
            .map(|(scope, at, module)| (self.execution_owner(*scope), *at, module.clone()))
            .collect();
        self.imports.extend(imports);
        let imports: Vec<_> = self
            .imports
            .iter()
            .filter(|(scope, _, _)| {
                self.scopes[*scope].function && !self.scopes[*scope].comprehension
            })
            .map(|(scope, at, module)| (self.scopes[*scope].global_scope, *at, module.clone()))
            .collect();
        self.imports.extend(imports);
    }

    fn collect_node(&mut self, node: Node<'a>, scope: usize, pending: &mut Vec<(Node<'a>, usize)>) {
        let (line, col) = crate::position::node_start_utf8(&node, self.source);
        self.nodes.insert((line + 1, col), (node, scope));
        if matches!(
            node.kind(),
            "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression"
        ) {
            let inner = self.scopes.len();
            self.scopes.push(Scope {
                parent: Some(scope),
                function: true,
                class: false,
                comprehension: true,
                generator: node.kind() == "generator_expression",
                global_scope: self.scopes[scope].global_scope,
                overlay: false,
            });
            let children = evaluation_children(node);
            let first_clause = children
                .iter()
                .find(|n| n.kind() == "for_in_clause")
                .map(Node::id);
            for child in children.into_iter().rev() {
                if Some(child.id()) == first_clause {
                    let mut cursor = child.walk();
                    let fields: Vec<_> = child.named_children(&mut cursor).collect();
                    let left = child.child_by_field_name("left");
                    if let Some(left) = left {
                        self.bind_pattern(
                            left,
                            inner,
                            0,
                            &Value::Target(PythonBindingTarget::default()),
                        );
                    }
                    for field in fields.into_iter().rev() {
                        pending.push((
                            field,
                            if left.is_some_and(|left| left.id() == field.id()) {
                                inner
                            } else {
                                scope
                            },
                        ));
                    }
                } else {
                    pending.push((child, inner));
                }
            }
            return;
        }
        if matches!(
            node.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                let text = node_text_lossy(&name, self.source);
                let qualified = self
                    .symbols
                    .iter()
                    .find(|s| s.name == text && s.line_start == line + 1)
                    .map_or_else(|| text.clone(), |s| s.qualified_name.clone());
                self.add(
                    scope,
                    text,
                    self.end(node),
                    Value::Target(PythonBindingTarget {
                        name: qualified,
                        definition_line: Some(line + 1),
                        module: None,
                        ..Default::default()
                    }),
                );
            }
            let Some(body) = node.child_by_field_name("body") else {
                return;
            };
            let inner = self.scopes.len();
            let class = node.kind() == "class_definition";
            let global_scope = self.scopes[scope].global_scope;
            self.scopes.push(Scope {
                parent: Some(scope),
                function: !class,
                class,
                comprehension: false,
                generator: false,
                global_scope: if class { global_scope } else { inner + 1 },
                overlay: false,
            });
            if !class {
                self.scopes.push(Scope {
                    parent: Some(global_scope),
                    function: false,
                    class: false,
                    comprehension: false,
                    generator: false,
                    global_scope: inner + 1,
                    overlay: true,
                });
            } else if let Some(name) = node.child_by_field_name("name") {
                let text = node_text_lossy(&name, self.source);
                let class_binding = self
                    .symbols
                    .iter()
                    .filter(|symbol| {
                        symbol.kind == codesage_protocol::SymbolKind::Class
                            && symbol.name == text
                            && symbol.line_start <= line + 1
                            && line < symbol.line_end
                    })
                    .min_by_key(|symbol| symbol.line_end - symbol.line_start)
                    .map_or((text, line + 1), |symbol| {
                        (symbol.qualified_name.clone(), symbol.line_start)
                    });
                self.classes.insert(inner, class_binding);
            }
            if let Some(parameters) = node.child_by_field_name("parameters") {
                let mut cursor = parameters.walk();
                for (index, parameter) in parameters.named_children(&mut cursor).enumerate() {
                    let name = if parameter.kind() == "identifier" {
                        Some(parameter)
                    } else {
                        parameter
                            .child_by_field_name("name")
                            .or_else(|| parameter.named_child(0))
                    };
                    if let Some(name) = name
                        && name.kind() == "identifier"
                    {
                        let name = node_text_lossy(&name, self.source);
                        let target = if index == 0
                            && self.scopes[scope].class
                            && matches!(name.as_str(), "self" | "cls")
                        {
                            self.symbols
                                .iter()
                                .filter(|symbol| {
                                    symbol.kind == codesage_protocol::SymbolKind::Class
                                        && symbol.line_start <= line + 1
                                        && line < symbol.line_end
                                })
                                .min_by_key(|symbol| symbol.line_end - symbol.line_start)
                                .map(|symbol| PythonBindingTarget {
                                    module: None,
                                    name: symbol.qualified_name.clone(),
                                    definition_line: Some(symbol.line_start),
                                    ..Default::default()
                                })
                                .unwrap_or_default()
                        } else {
                            PythonBindingTarget::default()
                        };
                        self.add(inner, name, 0, Value::Target(target));
                    }
                }
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                pending.push((
                    child,
                    if child.id() == body.id() {
                        inner
                    } else {
                        scope
                    },
                ));
            }
            return;
        }
        if matches!(node.kind(), "import_statement" | "import_from_statement") {
            let mut cursor = node.walk();
            for child in node.children_by_field_name("name", &mut cursor) {
                let imported = child.child_by_field_name("name").unwrap_or(child);
                let alias = child.child_by_field_name("alias");
                let imported_name = node_text_lossy(&imported, self.source);
                let local = alias
                    .map(|a| node_text_lossy(&a, self.source))
                    .unwrap_or_else(|| {
                        if node.kind() == "import_statement" {
                            imported_name.split('.').next().unwrap_or("").to_string()
                        } else {
                            imported_name.clone()
                        }
                    });
                let mut target = self.import_target(imported);
                target.import_sites = self.visible_imports(scope, self.start(node));
                let value = if node.kind() == "import_from_statement" {
                    Value::FromImport(target)
                } else {
                    Value::Target(target)
                };
                self.add(scope, local, self.end(node), value);
            }
            if let Some(module) = node.child_by_field_name("module_name") {
                let mut cursor = node.walk();
                if node
                    .named_children(&mut cursor)
                    .any(|n| n.kind() == "wildcard_import")
                {
                    self.add(
                        scope,
                        "*".into(),
                        self.end(node),
                        Value::Target(PythonBindingTarget {
                            module: Some(node_text_lossy(&module, self.source)),
                            name: String::new(),
                            definition_line: None,
                            ..Default::default()
                        }),
                    );
                }
            }
        }
        if matches!(
            node.kind(),
            "assignment" | "augmented_assignment" | "named_expression"
        ) && !node.parent().is_some_and(|parent| {
            node.kind() == "assignment"
                && parent.kind() == "assignment"
                && parent
                    .child_by_field_name("right")
                    .is_some_and(|right| right.id() == node.id())
        }) && let Some(left) = node
            .child_by_field_name("left")
            .or_else(|| node.child_by_field_name("name"))
        {
            let mut targets = vec![left];
            let mut rhs = node
                .child_by_field_name("right")
                .or_else(|| node.child_by_field_name("value"));
            while let Some(assignment) = rhs.filter(|rhs| rhs.kind() == "assignment") {
                if let Some(left) = assignment.child_by_field_name("left") {
                    targets.push(left);
                }
                rhs = assignment.child_by_field_name("right");
            }
            let value = if node.kind() == "assignment" && rhs.is_none() {
                Value::Declaration
            } else if node.kind() == "augmented_assignment" {
                Value::Target(PythonBindingTarget::default())
            } else {
                rhs.map(|rhs| Value::Alias(dotted_parts(rhs, self.source), self.start(rhs), scope))
                    .unwrap_or_else(|| Value::Target(PythonBindingTarget::default()))
            };
            let mut binding_scope = scope;
            if node.kind() == "named_expression" {
                let mut generator = None;
                while self.scopes[binding_scope].comprehension {
                    if self.scopes[binding_scope].generator {
                        generator = Some(binding_scope);
                    }
                    binding_scope = self.scopes[binding_scope].parent.unwrap_or(0);
                }
                if let Some(generator) = generator {
                    self.bind_pattern(left, binding_scope, 0, &Value::Declaration);
                    binding_scope = generator;
                }
            }
            for left in targets {
                if node.kind() == "assignment"
                    && rhs.is_some()
                    && node_text_lossy(&transparent_expression(left), self.source) == "__all__"
                {
                    let names =
                        rhs.and_then(|right| self.literal_names(transparent_expression(right)));
                    self.add(
                        scope,
                        "__all__".into(),
                        self.end(node),
                        Value::Target(PythonBindingTarget {
                            export_names: names,
                            ..Default::default()
                        }),
                    );
                } else {
                    self.bind_pattern(left, binding_scope, self.end(node), &value);
                }
            }
        }
        if matches!(node.kind(), "for_statement" | "for_in_clause")
            && let Some(left) = node.child_by_field_name("left")
        {
            self.bind_pattern(
                left,
                scope,
                if self.scopes[scope].comprehension {
                    0
                } else {
                    node.child_by_field_name("right")
                        .map_or(self.end(left), |right| self.end(right))
                },
                &Value::Target(PythonBindingTarget::default()),
            );
        }
        if node.kind() == "as_pattern"
            && let Some(alias) = node.child_by_field_name("alias")
        {
            self.bind_pattern(
                alias,
                scope,
                self.end(node),
                &Value::Target(PythonBindingTarget::default()),
            );
        }
        if node.kind() == "except_clause" {
            let alias = node.child_by_field_name("alias").or_else(|| {
                let value = node.child_by_field_name("value")?;
                value.child_by_field_name("alias")
            });
            if let Some(alias) = alias {
                self.bind_pattern(
                    alias,
                    scope,
                    self.end(alias),
                    &Value::Target(PythonBindingTarget::default()),
                );
                self.bind_pattern(
                    alias,
                    scope,
                    self.end(node),
                    &Value::Target(PythonBindingTarget {
                        deleted: true,
                        ..Default::default()
                    }),
                );
            }
        }
        if node.kind() == "delete_statement" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.bind_pattern(
                    child,
                    scope,
                    self.end(node),
                    &Value::Target(PythonBindingTarget {
                        deleted: true,
                        ..Default::default()
                    }),
                );
            }
        }
        if node.kind() == "case_pattern" {
            self.bind_match_pattern(node, scope);
        }
        if matches!(node.kind(), "global_statement" | "nonlocal_statement") {
            let mut cursor = node.walk();
            for name in node
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "identifier")
            {
                self.directives.insert(
                    (scope, node_text_lossy(&name, self.source)),
                    if node.kind() == "global_statement" {
                        Directive::Global
                    } else {
                        Directive::Nonlocal
                    },
                );
            }
        }
        let children = evaluation_children(node);
        pending.extend(children.into_iter().rev().map(|child| (child, scope)));
    }

    fn bind_pattern(&mut self, node: Node<'a>, scope: usize, at: usize, value: &Value) {
        let mut pending = vec![(node, value.clone())];
        while let Some((node, value)) = pending.pop() {
            let node = transparent_expression(node);
            if node.kind() == "identifier" {
                self.add(scope, node_text_lossy(&node, self.source), at, value);
            } else if matches!(
                node.kind(),
                "pattern_list"
                    | "tuple_pattern"
                    | "list_pattern"
                    | "as_pattern_target"
                    | "expression_list"
                    | "tuple"
                    | "list"
                    | "list_splat_pattern"
                    | "dictionary_splat_pattern"
                    | "splat_pattern"
            ) {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor).map(|child| {
                    (
                        child,
                        match &value {
                            Value::Target(target) if target.deleted => value.clone(),
                            _ => Value::Target(PythonBindingTarget::default()),
                        },
                    )
                }));
            }
        }
    }

    fn literal_names(&self, node: Node<'_>) -> Option<Vec<String>> {
        if !matches!(node.kind(), "list" | "tuple") {
            return None;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .map(|n| {
                let text = node_text_lossy(&n, self.source);
                if n.kind() != "string" || text.len() < 2 {
                    return None;
                }
                let quote = text.as_bytes()[0];
                if !matches!(quote, b'\'' | b'"')
                    || text.as_bytes().last() != Some(&quote)
                    || text[1..text.len() - 1].contains(['\\', '\'', '"'])
                {
                    return None;
                }
                Some(text[1..text.len() - 1].to_string())
            })
            .collect()
    }

    fn visible_imports(&self, mut scope: usize, mut at: usize) -> Vec<PythonImportSite> {
        let mut imports = Vec::new();
        loop {
            imports.extend(
                self.imports
                    .iter()
                    .filter(|(origin, site, _)| *origin == scope && *site <= at)
                    .map(|(_, _, module)| module.clone()),
            );
            let Some(parent) = self.scopes[scope].parent else {
                break;
            };
            if (self.scopes[scope].function && !self.scopes[scope].comprehension)
                || self.scopes[scope].overlay
            {
                at = self.complete;
            }
            scope = parent;
        }
        imports.sort();
        imports.dedup();
        imports
    }

    fn bind_match_pattern(&mut self, node: Node<'a>, scope: usize) {
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            if node.kind() == "dotted_name" {
                let name = node_text_lossy(&node, self.source);
                if !name.contains('.') && name != "_" {
                    self.add(
                        scope,
                        name,
                        self.end(node),
                        Value::Target(PythonBindingTarget::default()),
                    );
                }
            } else if node.kind() == "splat_pattern" {
                self.bind_pattern(
                    node,
                    scope,
                    self.end(node),
                    &Value::Target(PythonBindingTarget::default()),
                );
            } else if node.kind() == "as_pattern" {
                if let Some(alias) = node.child_by_field_name("alias") {
                    self.bind_pattern(
                        alias,
                        scope,
                        self.end(node),
                        &Value::Target(PythonBindingTarget::default()),
                    );
                }
            } else if matches!(node.kind(), "class_pattern" | "keyword_pattern") {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor).skip(1));
            } else {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
        }
    }

    fn directive_target(&self, row: &Reference, mut node: Node<'_>) -> PythonBindingTarget {
        loop {
            if node.kind() == "import_statement" {
                return PythonBindingTarget {
                    module: Some(row.to_name.clone()),
                    event_order: self.end(node),
                    ..Default::default()
                };
            }
            if node.kind() == "import_from_statement" {
                let Some(module) = node.child_by_field_name("module_name") else {
                    return PythonBindingTarget::default();
                };
                let module = node_text_lossy(&module, self.source);
                let name = row
                    .to_name
                    .strip_prefix(&module)
                    .unwrap_or("")
                    .trim_start_matches('.')
                    .trim_end_matches('*')
                    .trim_end_matches('.');
                return PythonBindingTarget {
                    module: Some(module),
                    name: name.into(),
                    wildcard: row.to_name.ends_with('*'),
                    event_order: self.end(node),
                    ..Default::default()
                };
            }
            let Some(parent) = node.parent() else {
                return PythonBindingTarget::default();
            };
            node = parent;
        }
    }

    fn import_target(&self, mut node: Node<'_>) -> PythonBindingTarget {
        if node.kind() == "aliased_import" {
            node = node.child_by_field_name("name").unwrap_or(node);
        }
        let name = node_text_lossy(&node, self.source);
        let mut statement = node.parent();
        while let Some(parent) = statement {
            if parent.kind() == "import_statement" {
                let aliased = node.parent().is_some_and(|p| p.kind() == "aliased_import");
                let (module, member) = name.split_once('.').unwrap_or((&name, ""));
                return PythonBindingTarget {
                    module: Some(module.into()),
                    name: if aliased {
                        member.into()
                    } else {
                        String::new()
                    },
                    definition_line: None,
                    module_binding: !aliased || member.is_empty(),
                    event_order: self.end(parent),
                    ..Default::default()
                };
            }
            if parent.kind() == "import_from_statement" {
                return PythonBindingTarget {
                    module: parent
                        .child_by_field_name("module_name")
                        .map(|m| node_text_lossy(&m, self.source)),
                    name,
                    definition_line: None,
                    event_order: self.end(parent),
                    ..Default::default()
                };
            }
            statement = parent.parent();
        }
        PythonBindingTarget::default()
    }

    fn resolve(
        &self,
        mut scope: usize,
        parts: &[String],
        mut at: usize,
        depth: usize,
    ) -> PythonBindingTarget {
        let Some(name) = parts.first() else {
            return PythonBindingTarget::default();
        };
        if depth >= 32 {
            return PythonBindingTarget::default();
        }
        let global_scope = self.scopes[scope].global_scope;
        let site = at;
        loop {
            let events = self
                .bindings
                .get(&(scope, name.clone()))
                .map_or(&[][..], Vec::as_slice);
            let binding = events
                .iter()
                .filter(|b| b.at <= at && !matches!(b.value, Value::Declaration))
                .max_by_key(|b| b.at);
            let wildcard = self
                .bindings
                .get(&(scope, "*".into()))
                .into_iter()
                .flatten()
                .filter(|b| {
                    b.at <= at && (name == "*" || binding.is_none_or(|binding| b.at > binding.at))
                })
                .max_by_key(|b| b.at);
            if let Some(wildcard) = wildcard
                && let Value::Target(target) = &wildcard.value
            {
                if let Some(target) =
                    self.resolve_self_wildcard(target, scope, parts, wildcard.at, depth + 1)
                {
                    return target;
                }
                return PythonBindingTarget {
                    module: target.module.clone(),
                    name: if name == "*" {
                        String::new()
                    } else {
                        parts.join(".")
                    },
                    wildcard: true,
                    bound: true,
                    event_order: wildcard.at,
                    captures: vec![PythonBindingCapture {
                        attributes: 1,
                        import_sites: self.visible_imports(scope, wildcard.at),
                    }],
                    fallback: Some(Box::new(self.resolve(
                        scope,
                        parts,
                        wildcard.at.saturating_sub(1),
                        depth + 1,
                    ))),
                    ..Default::default()
                };
            }
            if let Some(binding) = binding {
                let mut target = match &binding.value {
                    Value::Target(target) | Value::FromImport(target) => {
                        let mut target = if matches!(binding.value, Value::FromImport(_)) {
                            self.resolve_from_import_value(target, scope, binding.at, depth + 1)
                        } else {
                            target.clone()
                        };
                        if target.module.is_some() {
                            target.import_sites = self.visible_imports(scope, binding.at);
                            if !target.module_binding {
                                let modules = target.import_sites.clone();
                                freeze_prefix(&mut target, &modules, self.symbols);
                            }
                        }
                        target
                    }
                    Value::Alias(alias, site, origin) => {
                        let mut target = self.resolve(*origin, alias, *site, depth + 1);
                        if alias.len() > 1 {
                            freeze_prefix(
                                &mut target,
                                &self.visible_imports(*origin, *site),
                                self.symbols,
                            );
                        }
                        target
                    }
                    Value::Declaration => unreachable!(),
                };
                target.bound = true;
                target.event_order = binding.at;
                if parts.len() > 1 {
                    if target.module.is_none() && target.definition_line.is_none() {
                        return target;
                    }
                    append_suffix(&mut target, &parts[1..].join("."));
                }
                return target;
            }
            if let Some(directive) = self.directives.get(&(scope, name.clone())) {
                let outer = self.directive_scope(scope, name, *directive);
                let outer_at = if self.scopes[scope].class || matches!(directive, Directive::Global)
                {
                    at
                } else {
                    self.complete
                };
                return outer
                    .map(|outer| self.resolve(outer, parts, outer_at, depth + 1))
                    .unwrap_or_default();
            }
            if self.scopes[scope].function && events.iter().any(|event| event.local) {
                return PythonBindingTarget::default();
            }
            let Some(mut parent) = self.scopes[scope].parent else {
                return PythonBindingTarget::default();
            };
            if self.scopes[scope].function {
                while self.scopes[parent].class {
                    let Some(next) = self.scopes[parent].parent else {
                        break;
                    };
                    parent = next;
                }
                if !self.scopes[scope].comprehension {
                    at = self.complete;
                }
            }
            if parent == 0 && !self.scopes[scope].overlay {
                parent = global_scope;
                at = site;
            }
            if self.scopes[scope].overlay {
                at = self.complete;
            }
            scope = parent;
        }
    }
}

impl LexicalBindings<'_> {
    fn resolve_self_wildcard(
        &self,
        target: &PythonBindingTarget,
        scope: usize,
        parts: &[String],
        at: usize,
        depth: usize,
    ) -> Option<PythonBindingTarget> {
        let module = target.module.as_ref()?;
        if !codesage_protocol::python::module_candidates(module, &self.file).contains(&self.file) {
            return None;
        }
        let before = at.saturating_sub(1);
        let all = self.resolve(scope, &["__all__".into()], before, depth);
        let Some(names) = all.export_names else {
            if all.bound && !all.deleted {
                return None;
            }
            let getter = self.resolve(scope, &["__getattr__".into()], before, depth);
            return if getter.bound && !getter.deleted {
                None
            } else {
                Some(self.resolve(scope, parts, before, depth))
            };
        };
        let name = parts.first()?;
        if name == "*" {
            return None;
        }
        if !names.contains(name) {
            return Some(self.resolve(scope, parts, before, depth));
        }
        let mut imported = self.resolve_from_import_value(
            &PythonBindingTarget {
                module: Some(module.clone()),
                name: name.clone(),
                ..Default::default()
            },
            scope,
            at,
            depth,
        );
        imported.event_order = at;
        if imported.module.is_some() {
            imported.import_sites = self.visible_imports(scope, at);
            if !imported.module_binding {
                let sites = imported.import_sites.clone();
                freeze_prefix(&mut imported, &sites, self.symbols);
            }
        }
        if parts.len() > 1 && (imported.module.is_some() || imported.definition_line.is_some()) {
            append_suffix(&mut imported, &parts[1..].join("."));
        }
        Some(imported)
    }

    fn resolve_from_import_value(
        &self,
        target: &PythonBindingTarget,
        scope: usize,
        at: usize,
        depth: usize,
    ) -> PythonBindingTarget {
        let Some(module) = &target.module else {
            return target.clone();
        };
        if target.name.is_empty()
            || !codesage_protocol::python::module_candidates(module, &self.file)
                .contains(&self.file)
        {
            return target.clone();
        }
        let parts = target
            .name
            .split('.')
            .map(str::to_string)
            .collect::<Vec<_>>();
        let prior = self.resolve(
            self.scopes[scope].global_scope,
            &parts,
            at.saturating_sub(1),
            depth,
        );
        let separator = if module.ends_with('.') { "" } else { "." };
        let getter = self.resolve(
            self.scopes[scope].global_scope,
            &["__getattr__".into()],
            at.saturating_sub(1),
            depth,
        );
        let missing = if getter.wildcard {
            target.clone()
        } else if getter.bound && !getter.deleted {
            PythonBindingTarget::default()
        } else {
            PythonBindingTarget {
                module: Some(format!("{module}{separator}{}", target.name)),
                module_binding: true,
                bound: true,
                ..Default::default()
            }
        };
        import_missing_fallback(prior, &missing)
    }
}

fn import_missing_fallback(
    mut prior: PythonBindingTarget,
    missing: &PythonBindingTarget,
) -> PythonBindingTarget {
    if prior.deleted || !prior.bound {
        return missing.clone();
    }
    if prior.wildcard {
        prior.fallback = Some(Box::new(match prior.fallback.take() {
            Some(fallback) => import_missing_fallback(*fallback, missing),
            None => missing.clone(),
        }));
    }
    prior
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::Language;

    fn bindings(source: &str) -> PythonBindings {
        let tree = crate::parse::parse_file(source.as_bytes(), Language::Python).unwrap();
        let refs = crate::references::extract_references(
            &tree,
            source.as_bytes(),
            Language::Python,
            "caller.py",
        )
        .unwrap();
        let symbols = crate::extract::extract_symbols(
            &tree,
            source.as_bytes(),
            Language::Python,
            "caller.py",
        )
        .unwrap();
        extract_python_bindings(&tree, source.as_bytes(), &refs, &symbols)
    }

    #[test]
    fn python_import_alias_and_member_evidence_keeps_lookup_paths() {
        for (source, module, name) in [
            (
                "from unittest.mock import patch as p\np('x')\n",
                "unittest.mock",
                "patch",
            ),
            (
                "from unittest import mock as m\nm.patch('x')\n",
                "unittest",
                "mock.patch",
            ),
            (
                "import unittest.mock as m\nm.patch('x')\n",
                "unittest",
                "mock.patch",
            ),
            (
                "import unittest.mock\nunittest.mock.patch('x')\n",
                "unittest",
                "mock.patch",
            ),
            ("import copy\ncopy.copy('x')\n", "copy", "copy"),
        ] {
            let bindings = bindings(source);
            let call = bindings
                .references
                .iter()
                .find(|r| r.kind == ReferenceKind::Call)
                .unwrap();
            assert_eq!(call.target.module.as_deref(), Some(module), "{source}");
            assert_eq!(call.target.name, name, "{source}");
        }
    }

    #[test]
    fn python_function_local_assignment_blocks_a_global_import_even_before_assignment() {
        let bindings = bindings(
            "from unittest.mock import patch\ndef entry():\n    patch('unbound')\n    patch = object()\n",
        );
        let call = bindings
            .references
            .iter()
            .find(|r| r.line == 3 && r.kind == ReferenceKind::Call)
            .unwrap();
        assert_eq!(call.target, PythonBindingTarget::default());
    }

    #[test]
    fn python_binding_positions_preserve_unicode_columns() {
        let bindings = bindings("from unittest.mock import patch\nπ = 1; patch('x')\n");
        let call = bindings
            .references
            .iter()
            .find(|r| r.kind == ReferenceKind::Call)
            .unwrap();
        assert_eq!(call.target.module.as_deref(), Some("unittest.mock"));
    }
}
