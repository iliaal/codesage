/// What a [`PhpName`] row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhpNameKind {
    /// A namespace the file declares; `""` is the global namespace.
    Namespace,
    /// A class `use` clause.
    Class,
    /// A `use function` clause.
    Function,
    /// A `use const` clause.
    Const,
}

impl PhpNameKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Class => "class",
            Self::Function => "function",
            Self::Const => "const",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "namespace" => Self::Namespace,
            "class" => Self::Class,
            "function" => Self::Function,
            "const" => Self::Const,
            _ => return None,
        })
    }
}

/// One name-resolution fact of a PHP file, captured at index time: a
/// namespace it declares (`name` is the namespace, `target` is empty), or a
/// `use` clause (`name` is the alias it binds, explicit or implied, and
/// `target` the fully qualified name without a leading `\\`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PhpName {
    pub kind: PhpNameKind,
    pub name: String,
    pub target: String,
    pub line: u32,
}
