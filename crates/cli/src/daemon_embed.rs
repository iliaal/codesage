//! CLI-side client for the daemon's hidden `embed_texts` tool.
//!
//! Every process that constructs an [`codesage_embed::model::Embedder`] on a
//! GPU device creates its own CUDA context and loads the model onto the card
//! (~2.3 GB VRAM, ~950 MB host RSS per process, freed only when the session
//! drops). When a daemon spawned from this same binary is already running, it
//! holds that session, so `index` and `search` ask it to embed instead of
//! bringing up a second one. The daemon is never started from here: a CLI run
//! that finds none embeds privately.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use codesage_graph::TextEmbedder;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};

use crate::mcp::params::EmbedTextsResult;
use crate::mcp::{
    EMBED_TEXTS_FINGERPRINT_MISMATCH, EMBED_TEXTS_OVER_CAP, MAX_MCP_EMBED_TEXT_BYTES,
    MAX_MCP_EMBED_TEXTS, MAX_MCP_EMBED_TOTAL_BYTES,
};

/// Constructor for the private embedder a [`DaemonEmbedder`] falls back to
/// when the daemon refuses a request over its caps.
type PrivateEmbedderInit = Box<dyn FnOnce() -> Result<Box<dyn TextEmbedder>> + Send>;

/// Handshake plus the model/dimension probe. A daemon whose model is not yet
/// resident loads it inside this window, so it is sized for a cold load from
/// disk, not for a round trip.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
/// One commit batch of chunks, embedded on whatever device the daemon runs.
const EMBED_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) struct DaemonEmbedder {
    rt: tokio::runtime::Runtime,
    client: RunningService<RoleClient, ()>,
    project: String,
    model: String,
    dim: usize,
    /// The semantic fingerprint the daemon reported at the probe: the
    /// identity of the vectors its session produces.
    daemon_fingerprint: String,
    /// The fingerprint the pass attests under, once bound. Sent with every
    /// embed request so the daemon refuses the moment its own moves.
    expected_fingerprint: Option<String>,
    /// Private-embedder fallback for texts the daemon refuses as over cap
    /// — a single text past the per-text byte cap, or a refusal from a
    /// daemon of another build with tighter caps. Constructed on first use.
    private_init: Option<PrivateEmbedderInit>,
    private: Option<Box<dyn TextEmbedder>>,
}

impl DaemonEmbedder {
    /// Borrow the running daemon's session for `model`, or `None` when no
    /// daemon spawned from this binary answers, the project root is not
    /// UTF-8 (the tool takes a string path), or the daemon cannot serve this
    /// model. Every refusal is logged and falls back to a private embedder.
    ///
    /// `config` also seeds the private fallback used for texts the daemon
    /// refuses as over its byte caps.
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

    /// Dimension the daemon's session produces for this model.
    pub(crate) fn dim(&self) -> usize {
        self.dim
    }

    /// Install the constructor for the private embedder used when the
    /// daemon refuses a request as over cap. Without one, such a refusal is
    /// an error.
    pub(crate) fn with_private_fallback(mut self, init: PrivateEmbedderInit) -> Self {
        self.private_init = Some(init);
        self
    }

    /// Whether the private fallback has been constructed.
    #[cfg(test)]
    fn private_loaded(&self) -> bool {
        self.private.is_some()
    }

    fn private_embed(&mut self, texts: &[&str], why: &str) -> Result<Vec<Vec<f32>>> {
        if self.private.is_none() {
            let init = self.private_init.take().with_context(|| {
                format!("{why}, and no private embedder is configured to fall back to")
            })?;
            tracing::warn!(texts = texts.len(), "{why}; embedding these privately");
            self.private = Some(init()?);
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
            arguments.insert("fingerprint".into(), expected.clone().into());
        }
        arguments.insert(
            "texts".into(),
            serde_json::Value::Array(texts.iter().map(|t| (*t).into()).collect()),
        );
        let params = CallToolRequestParams::new("embed_texts").with_arguments(arguments);
        let result = self.rt.block_on(async {
            tokio::time::timeout(timeout, self.client.call_tool(params))
                .await
                .map_err(|_| anyhow::anyhow!("daemon embed_texts timed out after {timeout:?}"))?
                .map_err(|e| anyhow::anyhow!("daemon embed_texts failed: {e}"))
        })?;
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

/// Whether a daemon error is a cap refusal rather than a failure: the
/// request was well-formed but too large for that daemon.
fn is_over_cap_refusal(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(EMBED_TEXTS_OVER_CAP)
}

/// Whether a daemon error says the daemon's fingerprint is not the one this
/// pass attests under. Never a fallback case: a private session would embed
/// under yet another identity.
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
    /// Refuse a daemon whose session does not produce `expected`: same model
    /// name and dimension, but a pooling, device, or model-file change on
    /// the daemon's side. Nothing is embedded privately in its place — the
    /// pass aborts unattested, and the user re-runs once the two agree.
    fn bind_fingerprint(&mut self, expected: &codesage_graph::SemanticFingerprint) -> Result<()> {
        ensure!(
            self.daemon_fingerprint == expected.as_str(),
            "{EMBED_TEXTS_FINGERPRINT_MISMATCH} daemon session produces {:?}, this pass attests \
             {:?}; the daemon's config or model files moved — re-run `codesage index` once they \
             agree",
            self.daemon_fingerprint,
            expected.as_str()
        );
        self.expected_fingerprint = Some(expected.as_str().to_string());
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
                // The daemon's fingerprint moved mid-pass: abort, and never
                // embed the batch privately under a third identity.
                Err(e) if is_fingerprint_refusal(&e) => {
                    return Err(e.context("daemon semantic fingerprint moved during the pass"));
                }
                // A daemon of another build may hold tighter caps than this
                // binary planned for; its refusal is not a failure of the run.
                Err(e) if is_over_cap_refusal(&e) => self.private_embed(
                    &chunk,
                    &format!("daemon refused a batch as over cap ({e:#})"),
                )?,
                Err(e) => return Err(e),
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
        // Best effort: tell the daemon this client is gone so its per-client
        // task ends now rather than at the idle ceiling. `main` leaves via
        // `_exit`, so on the normal path this runs only when the embedder is
        // dropped before then.
        self.client.cancellation_token().cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ServerCapabilities,
        ServerInfo,
    };
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
    use serde_json::json;

    use super::*;

    /// A stand-in daemon: answers `embed_texts` with `dim`-wide vectors whose
    /// first component is the text's length, records the model it was asked
    /// for, and refuses any other model.
    #[derive(Clone)]
    struct FakeDaemon {
        dim: usize,
        model: String,
        /// Refuse any text longer than this with the shared over-cap prefix,
        /// as a daemon of another build with tighter caps would.
        text_cap: Option<usize>,
        /// The fingerprint this daemon's session produces, reported at the
        /// probe and required on every non-empty request.
        fingerprint: String,
        /// When set, the fingerprint the session produces from the first
        /// non-empty request on: a same-model config change landing on the
        /// daemon between the probe and a later batch.
        fingerprint_after_probe: Option<String>,
    }

    impl FakeDaemon {
        fn new(dim: usize, model: &str) -> Self {
            Self {
                dim,
                model: model.to_string(),
                text_cap: None,
                fingerprint: fp_a().as_str().to_string(),
                fingerprint_after_probe: None,
            }
        }
    }

    /// The fingerprint every client in these tests attests under.
    fn fp_a() -> codesage_graph::SemanticFingerprint {
        codesage_graph::SemanticFingerprint::with_artifact_digest(
            &codesage_embed::config::EmbeddingConfig::default(),
            4,
            "digest-a",
        )
    }

    /// The same model name and dimension, pooling the other way.
    fn fp_b() -> codesage_graph::SemanticFingerprint {
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
            _context: RequestContext<RoleServer>,
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

    /// Serve `daemon` on a fresh socket under a tempdir until the returned
    /// guard drops.
    fn spawn_fake(daemon: FakeDaemon) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<()>) {
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
                // One client per test; exit once it hangs up.
                let (stream, _) = listener.accept().await.unwrap();
                let service = daemon.serve(stream).await.unwrap();
                let _ = service.waiting().await;
            });
        });
        (dir, socket, handle)
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

    /// Private stand-in: `dim`-wide vectors whose first component is -1 so a
    /// test can tell which side produced each vector.
    struct FakePrivate {
        dim: usize,
    }

    impl TextEmbedder for FakePrivate {
        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
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
        // per-text 10, total 24, count 3.
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
        // A single text exactly at the cap is sent, one byte over is not.
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
                    Ok(Box::new(FakePrivate { dim: 4 }) as Box<dyn TextEmbedder>)
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
        // The daemon (another build) refuses texts over 3 bytes; this
        // client's own caps are wider, so the refusal arrives at call time.
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            text_cap: Some(3),
            ..FakeDaemon::new(4, "m")
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate { dim: 4 }) as Box<dyn TextEmbedder>)
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
        // Same model name, same dimension, pooling the other way: the probe
        // passes the model check and the bind must still refuse.
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            fingerprint: fp_b().as_str().to_string(),
            ..FakeDaemon::new(4, "m")
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate { dim: 4 }) as Box<dyn TextEmbedder>)
                }));
            let err = format!("{:#}", client.bind_fingerprint(&fp_a()).unwrap_err());
            assert!(err.contains(EMBED_TEXTS_FINGERPRINT_MISMATCH), "{err}");
            assert!(
                err.contains("pooling=cls") && err.contains("pooling=mean"),
                "{err}"
            );
            assert!(!client.private_loaded(), "no private embedder stands in");
            // Embedding without a successful bind never reaches the daemon
            // or the fallback either.
            let err = format!("{:#}", client.embed_batch(&["ab"]).unwrap_err());
            assert!(err.contains(EMBED_TEXTS_FINGERPRINT_MISMATCH), "{err}");
            assert!(!client.private_loaded());
        }
        handle.join().unwrap();
    }

    #[test]
    fn a_fingerprint_that_moves_mid_pass_aborts_the_batch_without_a_private_fallback() {
        // The probe and the bind agree on A; the daemon's config changes
        // under the same model name before the next batch and its session
        // now produces B. The batch must fail, not be embedded elsewhere.
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            fingerprint_after_probe: Some(fp_b().as_str().to_string()),
            ..FakeDaemon::new(4, "m")
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m")
                .unwrap()
                .with_private_fallback(Box::new(|| {
                    Ok(Box::new(FakePrivate { dim: 4 }) as Box<dyn TextEmbedder>)
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
    fn missing_socket_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let err = DaemonEmbedder::connect_to(&dir.path().join("none.sock"), "/p", "m")
            .err()
            .expect("no listener must fail")
            .to_string();
        assert!(err.contains("connecting to"), "{err}");
    }
}
