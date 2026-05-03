use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use ureq::Agent;

use crate::config::{BATCH_SIZE, EmbeddingBackendKind, EmbeddingConfig};
use crate::model::{EmbeddingBackend, normalize_embedding};

const DIMENSION_PROBE_TEXT: &str = "codesage embedding dimension probe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpEmbeddingProtocol {
    Ollama,
    OpenAiCompatible,
}

impl HttpEmbeddingProtocol {
    fn from_backend(kind: EmbeddingBackendKind) -> Result<Self> {
        match kind {
            EmbeddingBackendKind::Ollama => Ok(Self::Ollama),
            EmbeddingBackendKind::OpenAiCompatible => Ok(Self::OpenAiCompatible),
            EmbeddingBackendKind::Onnx => bail!("ONNX is not an HTTP embedding backend"),
        }
    }
}

pub(crate) struct HttpEmbedder {
    agent: Agent,
    endpoint_url: String,
    model: String,
    protocol: HttpEmbeddingProtocol,
    dim: usize,
    storage_model_id: String,
    dimensions: Option<usize>,
    api_key_env: Option<String>,
}

impl HttpEmbedder {
    pub(crate) fn new(config: &EmbeddingConfig, kind: EmbeddingBackendKind) -> Result<Self> {
        let protocol = HttpEmbeddingProtocol::from_backend(kind)?;
        let base_url = config.provider_base_url(kind);
        let endpoint_url = endpoint_url(&base_url, protocol);
        let agent_config = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build();

        let mut embedder = Self {
            agent: Agent::new_with_config(agent_config),
            endpoint_url,
            model: config.model.clone(),
            protocol,
            dim: 0,
            storage_model_id: config.storage_model_id()?,
            dimensions: config.dimensions,
            api_key_env: config.api_key_env.clone(),
        };

        let probe = embedder.embed_batch_raw(&[DIMENSION_PROBE_TEXT])?;
        let dim = probe
            .first()
            .map(Vec::len)
            .filter(|d| *d > 0)
            .ok_or_else(|| anyhow::anyhow!("HTTP embedding backend returned an empty probe"))?;
        if let Some(expected) = config.dimensions {
            ensure!(
                expected == dim,
                "configured embedding dimensions ({expected}) did not match HTTP backend output ({dim})"
            );
        }
        embedder.dim = dim;

        tracing::info!(
            backend = ?kind,
            endpoint = %embedder.endpoint_url,
            model = %embedder.model,
            dim,
            "HTTP embedding model loaded"
        );

        Ok(embedder)
    }

    fn embed_batch_raw(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = match self.protocol {
            HttpEmbeddingProtocol::Ollama => json!({
                "model": self.model,
                "input": texts,
            }),
            HttpEmbeddingProtocol::OpenAiCompatible => {
                let mut body = Map::new();
                body.insert("model".to_string(), json!(self.model));
                body.insert("input".to_string(), json!(texts));
                if let Some(dimensions) = self.dimensions {
                    body.insert("dimensions".to_string(), json!(dimensions));
                }
                Value::Object(body)
            }
        };

        let mut request = self.agent.post(&self.endpoint_url);
        if let Some(env_name) = &self.api_key_env {
            let api_key = std::env::var(env_name)
                .with_context(|| format!("reading API key from environment variable {env_name}"))?;
            ensure!(
                !api_key.trim().is_empty(),
                "environment variable {env_name} is configured for embedding auth but is empty"
            );
            request = request.header("Authorization", format!("Bearer {api_key}"));
        }

        let mut response = request
            .send_json(&body)
            .with_context(|| format!("calling embedding endpoint {}", self.endpoint_url))?;
        let response: Value = response
            .body_mut()
            .read_json()
            .context("parsing embedding HTTP response as JSON")?;

        let embeddings = match self.protocol {
            HttpEmbeddingProtocol::Ollama => parse_ollama_embeddings(response)?,
            HttpEmbeddingProtocol::OpenAiCompatible => parse_openai_embeddings(response)?,
        };
        normalize_and_validate(embeddings, texts.len())
    }
}

impl EmbeddingBackend for HttpEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn storage_model_id(&self) -> &str {
        &self.storage_model_id
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());
        for batch_start in (0..texts.len()).step_by(BATCH_SIZE) {
            let batch_end = (batch_start + BATCH_SIZE).min(texts.len());
            let batch = &texts[batch_start..batch_end];
            all_embeddings.extend(self.embed_batch_raw(batch)?);
        }
        Ok(all_embeddings)
    }
}

fn endpoint_url(base_url: &str, protocol: HttpEmbeddingProtocol) -> String {
    let base = base_url.trim_end_matches('/');
    match protocol {
        HttpEmbeddingProtocol::Ollama if base.ends_with("/api/embed") => base.to_string(),
        HttpEmbeddingProtocol::Ollama if base.ends_with("/api") => format!("{base}/embed"),
        HttpEmbeddingProtocol::Ollama => format!("{base}/api/embed"),
        HttpEmbeddingProtocol::OpenAiCompatible if base.ends_with("/v1/embeddings") => {
            base.to_string()
        }
        HttpEmbeddingProtocol::OpenAiCompatible if base.ends_with("/v1") => {
            format!("{base}/embeddings")
        }
        HttpEmbeddingProtocol::OpenAiCompatible => format!("{base}/v1/embeddings"),
    }
}

fn normalize_and_validate(
    embeddings: Vec<Vec<f32>>,
    expected_count: usize,
) -> Result<Vec<Vec<f32>>> {
    ensure!(
        embeddings.len() == expected_count,
        "embedding endpoint returned {} vectors for {} inputs",
        embeddings.len(),
        expected_count
    );

    let mut expected_dim = None;
    let mut normalized = Vec::with_capacity(embeddings.len());
    for embedding in embeddings {
        ensure!(
            !embedding.is_empty(),
            "embedding endpoint returned an empty vector"
        );
        match expected_dim {
            Some(dim) => ensure!(
                embedding.len() == dim,
                "embedding endpoint returned inconsistent dimensions: expected {dim}, got {}",
                embedding.len()
            ),
            None => expected_dim = Some(embedding.len()),
        }
        normalized.push(normalize_embedding(embedding));
    }
    Ok(normalized)
}

#[derive(Deserialize)]
struct OllamaEmbeddingResponse {
    embeddings: Vec<Vec<f32>>,
}

fn parse_ollama_embeddings(response: Value) -> Result<Vec<Vec<f32>>> {
    let parsed: OllamaEmbeddingResponse =
        serde_json::from_value(response).context("parsing Ollama embedding response")?;
    Ok(parsed.embeddings)
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingRow>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingRow {
    embedding: Vec<f32>,
    index: Option<usize>,
}

fn parse_openai_embeddings(response: Value) -> Result<Vec<Vec<f32>>> {
    let parsed: OpenAiEmbeddingResponse =
        serde_json::from_value(response).context("parsing OpenAI-compatible embedding response")?;
    let mut rows: Vec<(usize, Vec<f32>)> = parsed
        .data
        .into_iter()
        .enumerate()
        .map(|(fallback_index, row)| (row.index.unwrap_or(fallback_index), row.embedding))
        .collect();
    rows.sort_by_key(|(index, _)| *index);
    Ok(rows.into_iter().map(|(_, embedding)| embedding).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn spawn_ollama_test_server(request_count: usize) -> (String, thread::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let (path, body) = read_http_request(&mut stream);
                assert_eq!(path, "/api/embed");
                let input_count = body
                    .get("input")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(1);
                bodies.push(body);

                let response = json!({
                    "model": "embeddinggemma",
                    "embeddings": vec![vec![3.0, 4.0]; input_count],
                });
                write_json_response(&mut stream, &response);
            }
            bodies
        });
        (base_url, handle)
    }

    fn read_http_request(stream: &mut TcpStream) -> (String, Value) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut tmp).unwrap();
            assert!(n > 0, "connection closed before request headers completed");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                break pos;
            }
        };

        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let request_line = headers.lines().next().unwrap();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .expect("request path")
            .to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);

        let body_start = header_end + b"\r\n\r\n".len();
        while buf.len() < body_start + content_length {
            let n = stream.read(&mut tmp).unwrap();
            assert!(n > 0, "connection closed before request body completed");
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = serde_json::from_slice(&buf[body_start..body_start + content_length]).unwrap();
        (path, body)
    }

    fn write_json_response(stream: &mut TcpStream, value: &Value) {
        let body = value.to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        stream.flush().unwrap();
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn ollama_endpoint_url_appends_api_embed() {
        assert_eq!(
            endpoint_url("http://localhost:11434/", HttpEmbeddingProtocol::Ollama),
            "http://localhost:11434/api/embed"
        );
        assert_eq!(
            endpoint_url("http://localhost:11434/api", HttpEmbeddingProtocol::Ollama),
            "http://localhost:11434/api/embed"
        );
        assert_eq!(
            endpoint_url(
                "http://localhost:11434/api/embed",
                HttpEmbeddingProtocol::Ollama
            ),
            "http://localhost:11434/api/embed"
        );
    }

    #[test]
    fn openai_endpoint_url_appends_v1_embeddings() {
        assert_eq!(
            endpoint_url(
                "http://localhost:8080",
                HttpEmbeddingProtocol::OpenAiCompatible
            ),
            "http://localhost:8080/v1/embeddings"
        );
        assert_eq!(
            endpoint_url(
                "http://localhost:8080/v1/",
                HttpEmbeddingProtocol::OpenAiCompatible
            ),
            "http://localhost:8080/v1/embeddings"
        );
        assert_eq!(
            endpoint_url(
                "http://localhost:8080/v1/embeddings",
                HttpEmbeddingProtocol::OpenAiCompatible
            ),
            "http://localhost:8080/v1/embeddings"
        );
    }

    #[test]
    fn parses_ollama_embeddings() {
        let response = json!({
            "model": "embeddinggemma",
            "embeddings": [[3.0, 4.0], [0.0, 2.0]]
        });

        let embeddings = parse_ollama_embeddings(response).unwrap();

        assert_eq!(embeddings, vec![vec![3.0, 4.0], vec![0.0, 2.0]]);
    }

    #[test]
    fn parses_openai_embeddings_by_index_order() {
        let response = json!({
            "object": "list",
            "data": [
                {"object": "embedding", "index": 1, "embedding": [0.0, 2.0]},
                {"object": "embedding", "index": 0, "embedding": [3.0, 4.0]}
            ],
            "model": "local-embed"
        });

        let embeddings = parse_openai_embeddings(response).unwrap();

        assert_eq!(embeddings, vec![vec![3.0, 4.0], vec![0.0, 2.0]]);
    }

    #[test]
    fn ollama_embedder_probes_dimension_and_embeds_batches_over_http() {
        let (base_url, handle) = spawn_ollama_test_server(2);
        let config = EmbeddingConfig {
            backend: "ollama".to_string(),
            model: "embeddinggemma".to_string(),
            base_url: Some(base_url),
            ..EmbeddingConfig::default()
        };

        let mut embedder = HttpEmbedder::new(&config, EmbeddingBackendKind::Ollama).unwrap();
        let embeddings = embedder.embed_batch(&["alpha", "beta"]).unwrap();
        let requests = handle.join().unwrap();

        assert_eq!(embedder.dim(), 2);
        assert_eq!(embeddings.len(), 2);
        assert!((embeddings[0][0] - 0.6).abs() < 0.0001);
        assert!((embeddings[0][1] - 0.8).abs() < 0.0001);
        assert_eq!(requests[0]["model"], "embeddinggemma");
        assert_eq!(requests[1]["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn normalize_and_validate_returns_unit_vectors() {
        let embeddings = normalize_and_validate(vec![vec![3.0, 4.0]], 1).unwrap();

        assert!((embeddings[0][0] - 0.6).abs() < 0.0001);
        assert!((embeddings[0][1] - 0.8).abs() < 0.0001);
    }

    #[test]
    fn normalize_and_validate_rejects_count_mismatch() {
        let err = normalize_and_validate(vec![vec![1.0, 0.0]], 2).unwrap_err();

        assert!(
            err.to_string()
                .contains("embedding endpoint returned 1 vectors for 2 inputs")
        );
    }

    #[test]
    fn normalize_and_validate_rejects_inconsistent_dimensions() {
        let err = normalize_and_validate(vec![vec![1.0, 0.0], vec![1.0]], 2).unwrap_err();

        assert!(
            err.to_string()
                .contains("embedding endpoint returned inconsistent dimensions")
        );
    }
}
