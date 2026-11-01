//! Tool names retired by the surface consolidation, accepted for one minor
//! release. A retired call is rewritten into its successor's name and
//! arguments before anything else sees it, so validation, rendering, the
//! envelope, and `next[]` all answer as the successor would.

use rmcp::model::CallToolRequestParams;
use serde_json::Value;

use super::error::{ErrorCode, McpError};

pub(super) struct Retired {
    pub name: &'static str,
    pub successor: &'static str,
    /// Arguments the retired tool accepted besides `project` and the common
    /// row controls. Anything else was a parameter error before and stays one.
    accepted: &'static [&'static str],
    /// Arguments that make the successor answer the retired tool's question.
    implied: &'static [(&'static str, Implied)],
    /// Spellings moved onto the successor argument that keeps the retired
    /// tool's semantics.
    renamed: &'static [(&'static str, &'static str)],
}

#[derive(Clone, Copy)]
enum Implied {
    True,
    Str(&'static str),
}

impl Implied {
    fn value(self) -> Value {
        match self {
            Self::True => Value::Bool(true),
            Self::Str(value) => Value::String(value.to_owned()),
        }
    }
}

const COMMON: &[&str] = &["project", "detail", "budget_tokens"];

pub(super) const RETIRED: &[Retired] = &[
    Retired {
        name: "assess_risk_batch",
        successor: "assess_risk",
        accepted: &["file_paths", "targets", "verbose"],
        implied: &[],
        renamed: &[],
    },
    Retired {
        name: "assess_risk_diff",
        successor: "assess_risk",
        accepted: &["file_paths", "targets", "verbose"],
        implied: &[("aggregate", Implied::True)],
        renamed: &[],
    },
    Retired {
        name: "list_features",
        successor: "features",
        accepted: &["kind", "language", "tag", "since", "limit"],
        implied: &[],
        renamed: &[],
    },
    Retired {
        name: "find_feature",
        successor: "features",
        accepted: &["file_path", "target"],
        implied: &[],
        renamed: &[],
    },
    Retired {
        name: "export_context",
        successor: "bundle",
        accepted: &[
            "target",
            "is_symbol",
            "limit",
            "include_callers",
            "include_callees",
        ],
        implied: &[],
        renamed: &[],
    },
    Retired {
        name: "feature_bundle",
        successor: "bundle",
        accepted: &[
            "feature_id",
            "target",
            "limit",
            "include_callers",
            "include_callees",
        ],
        implied: &[],
        // `bundle` reads `feature_id` as a feature lookup, so a bare word
        // the retired tool would have reported missing never becomes a
        // semantic search.
        renamed: &[("target", "feature_id")],
    },
    Retired {
        name: "session_start",
        successor: "session",
        accepted: &["session_id"],
        implied: &[("action", Implied::Str("start"))],
        renamed: &[],
    },
    Retired {
        name: "session_end",
        successor: "session",
        accepted: &["session_id"],
        implied: &[("action", Implied::Str("end"))],
        renamed: &[],
    },
];

pub(super) fn retired(name: &str) -> Option<&'static Retired> {
    RETIRED.iter().find(|retired| retired.name == name)
}

/// Rewrite a retired call into its successor. Returns the retired name when
/// a rewrite happened. A call that passes an argument the retired tool never
/// took, or contradicts an implied one, is refused rather than resolved,
/// because answering it would mean guessing which question was meant.
pub(super) fn rewrite(
    request: &mut CallToolRequestParams,
) -> Result<Option<&'static str>, McpError> {
    let Some(retired) = retired(request.name.as_ref()) else {
        return Ok(None);
    };
    let arguments = request.arguments.get_or_insert_default();
    let implied = |key: &str| retired.implied.iter().any(|(name, _)| *name == key);
    if let Some(key) = arguments.keys().find(|key| {
        !COMMON.contains(&key.as_str())
            && !retired.accepted.contains(&key.as_str())
            && !implied(key)
    }) {
        return Err(McpError::new(
            ErrorCode::Param,
            format!(
                "{}: unknown field `{key}` (`{}` is a retired alias of `{}`, which accepts it; call `{}` directly)",
                retired.name, retired.name, retired.successor, retired.successor
            ),
        ));
    }
    for (key, value) in retired.implied {
        let value = value.value();
        match arguments.get(*key) {
            Some(existing) if *existing != value => {
                return Err(McpError::new(
                    ErrorCode::Param,
                    format!(
                        "{}: `{key}: {existing}` contradicts the retired alias, which means `{}` with `{key}: {value}`; call `{}` directly",
                        retired.name, retired.successor, retired.successor
                    ),
                ));
            }
            _ => {
                arguments.insert((*key).to_owned(), value);
            }
        }
    }
    for (from, to) in retired.renamed {
        let Some(value) = arguments.remove(*from) else {
            continue;
        };
        match arguments.get(*to) {
            Some(existing) if existing.as_str().map(str::trim) != value.as_str().map(str::trim) => {
                return Err(McpError::new(
                    ErrorCode::Param,
                    format!(
                        "{}: `{to}` ({existing}) and `{from}` ({value}) name different things; pass one of them",
                        retired.name
                    ),
                ));
            }
            Some(_) => {}
            None => {
                arguments.insert((*to).to_owned(), value);
            }
        }
    }
    request.name = retired.successor.into();
    Ok(Some(retired.name))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(name: &'static str, arguments: Value) -> CallToolRequestParams {
        CallToolRequestParams::new(name).with_arguments(arguments.as_object().unwrap().clone())
    }

    #[test]
    fn every_retired_name_rewrites_to_an_advertised_successor_with_its_implied_arguments() {
        let advertised: Vec<String> = super::super::CodeSageServer::new()
            .advertised_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        for retired in RETIRED {
            assert!(
                !advertised.iter().any(|name| name == retired.name),
                "{} must not be advertised",
                retired.name
            );
            assert!(
                advertised.iter().any(|name| name == retired.successor),
                "{} must be advertised",
                retired.successor
            );
            let mut request = call(retired.name, json!({"project": "/p"}));
            assert_eq!(rewrite(&mut request).unwrap(), Some(retired.name));
            assert_eq!(request.name, retired.successor);
            let arguments = request.arguments.unwrap();
            for (key, value) in retired.implied {
                assert_eq!(arguments[*key], value.value(), "{}", retired.name);
            }
        }
    }

    #[test]
    fn current_names_pass_through_untouched() {
        let mut request = call("assess_risk", json!({"project": "/p", "targets": ["a.rs"]}));
        assert_eq!(rewrite(&mut request).unwrap(), None);
        assert_eq!(request.name, "assess_risk");
        assert_eq!(request.arguments.unwrap().len(), 2);
    }

    #[test]
    fn contradicting_an_implied_argument_is_refused() {
        for (name, arguments) in [
            (
                "assess_risk_diff",
                json!({"project": "/p", "targets": ["a.rs"], "aggregate": false}),
            ),
            ("session_start", json!({"project": "/p", "action": "end"})),
            ("session_end", json!({"project": "/p", "action": "start"})),
        ] {
            let mut request = call(name, arguments);
            let error = rewrite(&mut request).unwrap_err();
            assert_eq!(error.code, ErrorCode::Param);
            assert!(error.to_string().contains("contradicts"), "{error}");
        }
        let mut agreeing = call(
            "assess_risk_diff",
            json!({"project": "/p", "targets": ["a.rs"], "aggregate": true}),
        );
        assert_eq!(rewrite(&mut agreeing).unwrap(), Some("assess_risk_diff"));
    }

    #[test]
    fn feature_bundle_keeps_its_feature_lookup_under_bundle() {
        for arguments in [
            json!({"project": "/p", "target": "feat_1"}),
            json!({"project": "/p", "feature_id": "feat_1"}),
            json!({"project": "/p", "feature_id": "feat_1", "target": "feat_1"}),
        ] {
            let mut request = call("feature_bundle", arguments.clone());
            rewrite(&mut request).unwrap();
            let rewritten = request.arguments.unwrap();
            assert_eq!(rewritten["feature_id"], "feat_1", "{arguments}");
            assert!(rewritten.get("target").is_none(), "{arguments}");
        }
        let mut conflict = call(
            "feature_bundle",
            json!({"project": "/p", "feature_id": "feat_1", "target": "feat_2"}),
        );
        let error = rewrite(&mut conflict).unwrap_err();
        assert_eq!(error.code, ErrorCode::Param);
        assert!(error.to_string().contains("different"), "{error}");
    }

    #[test]
    fn a_field_the_retired_tool_never_took_is_refused() {
        for (name, field) in [
            ("assess_risk_batch", "aggregate"),
            ("list_features", "target"),
            ("find_feature", "kind"),
            ("export_context", "feature_id"),
        ] {
            let mut request = call(name, json!({"project": "/p", field: "x"}));
            let error = rewrite(&mut request).unwrap_err();
            assert_eq!(error.code, ErrorCode::Param);
            assert!(error.to_string().contains(field), "{error}");
        }
    }
}
