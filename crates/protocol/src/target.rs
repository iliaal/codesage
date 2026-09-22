//! One resolution shape for every tool that names an entity.
//!
//! The grammar a resolver accepts is one string:
//!
//! ```text
//! target := handle | path | path ":" line | qualified | name
//!         | feature_id | route | command | text
//! handle := "sym:" path "#" qualified [ "@" line ]
//!         | "file:" path | "dir:" path | "chunk:" path ":" start "-" end
//!         | "feat_" hex16
//! route  := "route:" METHOD " " path
//! command := "cmd:" name
//! ```
//!
//! [`TargetResolution`] is what came back: the candidates, whether the input
//! named more than one entity, and how many existed before the candidate
//! limit. Every candidate carries a handle, so an agent that reads an
//! ambiguous answer can retry with one of them.

use serde::{Deserialize, Serialize};

/// What an input named. `Text` is the fallback for a string that names no
/// indexed entity: the caller may hand it to semantic search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Symbol,
    File,
    Dir,
    Chunk,
    Feature,
    Route,
    Command,
    Text,
}

/// How a candidate was reached, strongest evidence first.
///
/// The confidence ladder ([`ResolveVia::confidence`]):
///
/// | via | confidence | evidence |
/// |---|---|---|
/// | `exact` | 1.0 | a parsed handle, an indexed path, or `path:line` |
/// | `qualified` | 1.0 | the input equals a stored qualified name |
/// | `unique` | 0.9 | a bare name with exactly one indexed definition |
/// | `import` | 0.8 | a bare name narrowed to one by the calling file's imports |
/// | `casefold` | 0.5 | a name that differs only by letter case |
/// | `suffix` | 0.4 | a trailing path or name segment matched, including a `sym:` handle whose qualified name lives in another file |
///
/// [`CONFIDENT`] is the floor a caller may act on without asking. `casefold`
/// and `suffix` sit below it deliberately: they are the nearest candidates a
/// rename left behind, evidence for a retry rather than an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResolveVia {
    Exact,
    Qualified,
    Unique,
    Import,
    Casefold,
    Suffix,
}

/// Lowest confidence a caller may act on without disclosing a guess.
pub const CONFIDENT: f32 = 0.8;

impl ResolveVia {
    pub fn confidence(self) -> f32 {
        match self {
            Self::Exact | Self::Qualified => 1.0,
            Self::Unique => 0.9,
            Self::Import => 0.8,
            Self::Casefold => 0.5,
            Self::Suffix => 0.4,
        }
    }
}

/// One entity an input named.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResolvedTarget {
    /// `sym:`, `file:`, `dir:`, `chunk:`, or `feat_` handle. Feed it back as
    /// the target to name this candidate alone.
    pub handle: String,
    /// The symbol kind (`function`, `struct`, …) for a symbol, else the
    /// target kind (`file`, `dir`, `chunk`, `feature`, `route`, `command`).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    /// Derived from the path's test conventions. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_test: bool,
    pub confidence: f32,
    pub via: ResolveVia,
}

impl ResolvedTarget {
    /// True when the candidate is a match rather than a nearest-neighbour guess.
    pub fn confident(&self) -> bool {
        self.confidence >= CONFIDENT
    }
}

/// What an input resolved to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TargetResolution {
    /// The string as the caller wrote it.
    pub input: String,
    pub kind: TargetKind,
    /// Candidates, capped at the caller's limit; `candidates_total` says how
    /// many existed.
    pub resolved: Vec<ResolvedTarget>,
    /// More than one entity carries this name. Tools whose answer is a union
    /// of per-entity answers return it and set this; the rest refuse with
    /// `E_AMBIGUOUS` and these candidates.
    pub ambiguous: bool,
    pub candidates_total: usize,
    /// Definitions that share one qualified name inside one file (C++
    /// overloads, methods on several `impl` blocks). Their handles carry
    /// `@line`. Omitted when zero, which is the usual case.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub overloads: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl TargetResolution {
    /// `resolved` is already ordered and capped; `candidates_total` counts
    /// what existed before the cap.
    pub fn new(
        input: impl Into<String>,
        kind: TargetKind,
        resolved: Vec<ResolvedTarget>,
        candidates_total: usize,
        overloads: usize,
    ) -> Self {
        Self {
            input: input.into(),
            kind,
            ambiguous: candidates_total > 1,
            candidates_total,
            overloads,
            resolved,
        }
    }

    /// Nothing indexed carries this name.
    pub fn text(input: impl Into<String>) -> Self {
        Self::new(input, TargetKind::Text, Vec::new(), 0, 0)
    }

    /// The one entity the input named, when it named exactly one and did not
    /// have to guess. `None` for an ambiguous, empty, or guessed resolution.
    pub fn sole(&self) -> Option<&ResolvedTarget> {
        match self.resolved.as_slice() {
            [only] if !self.ambiguous && only.confident() => Some(only),
            _ => None,
        }
    }

    /// The candidates are nearest neighbours (case- or suffix-matched), not
    /// matches: the input names nothing as written.
    pub fn guessed(&self) -> bool {
        !self.resolved.is_empty() && self.resolved.iter().all(|c| !c.confident())
    }

    /// Every candidate's handle, in resolution order.
    pub fn handles(&self) -> Vec<String> {
        self.resolved.iter().map(|c| c.handle.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(handle: &str, via: ResolveVia) -> ResolvedTarget {
        ResolvedTarget {
            handle: handle.to_string(),
            kind: "function".to_string(),
            path: Some("a.rs".to_string()),
            line_start: Some(1),
            line_end: Some(2),
            is_test: false,
            confidence: via.confidence(),
            via,
        }
    }

    #[test]
    fn sole_requires_one_confident_unambiguous_candidate() {
        let one = TargetResolution::new(
            "search",
            TargetKind::Symbol,
            vec![candidate("sym:a.rs#search", ResolveVia::Unique)],
            1,
            0,
        );
        assert_eq!(
            one.sole().map(|c| c.handle.as_str()),
            Some("sym:a.rs#search")
        );
        assert!(!one.ambiguous);
        assert!(!one.guessed());

        let two = TargetResolution::new(
            "search",
            TargetKind::Symbol,
            vec![
                candidate("sym:a.rs#search", ResolveVia::Unique),
                candidate("sym:b.rs#search", ResolveVia::Unique),
            ],
            2,
            0,
        );
        assert!(two.ambiguous);
        assert_eq!(two.sole(), None);

        let guess = TargetResolution::new(
            "Search",
            TargetKind::Symbol,
            vec![candidate("sym:a.rs#search", ResolveVia::Casefold)],
            1,
            0,
        );
        assert_eq!(
            guess.sole(),
            None,
            "a casefold hit is a lead, not an answer"
        );
        assert!(guess.guessed());
    }

    #[test]
    fn capped_candidates_keep_the_full_total() {
        let capped = TargetResolution::new(
            "run",
            TargetKind::Symbol,
            vec![candidate("sym:a.rs#run", ResolveVia::Unique)],
            37,
            0,
        );
        assert!(capped.ambiguous);
        assert_eq!(capped.candidates_total, 37);
        assert_eq!(capped.resolved.len(), 1);
    }

    #[test]
    fn silent_fields_stay_out_of_the_wire_when_unremarkable() {
        let json = serde_json::to_value(TargetResolution::new(
            "search",
            TargetKind::Symbol,
            vec![candidate("sym:a.rs#search", ResolveVia::Exact)],
            1,
            0,
        ))
        .unwrap();
        assert_eq!(json["ambiguous"], serde_json::json!(false));
        assert_eq!(json["candidates_total"], serde_json::json!(1));
        assert!(json.get("overloads").is_none(), "{json}");
        assert!(json["resolved"][0].get("is_test").is_none(), "{json}");
        assert_eq!(json["resolved"][0]["via"], serde_json::json!("exact"));
        assert_eq!(json["kind"], serde_json::json!("symbol"));

        let overloaded = TargetResolution::new(
            "run",
            TargetKind::Symbol,
            vec![
                candidate("sym:a.cpp#Foo::run@10", ResolveVia::Unique),
                candidate("sym:a.cpp#Foo::run@30", ResolveVia::Unique),
            ],
            2,
            2,
        );
        let json = serde_json::to_value(&overloaded).unwrap();
        assert_eq!(json["overloads"], serde_json::json!(2));
        assert_eq!(
            overloaded.handles(),
            ["sym:a.cpp#Foo::run@10", "sym:a.cpp#Foo::run@30"]
        );
    }

    #[test]
    fn text_resolves_to_nothing_for_semantic_search() {
        let text = TargetResolution::text("where does auth happen");
        assert_eq!(text.kind, TargetKind::Text);
        assert!(text.resolved.is_empty());
        assert!(!text.ambiguous);
        assert!(!text.guessed());
        assert_eq!(text.sole(), None);
    }
}
