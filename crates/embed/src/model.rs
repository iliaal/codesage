use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

use anyhow::{Context, Result};
use ort::session::Session;
use tokenizers::Tokenizer;

use crate::config::{BATCH_SIZE, EmbeddingConfig, MAX_SEQ_LENGTH, PoolingStrategy};

static ORT_INIT: Once = Once::new();
static CUDA_PRELOAD: Once = Once::new();

pub(crate) fn public_nvidia_lib_dirs() -> Vec<PathBuf> {
    discover_nvidia_lib_dirs().clone()
}

/// Where ONNX Runtime and NVIDIA CUDA libraries live on this machine. Resolved once,
/// lazily, via: `CODESAGE_NVIDIA_LIBS` env var → pip site-packages probe → standard
/// system paths. Returns an empty Vec if nothing is found; callers must handle that.
fn discover_nvidia_lib_dirs() -> &'static Vec<PathBuf> {
    static CACHE: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut roots: Vec<PathBuf> = Vec::new();

        if let Ok(explicit) = std::env::var("CODESAGE_NVIDIA_LIBS")
            && !explicit.is_empty()
        {
            roots.push(PathBuf::from(explicit));
        }

        roots.extend(
            probe_python_site_packages()
                .into_iter()
                .map(|p| p.join("nvidia")),
        );

        for sys in [
            "/usr/lib/x86_64-linux-gnu/nvidia",
            "/usr/local/lib/nvidia",
            "/opt/nvidia",
        ] {
            roots.push(PathBuf::from(sys));
        }

        let mut lib_dirs: Vec<PathBuf> = Vec::new();
        for root in &roots {
            if !root.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let lib_dir = entry.path().join("lib");
                    if lib_dir.is_dir() && !lib_dirs.contains(&lib_dir) {
                        lib_dirs.push(lib_dir);
                    }
                }
            }
        }
        lib_dirs
    })
}

/// Best-effort probe of Python `site-packages` directories. Does not fail on
/// missing Python; just returns an empty Vec.
fn probe_python_site_packages() -> Vec<PathBuf> {
    let candidates = ["python3", "python"];
    for py in &candidates {
        let Ok(output) = std::process::Command::new(py)
            .args([
                "-c",
                "import site, sys; \
                 paths = list(site.getsitepackages()); \
                 paths.append(site.getusersitepackages()); \
                 print('\\n'.join(paths))",
            ])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    Vec::new()
}

pub fn preload_cuda_libs() {
    CUDA_PRELOAD.call_once(|| {
        let lib_dirs = discover_nvidia_lib_dirs();
        if lib_dirs.is_empty() {
            return;
        }

        prepend_ld_library_path(lib_dirs);

        let all_libs: Vec<&str> = ort::execution_providers::cuda::CUDA_DYLIBS
            .iter()
            .chain(ort::execution_providers::cuda::CUDNN_DYLIBS.iter())
            .copied()
            .collect();
        for lib_name in all_libs {
            for dir in lib_dirs {
                let path = dir.join(lib_name);
                if path.exists() {
                    if let Err(e) = ort::util::preload_dylib(path) {
                        eprintln!("CUDA preload warning for {lib_name}: {e}");
                    }
                    break;
                }
            }
        }
    });
}

fn prepend_ld_library_path<P: AsRef<Path>>(dirs: &[P]) {
    if dirs.is_empty() {
        return;
    }
    let current = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let joined: Vec<String> = dirs
        .iter()
        .map(|d| d.as_ref().to_string_lossy().to_string())
        .collect();
    let new_val = if current.is_empty() {
        joined.join(":")
    } else {
        format!("{}:{current}", joined.join(":"))
    };
    unsafe { std::env::set_var("LD_LIBRARY_PATH", &new_val) };
}

/// Locate the ONNX Runtime shared library. Order: `ORT_DYLIB_PATH` env var →
/// site-packages `onnxruntime/capi/libonnxruntime.so*` → standard system locations.
fn discover_ort_dylib() -> Option<PathBuf> {
    for base in probe_python_site_packages() {
        let capi = base.join("onnxruntime").join("capi");
        if !capi.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&capi) {
            // Prefer `.so.<version>` over plain `.so` (matches what pip installs).
            let mut best: Option<PathBuf> = None;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("libonnxruntime.so") {
                    let candidate = entry.path();
                    if candidate.is_file() {
                        match best {
                            None => best = Some(candidate),
                            Some(ref prev) if name.len() > prev.file_name().unwrap().len() => {
                                best = Some(candidate);
                            }
                            _ => {}
                        }
                    }
                }
            }
            if best.is_some() {
                return best;
            }
        }
    }

    for sys in [
        "/usr/lib/libonnxruntime.so",
        "/usr/local/lib/libonnxruntime.so",
    ] {
        let p = PathBuf::from(sys);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub(crate) fn init_ort_dylib() {
    ORT_INIT.call_once(|| {
        if std::env::var("ORT_DYLIB_PATH").is_ok() {
            // Caller took control. Still prepend discovered NVIDIA dirs so CUDA loads.
            let nvidia = discover_nvidia_lib_dirs();
            if !nvidia.is_empty() {
                prepend_ld_library_path(nvidia);
            }
            return;
        }

        let Some(ort_path) = discover_ort_dylib() else {
            return;
        };
        unsafe { std::env::set_var("ORT_DYLIB_PATH", &ort_path) };

        let mut extra_dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = ort_path.parent() {
            extra_dirs.push(dir.to_path_buf());
        }
        extra_dirs.extend(discover_nvidia_lib_dirs().iter().cloned());
        prepend_ld_library_path(&extra_dirs);
    });
}

pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    dim: usize,
    pooling: PoolingStrategy,
    has_token_type_ids: bool,
}

impl Embedder {
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        init_ort_dylib();
        eprintln!("Loading embedding model: {}", config.model);

        let api =
            hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
        let repo = api.model(config.model.clone());

        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("failed to download tokenizer.json")?;
        let model_path = repo
            .get("onnx/model.onnx")
            .context("failed to download onnx/model.onnx")?;
        let _ = repo.get("onnx/model.onnx_data"); // some models have external data

        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_LENGTH,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        let mut builder = Session::builder()?;

        let want_cuda = config.device == "gpu" || config.device == "cuda";
        if want_cuda {
            #[cfg(feature = "cuda")]
            {
                preload_cuda_libs();
                builder = builder
                    .with_execution_providers([
                        ort::execution_providers::CUDAExecutionProvider::default()
                            .build()
                            .error_on_failure(),
                    ])
                    .map_err(|e| anyhow::anyhow!("CUDA provider failed to register: {e}"))?;
            }
            #[cfg(not(feature = "cuda"))]
            {
                anyhow::bail!(
                    "GPU requested but binary built without cuda feature. Rebuild with: cargo build --features cuda"
                );
            }
        }

        let session = builder.commit_from_file(&model_path)?;

        let has_token_type_ids = session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids");

        let dim = detect_dim(&session)?;
        let pooling = config.pooling_strategy();

        eprintln!(
            "Embedding model loaded (dim={dim}, pooling={pooling:?}, token_type_ids={has_token_type_ids})"
        );

        Ok(Self {
            session,
            tokenizer,
            dim,
            pooling,
            has_token_type_ids,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let batch = self.embed_batch(&[text])?;
        Ok(batch.into_iter().next().unwrap())
    }

    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        for batch_start in (0..texts.len()).step_by(BATCH_SIZE) {
            let batch_end = (batch_start + BATCH_SIZE).min(texts.len());
            let batch = &texts[batch_start..batch_end];
            let batch_embs = self.embed_batch_inner(batch)?;
            all_embeddings.extend(batch_embs);
        }

        Ok(all_embeddings)
    }

    fn embed_batch_inner(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let batch_size = encodings.len();
        let seq_len = encodings[0].get_ids().len();

        let mut input_ids = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask = Vec::with_capacity(batch_size * seq_len);

        for enc in &encodings {
            input_ids.extend(enc.get_ids().iter().map(|&id| id as i64));
            attention_mask.extend(enc.get_attention_mask().iter().map(|&m| m as i64));
        }

        let ids_tensor = ort::value::Tensor::from_array(([batch_size, seq_len], input_ids))?;
        let mask_tensor =
            ort::value::Tensor::from_array(([batch_size, seq_len], attention_mask.clone()))?;

        let outputs = if self.has_token_type_ids {
            let token_type_ids = vec![0i64; batch_size * seq_len];
            let type_tensor =
                ort::value::Tensor::from_array(([batch_size, seq_len], token_type_ids))?;
            self.session.run(ort::inputs![
                "input_ids" => ids_tensor,
                "token_type_ids" => type_tensor,
                "attention_mask" => mask_tensor,
            ])?
        } else {
            self.session.run(ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
            ])?
        };

        let (_shape, hidden) = outputs[0].try_extract_tensor::<f32>()?;

        let mut embeddings = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let pooled = match self.pooling {
                PoolingStrategy::Mean => {
                    let mut vec = vec![0.0f32; self.dim];
                    let mut mask_sum = 0.0f32;
                    for j in 0..seq_len {
                        let m = attention_mask[i * seq_len + j] as f32;
                        mask_sum += m;
                        let offset = (i * seq_len + j) * self.dim;
                        for k in 0..self.dim {
                            vec[k] += hidden[offset + k] * m;
                        }
                    }
                    if mask_sum > 0.0 {
                        for v in &mut vec {
                            *v /= mask_sum;
                        }
                    }
                    vec
                }
                PoolingStrategy::Cls => {
                    let offset = i * seq_len * self.dim;
                    hidden[offset..offset + self.dim].to_vec()
                }
            };

            let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
            let mut normalized = pooled;
            if norm > 0.0 {
                for v in &mut normalized {
                    *v /= norm;
                }
            }

            embeddings.push(normalized);
        }

        Ok(embeddings)
    }
}

fn detect_dim(session: &Session) -> Result<usize> {
    let output = &session.outputs()[0];
    if let ort::value::ValueType::Tensor { shape, .. } = output.dtype()
        && let Some(&d) = shape.last()
        && d > 0
    {
        return Ok(d as usize);
    }
    Ok(crate::config::DEFAULT_EMBEDDING_DIM)
}
