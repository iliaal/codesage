//! Reuse the running daemon's embedding session to avoid duplicate model memory.
//! If no matching daemon is running, the CLI embeds privately without starting one.

use std::path::Path;
use std::time::Duration;

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

type PrivateEmbedderInit = Box<dyn FnOnce() -> Result<Box<dyn TextEmbedder>> + Send>;

/// Allow a cold model load during the handshake and dimension probe.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound each batch so a wedged daemon cannot indefinitely hold the index lock.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct DaemonEmbedder {
    rt: tokio::runtime::Runtime,
    client: RunningService<RoleClient, ()>,
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
                Some(embedder.with_private_fallback(Box::new(move || {
                    let embedder = codesage_embed::model::Embedder::new(&private_config)
                        .context("loading a private embedder for texts the daemon refused")?;
                    Ok(Box::new(embedder) as Box<dyn TextEmbedder>)
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
        let client = rt.block_on(async {
            tokio::time::timeout(CONNECT_TIMEOUT, async {
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .with_context(|| format!("connecting to {}", socket.display()))?;
                ().serve(stream)
                    .await
                    .map_err(|e| anyhow::anyhow!("MCP handshake with the daemon failed: {e}"))
            })
            .await
            .map_err(|_| anyhow::anyhow!("daemon handshake timed out after {CONNECT_TIMEOUT:?}"))?
        })?;
        let mut this = Self {
            rt,
            client,
            project: project.to_string(),
            model: model.to_string(),
            dim: 0,
            daemon_fingerprint: String::new(),
            expected_fingerprint: None,
            private_init: None,
            private: None,
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
                request
                .await_response()
                .await
                .context("daemon embed_texts failed")
            };
            match tokio::time::timeout(timeout, operation).await {
                Ok(result) => result,
                Err(_) => {
                    self.client.cancellation_token().cancel();
                    bail!("daemon embed_texts timeout after {timeout:?}; transport did not complete cancellation");
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
            MAX_MCP_EMBED_TOTAL_BYTES,
            MAX_MCP_EMBED_TEXTS,
        );
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for batch in batches {
            let chunk: Vec<&str> = batch.iter().map(|&i| texts[i]).collect();
            let embeddings = match self.call(&chunk, EMBED_TIMEOUT) {
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
                // Fall back for this batch only, preserving its fingerprint; retry the daemon next batch.
                Err(e) => self.private_embed(
                    &chunk,
                    &format!(
                        "daemon embed_texts failed for a batch of {} text(s) ({e:#})",
                        chunk.len()
                    ),
                )?,
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
        ServerInfo,
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

    impl ServerHandler for FakeDaemon {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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
                let (stream, _) = listener.accept().await.unwrap();
                let service = daemon.serve(stream).await.unwrap();
                let _ = service.waiting().await;
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
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            fingerprint_after_probe: Some(fp_b().as_str().to_string()),
            ..FakeDaemon::new(4, "m")
        });
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

    #[test]
    fn batch_timeout_is_bounded_to_one_batch_not_the_whole_pass() {
        assert!(
            EMBED_TIMEOUT.as_secs() <= 120,
            "one wedged batch must fail over in ~minutes, not pin the pass under the lock: {EMBED_TIMEOUT:?}"
        );
    }
}
