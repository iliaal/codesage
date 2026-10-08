//! Reuse the running daemon's embedding session to avoid duplicate model memory.
//! If no matching daemon is running, the CLI embeds privately without starting one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use codesage_graph::TextEmbedder;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientRequest, ServerResult};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};

use crate::mcp::params::EmbedTextsResult;
use crate::mcp::{
    EMBED_TEXTS_FINGERPRINT_MISMATCH, EMBED_TEXTS_OVER_CAP, MAX_MCP_EMBED_TEXT_BYTES,
    MAX_MCP_EMBED_TEXTS, MAX_MCP_EMBED_TOTAL_BYTES,
};

/// Texts per daemon call. The daemon admits one native call at a time and
/// rotates between projects per call, so short calls let concurrent index
/// runs take turns instead of queueing behind a 4,096-text request.
const DAEMON_CALL_MAX_TEXTS: usize = 256;
/// Bytes per daemon call, for the same reason.
const DAEMON_CALL_MAX_BYTES: usize = 2 * 1024 * 1024;

type PrivateEmbedderInit = Box<dyn FnOnce() -> Result<Box<dyn TextEmbedder>> + Send>;

/// Allow a cold model load during the handshake and dimension probe.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound each batch so a wedged daemon cannot indefinitely hold the index lock.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);
/// Retries of one indexing batch after the daemon timed out, was saturated,
/// or cancelled it, before the batch falls back to a private embedder.
const DAEMON_RETRIES: u32 = 2;
/// Wall time one batch may spend across the daemon attempts, so retries
/// cannot hold the index lock much past a single timeout.
const DAEMON_RETRY_BUDGET: Duration = Duration::from_secs(240);
/// Backoff before retry `n` is `n` times this.
const DAEMON_RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// A retry with less time than this left would only time out again.
const MIN_RETRY_ATTEMPT: Duration = Duration::from_secs(10);
/// Consecutive batches that may exhaust their retries on a saturated daemon
/// before the rest of the run embeds privately.
const BUSY_BATCHES_BEFORE_BYPASS: u32 = 2;

/// The transient kind of a daemon error code; other codes are not retried.
fn transient_code(code: &str) -> Option<TransientKind> {
    match code {
        "E_TIMEOUT" | "E_CANCELLED" => Some(TransientKind::Timeout),
        "E_SATURATED" => Some(TransientKind::Busy),
        _ => None,
    }
}

/// Why a daemon attempt failed without a model or input fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransientKind {
    /// The call ran out of time, stalled, or was cancelled.
    Timeout,
    /// The daemon refused new work as saturated.
    Busy,
}

/// A daemon attempt that failed for want of time or capacity; the same batch
/// may succeed on a retry. `reopen_session` means a retry must use a fresh
/// client session.
#[derive(Debug)]
struct TransientFailure {
    reason: &'static str,
    kind: TransientKind,
    reopen_session: bool,
}

impl std::fmt::Display for TransientFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for TransientFailure {}

fn transient_failure(error: &anyhow::Error) -> Option<&TransientFailure> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TransientFailure>())
}

/// The `error.code` of a failed tool result's contract block, if any.
fn daemon_error_code(result: &rmcp::model::CallToolResult) -> Option<String> {
    result.content.iter().find_map(|block| {
        let text = block.as_text()?;
        let value: serde_json::Value = serde_json::from_str(&text.text).ok()?;
        value["error"]["code"].as_str().map(str::to_owned)
    })
}

/// How long daemon attempts may take; tests shorten them.
#[derive(Clone, Copy)]
struct RetryPolicy {
    attempt_timeout: Duration,
    retries: u32,
    budget: Duration,
    backoff: Duration,
    min_attempt: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempt_timeout: EMBED_TIMEOUT,
            retries: DAEMON_RETRIES,
            budget: DAEMON_RETRY_BUDGET,
            backoff: DAEMON_RETRY_BACKOFF,
            min_attempt: MIN_RETRY_ATTEMPT,
        }
    }
}

pub(crate) struct DaemonEmbedder {
    rt: tokio::runtime::Runtime,
    client: RunningService<RoleClient, ()>,
    socket: PathBuf,
    retry: RetryPolicy,
    project: String,
    model: String,
    dim: usize,
    /// Vector identity reported by the daemon's probe.
    daemon_fingerprint: String,
    /// Bind every daemon request and private fallback to the pass's attested identity.
    expected_fingerprint: Option<codesage_graph::SemanticFingerprint>,
    /// Lazily constructed fallback for oversized texts and daemon failures.
    private_init: Option<PrivateEmbedderInit>,
    private: Option<Box<dyn TextEmbedder>>,
    /// The client session was torn down within the current batch's
    /// attempts; reopen it before the next retry.
    session_lost: bool,
    /// Consecutive batches that exhausted their retries on a saturated daemon.
    busy_batches: u32,
    /// The daemon kept timing out or stayed saturated; embed the rest privately.
    bypass_daemon: bool,
}

impl DaemonEmbedder {
    /// Borrow an existing daemon session; failed connections select private embedding.
    /// `config` also seeds the fallback for oversized texts and failed batches.
    pub(crate) fn connect(
        root: &Path,
        config: &codesage_embed::config::EmbeddingConfig,
    ) -> Option<Self> {
        let socket = crate::daemon::running_daemon_socket()?;
        let Some(project) = root.to_str() else {
            tracing::debug!(
                root = %root.display(),
                "project root is not UTF-8; embedding privately"
            );
            return None;
        };
        let model = config.model.as_str();
        match Self::connect_to(&socket, project, model) {
            Ok(embedder) => {
                tracing::info!(
                    socket = %socket.display(),
                    model,
                    dim = embedder.dim,
                    "embedding through the running daemon"
                );
                let private_config = config.clone();
                let private_root = root.to_path_buf();
                Some(embedder.with_private_fallback(Box::new(move || {
                    codesage_embed::model::ModelAuthorization::for_project(&private_root).scope(
                        || {
                            let embedder = codesage_embed::model::Embedder::new(&private_config)
                                .context(
                                    "loading a private embedder for texts the daemon refused",
                                )?;
                            Ok(Box::new(embedder) as Box<dyn TextEmbedder>)
                        },
                    )
                })))
            }
            Err(e) => {
                tracing::warn!(
                    socket = %socket.display(),
                    error = %format!("{e:#}"),
                    "daemon cannot embed for this run; embedding privately"
                );
                None
            }
        }
    }

    /// Connect to `socket`, complete the MCP handshake, and probe the daemon
    /// for `model`'s dimension with an empty text list.
    pub(crate) fn connect_to(socket: &Path, project: &str, model: &str) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building the daemon client runtime")?;
        let client = open_session(&rt, socket, CONNECT_TIMEOUT)?;
        let mut this = Self {
            rt,
            client,
            socket: socket.to_path_buf(),
            retry: RetryPolicy::default(),
            project: project.to_string(),
            model: model.to_string(),
            dim: 0,
            daemon_fingerprint: String::new(),
            expected_fingerprint: None,
            private_init: None,
            private: None,
            session_lost: false,
            busy_batches: 0,
            bypass_daemon: false,
        };
        let probe = this.call(&[], CONNECT_TIMEOUT)?;
        ensure!(
            probe.model == model,
            "daemon answered for model {:?}, caller asked for {model:?}",
            probe.model
        );
        ensure!(probe.dim > 0, "daemon reported a zero embedding dimension");
        ensure!(
            !probe.fingerprint.is_empty(),
            "daemon reported no semantic fingerprint"
        );
        this.dim = probe.dim;
        this.daemon_fingerprint = probe.fingerprint;
        Ok(this)
    }

    /// The semantic fingerprint the daemon reported for its session.
    #[cfg(test)]
    fn daemon_fingerprint(&self) -> &str {
        &self.daemon_fingerprint
    }

    pub(crate) fn dim(&self) -> usize {
        self.dim
    }

    /// Make each batch a single daemon attempt, for interactive callers that
    /// should fall back at once rather than wait out retries.
    pub(crate) fn single_attempt(mut self) -> Self {
        self.retry.retries = 0;
        self
    }

    /// Configure lazy fallback; without it, refused or failed batches are errors.
    pub(crate) fn with_private_fallback(mut self, init: PrivateEmbedderInit) -> Self {
        self.private_init = Some(init);
        self
    }

    #[cfg(test)]
    fn private_loaded(&self) -> bool {
        self.private.is_some()
    }

    fn private_embed(&mut self, texts: &[&str], why: &str) -> Result<Vec<Vec<f32>>> {
        if self.private.is_none() {
            let init = self.private_init.take().with_context(|| {
                format!("{why}, and no private embedder is configured to fall back to")
            })?;
            // Fallback must preserve the pass's attested identity.
            let expected = self
                .expected_fingerprint
                .as_ref()
                .context("no fingerprint bound before embedding privately")?;
            tracing::warn!(texts = texts.len(), "{why}; embedding these privately");
            let mut private = init()?;
            private.bind_fingerprint(expected).with_context(|| {
                format!(
                    "{why}; the private embedder does not produce the fingerprint this pass attests"
                )
            })?;
            self.private = Some(private);
        }
        let out = self
            .private
            .as_deref_mut()
            .expect("set above")
            .embed_batch(texts)?;
        ensure!(
            out.len() == texts.len(),
            "private embedder returned {} vectors for {} texts",
            out.len(),
            texts.len()
        );
        for (i, embedding) in out.iter().enumerate() {
            ensure!(
                embedding.len() == self.dim,
                "private embedding {i} has {} values, the daemon produces {}",
                embedding.len(),
                self.dim
            );
        }
        Ok(out)
    }

    /// Send one batch, retrying it through the daemon while failures are
    /// transient and the batch's retry budget allows. A lost session is
    /// reopened within the same budget. The final error is returned so the
    /// caller's fallback policy still applies.
    fn call_with_retries(&mut self, texts: &[&str]) -> Result<EmbedTextsResult> {
        let policy = self.retry;
        let started = Instant::now();
        let left = || policy.budget.saturating_sub(started.elapsed());
        let mut retry = 0u32;
        let mut last_error: Option<anyhow::Error> = None;
        loop {
            if self.session_lost {
                let reopened = open_session(&self.rt, &self.socket, CONNECT_TIMEOUT.min(left()));
                match reopened {
                    Ok(client) => {
                        self.client = client;
                        self.session_lost = false;
                    }
                    Err(reconnect) => {
                        let error = anyhow::Error::new(TransientFailure {
                            reason: "daemon session could not be reopened",
                            kind: TransientKind::Timeout,
                            reopen_session: true,
                        })
                        .context(format!("reconnecting to the daemon failed: {reconnect:#}"));
                        return Err(match last_error {
                            Some(last) => error.context(format!("{last:#}")),
                            None => error,
                        });
                    }
                }
                if let Some(last) = last_error.take()
                    && left() < policy.min_attempt
                {
                    return Err(last);
                }
            }
            let error = match self.call(texts, policy.attempt_timeout.min(left())) {
                Ok(result) => return Ok(result),
                Err(error) => error,
            };
            let Some(transient) = transient_failure(&error) else {
                return Err(error);
            };
            self.session_lost |= transient.reopen_session;
            retry += 1;
            let backoff = policy.backoff * retry;
            if retry > policy.retries || left().saturating_sub(backoff) < policy.min_attempt {
                return Err(error);
            }
            tracing::warn!(
                retry,
                retries = policy.retries,
                texts = texts.len(),
                backoff_ms = backoff.as_millis(),
                reconnect = self.session_lost,
                error = %format!("{error:#}"),
                "daemon embed_texts batch failed transiently; retrying through the daemon"
            );
            std::thread::sleep(backoff);
            last_error = Some(error);
        }
    }

    fn call(&self, texts: &[&str], timeout: Duration) -> Result<EmbedTextsResult> {
        let mut arguments = serde_json::Map::new();
        arguments.insert("project".into(), self.project.clone().into());
        arguments.insert("model".into(), self.model.clone().into());
        if let Some(expected) = &self.expected_fingerprint {
            arguments.insert("fingerprint".into(), expected.as_str().into());
        }
        arguments.insert(
            "texts".into(),
            serde_json::Value::Array(texts.iter().map(|t| (*t).into()).collect()),
        );
        let params = CallToolRequestParams::new("embed_texts").with_arguments(arguments);
        let result = self.rt.block_on(async {
            let delivery_grace = Duration::from_millis(100).min(timeout / 10);
            let operation = async {
                let request = self
                    .client
                    .send_cancellable_request(
                        ClientRequest::CallToolRequest(rmcp::model::Request::new(params)),
                        PeerRequestOptions::with_timeout(timeout.saturating_sub(delivery_grace)),
                    )
                    .await
                    .context("sending daemon embed_texts")?;
                match request.await_response().await {
                    Ok(response) => Ok(response),
                    // rmcp discards a failed cancellation send, so a broken
                    // socket also ends here; retry on a fresh session, which
                    // costs one handshake.
                    Err(error @ rmcp::ServiceError::Timeout { .. }) => {
                        Err(anyhow::Error::new(TransientFailure {
                            reason: "daemon embed_texts request timed out",
                            kind: TransientKind::Timeout,
                            reopen_session: true,
                        })
                        .context(error.to_string()))
                    }
                    Err(error) => {
                        Err(anyhow::Error::new(error).context("daemon embed_texts failed"))
                    }
                }
            };
            match tokio::time::timeout(timeout, operation).await {
                Ok(result) => result,
                Err(_) => {
                    // Cancelling the service token ends the whole client session.
                    self.client.cancellation_token().cancel();
                    Err(anyhow::Error::new(TransientFailure {
                        reason: "daemon embed_texts transport did not complete cancellation",
                        kind: TransientKind::Timeout,
                        reopen_session: true,
                    })
                    .context(format!("daemon embed_texts timeout after {timeout:?}")))
                }
            }
        })?;
        let ServerResult::CallToolResult(result) = result else {
            bail!("daemon embed_texts returned an unexpected response");
        };
        if result.is_error == Some(true) {
            let text = result
                .content
                .iter()
                .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(code) = daemon_error_code(&result)
                && let Some(kind) = transient_code(&code)
            {
                return Err(anyhow::Error::new(TransientFailure {
                    reason: "daemon could not schedule or finish embed_texts",
                    kind,
                    reopen_session: false,
                })
                .context(format!("daemon refused embed_texts ({code}): {text}")));
            }
            bail!("daemon refused embed_texts: {text}");
        }
        let value = result
            .structured_content
            .context("daemon embed_texts returned no structured content")?;
        let parsed: EmbedTextsResult =
            serde_json::from_value(value).context("parsing daemon embed_texts result")?;
        ensure!(
            parsed.embeddings.len() == texts.len(),
            "daemon returned {} embeddings for {} texts",
            parsed.embeddings.len(),
            texts.len()
        );
        Ok(parsed)
    }
}

/// Connect and complete the MCP handshake within `timeout`; the service
/// lives on `rt`.
fn open_session(
    rt: &tokio::runtime::Runtime,
    socket: &Path,
    timeout: Duration,
) -> Result<RunningService<RoleClient, ()>> {
    rt.block_on(async {
        tokio::time::timeout(timeout, async {
            let stream = tokio::net::UnixStream::connect(socket)
                .await
                .with_context(|| format!("connecting to {}", socket.display()))?;
            ().serve(stream)
                .await
                .map_err(|e| anyhow::anyhow!("MCP handshake with the daemon failed: {e}"))
        })
        .await
        .map_err(|_| anyhow::anyhow!("daemon handshake timed out after {timeout:?}"))?
    })
}

fn is_over_cap_refusal(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(EMBED_TEXTS_OVER_CAP)
}

/// Fingerprint mismatches cannot fall back to a different vector identity.
fn is_fingerprint_refusal(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(EMBED_TEXTS_FINGERPRINT_MISMATCH)
}

/// Split `lens` (byte length per text, in order) into daemon batches that
/// each stay within `max_count` texts and `max_total` bytes, and the indices
/// of texts over `max_text` bytes, which never go to the daemon at all.
fn plan_embed_batches(
    lens: &[usize],
    max_text: usize,
    max_total: usize,
    max_count: usize,
) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut oversize = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_bytes = 0usize;
    for (i, &len) in lens.iter().enumerate() {
        if len > max_text {
            oversize.push(i);
            continue;
        }
        if !current.is_empty() && (current.len() >= max_count || current_bytes + len > max_total) {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(i);
        current_bytes += len;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    (batches, oversize)
}

impl TextEmbedder for DaemonEmbedder {
    /// Model name and dimension alone cannot establish vector compatibility.
    /// Refuse mismatched fingerprints without constructing a fallback.
    fn bind_fingerprint(&mut self, expected: &codesage_graph::SemanticFingerprint) -> Result<()> {
        ensure!(
            self.daemon_fingerprint == expected.as_str(),
            "{EMBED_TEXTS_FINGERPRINT_MISMATCH} daemon session produces {:?}, this pass attests \
             {:?}; the daemon's config or model files moved — re-run `codesage index` once they \
             agree",
            self.daemon_fingerprint,
            expected.as_str()
        );
        if let Some(private) = self.private.as_deref_mut() {
            private.bind_fingerprint(expected)?;
        }
        self.expected_fingerprint = Some(expected.clone());
        Ok(())
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        ensure!(
            self.expected_fingerprint.is_some(),
            "{EMBED_TEXTS_FINGERPRINT_MISMATCH} no fingerprint bound before embedding through the \
             daemon"
        );
        let lens: Vec<usize> = texts.iter().map(|t| t.len()).collect();
        let (batches, oversize) = plan_embed_batches(
            &lens,
            MAX_MCP_EMBED_TEXT_BYTES,
            DAEMON_CALL_MAX_BYTES.min(MAX_MCP_EMBED_TOTAL_BYTES),
            DAEMON_CALL_MAX_TEXTS.min(MAX_MCP_EMBED_TEXTS),
        );
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for batch in batches {
            let chunk: Vec<&str> = batch.iter().map(|&i| texts[i]).collect();
            if self.bypass_daemon {
                let embeddings = self.private_embed(
                    &chunk,
                    "the daemon kept timing out or stayed saturated earlier this pass",
                )?;
                for (i, embedding) in batch.into_iter().zip(embeddings) {
                    out[i] = Some(embedding);
                }
                continue;
            }
            let result = self.call_with_retries(&chunk);
            let busy = result
                .as_ref()
                .err()
                .and_then(transient_failure)
                .is_some_and(|t| t.kind == TransientKind::Busy);
            if !busy {
                self.busy_batches = 0;
            }
            let embeddings = match result {
                Ok(result) => {
                    ensure!(
                        result.dim == self.dim,
                        "daemon dimension changed mid-run: {} then {}",
                        self.dim,
                        result.dim
                    );
                    for (i, embedding) in result.embeddings.iter().enumerate() {
                        ensure!(
                            embedding.len() == self.dim,
                            "daemon embedding {i} has {} values, expected {}",
                            embedding.len(),
                            self.dim
                        );
                    }
                    result.embeddings
                }
                Err(e) if is_fingerprint_refusal(&e) => {
                    return Err(e.context("daemon semantic fingerprint moved during the pass"));
                }
                // A different daemon build may enforce tighter caps than the client.
                Err(e) if is_over_cap_refusal(&e) => self.private_embed(
                    &chunk,
                    &format!("daemon refused a batch as over cap ({e:#})"),
                )?,
                // Fall back for this batch only, preserving its fingerprint; retry the
                // daemon next batch unless it kept timing out.
                Err(e) => {
                    let why = match transient_failure(&e).map(|t| t.kind) {
                        Some(TransientKind::Timeout) => Some("kept timing out"),
                        Some(TransientKind::Busy) => {
                            self.busy_batches += 1;
                            (self.busy_batches >= BUSY_BATCHES_BEFORE_BYPASS)
                                .then_some("stayed saturated")
                        }
                        None => None,
                    };
                    if let Some(why) = why {
                        self.bypass_daemon = true;
                        tracing::warn!(
                            texts = chunk.len(),
                            error = %format!("{e:#}"),
                            "daemon embed_texts {why}; embedding the rest of this pass privately"
                        );
                    }
                    self.private_embed(
                        &chunk,
                        &format!(
                            "daemon embed_texts failed for a batch of {} text(s) ({e:#})",
                            chunk.len()
                        ),
                    )?
                }
            };
            for (i, embedding) in batch.into_iter().zip(embeddings) {
                out[i] = Some(embedding);
            }
        }
        if !oversize.is_empty() {
            let chunk: Vec<&str> = oversize.iter().map(|&i| texts[i]).collect();
            let embeddings = self.private_embed(
                &chunk,
                &format!(
                    "{} text(s) exceed the daemon's per-text cap of {MAX_MCP_EMBED_TEXT_BYTES} bytes",
                    chunk.len()
                ),
            )?;
            for (i, embedding) in oversize.into_iter().zip(embeddings) {
                out[i] = Some(embedding);
            }
        }
        Ok(out
            .into_iter()
            .map(|v| v.expect("every text was routed to the daemon or the fallback"))
            .collect())
    }
}

impl Drop for DaemonEmbedder {
    fn drop(&mut self) {
        // Cancel while still alive; normal `_exit` bypasses this destructor.
        self.client.cancellation_token().cancel();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ServerCapabilities,
        ServerConfig,
    };
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
    use serde_json::json;

    use super::*;

    /// Socket fixture: returns text length as the first vector component.
    #[derive(Clone)]
    pub(crate) struct FakeDaemon {
        pub(crate) dim: usize,
        pub(crate) model: String,
        /// One generic failure before normal service resumes.
        pub(crate) fail_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// Simulate a daemon build with tighter caps than the client.
        pub(crate) text_cap: Option<usize>,
        /// Probe identity, required on every non-empty request.
        pub(crate) fingerprint: String,
        /// Simulate a config change between the probe and first batch.
        pub(crate) fingerprint_after_probe: Option<String>,
        /// Texts embedded so far, across every accepted non-empty request.
        pub(crate) embedded: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        pub(crate) cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        /// Non-empty requests to hold until the client cancels them.
        pub(crate) stall: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Error codes to refuse the next non-empty requests with, in order.
        pub(crate) refusals: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        /// Non-empty requests received.
        pub(crate) requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Text count and total bytes of each non-empty request.
        pub(crate) sizes: std::sync::Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
        /// Client sessions accepted.
        pub(crate) sessions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeDaemon {
        pub(crate) fn new(dim: usize, model: &str) -> Self {
            Self {
                dim,
                model: model.to_string(),
                text_cap: None,
                fingerprint: fp_a().as_str().to_string(),
                fingerprint_after_probe: None,
                fail_next: Default::default(),
                embedded: Default::default(),
                cancelled: None,
                stall: Default::default(),
                refusals: Default::default(),
                requests: Default::default(),
                sizes: Default::default(),
                sessions: Default::default(),
            }
        }
    }

    /// The fingerprint every client in these tests attests under.
    pub(crate) fn fp_a() -> codesage_graph::SemanticFingerprint {
        codesage_graph::SemanticFingerprint::with_artifact_digest(
            &codesage_embed::config::EmbeddingConfig::default(),
            4,
            "digest-a",
        )
    }

    /// The same model name and dimension, pooling the other way.
    pub(crate) fn fp_b() -> codesage_graph::SemanticFingerprint {
        let config = codesage_embed::config::EmbeddingConfig {
            pooling: Some(codesage_embed::config::PoolingStrategy::Cls),
            ..codesage_embed::config::EmbeddingConfig::default()
        };
        codesage_graph::SemanticFingerprint::with_artifact_digest(&config, 4, "digest-a")
    }

    // Keep the fixture compatible with Rust versions before fetch_update's
    // replacement, try_update, without suppressing deprecation diagnostics.
    fn take_stall(counter: &std::sync::atomic::AtomicUsize) -> bool {
        use std::sync::atomic::Ordering::SeqCst;

        let mut remaining = counter.load(SeqCst);
        while let Some(next) = remaining.checked_sub(1) {
            match counter.compare_exchange(remaining, next, SeqCst, SeqCst) {
                Ok(_) => return true,
                Err(current) => remaining = current,
            }
        }
        false
    }

    #[test]
    fn stall_counter_does_not_wrap_or_consume_an_empty_counter() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let counter = AtomicUsize::new(0);
        assert!(!take_stall(&counter));
        assert_eq!(counter.load(SeqCst), 0);
        counter.store(1, SeqCst);
        assert!(take_stall(&counter));
        assert!(!take_stall(&counter));
        assert_eq!(counter.load(SeqCst), 0);
        counter.store(usize::MAX, SeqCst);
        assert!(take_stall(&counter));
        assert_eq!(counter.load(SeqCst), usize::MAX - 1);
    }

    #[test]
    fn stall_counter_consumes_each_slot_once_across_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let counter = AtomicUsize::new(1000);
        let consumed = AtomicUsize::new(0);
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    while take_stall(&counter) {
                        consumed.fetch_add(1, SeqCst);
                    }
                });
            }
        });
        assert_eq!(consumed.load(SeqCst), 1000);
        assert_eq!(counter.load(SeqCst), 0);
    }

    impl ServerHandler for FakeDaemon {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            assert_eq!(request.name, "embed_texts");
            let args = request.arguments.unwrap_or_default();
            let model = args["model"].as_str().unwrap_or_default().to_string();
            if model != self.model {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "daemon serves model {:?}, caller asked for {model:?}",
                    self.model
                ))])
                .into());
            }
            let texts: Vec<String> = serde_json::from_value(args["texts"].clone()).unwrap();
            if !texts.is_empty()
                && let Some(cancelled) = &self.cancelled
            {
                context.ct.cancelled().await;
                cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                return Ok(CallToolResult::error(vec![ContentBlock::text("cancelled")]).into());
            }
            if !texts.is_empty() {
                self.requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.sizes
                    .lock()
                    .unwrap()
                    .push((texts.len(), texts.iter().map(String::len).sum()));
            }
            if !texts.is_empty() && take_stall(&self.stall) {
                context.ct.cancelled().await;
                return Ok(CallToolResult::error(vec![ContentBlock::text("cancelled")]).into());
            }
            let refusal = if texts.is_empty() {
                None
            } else {
                let mut refusals = self.refusals.lock().unwrap();
                (!refusals.is_empty()).then(|| refusals.remove(0))
            };
            if let Some(code) = refusal {
                return Ok(CallToolResult::error(vec![
                    ContentBlock::text(format!("refused with {code}")),
                    ContentBlock::text(
                        json!({"tool": "embed_texts",
                            "error": {"code": code, "message": "refused"}})
                        .to_string(),
                    ),
                ])
                .into());
            }
            if !texts.is_empty()
                && self
                    .fail_next
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                // Omit cap/fingerprint markers to exercise generic fallback.
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "boom: simulated transient daemon failure".to_string(),
                )])
                .into());
            }
            let produces = if texts.is_empty() {
                self.fingerprint.as_str()
            } else {
                self.fingerprint_after_probe
                    .as_deref()
                    .unwrap_or(self.fingerprint.as_str())
            };
            let expected = args.get("fingerprint").and_then(|v| v.as_str());
            if (!texts.is_empty() && expected.is_none()) || expected.is_some_and(|e| e != produces)
            {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{EMBED_TEXTS_FINGERPRINT_MISMATCH} daemon session produces {produces:?}, \
                     caller attests {expected:?}"
                ))])
                .into());
            }
            if let Some(cap) = self.text_cap
                && let Some(big) = texts.iter().find(|t| t.len() > cap)
            {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{EMBED_TEXTS_OVER_CAP} text is {} bytes, over the per-text cap of {cap}",
                    big.len()
                ))])
                .into());
            }
            for text in &texts {
                assert!(
                    text.len() <= MAX_MCP_EMBED_TEXT_BYTES,
                    "client must never send a text over its own per-text cap"
                );
            }
            self.embedded
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            let embeddings: Vec<Vec<f32>> = texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; self.dim];
                    v[0] = t.len() as f32;
                    v
                })
                .collect();
            Ok(CallToolResult::structured(json!({
                "model": self.model,
                "dim": self.dim,
                "fingerprint": produces,
                "embeddings": embeddings,
            }))
            .into())
        }
    }

    pub(crate) fn spawn_fake(
        daemon: FakeDaemon,
    ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp-test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                listener.set_nonblocking(true).unwrap();
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                // Serve every session a client opens; stop once none is left.
                let mut sessions = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            daemon
                                .sessions
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let daemon = daemon.clone();
                            sessions.spawn(async move {
                                let service = daemon.serve(stream).await.unwrap();
                                let _ = service.waiting().await;
                            });
                        }
                        Some(done) = sessions.join_next() => {
                            done.unwrap();
                            if sessions.is_empty() {
                                break;
                            }
                        }
                    }
                }
            });
        });
        (dir, socket, handle)
    }

    #[test]
    fn embedding_timeout_notifies_daemon_while_connection_stays_open() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let daemon = FakeDaemon {
            cancelled: Some(cancelled.clone()),
            ..FakeDaemon::new(4, "m")
        };
        let (_dir, socket, handle) = spawn_fake(daemon);
        let client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
        let error = client
            .call(&["wait for cancellation"], Duration::from_millis(50))
            .unwrap_err();
        assert!(format!("{error:#}").contains("timeout"), "{error:#}");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !cancelled.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            client
                .rt
                .block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        }
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(client.call(&[], Duration::from_secs(1)).is_ok());
        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn embedding_timeout_bounds_a_peer_that_stops_reading() {
        use std::io::{BufRead, BufReader, Write};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("non-reading.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let result = match request["method"].as_str() {
                    Some("initialize") => json!({
                        "protocolVersion": request["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "non-reading", "version": "1"}
                    }),
                    Some("tools/call") => json!({
                        "content": [],
                        "structuredContent": {"model": "m", "dim": 4,
                            "fingerprint": fp_a().as_str(), "embeddings": []}
                    }),
                    _ => continue,
                };
                writeln!(
                    stream,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                )
                .unwrap();
                if request["method"] == "tools/call" {
                    wait.recv_timeout(Duration::from_secs(3)).unwrap();
                    break;
                }
            }
        });
        let client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
        let large_text = "x".repeat(2 * 1024 * 1024);
        let started = std::time::Instant::now();
        let result = client.call(&[&large_text], Duration::from_millis(100));
        let elapsed = started.elapsed();
        release.send(()).unwrap();
        drop(client);
        server.join().unwrap();
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("timeout"), "{error:#}");
        assert!(elapsed < Duration::from_secs(1), "blocked for {elapsed:?}");
    }

    #[test]
    fn embeds_through_the_socket_and_reports_the_probed_dimension() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon::new(4, "m"));
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
            assert_eq!(client.dim(), 4);
            assert_eq!(client.daemon_fingerprint(), fp_a().as_str());
            client.bind_fingerprint(&fp_a()).unwrap();
            let out = client.embed_batch(&["ab", "abcd"]).unwrap();
            assert_eq!(out.len(), 2);
            assert_eq!(out[0][0], 2.0);
            assert_eq!(out[1][0], 4.0);
            assert!(out.iter().all(|v| v.len() == 4));
            assert_eq!(client.embed_one("abc").unwrap()[0], 3.0);
        }
        handle.join().unwrap();
    }

    #[test]
    fn model_mismatch_is_refused_at_connect() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon::new(4, "served"));
        let err = DaemonEmbedder::connect_to(&socket, "/p", "wanted")
            .err()
            .expect("mismatched model must not connect")
            .to_string();
        assert!(err.contains("daemon refused embed_texts"), "{err}");
        assert!(err.contains("wanted"), "{err}");
        handle.join().unwrap();
    }

    /// Private fixture: -1 distinguishes fallback vectors from daemon results.
    struct FakePrivate {
        dim: usize,
        produces: Option<codesage_graph::SemanticFingerprint>,
        bound_with: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        batches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakePrivate {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                produces: None,
                bound_with: Default::default(),
                batches: Default::default(),
            }
        }
    }

    impl TextEmbedder for FakePrivate {
        fn bind_fingerprint(
            &mut self,
            expected: &codesage_graph::SemanticFingerprint,
        ) -> Result<()> {
            self.bound_with
                .lock()
                .unwrap()
                .push(expected.as_str().to_string());
            if let Some(produces) = &self.produces {
                ensure!(
                    produces == expected,
                    "private session produces {produces}, this pass attests {expected}"
                );
            }
            Ok(())
        }

        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.batches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|_| {
                    let mut v = vec![0.0f32; self.dim];
                    v[0] = -1.0;
                    v
                })
                .collect())
        }
    }

    #[test]
    fn plan_embed_batches_splits_on_count_and_bytes_and_sets_oversize_aside() {
        let (batches, oversize) = plan_embed_batches(&[5, 5, 5, 5, 11, 10, 10, 10, 1], 10, 24, 3);
        assert_eq!(
            oversize,
            vec![4],
            "the 11-byte text never goes to the daemon"
        );
        assert_eq!(
            batches,
            vec![vec![0, 1, 2], vec![3, 5], vec![6, 7, 8]],
            "count splits after three; bytes split before 5+10+10 would pass 24"
        );
        let (batches, oversize) = plan_embed_batches(&[], 10, 25, 3);
        assert!(batches.is_empty() && oversize.is_empty());
        let (batches, oversize) = plan_embed_batches(&[10, 11], 10, 25, 3);
        assert_eq!((batches, oversize), (vec![vec![0]], vec![1]));
    }

    #[test]
    fn oversize_texts_go_to_the_private_fallback_never_the_daemon() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon::new(4, "m"));
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            client.bind_fingerprint(&fp_a()).unwrap();
            let big = "x".repeat(MAX_MCP_EMBED_TEXT_BYTES + 1);
            let out = client.embed_batch(&["ab", &big, "abcd"]).unwrap();
            assert_eq!(out.len(), 3);
            assert_eq!(out[0][0], 2.0, "small text embedded by the daemon");
            assert_eq!(
                out[1][0], -1.0,
                "oversize text embedded privately, in place"
            );
            assert_eq!(out[2][0], 4.0);
            assert!(client.private_loaded());
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_daemon_cap_refusal_falls_back_to_the_private_embedder() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            text_cap: Some(3),
            ..FakeDaemon::new(4, "m")
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            client.bind_fingerprint(&fp_a()).unwrap();
            assert!(!client.private_loaded());
            let out = client.embed_batch(&["ab", "abcd"]).unwrap();
            assert_eq!(out.len(), 2);
            assert_eq!(out[1][0], -1.0, "the refused batch is embedded privately");
            assert!(client.private_loaded());
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_cap_refusal_without_a_fallback_is_an_error_naming_it() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            text_cap: Some(3),
            ..FakeDaemon::new(4, "m")
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
            client.bind_fingerprint(&fp_a()).unwrap();
            let err = format!("{:#}", client.embed_batch(&["abcd"]).unwrap_err());
            assert!(err.contains("no private embedder is configured"), "{err}");
            assert!(err.contains(EMBED_TEXTS_OVER_CAP), "{err}");
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_daemon_producing_another_fingerprint_is_refused_at_bind_without_a_private_fallback() {
        // Only pooling differs; model and dimension checks must not suffice.
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            fingerprint: fp_b().as_str().to_string(),
            ..FakeDaemon::new(4, "m")
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            let err = format!("{:#}", client.bind_fingerprint(&fp_a()).unwrap_err());
            assert!(err.contains(EMBED_TEXTS_FINGERPRINT_MISMATCH), "{err}");
            assert!(
                err.contains("pooling=cls") && err.contains("pooling=mean"),
                "{err}"
            );
            assert!(!client.private_loaded(), "no private embedder stands in");
            let err = format!("{:#}", client.embed_batch(&["ab"]).unwrap_err());
            assert!(err.contains(EMBED_TEXTS_FINGERPRINT_MISMATCH), "{err}");
            assert!(!client.private_loaded());
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_fingerprint_that_moves_mid_pass_aborts_the_batch_without_a_private_fallback() {
        let daemon = FakeDaemon {
            fingerprint_after_probe: Some(fp_b().as_str().to_string()),
            ..FakeDaemon::new(4, "m")
        };
        let requests = daemon.requests.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            client.bind_fingerprint(&fp_a()).unwrap();
            let err = format!("{:#}", client.embed_batch(&["ab", "abcd"]).unwrap_err());
            assert!(err.contains(EMBED_TEXTS_FINGERPRINT_MISMATCH), "{err}");
            assert!(err.contains("moved during the pass"), "{err}");
            assert!(!client.private_loaded(), "no private embedder stands in");
            assert_eq!(
                requests.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "a fingerprint refusal is not retried"
            );
        }
        handle.join().unwrap();
    }

    #[test]
    fn the_private_fallback_is_bound_to_the_pass_fingerprint_before_it_embeds() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            text_cap: Some(3),
            ..FakeDaemon::new(4, "m")
        });
        {
            let bound_with = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (bw, b) = (bound_with.clone(), batches.clone());
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(move || {
                    Ok(Box::new(FakePrivate {
                        bound_with: bw,
                        batches: b,
                        ..FakePrivate::new(4)
                    }) as Box<dyn TextEmbedder>)
                }));
            client.bind_fingerprint(&fp_a()).unwrap();
            let out = client.embed_batch(&["abcd"]).unwrap();
            assert_eq!(out[0][0], -1.0, "the refused batch is embedded privately");
            assert_eq!(
                *bound_with.lock().unwrap(),
                vec![fp_a().as_str().to_string()],
                "the private session is bound to the pass fingerprint exactly once, before use"
            );
            assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_private_fallback_producing_another_fingerprint_is_an_error_not_a_silent_embed() {
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            text_cap: Some(3),
            ..FakeDaemon::new(4, "m")
        });
        {
            let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let b = batches.clone();
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(move || {
                    Ok(Box::new(FakePrivate {
                        produces: Some(fp_b()),
                        batches: b,
                        ..FakePrivate::new(4)
                    }) as Box<dyn TextEmbedder>)
                }));
            client.bind_fingerprint(&fp_a()).unwrap();
            let err = format!("{:#}", client.embed_batch(&["ab", "abcd"]).unwrap_err());
            assert!(err.contains("private session produces"), "{err}");
            assert!(
                err.contains("pooling=cls") && err.contains("pooling=mean"),
                "{err}"
            );
            assert!(
                err.contains("does not produce the fingerprint this pass attests"),
                "{err}"
            );
            assert_eq!(
                batches.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "no text reached the mismatched private session"
            );
            let big = "x".repeat(MAX_MCP_EMBED_TEXT_BYTES + 1);
            let err = format!("{:#}", client.embed_batch(&[big.as_str()]).unwrap_err());
            assert!(err.contains("no private embedder is configured"), "{err}");
            assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
        handle.join().unwrap();
    }

    #[test]
    fn missing_socket_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let err = DaemonEmbedder::connect_to(&dir.path().join("none.sock"), "/p", "m")
            .err()
            .expect("no listener must fail")
            .to_string();
        assert!(err.contains("connecting to"), "{err}");
    }

    #[test]
    fn a_transient_daemon_failure_falls_back_for_that_batch_then_retries_the_daemon() {
        let daemon = FakeDaemon {
            fail_next: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..FakeDaemon::new(4, "m")
        };
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            client.bind_fingerprint(&fp_a()).unwrap();
            let out = client.embed_batch(&["ab"]).unwrap();
            assert_eq!(out[0][0], -1.0, "failed batch falls back privately");
            assert!(client.private_loaded());
            let out = client.embed_batch(&["abcd"]).unwrap();
            assert_eq!(out[0][0], 4.0, "daemon is re-probed on the next batch");
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_transient_daemon_failure_without_a_fallback_names_the_batch() {
        let daemon = FakeDaemon {
            fail_next: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..FakeDaemon::new(4, "m")
        };
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
            client.bind_fingerprint(&fp_a()).unwrap();
            let err = format!("{:#}", client.embed_batch(&["ab"]).unwrap_err());
            assert!(err.contains("no private embedder is configured"), "{err}");
            assert!(err.contains("batch of 1"), "{err}");
        }
        handle.join().unwrap();
    }

    fn quick_retries(client: &mut DaemonEmbedder) {
        client.retry = RetryPolicy {
            attempt_timeout: Duration::from_millis(200),
            retries: 2,
            budget: Duration::from_secs(5),
            backoff: Duration::from_millis(10),
            min_attempt: Duration::from_millis(50),
        };
    }

    #[test]
    fn a_timed_out_batch_is_retried_through_the_daemon_before_any_fallback() {
        let daemon = FakeDaemon::new(4, "m");
        daemon.stall.store(1, std::sync::atomic::Ordering::SeqCst);
        daemon.refusals.lock().unwrap().push("E_SATURATED");
        let (stall, refusals) = (daemon.stall.clone(), daemon.refusals.clone());
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            client.bind_fingerprint(&fp_a()).unwrap();
            let out = client.embed_batch(&["ab", "abcd"]).unwrap();
            assert_eq!(
                out[0][0], 2.0,
                "the retried batch is embedded by the daemon"
            );
            assert_eq!(out[1][0], 4.0);
            assert_eq!(stall.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(refusals.lock().unwrap().is_empty());
            assert!(!client.private_loaded(), "no private embedder was loaded");
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_batch_that_keeps_timing_out_falls_back_after_bounded_retries() {
        let daemon = FakeDaemon::new(4, "m");
        daemon
            .stall
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
        let stall = daemon.stall.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            client.bind_fingerprint(&fp_a()).unwrap();
            let out = client.embed_batch(&["ab"]).unwrap();
            assert_eq!(
                out[0][0], -1.0,
                "the batch falls back once retries are spent"
            );
            assert_eq!(
                usize::MAX - stall.load(std::sync::atomic::Ordering::SeqCst),
                3,
                "one attempt plus two retries reached the daemon"
            );
            let out = client.embed_batch(&["abcd"]).unwrap();
            assert_eq!(out[0][0], -1.0);
            assert_eq!(
                usize::MAX - stall.load(std::sync::atomic::Ordering::SeqCst),
                3,
                "later batches of the pass skip a daemon that kept timing out"
            );
        }
        handle.join().unwrap();
    }

    fn sent_sizes(texts: &[&str]) -> Vec<(usize, usize)> {
        let daemon = FakeDaemon::new(4, "m");
        let sizes = daemon.sizes.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
            client.bind_fingerprint(&fp_a()).unwrap();
            let out = client.embed_batch(texts).unwrap();
            assert_eq!(out.len(), texts.len());
            assert!(
                out.iter().zip(texts).all(|(v, t)| v[0] == t.len() as f32),
                "every text is embedded by the daemon, in place"
            );
        }
        handle.join().unwrap();
        sizes.lock().unwrap().clone()
    }

    #[test]
    fn daemon_calls_stay_within_the_per_call_text_cap() {
        let texts: Vec<String> = (0..300).map(|i| format!("text {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let sizes = sent_sizes(&refs);
        assert_eq!(sizes.len(), 2, "{sizes:?}");
        assert!(
            sizes
                .iter()
                .all(|&(count, _)| count <= DAEMON_CALL_MAX_TEXTS)
        );
        assert_eq!(sizes.iter().map(|&(count, _)| count).sum::<usize>(), 300);
    }

    #[test]
    fn daemon_calls_stay_within_the_per_call_byte_cap() {
        let text = "x".repeat(40_000);
        let refs = vec![text.as_str(); 60];
        let sizes = sent_sizes(&refs);
        assert_eq!(sizes.len(), 2, "{sizes:?}");
        assert!(
            sizes
                .iter()
                .all(|&(_, bytes)| bytes <= DAEMON_CALL_MAX_BYTES)
        );
        assert_eq!(sizes.iter().map(|&(count, _)| count).sum::<usize>(), 60);
    }

    #[test]
    fn a_daemon_that_stays_saturated_is_bypassed_after_two_batches() {
        let daemon = FakeDaemon::new(4, "m");
        daemon
            .refusals
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n("E_SATURATED", 100));
        let requests = daemon.requests.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            client.bind_fingerprint(&fp_a()).unwrap();
            assert_eq!(client.embed_batch(&["a"]).unwrap()[0][0], -1.0);
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
            assert_eq!(client.embed_batch(&["b"]).unwrap()[0][0], -1.0);
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 6);
            assert_eq!(client.embed_batch(&["c"]).unwrap()[0][0], -1.0);
            assert_eq!(
                requests.load(std::sync::atomic::Ordering::SeqCst),
                6,
                "the third batch skips a daemon that stayed saturated"
            );
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_daemon_success_resets_the_saturation_count() {
        let daemon = FakeDaemon::new(4, "m");
        daemon
            .refusals
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n("E_SATURATED", 3));
        let refusals = daemon.refusals.clone();
        let requests = daemon.requests.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            client.bind_fingerprint(&fp_a()).unwrap();
            assert_eq!(client.embed_batch(&["a"]).unwrap()[0][0], -1.0);
            assert_eq!(client.embed_batch(&["bb"]).unwrap()[0][0], 2.0);
            refusals
                .lock()
                .unwrap()
                .extend(std::iter::repeat_n("E_SATURATED", 3));
            assert_eq!(client.embed_batch(&["c"]).unwrap()[0][0], -1.0);
            assert_eq!(
                client.embed_batch(&["dddd"]).unwrap()[0][0],
                4.0,
                "one saturated batch after a success does not bypass the daemon"
            );
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 8);
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_request_timeout_retries_on_a_fresh_session() {
        let daemon = FakeDaemon::new(4, "m");
        daemon.stall.store(1, std::sync::atomic::Ordering::SeqCst);
        let sessions = daemon.sessions.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
            quick_retries(&mut client);
            // A small request whose write completes: rmcp's own request
            // timeout fires, not the outer transport guard.
            client.retry.attempt_timeout = Duration::from_secs(1);
            client.bind_fingerprint(&fp_a()).unwrap();
            assert_eq!(client.embed_batch(&["ab"]).unwrap()[0][0], 2.0);
            assert_eq!(
                sessions.load(std::sync::atomic::Ordering::SeqCst),
                2,
                "the retry ran on a second session"
            );
        }
        handle.join().unwrap();
    }

    #[test]
    fn only_consecutive_saturated_batches_bypass_the_daemon() {
        let daemon = FakeDaemon::new(4, "m");
        daemon.refusals.lock().unwrap().extend(
            std::iter::repeat_n("E_SATURATED", 3)
                .chain(["E_INTERNAL"])
                .chain(std::iter::repeat_n("E_SATURATED", 3)),
        );
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            client.bind_fingerprint(&fp_a()).unwrap();
            assert_eq!(client.embed_batch(&["a"]).unwrap()[0][0], -1.0);
            assert_eq!(client.embed_batch(&["b"]).unwrap()[0][0], -1.0);
            assert_eq!(client.embed_batch(&["c"]).unwrap()[0][0], -1.0);
            assert_eq!(
                client.embed_batch(&["dddd"]).unwrap()[0][0],
                4.0,
                "an intervening non-saturation failure resets the count"
            );
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_single_attempt_client_falls_back_after_one_timeout() {
        let daemon = FakeDaemon::new(4, "m");
        daemon.stall.store(1, std::sync::atomic::Ordering::SeqCst);
        let requests = daemon.requests.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            let mut client = client.single_attempt();
            client.bind_fingerprint(&fp_a()).unwrap();
            assert_eq!(client.embed_one("ab").unwrap()[0], -1.0);
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        handle.join().unwrap();
    }

    #[test]
    fn shutdown_and_unknown_daemon_errors_are_not_retried() {
        let daemon = FakeDaemon::new(4, "m");
        daemon.refusals.lock().unwrap().push("E_SHUTDOWN");
        let requests = daemon.requests.clone();
        let (_dir, socket, handle) = spawn_fake(daemon);
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            client.bind_fingerprint(&fp_a()).unwrap();
            assert_eq!(client.embed_batch(&["ab"]).unwrap()[0][0], -1.0);
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(
                client.embed_batch(&["abcd"]).unwrap()[0][0],
                4.0,
                "a non-timeout failure does not bypass the daemon for the pass"
            );
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_stalled_transport_reconnects_and_retries_through_the_daemon() {
        use std::io::{BufRead, BufReader, Write};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("reconnect.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let daemon = FakeDaemon::new(4, "m");
        let requests = daemon.requests.clone();
        let server = std::thread::spawn(move || {
            // First session: answer the handshake and probe, then stop reading.
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let result = match request["method"].as_str() {
                    Some("initialize") => json!({
                        "protocolVersion": request["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "stalling", "version": "1"}
                    }),
                    Some("tools/call") => json!({
                        "content": [],
                        "structuredContent": {"model": "m", "dim": 4,
                            "fingerprint": fp_a().as_str(), "embeddings": []}
                    }),
                    _ => continue,
                };
                writeln!(
                    stream,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                )
                .unwrap();
                if request["method"] == "tools/call" {
                    break;
                }
            }
            // Second session: a working daemon.
            let (second, _) = listener.accept().unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                second.set_nonblocking(true).unwrap();
                let second = tokio::net::UnixStream::from_std(second).unwrap();
                let service = daemon.serve(second).await.unwrap();
                let _ = service.waiting().await;
            });
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(stream);
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate::new(4)) as Box<dyn TextEmbedder>)
                }));
            quick_retries(&mut client);
            // The retry carries 2 MB through a debug-build session; leave it
            // room on a loaded test machine.
            client.retry.attempt_timeout = Duration::from_secs(2);
            client.retry.budget = Duration::from_secs(20);
            client.bind_fingerprint(&fp_a()).unwrap();
            // Large enough that the write blocks on a peer that stopped reading.
            let text = "x".repeat(40_000);
            let texts = vec![text.as_str(); 50];
            let out = client.embed_batch(&texts).unwrap();
            assert!(
                out.iter().all(|v| v[0] == 40_000.0),
                "embedded by the daemon"
            );
            assert!(!client.private_loaded());
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn batch_timeout_is_bounded_to_one_batch_not_the_whole_pass() {
        assert!(
            EMBED_TIMEOUT.as_secs() <= 120,
            "one wedged batch must fail over in ~minutes, not pin the pass under the lock: {EMBED_TIMEOUT:?}"
        );
    }
}
