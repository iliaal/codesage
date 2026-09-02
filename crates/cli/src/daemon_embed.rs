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
}

impl DaemonEmbedder {
    /// Borrow the running daemon's session for `model`, or `None` when no
    /// daemon spawned from this binary answers, the project root is not
    /// UTF-8 (the tool takes a string path), or the daemon cannot serve this
    /// model. Every refusal is logged and falls back to a private embedder.
    pub(crate) fn connect(root: &Path, model: &str) -> Option<Self> {
        let socket = crate::daemon::running_daemon_socket()?;
        let Some(project) = root.to_str() else {
            tracing::debug!(
                root = %root.display(),
                "project root is not UTF-8; embedding privately"
            );
            return None;
        };
        match Self::connect_to(&socket, project, model) {
            Ok(embedder) => {
                tracing::info!(
                    socket = %socket.display(),
                    model,
                    dim = embedder.dim,
                    "embedding through the running daemon"
                );
                Some(embedder)
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
        };
        let probe = this.call(&[], CONNECT_TIMEOUT)?;
        ensure!(
            probe.model == model,
            "daemon answered for model {:?}, caller asked for {model:?}",
            probe.model
        );
        ensure!(probe.dim > 0, "daemon reported a zero embedding dimension");
        this.dim = probe.dim;
        Ok(this)
    }

    /// Dimension the daemon's session produces for this model.
    pub(crate) fn dim(&self) -> usize {
        self.dim
    }

    fn call(&self, texts: &[&str], timeout: Duration) -> Result<EmbedTextsResult> {
        let mut arguments = serde_json::Map::new();
        arguments.insert("project".into(), self.project.clone().into());
        arguments.insert("model".into(), self.model.clone().into());
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

impl TextEmbedder for DaemonEmbedder {
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(crate::mcp::MAX_MCP_EMBED_TEXTS) {
            let result = self.call(chunk, EMBED_TIMEOUT)?;
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
            out.extend(result.embeddings);
        }
        Ok(out)
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
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            dim: 4,
            model: "m".into(),
        });
        {
            let mut client = DaemonEmbedder::connect_to(&socket, "/p", "m").unwrap();
            assert_eq!(client.dim(), 4);
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
        let (_dir, socket, handle) = spawn_fake(FakeDaemon {
            dim: 4,
            model: "served".into(),
        });
        let err = DaemonEmbedder::connect_to(&socket, "/p", "wanted")
            .err()
            .expect("mismatched model must not connect")
            .to_string();
        assert!(err.contains("daemon refused embed_texts"), "{err}");
        assert!(err.contains("wanted"), "{err}");
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
