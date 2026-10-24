use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Arity {
    pub minimum: usize,
    pub maximum: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Parameter {
    pub declaration: String,
    pub name: Option<String>,
    pub type_name: Option<String>,
    pub default_value: Option<String>,
    pub optional: bool,
    pub variadic: bool,
}

/// Syntax recorded from the indexed source, without inferred runtime types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CallableSignature {
    pub declaration: String,
    /// Accepted positional argument counts, or unknown for unsupported semantics.
    pub arity: Option<Arity>,
    pub parameters: Vec<Parameter>,
    pub returns: Option<String>,
}
