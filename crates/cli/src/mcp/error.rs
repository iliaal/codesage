//! One error contract for every MCP failure: a machine-readable code, the
//! human-readable cause chain, and a remedy that is a tool call or a shell
//! command rather than prose.

use std::fmt;

use codesage_graph::TargetError;
use codesage_graph::edit_check::EditCheckRefusal;
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
    /// A shell command the operator runs, optionally in `cwd`. The directory is
    /// a separate field rather than interpolated so a path can never become
    /// shell syntax.
    Command {
        command: String,
        cwd: Option<String>,
    },
    /// A complete tool call.
    Call { tool: String, arguments: Value },
    /// The failing tool again, with the original arguments and these
    /// overrides applied. Resolved against the request when rendered.
    Retry { overrides: Map<String, Value> },
    /// The failing tool again, with whichever argument carried `input`
    /// replaced by `value`. The tools spell the same target `target`, `name`,
    /// `symbol_name`, `file_path`, `from`, `to`, and `feature_id`, and only
    /// the failing input says which one was meant. Only string arguments are
    /// rewritten: an entry of `targets` / `file_paths` would need the list
    /// rebuilt, which is unimplemented and today unreachable, since the
    /// file-set tools pass an unmatched path through instead of refusing.
    ReplaceInput { input: String, value: String },
}

impl Remedy {
    pub(crate) fn retry() -> Self {
        Self::Retry {
            overrides: Map::new(),
        }
    }

    pub(crate) fn retry_with(field: &str, value: Value) -> Self {
        Self::retry_with_all(&[field], value)
    }

    /// One value written to every field the caller supplied, so a retry that
    /// rewrites an argument with an accepted alias leaves the two agreeing.
    pub(crate) fn retry_with_all(fields: &[&str], value: Value) -> Self {
        let mut overrides = Map::new();
        for field in fields {
            overrides.insert((*field).to_owned(), value.clone());
        }
        Self::Retry { overrides }
    }

    pub(crate) fn command(command: &str) -> Self {
        Self::Command {
            command: command.to_owned(),
            cwd: None,
        }
    }

    pub(crate) fn command_in(command: &str, cwd: impl Into<String>) -> Self {
        Self::Command {
            command: command.to_owned(),
            cwd: Some(cwd.into()),
        }
    }

    fn resolve(self, tool: &str, arguments: Option<&Map<String, Value>>) -> Value {
        match self {
            Self::Command { command, cwd: None } => json!({ "command": command }),
            Self::Command {
                command,
                cwd: Some(cwd),
            } => json!({ "command": command, "cwd": cwd }),
            Self::Call { tool, arguments } => json!({ "tool": tool, "arguments": arguments }),
            Self::Retry { overrides } => {
                let mut merged = arguments.cloned().unwrap_or_default();
                merged.extend(overrides);
                json!({ "tool": tool, "arguments": Value::Object(merged) })
            }
            Self::ReplaceInput { input, value } => {
                let mut merged = arguments.cloned().unwrap_or_default();
                let mut replaced = false;
                // The resolver reports the trimmed input; the argument that
                // carried it may still have its padding.
                let input = input.trim();
                for argument in merged.values_mut() {
                    if argument
                        .as_str()
                        .is_some_and(|carried| carried == input || carried.trim() == input)
                    {
                        *argument = json!(value);
                        replaced = true;
                    }
                }
                if !replaced {
                    // No argument carried the input, so there is no call to
                    // rewrite; the candidates block still names the handles.
                    return Value::Null;
                }
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

    pub(crate) fn command(self, command: &str) -> Self {
        self.remedy(Remedy::command(command))
    }

    pub(crate) fn source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source);
        self
    }

    fn source_error(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(self)
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

/// Work-control stops and admission refusals anywhere in the chain outrank
/// code-level wrappers: a model load cancelled at a checkpoint is a
/// cancellation, not a model failure. Otherwise the outermost recognized cause
/// decides, so `IncompleteRiskRanking` over a busy database stays incomplete
/// while a typed wrapper over a busy database reports the contention.
pub(crate) fn classify(error: &anyhow::Error) -> Classified {
    if let Some(classified) = error.chain().find_map(classify_interruption) {
        return classified;
    }
    for cause in error.chain() {
        if let Some(typed) = cause.downcast_ref::<McpError>() {
            let beneath = std::iter::successors(typed.source_error(), |e| e.source());
            if let Some(busy) = beneath.into_iter().find_map(classify_busy) {
                return busy;
            }
            return Classified {
                code: typed.code,
                remedy: typed.remedy.clone(),
            };
        }
        if let Some(busy) = classify_busy(cause) {
            return busy;
        }
        if cause.is::<codesage_graph::IncompleteRiskRanking>() {
            return Classified {
                code: ErrorCode::Incomplete,
                remedy: None,
            };
        }
        if let Some(target) = cause.downcast_ref::<TargetError>() {
            return match target {
                TargetError::Ambiguous {
                    input, candidates, ..
                } => Classified {
                    code: ErrorCode::Ambiguous,
                    remedy: candidates.first().map(|candidate| Remedy::ReplaceInput {
                        input: input.clone(),
                        value: candidate.handle.clone(),
                    }),
                },
                // Nearest candidates ride in the `candidates` block; none of
                // them is a retry the agent should make blind.
                TargetError::NotFound { .. } => Classified {
                    code: ErrorCode::NotFound,
                    remedy: None,
                },
                // The message names the kinds the tool takes; no index state
                // would make the same call succeed.
                TargetError::Unsupported { .. } => Classified {
                    code: ErrorCode::Param,
                    remedy: None,
                },
            };
        }
        if cause.is::<codesage_graph::StaleSemanticTable>() {
            return Classified {
                code: ErrorCode::Model,
                remedy: Some(Remedy::command(REINDEX_FULL_COMMAND)),
            };
        }
        if let Some(refusal) = cause.downcast_ref::<EditCheckRefusal>() {
            return match refusal {
                EditCheckRefusal::ProjectPath(_) => Classified {
                    code: ErrorCode::ProjectPath,
                    remedy: None,
                },
                EditCheckRefusal::Param(_) => Classified {
                    code: ErrorCode::Param,
                    remedy: None,
                },
                EditCheckRefusal::OverCap(_) => Classified {
                    code: ErrorCode::OverCap,
                    remedy: None,
                },
                EditCheckRefusal::NotFound(_) => Classified {
                    code: ErrorCode::NotFound,
                    remedy: None,
                },
                EditCheckRefusal::Incomplete(_) => Classified {
                    code: ErrorCode::Incomplete,
                    remedy: None,
                },
                EditCheckRefusal::Ambiguous { lines, .. } => Classified {
                    code: ErrorCode::Ambiguous,
                    remedy: lines
                        .first()
                        .map(|line| Remedy::retry_with("line", json!(line))),
                },
            };
        }
        if let Some(rusqlite::Error::SqliteFailure(_, Some(message))) =
            cause.downcast_ref::<rusqlite::Error>()
            && message.contains(SCHEMA_TOO_NEW_MARKER)
        {
            return Classified {
                code: ErrorCode::SchemaTooNew,
                remedy: Some(Remedy::command(UPGRADE_COMMAND)),
            };
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

fn classify_interruption(cause: &(dyn std::error::Error + 'static)) -> Option<Classified> {
    if let Some(admission) = cause.downcast_ref::<super::work::AdmissionError>() {
        use super::work::AdmissionError;
        return match admission {
            AdmissionError::Saturated => Some(Classified {
                code: ErrorCode::Saturated,
                remedy: Some(Remedy::retry()),
            }),
            AdmissionError::Shutdown => Some(Classified {
                code: ErrorCode::Shutdown,
                remedy: None,
            }),
            AdmissionError::Stopped(reason) => Some(stopped(*reason)),
            // Programming errors, not interruptions; let the outer pass decide.
            AdmissionError::InvalidLimits | AdmissionError::ProjectAlreadyAttached => None,
        };
    }
    if let Some(work) = cause.downcast_ref::<WorkStopped>() {
        return Some(stopped(work.reason));
    }
    None
}

fn classify_busy(cause: &(dyn std::error::Error + 'static)) -> Option<Classified> {
    let rusqlite::Error::SqliteFailure(code, _) = cause.downcast_ref::<rusqlite::Error>()? else {
        return None;
    };
    matches!(
        code.code,
        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
    )
    .then(|| Classified {
        code: ErrorCode::DbBusy,
        remedy: Some(Remedy::retry()),
    })
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
    let mut failure = json!({
        "code": code.as_str(),
        "message": message,
        "remedy": remedy.map_or(Value::Null, |remedy| remedy.resolve(tool, arguments)),
    });
    let candidates = target_candidates(error);
    if !candidates.is_empty()
        && let Some(failure) = failure.as_object_mut()
    {
        failure.insert("candidates".to_owned(), json!(candidates));
    }
    let block = json!({
        "tool": tool,
        "error": failure,
        "status": code.status(),
        "complete": false,
        "next": null,
    });
    CallToolResult::error(vec![
        ContentBlock::text(message),
        ContentBlock::text(block.to_string()),
    ])
}

/// Handles for every entity an ambiguous target named, or the nearest
/// candidates a missing one left behind. Empty for every other failure.
fn target_candidates(error: &anyhow::Error) -> Vec<String> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TargetError>())
        .map(TargetError::handles)
        .or_else(|| {
            error
                .chain()
                .find_map(|cause| match cause.downcast_ref::<EditCheckRefusal>() {
                    Some(EditCheckRefusal::Ambiguous { handles, .. }) => Some(handles.clone()),
                    _ => None,
                })
        })
        .unwrap_or_default()
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
        assert_eq!(classified.remedy, Some(Remedy::command(UPGRADE_COMMAND)));
        assert_eq!(legacy_status(&error), None);
    }

    #[test]
    fn interruptions_beneath_a_typed_wrapper_outrank_it() {
        let wrap = |inner: anyhow::Error| {
            anyhow::Error::new(
                McpError::new(ErrorCode::Model, "embedding model unavailable")
                    .command(DOCTOR_COMMAND)
                    .source(inner.context("loading embedding model")),
            )
        };
        let timeout = wrap(anyhow::Error::new(WorkStopped {
            reason: StopReason::DeadlineExceeded,
        }));
        let classified = classify(&timeout);
        assert_eq!(classified.code, ErrorCode::Timeout);
        assert_eq!(classified.remedy, Some(Remedy::retry()));
        assert_eq!(legacy_status(&timeout), Some("timeout"));

        let saturated = wrap(anyhow::Error::new(AdmissionError::Saturated));
        assert_eq!(classify(&saturated).code, ErrorCode::Saturated);
        assert_eq!(legacy_status(&saturated), Some("saturated"));

        let busy = wrap(anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )));
        assert_eq!(classify(&busy).code, ErrorCode::DbBusy);
        assert_eq!(legacy_status(&busy), Some("database-busy"));

        let cancelled = wrap(anyhow::Error::new(WorkStopped {
            reason: StopReason::ClientCancelled,
        }));
        assert_eq!(classify(&cancelled).code, ErrorCode::Cancelled);
        assert_eq!(legacy_status(&cancelled), Some("cancelled"));

        let internal = wrap(anyhow::Error::new(AdmissionError::InvalidLimits));
        assert_eq!(
            classify(&internal).code,
            ErrorCode::Model,
            "a programming-error admission variant must not pre-empt the outer pass"
        );
    }

    #[test]
    fn incomplete_ranking_over_a_busy_database_stays_incomplete() {
        let busy = || {
            anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ))
        };
        let incomplete = anyhow::Error::new(codesage_graph::IncompleteRiskRanking::new(busy()));
        assert!(
            incomplete
                .chain()
                .any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some()),
            "fixture must expose the busy cause through source()"
        );
        assert_eq!(classify(&incomplete).code, ErrorCode::Incomplete);
        assert_eq!(legacy_status(&incomplete), Some("incomplete"));

        let bare = busy();
        assert_eq!(classify(&bare).code, ErrorCode::DbBusy);
        assert_eq!(legacy_status(&bare), Some("database-busy"));
    }

    #[test]
    fn command_remedies_keep_the_directory_out_of_the_command_string() {
        let hostile = "/tmp/a'; echo INJECTED; :'b";
        let result = render_mcp_error(
            "find_symbol",
            None,
            McpError::new(ErrorCode::NotOnboarded, "not onboarded")
                .remedy(Remedy::command_in(ONBOARD_COMMAND, hostile)),
        );
        let remedy = &block(&result)["error"]["remedy"];
        assert_eq!(remedy["command"], ONBOARD_COMMAND);
        assert_eq!(remedy["cwd"], hostile);
        assert!(!remedy["command"].as_str().unwrap().contains("/tmp"));

        let plain = render_mcp_error(
            "search",
            None,
            McpError::new(ErrorCode::Model, "model").command(DOCTOR_COMMAND),
        );
        assert_eq!(
            block(&plain)["error"]["remedy"],
            json!({ "command": DOCTOR_COMMAND })
        );
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
        assert_eq!(classified.remedy, Some(Remedy::command(DOCTOR_COMMAND)));
        let text = format!("{error:#}");
        assert!(text.contains("embedding model unavailable"), "{text}");
        assert!(text.contains("resolving model files"), "{text}");
        assert!(text.contains("not on the allowlist"), "{text}");
    }

    #[test]
    fn edit_check_refusals_classify_by_variant_through_context() {
        let cases = [
            (
                EditCheckRefusal::ProjectPath("project must be absolute".into()),
                ErrorCode::ProjectPath,
                None,
            ),
            (
                EditCheckRefusal::Param("file must be repository-relative".into()),
                ErrorCode::Param,
                None,
            ),
            (
                EditCheckRefusal::OverCap("replacement exceeds 1 MiB".into()),
                ErrorCode::OverCap,
                None,
            ),
            (
                EditCheckRefusal::NotFound("no declaration named 'f'".into()),
                ErrorCode::NotFound,
                None,
            ),
            (
                EditCheckRefusal::Ambiguous {
                    message: "found 2".into(),
                    lines: vec![3, 9],
                    handles: vec!["sym:a.rs#f@3".into(), "sym:a.rs#f@9".into()],
                },
                ErrorCode::Ambiguous,
                Some(Remedy::retry_with("line", json!(3))),
            ),
        ];
        for (refusal, code, remedy) in cases {
            let error = anyhow::Error::new(refusal.clone()).context("checking edit");
            let classified = classify(&error);
            assert_eq!(classified.code, code, "{refusal:?}");
            assert_eq!(classified.remedy, remedy, "{refusal:?}");
            assert_eq!(legacy_status(&error), None, "{refusal:?}");
        }
    }

    #[test]
    fn edit_check_unparsable_head_source_is_incomplete_not_internal() {
        let error = anyhow::Error::new(EditCheckRefusal::Incomplete(
            "HEAD source has syntax errors; compatibility is unknown".into(),
        ))
        .context("checking edit");
        let classified = classify(&error);
        assert_eq!(classified.code, ErrorCode::Incomplete);
        assert_eq!(classified.remedy, None);
        assert_eq!(legacy_status(&error), Some("incomplete"));
    }

    #[test]
    fn unknown_causes_fall_back_to_internal() {
        let classified = classify(&anyhow::anyhow!("boom"));
        assert_eq!(classified.code, ErrorCode::Internal);
        assert_eq!(classified.remedy, None);
        assert_eq!(legacy_status(&anyhow::anyhow!("boom")), None);
    }

    fn ambiguous_search() -> anyhow::Error {
        anyhow::Error::new(TargetError::Ambiguous {
            input: "search".into(),
            candidates: vec![
                candidate("sym:a.rs#search"),
                candidate("sym:crates/b.rs#Server::search"),
            ],
            candidates_total: 3,
            overloads: 0,
        })
    }

    fn candidate(handle: &str) -> codesage_protocol::ResolvedTarget {
        codesage_protocol::ResolvedTarget {
            handle: handle.into(),
            kind: "function".into(),
            path: None,
            line_start: None,
            line_end: None,
            is_test: false,
            confidence: 0.9,
            via: codesage_protocol::ResolveVia::Unique,
        }
    }

    #[test]
    fn render_resolves_retry_against_the_request_arguments() {
        let mut arguments = Map::new();
        arguments.insert("project".into(), json!("/p"));
        arguments.insert("target".into(), json!("search"));
        let result = render_error("impact_analysis", Some(&arguments), &ambiguous_search());
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
                "project": "/p", "target": "sym:a.rs#search"
            }})
        );
        assert_eq!(
            block["error"]["candidates"],
            json!(["sym:a.rs#search", "sym:crates/b.rs#Server::search"])
        );
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("ambiguous target 'search'"), "{text}");
        assert_eq!(block["error"]["message"], text);
    }

    #[test]
    fn the_retried_argument_is_the_one_that_carried_the_ambiguous_input() {
        let mut arguments = Map::new();
        arguments.insert("project".into(), json!("/p"));
        arguments.insert("from".into(), json!("main"));
        arguments.insert("to".into(), json!("search"));
        let block = block(&render_error(
            "trace_call_path",
            Some(&arguments),
            &ambiguous_search(),
        ));
        assert_eq!(
            block["error"]["remedy"],
            json!({"tool": "trace_call_path", "arguments": {
                "project": "/p", "from": "main", "to": "sym:a.rs#search"
            }}),
            "only the endpoint that was ambiguous may be rewritten"
        );
    }

    #[test]
    fn a_padded_argument_is_still_the_one_that_gets_replaced() {
        let mut arguments = Map::new();
        arguments.insert("project".into(), json!("/p"));
        arguments.insert("target".into(), json!("  search\n"));
        let block = block(&render_error(
            "impact_analysis",
            Some(&arguments),
            &ambiguous_search(),
        ));
        assert_eq!(
            block["error"]["remedy"]["arguments"],
            json!({ "project": "/p", "target": "sym:a.rs#search" }),
            "the resolver trims; the argument must still be matched"
        );
    }

    #[test]
    fn no_argument_carrying_the_input_means_no_remedy() {
        let mut arguments = Map::new();
        arguments.insert("project".into(), json!("/p"));
        arguments.insert("symbol".into(), json!("other"));
        let block = block(&render_error(
            "export_context",
            Some(&arguments),
            &ambiguous_search(),
        ));
        assert_eq!(block["error"]["code"], "E_AMBIGUOUS");
        assert_eq!(
            block["error"]["remedy"],
            Value::Null,
            "a `target` argument no tool but impact_analysis accepts must not be invented"
        );
        assert_eq!(
            block["error"]["candidates"],
            json!(["sym:a.rs#search", "sym:crates/b.rs#Server::search"])
        );
    }

    #[test]
    fn an_unsupported_target_kind_is_a_parameter_error_without_candidates() {
        let error = anyhow::Error::new(TargetError::Unsupported {
            input: "dir:src".into(),
            kind: codesage_protocol::TargetKind::Dir,
            accepted: "a `sym:` or `file:` handle",
        })
        .context("analyzing impact");
        let classified = classify(&error);
        assert_eq!(classified.code, ErrorCode::Param);
        assert_eq!(classified.remedy, None);
        let block = block(&render_error("impact_analysis", None, &error));
        assert_eq!(block["error"]["code"], "E_PARAM");
        assert_eq!(block["error"]["remedy"], Value::Null);
        assert!(block["error"].get("candidates").is_none(), "{block:?}");
        let message = block["error"]["message"].as_str().unwrap();
        assert!(message.contains("dir:src"), "{message}");
        assert!(message.contains("`sym:` or `file:`"), "{message}");
    }

    #[test]
    fn a_missing_target_carries_its_nearest_candidates_without_a_blind_retry() {
        let error = anyhow::Error::new(TargetError::NotFound {
            input: "serch".into(),
            nearest: vec![candidate("sym:a.rs#search")],
        })
        .context("analyzing impact");
        let classified = classify(&error);
        assert_eq!(classified.code, ErrorCode::NotFound);
        assert_eq!(classified.remedy, None);
        let block = block(&render_error("impact_analysis", None, &error));
        assert_eq!(block["error"]["code"], "E_NOT_FOUND");
        assert_eq!(block["error"]["remedy"], Value::Null);
        assert_eq!(block["error"]["candidates"], json!(["sym:a.rs#search"]));
    }

    #[test]
    fn an_ordinary_failure_carries_no_candidates_block() {
        let block = block(&render_error("find_symbol", None, &anyhow::anyhow!("boom")));
        assert!(block["error"].get("candidates").is_none(), "{block:?}");
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
