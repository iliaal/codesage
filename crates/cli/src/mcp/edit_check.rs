use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EditCheckParams {
    /// Absolute Git worktree root.
    pub project: String,
    /// Repository-relative source path at HEAD.
    pub file_path: String,
    /// Exact unqualified declaration name.
    pub symbol_name: String,
    /// One-based declaration start line at HEAD, required for ambiguous names.
    pub line: Option<usize>,
    /// Complete replacement declaration, including signature and body.
    pub replacement: String,
}
