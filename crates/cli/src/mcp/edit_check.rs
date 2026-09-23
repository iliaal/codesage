use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EditCheckParams {
    /// Absolute Git worktree root.
    pub project: String,
    /// Repository-relative source path at HEAD.
    pub file_path: String,
    /// DEPRECATED, removed in the next minor: pass `target`. Exact
    /// unqualified declaration name, or a `sym:` handle naming `file_path`.
    pub symbol_name: Option<String>,
    /// Preferred spelling of the declaration: the exact unqualified name, or
    /// a `sym:` handle naming the same `file_path` (its `@line` supplies
    /// `line`). Alias of `symbol_name`: pass one, or the same value in both.
    pub target: Option<String>,
    /// One-based declaration start line at HEAD, required for ambiguous names.
    pub line: Option<usize>,
    /// Complete replacement declaration, including signature and body.
    pub replacement: String,
}

impl EditCheckParams {
    pub(super) fn target_arg(&self) -> Result<String> {
        super::params::one_target(
            "edit_check",
            "symbol_name",
            self.symbol_name.as_deref(),
            self.target.as_deref(),
        )
    }
}
