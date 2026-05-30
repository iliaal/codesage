use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use ort::session::Session;
use tokenizers::Tokenizer;
use wait_timeout::ChildExt;

use crate::config::{BATCH_SIZE, EmbeddingConfig, MAX_SEQ_LENGTH, PoolingStrategy, wants_cuda};

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

/// Maximum time to wait for the Python site-packages probe before killing the
/// child and falling back. The probe runs inside `OnceLock::get_or_init`, so a
/// hung interpreter would block every thread waiting on `Embedder::new` /
/// `Reranker::new` indefinitely.
const PROBE_PYTHON_TIMEOUT: Duration = Duration::from_secs(10);

/// Best-effort probe of Python `site-packages` directories. Does not fail on
/// missing Python; just returns an empty Vec. Bounded by `PROBE_PYTHON_TIMEOUT`
/// so a wedged interpreter can't deadlock the whole process.
fn probe_python_site_packages() -> Vec<PathBuf> {
    static CACHE: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let candidates = ["python3", "python"];
            for py in &candidates {
                if let Some(paths) = probe_one(py) {
                    return paths;
                }
            }
            Vec::new()
        })
        .clone()
}

fn probe_one(py: &str) -> Option<Vec<PathBuf>> {
    let mut child = std::process::Command::new(py)
        .args([
            "-c",
            "import site, sys; \
             paths = list(site.getsitepackages()); \
             paths.append(site.getusersitepackages()); \
             print('\\n'.join(paths))",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let status = match child.wait_timeout(PROBE_PYTHON_TIMEOUT) {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(
                interpreter = py,
                timeout_ms = PROBE_PYTHON_TIMEOUT.as_millis() as u64,
                "python site-packages probe timed out; killing child and skipping"
            );
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Err(e) => {
            tracing::warn!(interpreter = py, error = %e, "python probe wait failed");
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take()
        && out.read_to_string(&mut stdout).is_err()
    {
        return None;
    }
    Some(
        stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
    )
}

/// Verifies that the current Linux process has the CUDA runtime + cuDNN +
/// cuBLAS libraries actually mapped, after a session has been built with
/// `device = "gpu"`. The reason for this check, from a 2026-05-02 incident:
/// ORT can register a CUDA execution provider successfully (logs "Successfully
/// registered CUDAExecutionProvider") and still run inference entirely on CPU
/// when the underlying CUDA loader couldn't bind the shared libraries — and
/// it does so silently. The visible symptom was a 10+ minute reindex on a
/// 256-file project that should have taken ~10s on GPU. This check makes
/// that failure mode loud.
#[cfg(all(feature = "cuda", target_os = "linux"))]
pub(crate) fn require_cuda_libs_mapped() -> anyhow::Result<()> {
    let maps = std::fs::read_to_string("/proc/self/maps")
        .context("reading /proc/self/maps to verify CUDA libraries are loaded")?;
    let has_libcuda = maps.contains("libcuda.so");
    let has_cudart = maps.contains("libcudart.so");
    // Either cuDNN or cuBLAS is enough evidence the CUDA EP is functional;
    // they get pulled in by the first GPU op, not at session-create time.
    let has_dnn_or_blas = maps.contains("libcudnn") || maps.contains("libcublas");

    if !has_libcuda || !has_cudart {
        anyhow::bail!(
            "GPU was requested (device = \"gpu\") but CUDA runtime libraries are not loaded \
             into this process — ORT silently fell back to CPU. Refusing to continue.\n\
             Missing: libcuda={missing_libcuda}, libcudart={missing_cudart}\n\
             Fixes: (1) confirm the binary was built with `--features cuda`; \
             (2) install nvidia-*-cu12 pip packages (cuda-runtime, cudnn, cublas, cudart, \
             nvrtc); (3) set CODESAGE_NVIDIA_LIBS to the directory containing them; \
             (4) check `codesage doctor` for nvidia-lib discovery details. \
             To override (e.g. for tests), set CODESAGE_ALLOW_CPU_FALLBACK=1.",
            missing_libcuda = !has_libcuda,
            missing_cudart = !has_cudart,
        );
    }
    if !has_dnn_or_blas {
        tracing::warn!(
            "CUDA runtime is mapped but neither libcudnn nor libcublas were found in /proc/self/maps. \
             Inference may fall back to CPU on attention ops."
        );
    }
    Ok(())
}

/// One-shot startup hook: must be called from `main` before any tokio runtime
/// or background thread is spawned. Resolves the ONNX Runtime + NVIDIA library
/// locations and writes `LD_LIBRARY_PATH` / `ORT_DYLIB_PATH`. The underlying
/// `std::env::set_var` calls are `unsafe` under Rust 2024 because concurrent
/// `getenv` from another thread is UB; pinning the work to single-threaded
/// startup eliminates the race even though the syntactic `unsafe` remains.
///
/// `Embedder::new` / `Reranker::new` still call these helpers under
/// `Once::call_once` as a defensive fallback (so direct library users aren't
/// silently broken), but in the bin path the work has already happened by
/// then and the call is a cheap no-op.
pub fn init_for_main() {
    init_ort_dylib();
    #[cfg(feature = "cuda")]
    {
        preload_cuda_libs();
    }
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
                        tracing::warn!(lib = lib_name, error = %e, "CUDA preload failed");
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

pub fn init_ort_dylib() {
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

/// Shared model-loading path for [`Embedder`] and [`Reranker`]: downloads the
/// tokenizer + ONNX model (and the optional external-weights sidecar), builds a
/// padding/truncating tokenizer, creates an ORT session — registering the CUDA
/// execution provider when `device` requests the GPU — and asserts the CUDA
/// libraries actually mapped (the silent-CPU-fallback guard). Returns the
/// session, tokenizer, and whether the model takes a `token_type_ids` input.
pub(crate) fn load_onnx_session(model: &str, device: &str) -> Result<(Session, Tokenizer, bool)> {
    init_ort_dylib();

    let want_cuda = wants_cuda(device);
    if want_cuda {
        #[cfg(not(feature = "cuda"))]
        {
            anyhow::bail!(
                "GPU requested but binary built without cuda feature. Rebuild with: cargo build --features cuda"
            );
        }
    }

    let api = hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
    let repo = api.model(model.to_string());

    let tokenizer_path = repo
        .get("tokenizer.json")
        .context("failed to download tokenizer.json")?;
    let model_path = repo
        .get("onnx/model.onnx")
        .context("failed to download onnx/model.onnx")?;
    // External-weights sidecar (>2GB models like Jina v2 base, BGE-large).
    // Most models don't have this file — a 404 is the expected outcome and
    // not worth surfacing. A real failure (network, disk, permission) is
    // worth a debug-level breadcrumb so users running with RUST_LOG=debug
    // don't have to guess when commit_from_file later errors with an
    // opaque ORT external-data load failure.
    if let Err(e) = repo.get("onnx/model.onnx_data") {
        tracing::debug!(error = %e, "onnx/model.onnx_data not fetched (normal for small models)");
    }

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
    }

    let session = builder.commit_from_file(&model_path)?;

    // Hard-fail when device = "gpu" was requested but ORT silently fell back to
    // CPU. Failure mode observed 2026-05-02 on a flip-all script: CUDA
    // registration logged "Successfully registered" but the process had ZERO
    // cuda libs mapped per /proc/self/maps and ran for 10+ minutes on a
    // 256-file project. Rather than time-based heuristics, assert the CUDA
    // loader actually ran by checking the process has libcuda + libcudart +
    // (libcudnn OR libcublas) mapped. No-op on non-Linux. Bypass with
    // CODESAGE_ALLOW_CPU_FALLBACK=1 (e.g. for unit tests).
    #[cfg(all(feature = "cuda", target_os = "linux"))]
    if want_cuda
        && !matches!(
            std::env::var("CODESAGE_ALLOW_CPU_FALLBACK").as_deref(),
            Ok("1") | Ok("true")
        )
    {
        require_cuda_libs_mapped()?;
    }

    let has_token_type_ids = session
        .inputs()
        .iter()
        .any(|i| i.name() == "token_type_ids");

    Ok((session, tokenizer, has_token_type_ids))
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
        tracing::info!(model = %config.model, "loading embedding model");
        let (session, tokenizer, has_token_type_ids) =
            load_onnx_session(&config.model, &config.device)?;
        let dim = detect_dim(&session)?;
        let pooling = config.pooling_strategy();

        tracing::info!(
            dim,
            pooling = ?pooling,
            token_type_ids = has_token_type_ids,
            "embedding model loaded"
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

        for batch in texts.chunks(BATCH_SIZE) {
            all_embeddings.extend(self.embed_batch_inner(batch)?);
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
    // Refuse to guess. A silent fallback to 384 stores wrong-dimension
    // vectors (or the right dimension by coincidence with MiniLM-L6),
    // and `search_knn` then returns either an error or noise — either way,
    // the failure shows up far from the cause. Better to fail at load time.
    anyhow::bail!(
        "could not infer embedding dimension from model output shape; \
         the model's first output tensor has no static last-dimension. \
         Refusing to fall back to a default — index would store \
         wrong-dimension vectors. Set the model explicitly in \
         .codesage/config.toml to one with a static output shape."
    )
}
