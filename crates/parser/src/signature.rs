use codesage_protocol::{Arity, CallableSignature, Language, Parameter};
use tree_sitter::Node;

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk()).collect()
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub fn declarator(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    while let Some(next) = current.child_by_field_name("declarator").or_else(|| {
        (current.kind() == "reference_declarator")
            .then(|| current.named_child(0))
            .flatten()
    }) {
        current = next;
        if current.kind() == "function_declarator" {
            break;
        }
    }
    current
}

pub fn parameter_arity(params: Node<'_>, source: &str, language: Language) -> Option<Arity> {
    let mut minimum = 0;
    let mut maximum = Some(0);
    let has_ellipsis = params
        .children(&mut params.walk())
        .any(|node| node.kind() == "...");
    for parameter in children(params) {
        let kind = parameter.kind();
        if matches!(kind, "comment" | "line_comment" | "block_comment") {
            continue;
        }
        if kind.contains("attribute") {
            return None;
        }
        if kind == "self_parameter"
            || (language == Language::TypeScript
                && parameter
                    .child_by_field_name("pattern")
                    .is_some_and(|n| text(n, source) == "this"))
        {
            return None;
        }
        if matches!(
            kind,
            "positional_separator"
                | "keyword_separator"
                | "dictionary_splat_pattern"
                | "list_splat_pattern"
        ) || children(parameter)
            .iter()
            .any(|n| matches!(n.kind(), "dictionary_splat_pattern" | "list_splat_pattern"))
        {
            return None;
        }
        if text(parameter, source) == "void" && matches!(language, Language::C | Language::Cpp) {
            continue;
        }
        if kind.contains("variadic")
            || matches!(kind, "rest_pattern" | "spread_parameter")
            || children(parameter)
                .iter()
                .any(|n| n.kind() == "rest_pattern")
        {
            maximum = None;
            continue;
        }
        let optional = is_optional(parameter);
        let count = if language == Language::Go {
            let mut cursor = parameter.walk();
            parameter
                .children(&mut cursor)
                .enumerate()
                .filter(|(index, _)| parameter.field_name_for_child(*index as u32) == Some("name"))
                .count()
                .max(1)
        } else {
            1
        };
        maximum = maximum.map(|n| n + count);
        if !optional {
            minimum = maximum?;
        }
    }
    if language == Language::C && children(params).is_empty() {
        return None;
    }
    if language == Language::JavaScript {
        return None;
    }
    if language == Language::Php || has_ellipsis {
        maximum = None;
    }
    Some(Arity { minimum, maximum })
}

fn is_optional(parameter: Node<'_>) -> bool {
    matches!(
        parameter.kind(),
        "default_parameter"
            | "typed_default_parameter"
            | "optional_parameter"
            | "assignment_pattern"
            | "optional_parameter_declaration"
    ) || parameter.child_by_field_name("default_value").is_some()
        || parameter.child_by_field_name("value").is_some()
}

fn callable<'a>(node: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    if matches!(node.kind(), "declaration" | "field_declaration") {
        let declarators = node
            .children(&mut node.walk())
            .enumerate()
            .filter(|(index, _)| node.field_name_for_child(*index as u32) == Some("declarator"))
            .count();
        if declarators > 1 {
            return None;
        }
    }
    if matches!(
        node.kind(),
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_declaration"
            | "method_definition"
            | "constructor_declaration"
            | "function_signature"
            | "method_signature"
            | "abstract_method_signature"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
    ) || declarator(node).kind() == "function_declarator"
    {
        return Some(node);
    }
    if matches!(node.kind(), "lexical_declaration" | "variable_declaration") {
        let declaration = children(node).into_iter().find(|n| {
            n.kind() == "variable_declarator"
                && n.child_by_field_name("name")
                    .is_some_and(|n| text(n, source) == name)
        })?;
        return declaration
            .child_by_field_name("value")
            .and_then(|node| callable(node, source, name));
    }
    if matches!(node.kind(), "variable_declarator" | "assignment_expression") {
        return node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("right"))
            .and_then(|node| callable(node, source, name));
    }
    None
}

fn parameter_name_node(node: Node<'_>) -> Option<Node<'_>> {
    let candidate = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("pattern"))
        .or_else(|| node.child_by_field_name("left"))
        .or_else(|| node.child_by_field_name("declarator"))
        .unwrap_or(node);
    let mut candidate = candidate;
    loop {
        if matches!(
            candidate.kind(),
            "identifier" | "field_identifier" | "variable_name" | "self"
        ) {
            return Some(candidate);
        }
        candidate = candidate
            .child_by_field_name("declarator")
            .or_else(|| candidate.child_by_field_name("name"))
            .or_else(|| {
                (candidate.kind() == "parenthesized_declarator")
                    .then(|| candidate.named_child(0))
                    .flatten()
            })
            .or_else(|| {
                matches!(
                    candidate.kind(),
                    "typed_parameter"
                        | "spread_parameter"
                        | "rest_pattern"
                        | "list_splat_pattern"
                        | "dictionary_splat_pattern"
                        | "self_parameter"
                )
                .then(|| {
                    children(candidate).into_iter().find(|n| {
                        matches!(
                            n.kind(),
                            "identifier"
                                | "self"
                                | "variable_name"
                                | "variable_declarator"
                                | "list_splat_pattern"
                                | "dictionary_splat_pattern"
                        )
                    })
                })
                .flatten()
            })?;
    }
}

fn parameter(node: Node<'_>, source: &str, language: Language) -> Parameter {
    let declaration = text(node, source).trim().to_owned();
    let variadic = node.kind().contains("variadic")
        || matches!(
            node.kind(),
            "rest_pattern" | "spread_parameter" | "list_splat_pattern" | "dictionary_splat_pattern"
        )
        || declaration.starts_with("...")
        || children(node).iter().any(|n| {
            matches!(
                n.kind(),
                "rest_pattern" | "list_splat_pattern" | "dictionary_splat_pattern"
            )
        });
    let name = parameter_name_node(node);
    let type_name = if matches!(language, Language::C | Language::Cpp) {
        let end = node
            .child_by_field_name("default_value")
            .map_or(node.end_byte(), |value| {
                source[node.start_byte()..value.start_byte()]
                    .rfind('=')
                    .map_or(value.start_byte(), |offset| node.start_byte() + offset)
            });
        name.and_then(|name| {
            (name.end_byte() <= end).then(|| {
                format!(
                    "{}{}",
                    &source[node.start_byte()..name.start_byte()],
                    &source[name.end_byte()..end]
                )
                .trim()
                .to_owned()
            })
        })
        .or_else(|| {
            node.child_by_field_name("type")
                .map(|_| source[node.start_byte()..end].trim().to_owned())
        })
    } else if language == Language::Java {
        node.child_by_field_name("type")
            .or_else(|| {
                (node.kind() == "spread_parameter")
                    .then(|| {
                        children(node)
                            .into_iter()
                            .find(|n| !matches!(n.kind(), "modifiers" | "variable_declarator"))
                    })
                    .flatten()
            })
            .map(|ty| {
                let dimensions = node
                    .child_by_field_name("dimensions")
                    .map_or("", |n| text(n, source));
                format!("{}{dimensions}", text(ty, source))
            })
    } else {
        node.child_by_field_name("type").map(|n| {
            text(n, source)
                .trim()
                .trim_start_matches(':')
                .trim()
                .to_owned()
        })
    };
    Parameter {
        declaration,
        name: name.map(|name| text(name, source).to_owned()),
        type_name,
        default_value: node
            .child_by_field_name("default_value")
            .or_else(|| node.child_by_field_name("value"))
            .or_else(|| {
                (node.kind() == "assignment_pattern")
                    .then(|| node.child_by_field_name("right"))
                    .flatten()
            })
            .map(|n| text(n, source).to_owned()),
        optional: is_optional(node),
        variadic,
    }
}

fn returns(node: Node<'_>, source: &str, language: Language) -> Option<String> {
    if let Some(result) = node
        .child_by_field_name("return_type")
        .or_else(|| node.child_by_field_name("result"))
    {
        return Some(
            text(result, source)
                .trim()
                .trim_start_matches(':')
                .trim()
                .to_owned(),
        );
    }
    if matches!(language, Language::C | Language::Cpp) {
        let decl = declarator(node);
        if let Some(result) = children(decl)
            .into_iter()
            .find(|n| n.kind() == "trailing_return_type")
        {
            return Some(
                text(result, source)
                    .trim()
                    .trim_start_matches("->")
                    .trim()
                    .to_owned(),
            );
        }
        let ty = node.child_by_field_name("type")?;
        if decl.start_byte() < ty.end_byte() {
            return None;
        }
        let suffix = source[ty.end_byte()..decl.start_byte()].trim();
        if suffix.chars().any(|c| c == '(' || c == ')') {
            return None;
        }
        let qualifiers: String = children(node)
            .into_iter()
            .filter(|n| n.kind() == "type_qualifier" && n.end_byte() <= ty.start_byte())
            .map(|n| format!("{} ", text(n, source)))
            .collect();
        return Some(format!(
            "{qualifiers}{}{}{}",
            text(ty, source),
            if suffix.is_empty() { "" } else { " " },
            suffix
        ));
    }
    if language == Language::Java {
        let ty = node.child_by_field_name("type")?;
        let dimensions = node
            .child_by_field_name("dimensions")
            .map_or("", |n| text(n, source));
        return Some(format!("{}{dimensions}", text(ty, source)));
    }
    None
}

pub fn extract(
    node: Node<'_>,
    source: &[u8],
    language: Language,
    name: &str,
) -> Option<CallableSignature> {
    let source = std::str::from_utf8(source).ok()?;
    let node = callable(node, source, name)?;
    let params = node
        .child_by_field_name("parameters")
        .or_else(|| declarator(node).child_by_field_name("parameters"))
        .or_else(|| node.child_by_field_name("parameter"));
    let params = params?;
    if params.has_error() {
        return None;
    }
    let parameters = if params.kind() == "identifier" {
        vec![parameter(params, source, language)]
    } else {
        children(params)
            .into_iter()
            .filter(|n| {
                !(matches!(
                    n.kind(),
                    "comment"
                        | "line_comment"
                        | "block_comment"
                        | "positional_separator"
                        | "keyword_separator"
                ) || (matches!(language, Language::C | Language::Cpp)
                    && text(*n, source) == "void"))
            })
            .flat_map(|n| {
                let param = parameter(n, source, language);
                if language != Language::Go {
                    return vec![param];
                }
                let names: Vec<_> = n
                    .children(&mut n.walk())
                    .enumerate()
                    .filter(|(i, _)| n.field_name_for_child(*i as u32) == Some("name"))
                    .map(|(_, name)| text(name, source).to_owned())
                    .collect();
                if names.is_empty() {
                    vec![param]
                } else {
                    names
                        .into_iter()
                        .map(|name| Parameter {
                            name: Some(name),
                            ..param.clone()
                        })
                        .collect()
                }
            })
            .collect()
    };
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    let declaration = source[node.start_byte()..end]
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_owned();
    Some(CallableSignature {
        declaration,
        arity: if params.kind() == "identifier"
            || (language == Language::Python && python_method(node))
            || (language == Language::TypeScript && typescript_rest_arity_unknown(params))
            || (language == Language::Rust
                && children(params).iter().any(|parameter| {
                    parameter
                        .child_by_field_name("pattern")
                        .is_some_and(|pattern| pattern.kind() == "self")
                }))
        {
            None
        } else {
            parameter_arity(params, source, language)
        },
        parameters,
        returns: returns(node, source, language),
    })
}

fn python_method(node: Node<'_>) -> bool {
    let mut parent = node.parent();
    while let Some(node) = parent {
        match node.kind() {
            "class_definition" => return true,
            "function_definition" => return false,
            _ => parent = node.parent(),
        }
    }
    false
}

fn typescript_rest_arity_unknown(params: Node<'_>) -> bool {
    children(params).iter().any(|parameter| {
        parameter
            .child_by_field_name("pattern")
            .is_some_and(|pattern| pattern.kind() == "rest_pattern")
            && parameter.child_by_field_name("type").is_some_and(|ty| {
                ty.kind() != "array_type"
                    && !(ty.kind() == "type_annotation"
                        && ty
                            .named_child(0)
                            .is_some_and(|inner| inner.kind() == "array_type"))
            })
    })
}
