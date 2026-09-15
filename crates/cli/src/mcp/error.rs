//! One error contract for every MCP failure: a machine-readable code, the
//! human-readable cause chain, and a remedy that is a tool call or a shell
//! command rather than prose.

use std::fmt;

use codesage_protocol::work::{StopReason, WorkStopped};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    Param,
    ProjectPath,
    NotOnboarded,
    SchemaTooNew,
    NotFound,
    Ambiguous,
    EmptyInput,
    OverCap,
    Model,
    DbBusy,
    Saturated,
    Timeout,
    Cancelled,
    Shutdown,
    Incomplete,
    Internal,
}

impl ErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Param => "E_PARAM",
            Self::ProjectPath => "E_PROJECT_PATH",
            Self::NotOnboarded => "E_NOT_ONBOARDED",
            Self::SchemaTooNew => "E_SCHEMA_TOO_NEW",
            Self::NotFound => "E_NOT_FOUND",
            Self::Ambiguous => "E_AMBIGUOUS",
            Self::EmptyInput => "E_EMPTY_INPUT",
            Self::OverCap => "E_OVER_CAP",
            Self::Model => "E_MODEL",
            Self::DbBusy => "E_DB_BUSY",
            Self::Saturated => "E_SATURATED",
            Self::Timeout => "E_TIMEOUT",
            Self::Cancelled => "E_CANCELLED",
            Self::Shutdown => "E_SHUTDOWN",
            Self::Incomplete => "E_INCOMPLETE",
            Self::Internal => "E_INTERNAL",
        }
    }

    /// Legacy `status` value that existing consumers and diagnostics switch on.
    pub(crate) fn status(self) -> &'static str {
        match self {
            Self::DbBusy => "database-busy",
            Self::Saturated => "saturated",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Shutdown => "shutdown",
            Self::Incomplete => "incomplete",
            _ => "error",
        }
    }

    fn from_stop_reason(reason: StopReason) -> Self {
        match reason {
            StopReason::ClientCancelled
            | StopReason::ConnectionClosed
            | StopReason::NoConsumers => Self::Cancelled,
            StopReason::DeadlineExceeded => Self::Timeout,
            StopReason::Shutdown => Self::Shutdown,
            StopReason::Incomplete => Self::Incomplete,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Remedy {
    /// A shell command the operator runs.
    Command(String),
    /// A complete tool call.
    Call { tool: String, arguments: Value },
    /// The failing tool again, with the original arguments and these
    /// overrides applied. Resolved against the request when rendered.
    Retry { overrides: Map<String, Value> },
}

impl Remedy {
    pub(crate) fn retry() -> Self {
        Self::Retry {
            overrides: Map::new(),
        }
    }

    pub(crate) fn retry_with(field: &str, value: Value) -> Self {
        let mut overrides = Map::new();
        overrides.insert(field.to_owned(), value);
        Self::Retry { overrides }
    }

    fn resolve(self, tool: &str, arguments: Option<&Map<String, Value>>) -> Value {
        match self {
            Self::Command(command) => json!({ "command": command }),
            Self::Call { tool, arguments } => json!({ "tool": tool, "arguments": arguments }),
            Self::Retry { overrides } => {
                let mut merged = arguments.cloned().unwrap_or_default();
                merged.extend(overrides);
                json!({ "tool": tool, "arguments": Value::Object(merged) })
            }
        }
    }
}

/// A failure that already knows its code and remedy. Sites that can name the
/// cause construct one; everything else is classified from the anyhow chain.
#[derive(Debug)]
pub(crate) struct McpError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    pub(crate) remedy: Option<Remedy>,
    source: Option<anyhow::Error>,
}

impl McpError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            remedy: None,
            source: None,
        }
    }

    pub(crate) fn remedy(mut self, remedy: Remedy) -> Self {
        self.remedy = Some(remedy);
        self
    }

    pub(crate) fn command(self, command: impl Into<String>) -> Self {
        self.remedy(Remedy::Command(command.into()))
    }

    pub(crate) fn source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source);
        self
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error.as_ref() as &(dyn std::error::Error + 'static))
    }
}

pub(crate) const ONBOARD_COMMAND: &str = "codesage init && codesage index";
pub(crate) const DOCTOR_COMMAND: &str = "codesage doctor";
pub(crate) const REINDEX_FULL_COMMAND: &str = "codesage index --full";
pub(crate) const UPGRADE_COMMAND: &str = "cargo build --release -p codesage";
const SCHEMA_TOO_NEW_MARKER: &str = "migrated by a newer codesage";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Classified {
    pub(crate) code: ErrorCode,
    pub(crate) remedy: Option<Remedy>,
}

/// Walk the cause chain outermost-first; the first recognized cause decides.
pub(crate) fn classify(error: &anyhow::Error) -> Classified {
    for cause in error.chain() {
        if let Some(typed) = cause.downcast_ref::<McpError>() {
            return Classified {
                code: typed.code,
                remedy: typed.remedy.clone(),
            };
        }
        if let Some(admission) = cause.downcast_ref::<super::work::AdmissionError>() {
            use super::work::AdmissionError;
            return match admission {
                AdmissionError::Saturated => Classified {
                    code: ErrorCode::Saturated,
                    remedy: Some(Remedy::retry()),
                },
                AdmissionError::Shutdown => Classified {
                    code: ErrorCode::Shutdown,
                    remedy: None,
                },
                AdmissionError::Stopped(reason) => stopped(*reason),
                AdmissionError::InvalidLimits | AdmissionError::ProjectAlreadyAttached => {
                    Classified {
                        code: ErrorCode::Internal,
                        remedy: None,
                    }
                }
            };
        }
        if let Some(work) = cause.downcast_ref::<WorkStopped>() {
            return stopped(work.reason);
        }
        if cause.is::<codesage_graph::IncompleteRiskRanking>() {
            return Classified {
                code: ErrorCode::Incomplete,
                remedy: None,
            };
        }
        if let Some(ambiguous) = cause.downcast_ref::<codesage_graph::AmbiguousSymbol>() {
            return Classified {
                code: ErrorCode::Ambiguous,
                remedy: ambiguous.candidates.first().map(|candidate| {
                    let mut overrides = Map::new();
                    overrides.insert("target".to_owned(), json!(candidate));
                    overrides.insert("is_file".to_owned(), json!(false));
                    Remedy::Retry { overrides }
                }),
            };
        }
        if cause.is::<codesage_graph::StaleSemanticTable>() {
            return Classified {
                code: ErrorCode::Model,
                remedy: Some(Remedy::Command(REINDEX_FULL_COMMAND.to_owned())),
            };
        }
        if let Some(rusqlite::Error::SqliteFailure(code, message)) =
            cause.downcast_ref::<rusqlite::Error>()
        {
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) {
                return Classified {
                    code: ErrorCode::DbBusy,
                    remedy: Some(Remedy::retry()),
                };
            }
            if message
                .as_deref()
                .is_some_and(|text| text.contains(SCHEMA_TOO_NEW_MARKER))
            {
                return Classified {
                    code: ErrorCode::SchemaTooNew,
                    remedy: Some(Remedy::Command(UPGRADE_COMMAND.to_owned())),
                };
            }
        }
        if cause.is::<serde_json::Error>() {
            return Classified {
                code: ErrorCode::Param,
                remedy: None,
            };
        }
    }
    Classified {
        code: ErrorCode::Internal,
        remedy: None,
    }
}

fn stopped(reason: StopReason) -> Classified {
    let code = ErrorCode::from_stop_reason(reason);
    Classified {
        code,
        remedy: (code == ErrorCode::Timeout).then(Remedy::retry),
    }
}

/// Legacy status for diagnostics counters; `None` for the plain `error` bucket.
pub(crate) fn legacy_status(error: &anyhow::Error) -> Option<&'static str> {
    let status = classify(error).code.status();
    (status != "error").then_some(status)
}

/// The single constructor for failed tool results: a readable cause chain
/// followed by the contract block. Dispatch later merges request metadata
/// into that block; see `dispatch::normalize_error`.
pub(crate) fn render_error(
    tool: &str,
    arguments: Option<&Map<String, Value>>,
    error: &anyhow::Error,
) -> CallToolResult {
    let Classified { code, remedy } = classify(error);
    let message = format!("{error:#}");
    let block = json!({
        "tool": tool,
        "error": {
            "code": code.as_str(),
            "message": message,
            "remedy": remedy.map_or(Value::Null, |remedy| remedy.resolve(tool, arguments)),
        },
        "status": code.status(),
        "complete": false,
        "next": null,
    });
    CallToolResult::error(vec![
        ContentBlock::text(message),
        ContentBlock::text(block.to_string()),
    ])
}

/// Render a failure that never became an `anyhow::Error`.
pub(crate) fn render_mcp_error(
    tool: &str,
    arguments: Option<&Map<String, Value>>,
    error: McpError,
) -> CallToolResult {
    render_error(tool, arguments, &anyhow::Error::new(error))
}

/// The contract block of a failed result, if it carries one.
pub(crate) fn contract_block(result: &CallToolResult) -> Option<Map<String, Value>> {
    result.content.iter().find_map(|block| {
        let text = block.as_text()?;
        let value = serde_json::from_str::<Value>(&text.text).ok()?;
        let object = value.as_object()?;
        object.contains_key("status").then(|| object.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::work::AdmissionError;

    fn block(result: &CallToolResult) -> Map<String, Value> {
        contract_block(result).expect("contract block")
    }

    #[test]
    fn admission_errors_classify_with_retry_only_for_saturation() {
        let saturated = classify(&anyhow::Error::new(AdmissionError::Saturated));
        assert_eq!(saturated.code, ErrorCode::Saturated);
        assert_eq!(saturated.remedy, Some(Remedy::retry()));
        assert_eq!(saturated.code.status(), "saturated");

        let shutdown = classify(&anyhow::Error::new(AdmissionError::Shutdown));
        assert_eq!(shutdown.code, ErrorCode::Shutdown);
        assert_eq!(shutdown.remedy, None);

        let stopped = classify(&anyhow::Error::new(AdmissionError::Stopped(
            StopReason::DeadlineExceeded,
        )));
        assert_eq!(stopped.code, ErrorCode::Timeout);
        assert_eq!(stopped.remedy, Some(Remedy::retry()));

        let internal = classify(&anyhow::Error::new(AdmissionError::InvalidLimits));
        assert_eq!(internal.code, ErrorCode::Internal);
    }

    #[test]
    fn stop_reasons_map_onto_codes_and_legacy_statuses() {
        for (reason, code, status) in [
            (
                StopReason::ClientCancelled,
                ErrorCode::Cancelled,
                "cancelled",
            ),
            (
                StopReason::ConnectionClosed,
                ErrorCode::Cancelled,
                "cancelled",
            ),
            (StopReason::NoConsumers, ErrorCode::Cancelled, "cancelled"),
            (StopReason::DeadlineExceeded, ErrorCode::Timeout, "timeout"),
            (StopReason::Shutdown, ErrorCode::Shutdown, "shutdown"),
            (StopReason::Incomplete, ErrorCode::Incomplete, "incomplete"),
        ] {
            let classified = classify(&anyhow::Error::new(WorkStopped { reason }));
            assert_eq!(classified.code, code, "{reason:?}");
            assert_eq!(classified.code.status(), status, "{reason:?}");
            assert_eq!(
                legacy_status(&anyhow::Error::new(WorkStopped { reason })),
                Some(status)
            );
        }
    }

    #[test]
    fn sqlite_busy_and_locked_classify_as_db_busy_through_context() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
            let error = anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
            .context("opening index");
            let classified = classify(&error);
            assert_eq!(classified.code, ErrorCode::DbBusy);
            assert_eq!(classified.remedy, Some(Remedy::retry()));
            assert_eq!(legacy_status(&error), Some("database-busy"));
        }
    }

    #[test]
    fn schema_too_new_is_recognized_from_the_storage_refusal() {
        let error = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some("index was migrated by a newer codesage (breaking migration \"0099_x\")".into()),
        ));
        let classified = classify(&error);
        assert_eq!(classified.code, ErrorCode::SchemaTooNew);
        assert_eq!(
            classified.remedy,
            Some(Remedy::Command(UPGRADE_COMMAND.to_owned()))
        );
        assert_eq!(legacy_status(&error), None);
    }

    #[test]
    fn typed_error_wins_over_inner_causes_and_keeps_the_chain() {
        let inner = anyhow::anyhow!("not on the allowlist").context("resolving model files");
        let error = anyhow::Error::new(
            McpError::new(ErrorCode::Model, "embedding model unavailable")
                .command(DOCTOR_COMMAND)
                .source(inner),
        );
        let classified = classify(&error);
        assert_eq!(classified.code, ErrorCode::Model);
        assert_eq!(
            classified.remedy,
            Some(Remedy::Command(DOCTOR_COMMAND.to_owned()))
        );
        let text = format!("{error:#}");
        assert!(text.contains("embedding model unavailable"), "{text}");
        assert!(text.contains("resolving model files"), "{text}");
        assert!(text.contains("not on the allowlist"), "{text}");
    }

    #[test]
    fn unknown_causes_fall_back_to_internal() {
        let classified = classify(&anyhow::anyhow!("boom"));
        assert_eq!(classified.code, ErrorCode::Internal);
        assert_eq!(classified.remedy, None);
        assert_eq!(legacy_status(&anyhow::anyhow!("boom")), None);
    }

    #[test]
    fn render_resolves_retry_against_the_request_arguments() {
        let mut arguments = Map::new();
        arguments.insert("project".into(), json!("/p"));
        arguments.insert("target".into(), json!("search"));
        let error = anyhow::Error::new(codesage_graph::AmbiguousSymbol {
            name: "search".into(),
            definitions: 3,
            candidates: vec!["a::search".into(), "b::search".into()],
        });
        let result = render_error("impact_analysis", Some(&arguments), &error);
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
        let block = block(&result);
        assert_eq!(block["tool"], "impact_analysis");
        assert_eq!(block["error"]["code"], "E_AMBIGUOUS");
        assert_eq!(block["status"], "error");
        assert_eq!(block["complete"], false);
        assert_eq!(block["next"], Value::Null);
        assert_eq!(
            block["error"]["remedy"],
            json!({"tool": "impact_analysis", "arguments": {
                "project": "/p", "target": "a::search", "is_file": false
            }})
        );
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("ambiguous symbol 'search'"), "{text}");
        assert_eq!(block["error"]["message"], text);
    }

    #[test]
    fn render_without_remedy_emits_null_and_legacy_status() {
        let result = render_mcp_error(
            "find_symbol",
            None,
            McpError::new(ErrorCode::ProjectPath, "`project` must be absolute"),
        );
        let block = block(&result);
        assert_eq!(block["error"]["code"], "E_PROJECT_PATH");
        assert_eq!(block["error"]["remedy"], Value::Null);
        assert_eq!(block["status"], "error");

        let busy = render_error(
            "search",
            None,
            &anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            )),
        );
        let busy_block = self::block(&busy);
        assert_eq!(busy_block["status"], "database-busy");
        assert_eq!(busy_block["error"]["code"], "E_DB_BUSY");
        assert_eq!(
            busy_block["error"]["remedy"],
            json!({"tool": "search", "arguments": {}})
        );
    }
}
