use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::{Result, ensure};
use codesage_embed::reranker::Reranker;

pub(crate) const MAX_RERANK_DOCUMENTS: usize = 32;
pub(crate) const MAX_RERANK_TEXT_BYTES: usize = 65_536;
pub(crate) const MAX_RERANK_TOTAL_BYTES: usize = 1_048_576;

pub(crate) fn check_caps(query: &str, documents: &[&str]) -> Result<()> {
    ensure!(
        documents.len() <= MAX_RERANK_DOCUMENTS,
        "rerank_pairs: over cap: document count"
    );
    ensure!(
        query.len() <= MAX_RERANK_TEXT_BYTES,
        "rerank_pairs: over cap: query bytes"
    );
    let mut total = query.len();
    for document in documents {
        ensure!(
            document.len() <= MAX_RERANK_TEXT_BYTES,
            "rerank_pairs: over cap: document bytes"
        );
        total += document.len();
    }
    ensure!(
        total <= MAX_RERANK_TOTAL_BYTES,
        "rerank_pairs: over cap: total bytes"
    );
    Ok(())
}

pub(crate) fn check_scores(scores: &[f32], count: usize) -> Result<()> {
    ensure!(
        scores.len() == count,
        "reranker returned {} scores for {count} documents",
        scores.len()
    );
    ensure!(
        scores.iter().all(|score| score.is_finite()),
        "reranker returned a non-finite score"
    );
    Ok(())
}

enum Backend {
    Private(Box<Reranker>),
    #[cfg(unix)]
    Daemon(Box<daemon::Client>),
}

pub(crate) struct QueryReranker {
    #[cfg(unix)]
    root: PathBuf,
    #[cfg(unix)]
    model: String,
    #[cfg(unix)]
    device: String,
    backend: Backend,
}

impl QueryReranker {
    pub(crate) fn new(root: &Path, model: &str, device: &str) -> Result<Self> {
        #[cfg(unix)]
        if let Some(socket) = crate::daemon::running_daemon_socket()
            && let Some(project) = root.to_str()
        {
            codesage_embed::model::validate_model_allowed(
                model,
                codesage_embed::model::allow_any_model_from_env(),
            )?;
            let client = daemon::Client::connect(&socket, project, model, device)?;
            client.call("", &[])?;
            tracing::info!(socket = %socket.display(), model, "reranking through the running daemon");
            return Ok(Self {
                root: root.into(),
                model: model.into(),
                device: device.into(),
                backend: Backend::Daemon(Box::new(client)),
            });
        }
        Self::private(root, model, device)
    }

    fn private(root: &Path, model: &str, device: &str) -> Result<Self> {
        tracing::info!(project = %root.display(), "reranking privately");
        Ok(Self {
            #[cfg(unix)]
            root: root.into(),
            #[cfg(unix)]
            model: model.into(),
            #[cfg(unix)]
            device: device.into(),
            backend: Backend::Private(Box::new(Reranker::new(model, device)?)),
        })
    }

    pub(crate) fn score_pairs(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(unix)]
        if matches!(self.backend, Backend::Daemon(_))
            && (query.len() > MAX_RERANK_TEXT_BYTES
                || documents.iter().any(|d| d.len() > MAX_RERANK_TEXT_BYTES))
        {
            tracing::warn!("reranker input exceeds daemon byte caps; reranking privately");
            *self = Self::private(&self.root, &self.model, &self.device)?;
        }
        let scores = match &mut self.backend {
            Backend::Private(reranker) => reranker.score_pairs(query, documents)?,
            #[cfg(unix)]
            Backend::Daemon(client) => {
                client.score_pairs(query, documents).inspect_err(|error| {
                    tracing::warn!(error = %format!("{error:#}"), "daemon reranking failed");
                })?
            }
        };
        check_scores(&scores, documents.len())?;
        Ok(scores)
    }
}

#[cfg(unix)]
mod daemon {
    use super::*;
    use crate::mcp::params::RerankPairsResult;
    use anyhow::{Context, bail};
    use rmcp::ServiceExt;
    use rmcp::model::{CallToolRequestParams, ClientRequest, ServerResult};
    use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(120);

    pub(super) struct Client {
        rt: tokio::runtime::Runtime,
        client: RunningService<RoleClient, ()>,
        project: String,
        model: String,
        device: String,
        timeout: Duration,
    }

    impl Client {
        pub(super) fn connect(
            socket: &Path,
            project: &str,
            model: &str,
            device: &str,
        ) -> Result<Self> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let client = rt.block_on(async {
                tokio::time::timeout(TIMEOUT, async {
                    let stream = tokio::net::UnixStream::connect(socket)
                        .await
                        .context("connecting to daemon for reranking")?;
                    ().serve(stream).await.context("reranker MCP handshake")
                })
                .await
                .context("reranker MCP handshake timed out")?
            })?;
            Ok(Self {
                rt,
                client,
                project: project.into(),
                model: model.into(),
                device: device.into(),
                timeout: TIMEOUT,
            })
        }

        pub(super) fn call(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
            check_caps(query, documents)?;
            let args = serde_json::json!({ "project": self.project, "model": self.model, "device": self.device, "query": query, "documents": documents });
            let request = CallToolRequestParams::new("rerank_pairs").with_arguments(
                args.as_object()
                    .context("reranker arguments must be an object")?
                    .clone(),
            );
            let result = self.rt.block_on(async {
                let request = self
                    .client
                    .send_cancellable_request(
                        ClientRequest::CallToolRequest(rmcp::model::Request::new(request)),
                        PeerRequestOptions::with_timeout(self.timeout),
                    )
                    .await
                    .context("sending daemon rerank_pairs")?;
                request
                    .await_response()
                    .await
                    .context("daemon rerank_pairs failed")
            })?;
            let ServerResult::CallToolResult(result) = result else {
                bail!("daemon rerank_pairs returned an unexpected response");
            };
            if result.is_error == Some(true) {
                let message = result
                    .content
                    .iter()
                    .filter_map(|b| b.as_text().map(|t| t.text.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("daemon refused rerank_pairs: {message}");
            }
            let result: RerankPairsResult = serde_json::from_value(
                result
                    .structured_content
                    .context("daemon rerank_pairs returned no structured content")?,
            )
            .context("parsing daemon rerank_pairs result")?;
            ensure!(
                result.model == self.model && result.device == self.device,
                "daemon reranker identity differs from requested model/device"
            );
            check_scores(&result.scores, documents.len())?;
            Ok(result.scores)
        }

        pub(super) fn score_pairs(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
            ensure!(
                query.len() <= MAX_RERANK_TEXT_BYTES
                    && documents.iter().all(|d| d.len() <= MAX_RERANK_TEXT_BYTES),
                "rerank_pairs: input exceeds daemon byte caps"
            );
            let mut result = Vec::with_capacity(documents.len());
            let mut start = 0;
            while start < documents.len() {
                let mut end = start;
                let mut bytes = query.len();
                while end < documents.len()
                    && end - start < MAX_RERANK_DOCUMENTS
                    && bytes + documents[end].len() <= MAX_RERANK_TOTAL_BYTES
                {
                    bytes += documents[end].len();
                    end += 1;
                }
                result.extend(self.call(query, &documents[start..end])?);
                start = end;
            }
            Ok(result)
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            self.client.cancellation_token().cancel();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rmcp::model::{
            CallToolResponse, CallToolResult, ContentBlock, ServerCapabilities, ServerInfo,
        };
        use rmcp::service::RequestContext;
        use rmcp::{ErrorData, RoleServer, ServerHandler};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct FakeDaemon {
            batches: Arc<Mutex<Vec<Vec<String>>>>,
            wrong_identity: bool,
            wrong_count: bool,
            refuse: bool,
            wait_for_cancel: bool,
            cancelled: Arc<std::sync::atomic::AtomicBool>,
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
                assert_eq!(request.name, "rerank_pairs");
                let args = request.arguments.unwrap();
                assert_eq!(args["model"], "reranker");
                assert_eq!(args["device"], "cpu");
                assert_eq!(args["project"], "/project");
                let documents: Vec<String> =
                    serde_json::from_value(args["documents"].clone()).unwrap();
                check_caps(
                    args["query"].as_str().unwrap(),
                    &documents.iter().map(String::as_str).collect::<Vec<_>>(),
                )
                .unwrap();
                self.batches.lock().unwrap().push(documents.clone());
                if self.wait_for_cancel {
                    context.ct.cancelled().await;
                    self.cancelled
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                if self.refuse {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "native inference failed: detail",
                    )])
                    .into());
                }
                let mut scores: Vec<f32> = documents.iter().map(|d| d.len() as f32).collect();
                if self.wrong_count {
                    scores.pop();
                }
                Ok(CallToolResult::structured(serde_json::json!({ "model": if self.wrong_identity { "other" } else { "reranker" }, "device": "cpu", "scores": scores })).into())
            }
        }

        fn spawn_fake(
            fake: FakeDaemon,
        ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<()>) {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("rerank.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let task = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    listener.set_nonblocking(true).unwrap();
                    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    let service = fake.serve(stream).await.unwrap();
                    let _ = service.waiting().await;
                });
            });
            (dir, socket, task)
        }

        #[test]
        fn daemon_batches_preserve_order_and_bound_count_and_bytes() {
            let fake = FakeDaemon::default();
            let batches = fake.batches.clone();
            let (_dir, socket, task) = spawn_fake(fake);
            {
                let client = Client::connect(&socket, "/project", "reranker", "cpu").unwrap();
                let docs: Vec<String> = (1..=70).map(|n| "x".repeat(n)).collect();
                let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
                assert_eq!(
                    client.score_pairs("query", &refs).unwrap(),
                    (1..=70).map(|n| n as f32).collect::<Vec<_>>()
                );
                assert_eq!(
                    batches
                        .lock()
                        .unwrap()
                        .iter()
                        .map(Vec::len)
                        .collect::<Vec<_>>(),
                    [32, 32, 6]
                );
                let query = "q".repeat(MAX_RERANK_TEXT_BYTES);
                let doc = "x".repeat(MAX_RERANK_TEXT_BYTES);
                assert_eq!(
                    client
                        .score_pairs(&query, &[doc.as_str(); 32])
                        .unwrap()
                        .len(),
                    32
                );
                assert_eq!(
                    batches.lock().unwrap()[3..]
                        .iter()
                        .map(Vec::len)
                        .collect::<Vec<_>>(),
                    [15, 15, 2]
                );
            }
            task.join().unwrap();
        }

        #[test]
        fn daemon_errors_and_malformed_responses_are_not_scores() {
            for (fake, expected) in [
                (
                    FakeDaemon {
                        wrong_identity: true,
                        ..Default::default()
                    },
                    "identity differs",
                ),
                (
                    FakeDaemon {
                        wrong_count: true,
                        ..Default::default()
                    },
                    "0 scores for 1",
                ),
                (
                    FakeDaemon {
                        refuse: true,
                        ..Default::default()
                    },
                    "native inference failed: detail",
                ),
            ] {
                let (_dir, socket, task) = spawn_fake(fake);
                {
                    let client = Client::connect(&socket, "/project", "reranker", "cpu").unwrap();
                    let error = format!("{:#}", client.score_pairs("q", &["doc"]).unwrap_err());
                    assert!(error.contains(expected), "{error}");
                }
                task.join().unwrap();
            }
        }

        #[test]
        fn timeout_cancels_remote_request_without_starting_private_inference() {
            let fake = FakeDaemon {
                wait_for_cancel: true,
                ..Default::default()
            };
            let cancelled = fake.cancelled.clone();
            let (_dir, socket, task) = spawn_fake(fake);
            {
                let mut client = Client::connect(&socket, "/project", "reranker", "cpu").unwrap();
                client.timeout = Duration::from_millis(30);
                let error = format!("{:#}", client.score_pairs("q", &["doc"]).unwrap_err());
                assert!(
                    error.contains("timeout") || error.contains("Timeout"),
                    "{error}"
                );
                client.rt.block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .unwrap();
                });
            }
            task.join().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_scores_reject_invalid_data() {
        let text = "x".repeat(MAX_RERANK_TEXT_BYTES);
        check_caps(&text, &[text.as_str(); 15]).unwrap();
        assert!(check_caps(&text, &[text.as_str(); 16]).is_err());
        assert!(check_caps("q", &["x"; MAX_RERANK_DOCUMENTS + 1]).is_err());
        let oversized = format!("{text}x");
        assert!(check_caps(&oversized, &[]).is_err());
        assert!(check_caps("q", &[&oversized]).is_err());
        assert!(check_scores(&[f32::NAN], 1).is_err());
        assert!(check_scores(&[f32::INFINITY], 1).is_err());
        assert!(check_scores(&[], 1).is_err());
    }

    #[test]
    fn private_model_load_failure_is_returned_during_stack_construction() {
        let error = QueryReranker::private(Path::new("/missing"), "missing-model", "cpu")
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("validated-model allowlist"),
            "{error:#}"
        );
    }
}
