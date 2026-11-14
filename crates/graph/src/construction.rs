use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use codesage_protocol::{Language, Reference, ReferenceKind, Symbol, SymbolKind};
use codesage_storage::Database;

#[derive(Default)]
pub(crate) struct Constructors {
    files: HashMap<String, Vec<Symbol>>,
    imports: crate::constructor_imports::ConstructorImports,
}

impl Constructors {
    pub(crate) fn capped(&self) -> bool {
        self.imports.capped()
    }

    fn symbols(&mut self, db: &Database, file: &str) -> Result<&[Symbol]> {
        if !self.files.contains_key(file) {
            let mut symbols = Vec::new();
            for symbol in db.symbols_for_file(file)? {
                if symbol.kind == SymbolKind::Method
                    && symbol.name == "constructor"
                    && !is_instance_constructor(&symbol)?
                {
                    continue;
                }
                symbols.push(symbol);
            }
            self.files.insert(file.to_owned(), symbols);
        }
        Ok(&self.files[file])
    }

    pub(crate) fn append_targets(
        &mut self,
        db: &Database,
        row: &Reference,
        candidates: &mut Vec<Symbol>,
    ) -> Result<()> {
        let language =
            codesage_parser::detect::detect_language(std::path::Path::new(&row.from_file));
        if !matches!(
            (language, row.kind),
            (Some(Language::Python), ReferenceKind::Call)
                | (
                    Some(Language::Php | Language::JavaScript | Language::TypeScript),
                    ReferenceKind::Instantiation
                )
        ) {
            return Ok(());
        }
        let classes: Vec<_> = candidates
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        let class = match classes.as_slice() {
            [only] if candidates.len() == 1 => (*only).clone(),
            [] => return Ok(()),
            _ => match self.imports.select(db, row, candidates)? {
                Some(selected) => selected,
                None => return Ok(()),
            },
        };
        let Some((name, separator)) = constructor_name(&class.file_path) else {
            return Ok(());
        };
        let matches: Vec<_> = self
            .symbols(db, &class.file_path)?
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Method
                    && constructor_name_matches(&class.file_path, &symbol.name, name)
                    && symbol.qualified_name.rsplit_once(separator).is_some_and(
                        |(owner, member)| owner == class.qualified_name && member == symbol.name,
                    )
                    && class.line_start <= symbol.line_start
                    && symbol.line_end <= class.line_end
            })
            .collect();
        if let [only] = matches.as_slice()
            && !candidates.iter().any(|s| same_symbol(s, only))
        {
            candidates.push((*only).clone());
        }
        Ok(())
    }

    pub(crate) fn owner(&mut self, db: &Database, constructor: &Symbol) -> Result<Option<Symbol>> {
        let Some((name, separator)) = constructor_name(&constructor.file_path) else {
            return Ok(None);
        };
        if constructor.kind != SymbolKind::Method
            || !constructor_name_matches(&constructor.file_path, &constructor.name, name)
        {
            return Ok(None);
        }
        let Some(owner) = constructor
            .qualified_name
            .strip_suffix(&format!("{separator}{}", constructor.name))
        else {
            return Ok(None);
        };
        let symbols = self.symbols(db, &constructor.file_path)?;
        if !symbols
            .iter()
            .any(|symbol| same_symbol(symbol, constructor))
        {
            return Ok(None);
        }
        let classes: Vec<_> = symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Class
                    && symbol.qualified_name == owner
                    && symbol.line_start <= constructor.line_start
                    && constructor.line_end <= symbol.line_end
            })
            .collect();
        Ok(match classes.as_slice() {
            [only] => Some((*only).clone()),
            _ => None,
        })
    }

    pub(crate) fn reference_names(
        &mut self,
        db: &Database,
        symbol: &Symbol,
    ) -> Result<Vec<String>> {
        let mut names = vec![symbol.name.clone()];
        if let Some(owner) = self.owner(db, symbol)?
            && owner.name != symbol.name
        {
            names.push(owner.name);
        }
        Ok(names)
    }
}

fn is_instance_constructor(symbol: &Symbol) -> Result<bool> {
    let Some(language @ (Language::JavaScript | Language::TypeScript)) =
        codesage_parser::detect::detect_language(std::path::Path::new(&symbol.file_path))
    else {
        return Ok(true);
    };
    let Some(signature) = &symbol.signature else {
        return Ok(false);
    };
    let source = format!("class Constructor {{ {}\n{{}} }}", signature.declaration);
    let parsed = codesage_parser::parse::parse_file_tolerant(source.as_bytes(), language)?;
    if parsed.degraded {
        return Ok(false);
    }
    let Some(body) = parsed
        .tree
        .root_node()
        .named_child(0)
        .and_then(|class| class.child_by_field_name("body"))
    else {
        return Ok(false);
    };
    let Some(method) = body
        .named_children(&mut body.walk())
        .find(|child| child.kind() == "method_definition")
    else {
        return Ok(false);
    };
    Ok(method
        .child_by_field_name("name")
        .is_some_and(|name| matches!(name.kind(), "property_identifier" | "string"))
        && !method
            .children(&mut method.walk())
            .any(|child| matches!(child.kind(), "static" | "get" | "set" | "async" | "*")))
}

/// Rows that may reference `symbol`: those spelled with its name, those
/// spelled with a Rust `use` alias of it (`modules` loads the module context
/// only when an import of the name exists), and for a constructor, the rows
/// instantiating its owner.
pub(crate) fn references_for_symbol(
    db: &Database,
    symbol: &Symbol,
    modules: &mut dyn FnMut() -> Result<Arc<crate::rust_modules::RustModules>>,
) -> Result<Vec<Reference>> {
    let mut rows = db.find_references(&symbol.name, None)?;
    if symbol.file_path.ends_with(".rs") && rust_imports_name(&rows, &symbol.name) {
        let aliased = modules()?.alias_references(db, &symbol.name, &rows)?;
        rows.extend(aliased);
    }
    extend_php_qualified_references(db, symbol, &mut rows)?;
    if let Some(owner) = Constructors::default().owner(db, symbol)? {
        rows.extend(
            db.find_references(&owner.name, None)?
                .into_iter()
                .filter(|row| {
                    matches!(row.kind, ReferenceKind::Call | ReferenceKind::Instantiation)
                }),
        );
    }
    Ok(rows)
}

/// A Rust `use` among `rows` imports something by the bare `name`.
pub(crate) fn rust_imports_name(rows: &[Reference], name: &str) -> bool {
    rows.iter().any(|row| {
        row.kind == ReferenceKind::Import
            && row.from_file.ends_with(".rs")
            && row.to_name.rsplit("::").next() == Some(name)
    })
}

/// Add the rows that spell a PHP class by its qualified name in another
/// ASCII case. PHP class names are case-insensitive and the parser stores
/// qualified spellings fully qualified, so `\lib\USER` shares no bare tail
/// with `User` but does share its qualified name.
pub(crate) fn extend_php_qualified_references(
    db: &Database,
    symbol: &Symbol,
    rows: &mut Vec<Reference>,
) -> Result<()> {
    if codesage_parser::detect::detect_language(std::path::Path::new(&symbol.file_path))
        != Some(Language::Php)
        || !matches!(
            symbol.kind,
            SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait | SymbolKind::Enum
        )
    {
        return Ok(());
    }
    let qualified = if symbol.qualified_name.contains('\\') {
        symbol.qualified_name.clone()
    } else {
        format!("\\{}", symbol.qualified_name)
    };
    let seen: std::collections::HashSet<_> = rows
        .iter()
        .map(|r| (r.from_file.clone(), r.line, r.col, r.kind))
        .collect();
    rows.extend(
        db.find_references(&qualified, None)?
            .into_iter()
            .filter(|r| !seen.contains(&(r.from_file.clone(), r.line, r.col, r.kind))),
    );
    Ok(())
}

pub(crate) fn same_symbol(left: &Symbol, right: &Symbol) -> bool {
    left.file_path == right.file_path
        && left.qualified_name == right.qualified_name
        && left.line_start == right.line_start
        && left.col_start == right.col_start
        && left.kind == right.kind
}

fn constructor_name(file: &str) -> Option<(&'static str, char)> {
    match codesage_parser::detect::detect_language(std::path::Path::new(file))? {
        Language::Python => Some(("__init__", '.')),
        Language::Php => Some(("__construct", '\\')),
        Language::JavaScript | Language::TypeScript => Some(("constructor", '.')),
        _ => None,
    }
}

fn constructor_name_matches(file: &str, actual: &str, expected: &str) -> bool {
    if codesage_parser::detect::detect_language(std::path::Path::new(file)) == Some(Language::Php) {
        actual.eq_ignore_ascii_case(expected)
    } else {
        actual == expected
    }
}
