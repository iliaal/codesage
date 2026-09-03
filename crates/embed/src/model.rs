use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "cuda", not(target_vendor = "apple")))]
use std::sync::Once;
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use hf_hub::{Repo, RepoType};
use ort::session::Session;
use tokenizers::Tokenizer;
use wait_timeout::ChildExt;

use crate::config::{EmbeddingConfig, MAX_SEQ_LENGTH, PoolingStrategy, wants_coreml, wants_cuda};

#[cfg(not(target_vendor = "apple"))]
static ORT_INIT: Once = Once::new();
#[cfg(feature = "cuda")]
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

        collect_nvidia_lib_dirs(&roots)
    })
}

fn collect_nvidia_lib_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut lib_dirs: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        if looks_like_cuda_lib_dir(root) && !lib_dirs.contains(root) {
            lib_dirs.push(root.clone());
        }
        let root_lib = root.join("lib");
        if root_lib.is_dir() && !lib_dirs.contains(&root_lib) {
            lib_dirs.push(root_lib);
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
}

fn looks_like_cuda_lib_dir(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(is_cuda_shared_library_name)
    })
}

fn is_cuda_shared_library_name(name: &str) -> bool {
    [
        "libcuda.so",
        "libcudart.so",
        "libcublas.so",
        "libcudnn.so",
        "libcufft.so",
        "libcurand.so",
        "libnvrtc.so",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// Maximum time to wait for the Python site-packages probe before killing the
/// child and falling back. The probe runs inside `OnceLock::get_or_init`, so a
/// hung interpreter would block every thread waiting on `Embedder::new` /
/// `Reranker::new` indefinitely.
const PROBE_PYTHON_TIMEOUT: Duration = Duration::from_secs(10);
const HF_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const HF_DOWNLOAD_TIMEOUT_ENV: &str = "CODESAGE_HF_DOWNLOAD_TIMEOUT_SECS";

fn hf_download_timeout_from_env_value(value: Option<&str>) -> Result<Duration> {
    let Some(value) = value else {
        return Ok(HF_DOWNLOAD_TIMEOUT);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(HF_DOWNLOAD_TIMEOUT);
    }
    let secs: u64 = value.parse().with_context(|| {
        format!("{HF_DOWNLOAD_TIMEOUT_ENV} must be a positive integer number of seconds")
    })?;
    if secs == 0 {
        anyhow::bail!("{HF_DOWNLOAD_TIMEOUT_ENV} must be greater than zero seconds");
    }
    Ok(Duration::from_secs(secs))
}

fn hf_download_timeout() -> Duration {
    match hf_download_timeout_from_env_value(std::env::var(HF_DOWNLOAD_TIMEOUT_ENV).ok().as_deref())
    {
        Ok(timeout) => timeout,
        Err(e) => {
            tracing::warn!(
                error = %e,
                fallback_secs = HF_DOWNLOAD_TIMEOUT.as_secs(),
                "invalid HuggingFace download timeout; using default"
            );
            HF_DOWNLOAD_TIMEOUT
        }
    }
}

/// Runs `work` on a worker thread and waits at most [`hf_download_timeout`]
/// for its result. hf-hub's client has no overall deadline, so a hub that
/// accepts the connection and then stalls would otherwise pin the calling
/// thread (a daemon request thread) indefinitely; the worker is abandoned on
/// timeout and exits on its own when the request does. `action` names the
/// operation in the timeout message ("downloading onnx/model.onnx").
fn hf_with_deadline<T: Send + 'static>(
    action: &str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let timeout = hf_download_timeout();
    let (tx, rx) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let _ = tx.send(work());
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!(
                "timed out after {}s {action} from HuggingFace; \
                 set {HF_DOWNLOAD_TIMEOUT_ENV} to adjust the limit",
                timeout.as_secs()
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("HuggingFace worker exited before {action}")
        }
    }
}

/// The HuggingFace client every download and file-list probe shares, built
/// from the same [`hf_cache_from_env`] root the cached-only probe
/// ([`cached_model_artifacts`]) consults — never `Api::new`, whose
/// `Cache::default` panics HOME-less (`dirs::home_dir().expect(..)`) and
/// ignores `HF_HOME`, so a custom cache root would download into one
/// directory while the probe reads another. Unresolvable environments
/// (neither `HF_HOME` nor `HOME` set) are an error naming both variables,
/// raised here rather than as a worker-thread panic surfacing as a
/// misleading `RecvTimeoutError::Disconnected`.
fn hf_api() -> Result<hf_hub::api::sync::Api> {
    let Some(cache) = hf_cache_from_env() else {
        anyhow::bail!(
            "cannot resolve the HuggingFace cache directory: HF_HOME is unset and HOME is \
             unset or empty, so neither an explicit cache root nor the ~/.cache/huggingface \
             fallback resolves; set HF_HOME (or HOME) to a writable directory"
        );
    };
    // Mirror `ApiBuilder::from_env`'s endpoint override without its
    // `Cache::from_env` fallback, which panics HOME-less via `Cache::default`.
    let mut builder = hf_hub::api::sync::ApiBuilder::from_cache(cache);
    if let Ok(endpoint) = std::env::var("HF_ENDPOINT") {
        builder = builder.with_endpoint(endpoint);
    }
    builder
        .build()
        .context("failed to create HuggingFace API client")
}

fn hf_api_repo(model: String, revision: Option<String>) -> Result<hf_hub::api::sync::ApiRepo> {
    let api = hf_api()?;
    Ok(if let Some(revision) = revision {
        api.repo(Repo::with_revision(model, RepoType::Model, revision))
    } else {
        api.model(model)
    })
}

fn hf_get_model_file(
    model: &str,
    revision: Option<&str>,
    artifact: &'static str,
) -> Result<PathBuf> {
    let model = model.to_string();
    let revision = revision.map(str::to_string);
    hf_with_deadline(&format!("downloading {artifact}"), move || {
        hf_api_repo(model, revision)?
            .get(artifact)
            .with_context(|| format!("failed to download {artifact}"))
    })
}

/// Whether the repository ships `onnx/model.onnx_data` at `revision`,
/// according to the hub's file list for that revision (`Repo::api_url`
/// embeds `revision/<rev>`, so the answer is for the pinned snapshot, not
/// `main`). One metadata request; does not download anything.
fn hf_sidecar_listed(model: &str, revision: Option<&str>) -> Result<bool> {
    let model = model.to_string();
    let revision = revision.map(str::to_string);
    hf_with_deadline(&format!("listing the files of {model}"), move || {
        let info = hf_api_repo(model.clone(), revision)?
            .info()
            .with_context(|| format!("failed to list the files of {model}"))?;
        Ok(info
            .siblings
            .iter()
            .any(|sibling| sibling.rfilename == ONNX_DATA_ARTIFACT))
    })
}

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
            .filter_map(validate_site_packages_dir)
            .collect(),
    )
}

fn validate_site_packages_dir(line: &str) -> Option<PathBuf> {
    let path = PathBuf::from(line);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return None;
    }
    let name = path.file_name().and_then(|n| n.to_str())?;
    if name != "site-packages" && name != "dist-packages" {
        return None;
    }
    Some(path)
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
    verify_cuda_libs_mapped(&maps)
}

#[cfg(all(feature = "cuda", target_os = "linux"))]
fn verify_cuda_libs_mapped(maps: &str) -> anyhow::Result<()> {
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
        anyhow::bail!(
            "GPU was requested (device = \"gpu\") but neither libcudnn nor libcublas \
             is loaded into this process — ORT may be running inference on CPU. \
             Refusing to continue.\n\
             Missing: libcudnn=true, libcublas=true\n\
             Fixes: install nvidia-cudnn-cu12 and nvidia-cublas-cu12, set \
             CODESAGE_NVIDIA_LIBS to the directory containing them, and check \
             `codesage doctor` for nvidia-lib discovery details. \
             To override (e.g. for tests), set CODESAGE_ALLOW_CPU_FALLBACK=1."
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
/// This is the environment-only half of startup and costs microseconds. The
/// expensive half — dlopen of the CUDA/cuDNN stack — lives in
/// [`preload_native_libs`], which writes no environment and therefore runs
/// lazily from the session loader, so a command that never builds a session
/// never maps those libraries.
///
/// `load_onnx_session` still calls `init_ort_dylib` under `Once::call_once` as
/// a defensive fallback (so direct library users aren't silently broken), but
/// in the bin path the work has already happened by then and the call is a
/// cheap no-op.
pub fn init_for_main() {
    #[cfg(not(target_vendor = "apple"))]
    init_ort_dylib();
}

/// dlopen the CUDA/cuDNN stack from the discovered NVIDIA library
/// directories, once per process. Maps ~200 MB RSS and ~1.9 GB virtual, so
/// it runs only from the session loader when a session actually requests the
/// CUDA execution provider — never at startup.
///
/// Writes no environment variables: this runs from whatever thread builds the
/// session, where a `set_var` would race every concurrent `getenv` in the
/// process. Library-path discovery is the job of [`init_for_main`].
#[cfg(feature = "cuda")]
pub fn preload_native_libs() {
    CUDA_PRELOAD.call_once(|| {
        let lib_dirs = discover_nvidia_lib_dirs();
        if lib_dirs.is_empty() {
            return;
        }

        // ort::ep::cuda::preload_dylibs accepts one CUDA root and one cuDNN root,
        // but pip's nvidia-* packages place the libraries in separate directories.
        let all_libs: Vec<&str> = ort::ep::cuda::CUDA_DYLIBS
            .iter()
            .chain(ort::ep::cuda::CUDNN_DYLIBS.iter())
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
#[cfg(not(target_vendor = "apple"))]
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

/// `ORT_DYLIB_PATH` as the loader honours it: an empty value is the same as
/// unset (an empty path would otherwise count as "caller took control" in
/// `init_ort_dylib` and then hard-fail inside ORT, where unsetting it would
/// have discovered the library). One helper so discovery and the
/// fingerprint's runtime probe agree on what counts as set.
#[cfg(not(target_vendor = "apple"))]
fn ort_dylib_path_from_env() -> Option<PathBuf> {
    std::env::var_os("ORT_DYLIB_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Runtime ONNX Runtime dylib discovery (`ORT_DYLIB_PATH`, pip site-packages).
/// No-op on Apple targets: macOS builds statically link ORT with the CoreML EP.
#[cfg(not(target_vendor = "apple"))]
pub fn init_ort_dylib() {
    ORT_INIT.call_once(|| {
        if ort_dylib_path_from_env().is_some() {
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

/// Embedding / reranker models this project has validated end-to-end. The
/// model name comes from the indexed repo's own `.codesage/config.toml`, so
/// it is attacker-controlled when indexing a cloned repo; passing it straight
/// to hf-hub would let that repo pick an arbitrary ONNX graph to download and
/// load into the native ONNX Runtime. Gate every load on this allowlist plus
/// pinned artifact hashes unless the user (not the repo) activates the
/// per-project bypass: `CODESAGE_ALLOW_ANY_MODEL=1` for eligibility plus the
/// project's canonical root listed in the user-owned allowlist file (see
/// [`allow_any_model_from_env`]). The env var alone never suffices.
const ALLOWED_MODELS: &[&str] = &[
    "sentence-transformers/all-MiniLM-L6-v2",
    "cross-encoder/ms-marco-MiniLM-L6-v2",
    "jinaai/jina-embeddings-v2-base-code",
    "nomic-ai/nomic-embed-text-v1.5",
];

struct ModelPin {
    model: &'static str,
    revision: &'static str,
    tokenizer_sha256: &'static str,
    onnx_sha256: &'static str,
    /// Pin-authoring contract: when the ONNX graph at `revision` uses external
    /// weights (`onnx/model.onnx_data`), this must be `Some`. `None` is read as
    /// "this revision ships no sidecar": the loader never fetches one, and
    /// resolution refuses to proceed when a `model.onnx_data` is already on
    /// disk next to the graph (`refuse_undeclared_sidecar`). That refusal is
    /// what keeps a mis-authored `None` from loading unverified weights: ONNX
    /// Runtime resolves external data relative to the graph file's directory
    /// on its own, so a sidecar left there by an earlier run or an external
    /// download would be loaded whether or not CodeSage fetched it.
    onnx_data_sha256: Option<&'static str>,
}

const MODEL_PINS: &[ModelPin] = &[
    ModelPin {
        model: "sentence-transformers/all-MiniLM-L6-v2",
        revision: "c9745ed1d9f207416be6d2e6f8de32d1f16199bf",
        tokenizer_sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
        onnx_sha256: "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452",
        onnx_data_sha256: None,
    },
    ModelPin {
        model: "cross-encoder/ms-marco-MiniLM-L6-v2",
        revision: "c5ee24cb16019beea0893ab7796b1df96625c6b8",
        tokenizer_sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        onnx_sha256: "5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a",
        onnx_data_sha256: None,
    },
    ModelPin {
        model: "jinaai/jina-embeddings-v2-base-code",
        revision: "516f4baf13dec4ddddda8631e019b5737c8bc250",
        tokenizer_sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
        onnx_sha256: "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b",
        onnx_data_sha256: None,
    },
    ModelPin {
        model: "nomic-ai/nomic-embed-text-v1.5",
        revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
        tokenizer_sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        onnx_sha256: "147d5aa88c2101237358e17796cf3a227cead1ec304ec34b465bb08e9d952965",
        onnx_data_sha256: None,
    },
];

fn model_pin(model: &str) -> Option<&'static ModelPin> {
    MODEL_PINS.iter().find(|pin| pin.model == model)
}

/// The files a model load opens: the tokenizer, the ONNX graph, and the
/// external-weights sidecar when the repository ships one. Resolved by
/// [`resolve_model_artifacts`] through the same hf-hub lookups the session
/// loader performs, so a digest over these paths is a digest over the bytes
/// ONNX Runtime will execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArtifacts {
    pub tokenizer: PathBuf,
    pub onnx: PathBuf,
    pub onnx_data: Option<PathBuf>,
    /// The ONNX Runtime shared library a session would `dlopen`, on a
    /// target that loads it dynamically; `None` where the runtime is linked
    /// statically. Part of the artifact set because the runtime decides the
    /// graph partitioning and the kernels, so an upgraded library produces
    /// other vectors from unchanged model files.
    pub ort_runtime: Option<PathBuf>,
}

impl ModelArtifacts {
    /// Path, size, and mtime of every artifact: the same key the digest
    /// cache is invalidated by, without reading a byte. For a per-call
    /// change detector that must stay cheap; `None` when a file cannot be
    /// stat'ed or its path cannot be canonicalised.
    ///
    /// The path component is canonical — absolute, symlinks resolved — so a
    /// relative `HF_HOME` keys the same files as its absolute spelling, and
    /// the same relative spelling from another directory keys other files.
    pub fn stat_key(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (label, path) in self.labelled_files() {
            let path = std::fs::canonicalize(path).ok()?;
            let meta = std::fs::metadata(&path).ok()?;
            let modified = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            parts.push(format!(
                "{label}={}:{}:{}.{:09}",
                path.display(),
                meta.len(),
                modified.as_secs(),
                modified.subsec_nanos()
            ));
        }
        Some(parts.join(";"))
    }

    /// Every artifact with its label, in the fixed order a digest covers.
    pub fn labelled_files(&self) -> Vec<(&'static str, &Path)> {
        let mut files = vec![
            ("tokenizer", self.tokenizer.as_path()),
            ("onnx", self.onnx.as_path()),
        ];
        if let Some(data) = &self.onnx_data {
            files.push(("onnx_data", data.as_path()));
        }
        if let Some(runtime) = &self.ort_runtime {
            files.push(("ort_runtime", runtime.as_path()));
        }
        files
    }
}

/// The ONNX Runtime shared library a session built by this process would
/// load: the `ORT_DYLIB_PATH` the loader honours after discovery. `Ok(None)`
/// on a target that links the runtime statically. On a dynamic-loading
/// target an unlocated library is an error, never an absent component: the
/// loader would resolve a bare soname through the dynamic linker's search
/// path, and a fingerprint that cannot name the runtime it attests must not
/// vouch for the vectors.
pub fn ort_runtime_dylib() -> Result<Option<PathBuf>> {
    #[cfg(target_vendor = "apple")]
    {
        Ok(None)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        init_ort_dylib();
        match ort_dylib_path_from_env() {
            Some(path) => Ok(Some(path)),
            _ => anyhow::bail!(
                "ONNX Runtime shared library not located (no ORT_DYLIB_PATH, no pip \
                 onnxruntime, nothing under /usr/lib or /usr/local/lib); set \
                 ORT_DYLIB_PATH to the libonnxruntime.so the embedder should load"
            ),
        }
    }
}

/// The revision a load of `model` resolves against: the pinned commit for a
/// validated model, the repository head under the per-project unpinned
/// bypass (see [`allow_any_model_from_env`]). The fingerprint and the
/// session loader must agree on this, or the fingerprint would digest files
/// the session never opens.
fn load_revision(model: &str, allow_any: bool) -> Option<&'static str> {
    load_pin(model, allow_any).map(|pin| pin.revision)
}

/// The pin a load verifies against: the entry for a validated model, none
/// under the per-project unpinned bypass (see [`allow_any_model_from_env`]).
/// The fingerprint's revision and sidecar expectation and the loader's
/// derive from this one lookup, so they agree by construction.
fn load_pin(model: &str, allow_any: bool) -> Option<&'static ModelPin> {
    if allow_any { None } else { model_pin(model) }
}

/// What the pin a load verifies against says about the external-weights
/// sidecar. The one derivation the loader, the resolver, and the cache probe
/// share, so a pinned verdict can never be applied to an unpinned-bypass
/// resolution by one of them alone.
fn sidecar_expectation(model: &str, allow_any: bool) -> SidecarExpectation {
    SidecarExpectation::from_pin(load_pin(model, allow_any))
}

/// Locate exactly the files [`load_onnx_session`] would open for `model`,
/// downloading on a cache miss the way the loader does. The allowlist is
/// checked first, as the loader checks it: the model name comes from the
/// indexed repo's config, and an unvalidated name must not reach the network
/// through this path any more than through a session build. Performs no pin
/// verification: that is the loader's separate gate, and the semantic
/// fingerprint must reflect the bytes on disk whether or not they match a
/// pin.
pub fn resolve_model_artifacts(model: &str) -> Result<ModelArtifacts> {
    let allow_any = allow_any_model_from_env();
    validate_model_allowed(model, allow_any)?;
    resolve_model_artifacts_at(
        model,
        load_revision(model, allow_any),
        sidecar_expectation(model, allow_any),
    )
}

/// The artifacts for `model` already present in the local hf-hub cache, at
/// the revision a load would open. Never touches the network: `None` when
/// the model is not allowlisted or the tokenizer or the graph is not cached
/// yet. For a caller that must not block on a download (a per-call key, a
/// status line), where a later load changing the answer is acceptable.
pub fn cached_model_artifacts(model: &str) -> Option<ModelArtifacts> {
    let allow_any = allow_any_model_from_env();
    validate_model_allowed(model, allow_any).ok()?;
    let revision = load_revision(model, allow_any);
    let repo = match revision {
        Some(revision) => {
            Repo::with_revision(model.to_string(), RepoType::Model, revision.to_string())
        }
        None => Repo::model(model.to_string()),
    };
    let cache = hf_cache_from_env()?.repo(repo);
    let ort_runtime = ort_runtime_dylib().ok()?;
    // The same artifact set `resolve_model_artifacts` describes, or the two
    // digests never converge and every pass re-embeds: a pin that declares
    // no sidecar yields `onnx_data: None` here as there, even when a
    // `model.onnx_data` is sitting in the snapshot. That anomaly is not this
    // probe's to report (it returns `Option`, never an error); resolution
    // refuses it in `refuse_undeclared_sidecar` before any session is served.
    let onnx_data = match sidecar_expectation(model, allow_any) {
        SidecarExpectation::Absent => None,
        SidecarExpectation::Required | SidecarExpectation::Unknown => cache.get(ONNX_DATA_ARTIFACT),
    };
    Some(ModelArtifacts {
        tokenizer: cache.get("tokenizer.json")?,
        onnx: cache.get("onnx/model.onnx")?,
        onnx_data,
        ort_runtime,
    })
}

/// The directory `hf_hub::Cache::from_env` would consult, built without its
/// unset-`HF_HOME` fallback: `Cache::default` is
/// `dirs::home_dir().expect(..)`, and `dirs::home_dir` is `None` when `HOME`
/// is unset or empty and the UID has no passwd entry (`env -i`, a systemd
/// unit without `Environment=HOME`, a container UID absent from
/// `/etc/passwd`). That panic sits on the per-tool-call path in the daemon,
/// where it is a request with no response. `HF_HOME` is read with
/// `std::env::var` exactly as hf-hub reads it, so a non-UTF-8 value falls
/// back the same way. The `HOME`-only fallback drops `dirs`' passwd lookup
/// (`dirs` is not a dependency of this crate): with `HOME` unset the answer
/// is `None` rather than a passwd-derived directory.
fn hf_cache_from_env() -> Option<hf_hub::Cache> {
    let root = match std::env::var("HF_HOME") {
        Ok(hf_home) => PathBuf::from(hf_home).join("hub"),
        Err(_) => {
            let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
            PathBuf::from(home)
                .join(".cache")
                .join("huggingface")
                .join("hub")
        }
    };
    Some(hf_hub::Cache::new(root))
}

const ONNX_DATA_ARTIFACT: &str = "onnx/model.onnx_data";

/// What the pin says about `onnx/model.onnx_data` at the revision being
/// resolved. Every allowlisted model is pinned, so on a real deployment the
/// answer to "is there a sidecar?" costs no network request at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidecarExpectation {
    /// The pin declares the sidecar and its hash: fetch it.
    Required,
    /// The pin declares this revision ships none. A HuggingFace revision is
    /// an immutable commit, so the declaration cannot go stale: no fetch, no
    /// file-list probe, no network. One `fs::metadata` on the path ONNX
    /// Runtime would load a sidecar from, and a refusal if a file is there
    /// (`refuse_undeclared_sidecar`).
    Absent,
    /// No pin (per-project unpinned bypass, or an allowlisted model without
    /// a pin entry): fetch, and fall back to the hub's file list when that
    /// fetch fails.
    Unknown,
}

impl SidecarExpectation {
    fn from_pin(pin: Option<&ModelPin>) -> Self {
        match pin {
            Some(ModelPin {
                onnx_data_sha256: Some(_),
                ..
            }) => Self::Required,
            Some(ModelPin {
                onnx_data_sha256: None,
                ..
            }) => Self::Absent,
            None => Self::Unknown,
        }
    }
}

/// Tokenizer, graph, and optional external-weights sidecar, in that order.
type ArtifactPaths = (PathBuf, PathBuf, Option<PathBuf>);

/// `HF_HOME` as set in the environment (`None` when unset), model, revision:
/// everything that decides which snapshot files a resolution lands on. The
/// root is read with `var_os` rather than through `hf_hub::Cache::from_env`,
/// whose unset-`HF_HOME` fallback panics when no home directory resolves;
/// every download and probe goes through [`hf_api`], built from the same
/// [`hf_cache_from_env`] root this key names, so a custom `HF_HOME` can only
/// cost an extra fetch under another root, never serve another root's paths.
type ArtifactKey = (Option<PathBuf>, String, Option<String>);

/// One memo entry. `sidecar` is `None` while the sidecar outcome is
/// unresolved (a pinned sidecar's fetch failed, or an unpinned one's fetch
/// failed and the hub's file list either names the file or could not be
/// retrieved), `Some(None)` once the pin or the file list established that
/// the revision does not ship it, and `Some(Some(path))` once it was
/// fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ArtifactMemo {
    tokenizer: PathBuf,
    onnx: PathBuf,
    sidecar: Option<Option<PathBuf>>,
}

// Resolved snapshot paths are revision-pinned and stable for the life of
// the process, while each hf-hub lookup builds a fresh API client (two TLS
// agents, a token-file read) and then reads `refs/<revision>` and stats the
// snapshot path — and the fingerprint asks for
// them on every query. An eviction plus refetch of a corrupted artifact
// recreates the symlink at the same snapshot path, so a memoized path stays
// valid across it; the bytes behind it are re-digested because
// `fingerprint::cached_file_digest` keys on size and mtime. Three
// resolutions run per MCP semantic query — the embedder pool key, the
// session fingerprint, and the table fingerprint — so an unmemoized
// resolution cost nine hub-client constructions per query before this
// (measured on a live daemon: 60 over a six-query probe, six of them the
// cold session load). A memo hit replaces a resolution's two lookups for a
// pinned model, three when a sidecar is fetched, with two or three
// `fs::metadata` calls: a path whose file is gone — the cache
// was cleared by hand, an `hf cache delete` ran, or an eviction's refetch
// failed — is dropped so the next resolution re-downloads instead of every
// later query failing with ENOENT until the daemon restarts. `fs::metadata`
// rather than `symlink_metadata`, so a snapshot symlink over a deleted blob
// counts as gone too and the entry is dropped; that case does not recover
// by itself, though. hf-hub's `symlink_or_rename` skips when `dst.exists()`
// (false for a dangling link) and then `symlink`s onto the occupied name,
// so the refetch fails with `EEXIST` on every resolution until the dangling
// link is removed by hand. A deleted file re-downloads; a dangling snapshot
// symlink surfaces an error.
static ARTIFACT_PATHS: OnceLock<Mutex<HashMap<ArtifactKey, ArtifactMemo>>> = OnceLock::new();

fn artifact_paths_memo() -> &'static Mutex<HashMap<ArtifactKey, ArtifactMemo>> {
    ARTIFACT_PATHS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memoized_artifacts(key: &ArtifactKey) -> Option<ArtifactMemo> {
    let mut memo = artifact_paths_memo()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let entry = memo.get(key)?;
    let unreadable = [&entry.tokenizer, &entry.onnx]
        .into_iter()
        .chain(entry.sidecar.iter().flatten())
        .find_map(|path| std::fs::metadata(path).err().map(|e| (path.clone(), e)));
    match unreadable {
        None => Some(entry.clone()),
        Some((path, error)) => {
            // Any stat failure invalidates, not only ENOENT: EACCES, EIO, and
            // ELOOP also mean the path cannot be opened, and re-resolving is
            // the safe direction. The line is what distinguishes "the cache
            // was cleared" from a directory that lost read permission and now
            // costs a full resolution per pass.
            tracing::debug!(
                path = %path.display(),
                error = %error,
                "memoized model artifact path no longer stats; dropping the entry so the next resolution re-fetches"
            );
            memo.remove(key);
            None
        }
    }
}

/// The three artifact paths for `model` at `revision` under `hf_home`,
/// fetched through `fetch` on the first call for that key and served from
/// the memo afterwards while the files still exist. A failed tokenizer or
/// graph fetch memoizes nothing.
///
/// The sidecar is settled by `expectation` first. A pin answers outright:
/// `Absent` is memoized without a fetch or a probe, after one stat of the
/// path ONNX Runtime would load a sidecar from (`refuse_undeclared_sidecar`,
/// repeated on every memo hit so a sidecar that appears later is caught
/// too); `Required` is fetched and a failed fetch is left unresolved for the
/// loader's pin check to report (the next call re-attempts only that
/// fetch). Only `Unknown` reaches the network to find out.
///
/// For `Unknown`, a failed sidecar fetch is not an answer by itself: hf-hub
/// reports a 404, a 503, a refused connection, a DNS miss, and a TLS failure
/// through the one `ApiError::RequestError` variant (ureq's
/// `http_status_as_error` is on and hf-hub sets `max_retries: 0`), so
/// inferring absence from the error would let one blip during warm-up
/// memoize "no external weights" for the process lifetime and every later
/// session build fail on the missing sidecar. Absence is established
/// positively instead, through `sidecar_listed`, the hub's file list for the
/// revision: listed and fetch failed, or list unavailable, leaves the
/// sidecar unresolved so the next call re-attempts only that fetch; not
/// listed memoizes absence. For an unpinned model without a sidecar that is
/// one extra metadata request per key per process, after the 404, not per
/// query. The lock is never held across `fetch` or `sidecar_listed`, which
/// may block on the network: two first-callers racing on one key both fetch
/// and both insert the same paths.
fn resolve_hf_artifact_paths(
    hf_home: Option<&Path>,
    model: &str,
    revision: Option<&str>,
    expectation: SidecarExpectation,
    mut fetch: impl FnMut(&'static str) -> Result<PathBuf>,
    sidecar_listed: impl FnOnce() -> Result<bool>,
) -> Result<ArtifactPaths> {
    let key = (
        hf_home.map(Path::to_path_buf),
        model.to_string(),
        revision.map(str::to_string),
    );
    let (tokenizer, onnx) = match memoized_artifacts(&key) {
        Some(ArtifactMemo {
            tokenizer,
            onnx,
            sidecar: Some(sidecar),
        }) => {
            if expectation == SidecarExpectation::Absent {
                refuse_undeclared_sidecar(&onnx)?;
            }
            return Ok((tokenizer, onnx, sidecar));
        }
        Some(ArtifactMemo {
            tokenizer,
            onnx,
            sidecar: None,
        }) => (tokenizer, onnx),
        None => (fetch("tokenizer.json")?, fetch("onnx/model.onnx")?),
    };
    // External-weights sidecar (>2GB models like Jina v2 base, BGE-large).
    // Most models don't have this file — a 404 is the expected outcome and
    // not worth surfacing. Every failure gets a debug-level breadcrumb so
    // users running with RUST_LOG=debug don't have to guess when
    // commit_from_file later errors with an opaque ORT external-data load
    // failure.
    let sidecar = match expectation {
        SidecarExpectation::Absent => {
            refuse_undeclared_sidecar(&onnx)?;
            Some(None)
        }
        SidecarExpectation::Required => match fetch(ONNX_DATA_ARTIFACT) {
            Ok(path) => Some(Some(path)),
            Err(fetch_error) => {
                tracing::debug!(
                    error = %fetch_error,
                    "onnx/model.onnx_data is pinned but its fetch failed; will retry on the next resolution"
                );
                None
            }
        },
        SidecarExpectation::Unknown => match fetch(ONNX_DATA_ARTIFACT) {
            Ok(path) => Some(Some(path)),
            Err(fetch_error) => match sidecar_listed() {
                Ok(false) => {
                    tracing::debug!(
                        error = %fetch_error,
                        "onnx/model.onnx_data is not in the repository's file list (normal for small models); memoized as absent"
                    );
                    Some(None)
                }
                Ok(true) => {
                    tracing::debug!(
                        error = %fetch_error,
                        "onnx/model.onnx_data is listed in the repository but its fetch failed; will retry on the next resolution"
                    );
                    None
                }
                Err(list_error) => {
                    tracing::debug!(
                        fetch_error = %fetch_error,
                        list_error = %list_error,
                        "onnx/model.onnx_data fetch failed and the repository's file list is unavailable; will retry on the next resolution"
                    );
                    None
                }
            },
        },
    };
    let entry = ArtifactMemo {
        tokenizer,
        onnx,
        sidecar,
    };
    artifact_paths_memo()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, entry.clone());
    Ok((entry.tokenizer, entry.onnx, entry.sidecar.flatten()))
}

/// Refuse a `model.onnx_data` next to the graph when the pin declares this
/// revision ships none. ONNX Runtime resolves external tensor data relative
/// to the graph file's directory, and the session is built from the graph
/// path alone (`commit_from_file`), so a sidecar already at that path — left
/// by an earlier run or an external `huggingface-cli download` — would be
/// loaded whether or not this process fetched it, with no pin to verify it
/// against. One stat, no network. A stat failure other than `NotFound` is
/// refused too: the check cannot vouch for a path it cannot inspect.
fn refuse_undeclared_sidecar(onnx: &Path) -> Result<()> {
    let sidecar = onnx
        .parent()
        .unwrap_or(Path::new(""))
        .join("model.onnx_data");
    match std::fs::metadata(&sidecar) {
        Ok(_) => anyhow::bail!(
            "found {} next to the ONNX graph, but CodeSage's model pin declares no \
             external-weights sidecar for this revision; the file is unverified and \
             refused. Remove it, or update the model pin deliberately.",
            sidecar.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "checking for an undeclared external-weights sidecar at {}",
                sidecar.display()
            )
        }),
    }
}

fn resolve_model_artifacts_at(
    model: &str,
    revision: Option<&str>,
    expectation: SidecarExpectation,
) -> Result<ModelArtifacts> {
    let hf_home = std::env::var_os("HF_HOME").map(PathBuf::from);
    let (tokenizer, onnx, onnx_data) = resolve_hf_artifact_paths(
        hf_home.as_deref(),
        model,
        revision,
        expectation,
        |artifact| hf_get_model_file(model, revision, artifact),
        || hf_sidecar_listed(model, revision),
    )?;
    Ok(ModelArtifacts {
        tokenizer,
        onnx,
        onnx_data,
        ort_runtime: ort_runtime_dylib()?,
    })
}

/// Migration note (cs-72x): `CODESAGE_ALLOW_ANY_MODEL=1` used to be a
/// process-global bypass — export it once in a shell profile and every cloned
/// repo indexed from that shell could point `.codesage/config.toml` at an
/// arbitrary ONNX graph and have it downloaded and executed. It now grants
/// only *eligibility*; the bypass *activates* only for a project the user
/// opted in by listing its canonical root, one per line, in the user-owned
/// allowlist file from [`user_allowlist_path`] (default
/// `~/.config/codesage/allowed-models`; blank lines and `#` comments
/// ignored). To migrate a workflow that relied on the old global bypass for
/// a project you trust, keep the env var and append that project's root:
/// `mkdir -p ~/.config/codesage && pwd >> ~/.config/codesage/allowed-models`.
///
/// There is deliberately no repo-local opt-in (no `.codesage/allow-any-model`
/// marker): the model name comes from the project's own config, so anything
/// inside the project directory is attacker-controlled for a cloned repo — a
/// repo-local marker would let a malicious repo self-authorize under a
/// globally-exported env var, the exact bypass this closes. Activation state
/// lives outside every repo, under the user's config home.
///
/// The current project is the nearest ancestor of the process working
/// directory holding a `.codesage/` directory; outside any project the answer
/// is `false` (fail closed). Name kept for the `doctor` check and the
/// existing callers; semantics are now "env-eligible AND project opted in".
pub fn allow_any_model_from_env() -> bool {
    if !allow_any_eligible_from_env() {
        return false;
    }
    let Some(root) = current_project_root() else {
        return false;
    };
    project_allow_any_opted_in(&root)
}

/// Raw eligibility signal behind [`allow_any_model_from_env`]: the env var
/// alone. Process-global — never consult it directly on a load path; loads
/// require the per-project opt-in on top.
fn allow_any_eligible_from_env() -> bool {
    matches!(
        std::env::var("CODESAGE_ALLOW_ANY_MODEL").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Nearest ancestor of the process working directory holding a `.codesage/`
/// directory, if any. `None` outside a project or when the cwd is unknown:
/// callers fail closed.
fn current_project_root() -> Option<PathBuf> {
    find_project_root_from(&std::env::current_dir().ok()?)
}

/// Pure core over an explicit start directory (the env/cwd wrappers above
/// cannot be touched by unit tests without racing process-global state):
/// walk up until a directory holding a `.codesage/` entry is found.
fn find_project_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".codesage").is_dir())
        .map(Path::to_path_buf)
}

/// Whether `root` is opted in: its canonical path is listed in the
/// user-owned allowlist file. A missing or unreadable file, or an
/// unresolvable config home (notably HOME-less daemon environments — see
/// [`hf_cache_from_env`]), is "not listed": fail closed, never panic.
fn project_allow_any_opted_in(root: &Path) -> bool {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(path) = user_allowlist_path() else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    // Eligibility was established by the caller (`allow_any_model_from_env`);
    // this is the activation half, through the same pure core the tests pin.
    allow_any_for_project_root(Some(&canonical), true, &contents)
}

/// The user-owned allowlist file consulted at load time, or `None` when no
/// config home resolves. `$XDG_CONFIG_HOME/codesage/allowed-models` wins
/// when set and non-empty, else `$HOME/.config/codesage/allowed-models`.
/// Read with `var_os` exactly like [`hf_cache_from_env`], so a non-UTF-8
/// value falls back the same way instead of erroring.
fn user_allowlist_path() -> Option<PathBuf> {
    user_allowlist_path_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Pure core of [`user_allowlist_path`] so tests can prove the precedence
/// without touching process env.
fn user_allowlist_path_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("codesage").join("allowed-models"));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("codesage")
            .join("allowed-models"),
    )
}

/// Pure core: does `contents` (the allowlist file) name `canonical_root`?
/// Blank lines and `#` comments are ignored; entries compare as paths, so a
/// trailing slash on either side still matches.
fn project_listed_in_allowlist(canonical_root: &Path, contents: &str) -> bool {
    contents.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#') && Path::new(line) == canonical_root
    })
}

/// Effective bypass for an explicit project root: env eligibility AND that
/// project listed in `allowlist_contents`. Pure core so unit tests can prove
/// the matrix without touching process env or cwd: an env flag set for a
/// non-allowlisted project still refuses.
fn allow_any_for_project_root(
    project_root: Option<&Path>,
    eligible: bool,
    allowlist_contents: &str,
) -> bool {
    eligible
        && project_root.is_some_and(|root| project_listed_in_allowlist(root, allowlist_contents))
}

/// Message for the loud warning emitted when a load proceeds unpinned: names
/// the model and the resolved revision (`None` is the floating repository
/// head — the bypass never pins).
fn unpinned_load_message(model: &str, revision: Option<&str>) -> String {
    format!(
        "loading model {model:?} outside the validated-model pin set at {} — no hash \
         verification will run; the bytes ONNX Runtime executes are whatever the hub serves. \
         Bypass is active for this project (CODESAGE_ALLOW_ANY_MODEL plus project opt-in): \
         only use models you trust.",
        revision.map_or_else(
            || "the floating repository head (unpinned)".to_string(),
            |rev| format!("pinned revision {rev:?}")
        )
    )
}

pub fn validate_model_allowed(model: &str, allow_any: bool) -> Result<()> {
    if allow_any || ALLOWED_MODELS.contains(&model) {
        return Ok(());
    }
    anyhow::bail!(
        "model {model:?} is not on CodeSage's validated-model allowlist. \
         The model name comes from the project's .codesage/config.toml, which \
         is untrusted when the repo isn't yours — an arbitrary name would \
         download and execute an attacker-chosen ONNX graph inside ONNX \
         Runtime. Validated models: {ALLOWED_MODELS:?}. To experiment with \
         another model you trust, set CODESAGE_ALLOW_ANY_MODEL=1 AND list \
         this project's canonical root in {}.",
        user_allowlist_display()
    )
}

/// Where [`validate_model_allowed`] tells the user to opt their project in:
/// the resolved allowlist path, or the `~` shorthand when no config home
/// resolves (e.g. HOME-less environments) so the error still names the file.
fn user_allowlist_display() -> String {
    user_allowlist_path().map_or_else(
        || "~/.config/codesage/allowed-models".to_string(),
        |path| path.display().to_string(),
    )
}

fn verify_model_artifact_sha256(
    model: &str,
    artifact: &str,
    path: &Path,
    expected: &str,
) -> Result<()> {
    // Shares the fingerprint's per-process digest cache: the same bytes are
    // digested for the pin gate and for the table fingerprint, and a model
    // file is read once for both.
    let actual = crate::fingerprint::cached_file_digest(path)
        .with_context(|| format!("reading {artifact} for pinned model {model:?}"))?;
    if actual != expected {
        anyhow::bail!(
            "pinned model artifact hash mismatch for {model:?} {artifact}: \
             expected sha256 {expected}, got {actual}. Refusing to load cached/downloaded \
             bytes into ONNX Runtime; clear the HuggingFace cache or update CodeSage's model pin deliberately."
        );
    }
    Ok(())
}

/// One-retry policy for pinned-artifact verification. A first hash mismatch
/// is usually local cache corruption (bit rot, truncated write), so the
/// cached file is evicted and fetched fresh; a mismatch on the freshly
/// fetched bytes is the supply-chain signal the pins exist to catch and
/// stays a hard failure.
fn verify_with_refetch<P>(
    cached: P,
    mut verify: impl FnMut(&P) -> Result<()>,
    evict: impl FnOnce(&P) -> Result<()>,
    refetch: impl FnOnce() -> Result<P>,
) -> Result<P> {
    let Err(cache_err) = verify(&cached) else {
        return Ok(cached);
    };
    evict(&cached).with_context(|| {
        format!("evicting cached artifact that failed verification: {cache_err}")
    })?;
    let fresh = refetch()?;
    verify(&fresh).context(
        "artifact hash still mismatched after evicting the cached copy and re-downloading once",
    )?;
    Ok(fresh)
}

/// Remove a cached hf-hub artifact so the next fetch re-downloads it. The
/// snapshot path hf-hub hands out is a symlink into the cache's `blobs/`
/// directory; the blob holds the actual corrupted bytes and must go too.
/// The blob target is only deleted after proving it resolves inside the
/// cache boundary — a crafted or corrupted link (`../../../../some-file`)
/// must not get an arbitrary external file deleted on the eviction path.
/// On containment failure only the symlink itself is removed.
fn evict_cached_artifact(path: &Path) -> Result<()> {
    // The pin verifier shares `fingerprint::cached_file_digest`, keyed on
    // (size, mtime): a refetch that lands within the same mtime tick with
    // the same length would otherwise verify against the evicted bytes'
    // hash. Purge first; a purge with no refetch just costs a re-read.
    crate::fingerprint::forget_cached_digest(path);
    if let Ok(target) = std::fs::read_link(path) {
        let blob = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(Path::new("")).join(target)
        };
        crate::fingerprint::forget_cached_digest(&blob);
        match blob_within_cache_boundary(path, &blob) {
            Some(true) => {
                if let Err(e) = std::fs::remove_file(&blob)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(e)
                        .with_context(|| format!("removing cached blob {}", blob.display()));
                }
            }
            Some(false) => {
                tracing::warn!(
                    link = %path.display(),
                    target = %blob.display(),
                    "cached artifact symlink resolves outside the model cache; removing only the link"
                );
            }
            // Dangling target: nothing to remove beyond the link itself.
            None => {}
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing cached artifact {}", path.display())),
    }
}

/// Containment check for blob eviction. The boundary is derived from the
/// snapshot link itself: the nearest ancestor holding a `blobs/` directory
/// is the hf-hub `models--*` cache entry that owns both the snapshot tree
/// and the blob store. Both sides are canonicalized so `..` components and
/// symlink hops can't smuggle the resolved target outside the boundary.
/// `None` means the blob doesn't exist (dangling link); `Some(false)` means
/// it exists but resolves outside the cache (or no boundary was found).
fn blob_within_cache_boundary(link: &Path, blob: &Path) -> Option<bool> {
    let blob_canon = blob.canonicalize().ok()?;
    let boundary = link
        .ancestors()
        .skip(1)
        .find(|ancestor| ancestor.join("blobs").is_dir());
    let Some(boundary) = boundary else {
        return Some(false);
    };
    let Ok(boundary_canon) = boundary.canonicalize() else {
        return Some(false);
    };
    Some(blob_canon.starts_with(&boundary_canon))
}

fn verify_or_refetch_artifact(
    model: &str,
    revision: &str,
    artifact: &'static str,
    cached: PathBuf,
    expected: &str,
) -> Result<PathBuf> {
    verify_with_refetch(
        cached,
        |path| verify_model_artifact_sha256(model, artifact, path, expected),
        |path| {
            tracing::warn!(
                model,
                artifact,
                path = %path.display(),
                "cached model artifact failed sha256 verification; evicting and re-downloading once"
            );
            evict_cached_artifact(path)
        },
        || hf_get_model_file(model, Some(revision), artifact),
    )
}

/// Verify every pinned artifact, tolerating one locally-corrupted cache file
/// per artifact via [`verify_with_refetch`]. Returns the verified tokenizer
/// and model paths (re-downloaded ones when the cache was evicted).
fn verify_pinned_model_artifacts(
    model: &str,
    pin: &ModelPin,
    tokenizer_path: PathBuf,
    model_path: PathBuf,
    onnx_data_path: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf)> {
    let tokenizer_path = verify_or_refetch_artifact(
        model,
        pin.revision,
        "tokenizer.json",
        tokenizer_path,
        pin.tokenizer_sha256,
    )?;
    let model_path = verify_or_refetch_artifact(
        model,
        pin.revision,
        "onnx/model.onnx",
        model_path,
        pin.onnx_sha256,
    )?;
    match (pin.onnx_data_sha256, onnx_data_path) {
        (Some(expected), Some(path)) => {
            verify_or_refetch_artifact(
                model,
                pin.revision,
                "onnx/model.onnx_data",
                path,
                expected,
            )?;
        }
        (Some(_), None) => {
            anyhow::bail!(
                "pinned model {model:?} requires onnx/model.onnx_data at revision {}, but it was not downloaded",
                pin.revision
            );
        }
        (None, Some(path)) => {
            // Unreachable on the pinned path: `SidecarExpectation::Absent`
            // never fetches the sidecar, so resolution cannot hand one back.
            // The live guard for a pin that declares none is
            // `refuse_undeclared_sidecar`, which runs during resolution
            // against the on-disk path ONNX Runtime would load. Retained as
            // the fail-closed arm should resolution ever grow another way to
            // produce a path for such a pin.
            anyhow::bail!(
                "pinned model {model:?} unexpectedly resolved onnx/model.onnx_data at {}; refusing unpinned sidecar",
                path.display()
            );
        }
        (None, None) => {}
    }
    Ok((tokenizer_path, model_path))
}

/// Whether a model's declared input names include `token_type_ids`, i.e. the
/// inference call must supply that third tensor alongside ids + mask.
pub(crate) fn wants_token_type_ids<'a>(mut input_names: impl Iterator<Item = &'a str>) -> bool {
    input_names.any(|name| name == "token_type_ids")
}

/// Shared model-loading path for [`Embedder`] and [`Reranker`]: downloads the
/// tokenizer + ONNX model (and the optional external-weights sidecar), builds a
/// padding/truncating tokenizer, creates an ORT session — registering the CUDA
/// execution provider when `device` requests the GPU — and asserts the CUDA
/// libraries actually mapped (the silent-CPU-fallback guard). Returns the
/// session, tokenizer, and whether the model takes a `token_type_ids` input.
pub(crate) fn load_onnx_session(model: &str, device: &str) -> Result<(Session, Tokenizer, bool)> {
    let loaded = load_onnx_session_with_provider(model, device)?;
    Ok((loaded.session, loaded.tokenizer, loaded.has_token_type_ids))
}

/// What [`load_onnx_session_with_provider`] produced, including the
/// execution provider the session actually runs on.
pub(crate) struct LoadedSession {
    pub(crate) session: Session,
    pub(crate) tokenizer: Tokenizer,
    pub(crate) has_token_type_ids: bool,
    /// `cpu`, `cuda`, or `coreml`: the provider that succeeded, which under
    /// `CODESAGE_ALLOW_CPU_FALLBACK=1` may differ from the configured one.
    pub(crate) execution_provider: &'static str,
}

/// The provider a session runs on after the CUDA-library check. `configured`
/// is the provider the device string selects; `cuda_check` is the result of
/// the mapped-libraries guard (only meaningful when CUDA was requested). A
/// failed check is fatal unless the fallback is allowed, in which case the
/// session runs on the CPU and the second element says so.
#[cfg(any(test, all(feature = "cuda", target_os = "linux")))]
fn effective_execution_provider(
    configured: &'static str,
    cuda_check: Result<()>,
    allow_cpu_fallback: bool,
) -> Result<(&'static str, bool)> {
    match cuda_check {
        Ok(()) => Ok((configured, false)),
        Err(_) if allow_cpu_fallback => Ok(("cpu", true)),
        Err(e) => Err(e),
    }
}

#[cfg(all(feature = "cuda", target_os = "linux"))]
fn cpu_fallback_allowed_from_env() -> bool {
    matches!(
        std::env::var("CODESAGE_ALLOW_CPU_FALLBACK").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// [`load_onnx_session`], also reporting the execution provider the session
/// initialised on.
pub(crate) fn load_onnx_session_with_provider(model: &str, device: &str) -> Result<LoadedSession> {
    let allow_any = allow_any_model_from_env();
    validate_model_allowed(model, allow_any)?;
    let pin = load_pin(model, allow_any);
    debug_assert_eq!(
        pin.map(|pin| pin.revision),
        load_revision(model, allow_any),
        "the fingerprint must resolve the revision the loader opens"
    );
    if !allow_any && pin.is_none() {
        anyhow::bail!(
            "validated model {model:?} has no pinned revision/hash metadata; refusing unpinned load"
        );
    }
    if allow_any {
        // Unpinned by construction (`load_pin` returns `None` under the
        // bypass): the hub head executes with no hash check. Loud on purpose —
        // the line names the model plus the resolved revision so the log alone
        // answers "what ran unpinned". Session builds are cached, so this
        // fires once per session, not per query.
        let unpinned = unpinned_load_message(model, load_revision(model, allow_any));
        tracing::warn!("{unpinned}");
    }

    #[cfg(not(target_vendor = "apple"))]
    init_ort_dylib();

    crate::config::validate_device(device)?;
    let want_cuda = wants_cuda(device);
    if want_cuda {
        #[cfg(not(feature = "cuda"))]
        {
            anyhow::bail!(
                "GPU requested but binary built without cuda feature. Rebuild with: cargo build --features cuda"
            );
        }
    }

    let want_coreml = wants_coreml(device);
    if want_coreml && !cfg!(target_vendor = "apple") {
        anyhow::bail!(
            "CoreML requested in .codesage/config.toml but this binary is not running on Apple hardware"
        );
    }

    let artifacts = resolve_model_artifacts_at(
        model,
        pin.map(|pin| pin.revision),
        sidecar_expectation(model, allow_any),
    )?;
    let (tokenizer_path, model_path) = if let Some(pin) = pin {
        verify_pinned_model_artifacts(
            model,
            pin,
            artifacts.tokenizer,
            artifacts.onnx,
            artifacts.onnx_data,
        )?
    } else {
        (artifacts.tokenizer, artifacts.onnx)
    };

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
            preload_native_libs();
            builder = builder
                .with_execution_providers([ort::ep::CUDA::default().build().error_on_failure()])
                .map_err(|e| anyhow::anyhow!("CUDA provider failed to register: {e}"))?;
        }
    }

    if want_coreml {
        #[cfg(target_vendor = "apple")]
        {
            builder = builder
                .with_execution_providers([ort::ep::CoreML::default()
                    .with_compute_units(ort::ep::coreml::ComputeUnits::All)
                    .build()
                    .error_on_failure()])
                .map_err(|e| anyhow::anyhow!("CoreML provider failed to register: {e}"))?;
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
    // CODESAGE_ALLOW_CPU_FALLBACK=1 (e.g. for unit tests) — the session then
    // reports `cpu` as its provider, so the vectors it produces are never
    // attested as CUDA output.
    let configured = crate::fingerprint::configured_execution_provider(device);
    #[cfg(all(feature = "cuda", target_os = "linux"))]
    let execution_provider = if want_cuda {
        let (provider, fell_back) = effective_execution_provider(
            configured,
            require_cuda_libs_mapped(),
            cpu_fallback_allowed_from_env(),
        )?;
        if fell_back {
            tracing::warn!(
                model,
                configured_device = device,
                execution_provider = provider,
                "CODESAGE_ALLOW_CPU_FALLBACK: CUDA was requested but its libraries are not \
                 mapped; this session runs on the CPU and fingerprints as a CPU setup"
            );
        }
        provider
    } else {
        configured
    };
    #[cfg(not(all(feature = "cuda", target_os = "linux")))]
    // No functional check here for CoreML, deliberately: unlike CUDA — whose
    // loader can report "Successfully registered" while binding nothing,
    // which is what the /proc/self/maps guard above catches — the CoreML
    // provider is registered with `error_on_failure`, so a registration
    // failure is already loud, and ORT exposes no query for the provider a
    // committed session actually runs on nor any userspace-visible mapping
    // to assert. The attested provider is the configured one (pinned by
    // `coreml_provider_is_attested_from_configuration`).
    let execution_provider = configured;

    let has_token_type_ids = wants_token_type_ids(session.inputs().iter().map(|i| i.name()));

    Ok(LoadedSession {
        session,
        tokenizer,
        has_token_type_ids,
        execution_provider,
    })
}

pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    dim: usize,
    pooling: PoolingStrategy,
    has_token_type_ids: bool,
    batch_size: NonZeroUsize,
    execution_provider: &'static str,
}

impl Embedder {
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        tracing::info!(model = %config.model, "loading embedding model");
        let LoadedSession {
            session,
            tokenizer,
            has_token_type_ids,
            execution_provider,
        } = load_onnx_session_with_provider(&config.model, &config.device)?;
        let dim = detect_dim(&session)?;
        let pooling = config.pooling_strategy();
        let batch_size = config.effective_batch_size()?;

        tracing::info!(
            dim,
            pooling = ?pooling,
            token_type_ids = has_token_type_ids,
            batch_size = batch_size.get(),
            execution_provider,
            "embedding model loaded"
        );

        Ok(Self {
            session,
            tokenizer,
            dim,
            pooling,
            has_token_type_ids,
            batch_size,
            execution_provider,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The execution provider this session actually initialised on (`cpu`,
    /// `cuda`, `coreml`) — the one a fingerprint over its vectors must name.
    pub fn execution_provider(&self) -> &'static str {
        self.execution_provider
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

        for batch in texts.chunks(self.batch_size.get()) {
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
        // Moved, not cloned: mean pooling reads the same values back from
        // `encodings` below instead of a second copy of this vector.
        let mask_tensor = ort::value::Tensor::from_array(([batch_size, seq_len], attention_mask))?;

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

        // The pooling loops below index `hidden` as a `[batch, seq, dim]`
        // tensor. `detect_dim` only validates the last dimension, so a model
        // whose first output is already pooled (`[batch, dim]`) passes load but
        // would make the `(i*seq_len + j)*dim` offsets run off the end of the
        // slice — an out-of-bounds panic on the first embed. In the daemon that
        // panic is swallowed by rmcp and the client hangs. Verify the shape up
        // front and fail with an actionable message instead.
        let expected = batch_size
            .checked_mul(seq_len)
            .and_then(|n| n.checked_mul(self.dim))
            .context("token-level output size overflow")?;
        if hidden.len() != expected {
            anyhow::bail!(
                "model output has {} values but mean/CLS pooling expects a \
                 token-level [batch={}, seq={}, dim={}] tensor ({} values). \
                 The configured model likely emits a pre-pooled [batch, dim] \
                 output; pick a model whose first output is token-level hidden \
                 states, or set pooling accordingly in .codesage/config.toml.",
                hidden.len(),
                batch_size,
                seq_len,
                self.dim,
                expected
            );
        }

        let mut embeddings = Vec::with_capacity(batch_size);

        for (i, enc) in encodings.iter().enumerate() {
            let pooled = match self.pooling {
                PoolingStrategy::Mean => {
                    let mask = enc.get_attention_mask();
                    let mut vec = vec![0.0f32; self.dim];
                    let mut mask_sum = 0.0f32;
                    for (j, &m) in mask.iter().enumerate() {
                        let m = m as f32;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ort_api_floor_matches_supported_runtime() {
        assert_eq!(ort::MINOR_VERSION, 24);
    }

    #[test]
    fn nvidia_lib_root_can_point_directly_at_shared_libraries() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codesage-nvidia-lib-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("libcudart.so.12"), b"").unwrap();

        let dirs = collect_nvidia_lib_dirs(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        assert!(
            dirs.contains(&root),
            "CODESAGE_NVIDIA_LIBS should accept a directory that directly contains CUDA shared libraries: {dirs:?}"
        );
    }

    #[test]
    fn allowlisted_models_pass_validation() {
        for model in ALLOWED_MODELS {
            assert!(
                validate_model_allowed(model, false).is_ok(),
                "{model} should be allowed"
            );
        }
    }

    #[test]
    fn allowlisted_models_have_pinned_artifacts() {
        for model in ALLOWED_MODELS {
            let pin = model_pin(model).unwrap_or_else(|| panic!("{model} missing model pin"));
            assert_eq!(
                pin.revision.len(),
                40,
                "{model} revision should be a git SHA"
            );
            assert_eq!(
                pin.tokenizer_sha256.len(),
                64,
                "{model} tokenizer hash should be sha256 hex"
            );
            assert_eq!(
                pin.onnx_sha256.len(),
                64,
                "{model} ONNX hash should be sha256 hex"
            );
        }
    }

    #[test]
    fn model_artifact_sha256_rejects_mismatch() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codesage-model-hash-test-{}-{unique}",
            std::process::id()
        ));
        fs::write(&path, b"abc").unwrap();

        verify_model_artifact_sha256(
            "test/model",
            "tokenizer.json",
            &path,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        let err = verify_model_artifact_sha256(
            "test/model",
            "tokenizer.json",
            &path,
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .expect_err("wrong pinned hash must be rejected");
        let _ = fs::remove_file(&path);

        assert!(
            err.to_string().contains("hash mismatch"),
            "error should explain the integrity failure: {err}"
        );
    }

    #[test]
    fn verify_with_refetch_returns_cached_when_hash_matches() {
        let mut evictions = 0u32;
        let mut refetches = 0u32;
        let out = verify_with_refetch(
            "cached",
            |_| Ok(()),
            |_| {
                evictions += 1;
                Ok(())
            },
            || {
                refetches += 1;
                Ok("fresh")
            },
        )
        .unwrap();
        assert_eq!(out, "cached");
        assert_eq!((evictions, refetches), (0, 0));
    }

    #[test]
    fn verify_with_refetch_evicts_and_refetches_once_on_cache_corruption() {
        let mut evictions = 0u32;
        let out = verify_with_refetch(
            "cached",
            |p: &&str| {
                if *p == "cached" {
                    Err(anyhow::anyhow!("hash mismatch"))
                } else {
                    Ok(())
                }
            },
            |_| {
                evictions += 1;
                Ok(())
            },
            || Ok("fresh"),
        )
        .unwrap();
        assert_eq!(out, "fresh");
        assert_eq!(evictions, 1);
    }

    #[test]
    fn verify_with_refetch_fails_closed_when_mismatch_survives_redownload() {
        let mut verifies = 0u32;
        let mut refetches = 0u32;
        let err = verify_with_refetch(
            "cached",
            |_: &&str| {
                verifies += 1;
                Err(anyhow::anyhow!("hash mismatch"))
            },
            |_| Ok(()),
            || {
                refetches += 1;
                Ok("fresh")
            },
        )
        .expect_err("a mismatch on freshly downloaded bytes must stay a hard failure");
        assert_eq!(refetches, 1, "exactly one re-download attempt");
        assert_eq!(verifies, 2);
        let chain = format!("{err:#}");
        assert!(
            chain.contains("hash mismatch"),
            "original error must be preserved: {chain}"
        );
        assert!(
            chain.contains("re-downloading"),
            "context should say the retry happened: {chain}"
        );
    }

    #[test]
    fn verify_with_refetch_propagates_eviction_failure_without_refetching() {
        let mut refetches = 0u32;
        let err = verify_with_refetch(
            "cached",
            |_: &&str| Err(anyhow::anyhow!("hash mismatch")),
            |_| Err(anyhow::anyhow!("permission denied")),
            || {
                refetches += 1;
                Ok("fresh")
            },
        )
        .expect_err("eviction failure must propagate");
        assert_eq!(refetches, 0);
        assert!(format!("{err:#}").contains("permission denied"));
    }

    #[test]
    fn evicting_a_plain_cached_file_removes_it() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codesage-evict-plain-test-{}-{unique}",
            std::process::id()
        ));
        fs::write(&path, b"corrupted").unwrap();

        evict_cached_artifact(&path).unwrap();

        assert!(!path.exists(), "cached file should be gone");
    }

    #[cfg(unix)]
    #[test]
    fn evicting_a_cached_symlink_removes_blob_and_pointer() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codesage-evict-symlink-test-{}-{unique}",
            std::process::id()
        ));
        let blobs = root.join("blobs");
        let snapshots = root.join("snapshots");
        fs::create_dir_all(&blobs).unwrap();
        fs::create_dir_all(&snapshots).unwrap();
        let blob = blobs.join("deadbeef");
        fs::write(&blob, b"corrupted").unwrap();
        let pointer = snapshots.join("tokenizer.json");
        std::os::unix::fs::symlink("../blobs/deadbeef", &pointer).unwrap();

        evict_cached_artifact(&pointer).unwrap();

        let pointer_gone = pointer.symlink_metadata().is_err();
        let blob_gone = !blob.exists();
        let _ = fs::remove_dir_all(&root);
        assert!(pointer_gone, "snapshot symlink should be gone");
        assert!(
            blob_gone,
            "blob holding the corrupted bytes should be gone too"
        );
    }

    #[cfg(unix)]
    #[test]
    fn evicting_a_symlink_escaping_the_cache_removes_only_the_link() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "codesage-evict-escape-test-{}-{unique}",
            std::process::id()
        ));
        // Cache layout with a victim file OUTSIDE the cache entry.
        let cache = base.join("cache");
        let blobs = cache.join("blobs");
        let snapshots = cache.join("snapshots");
        fs::create_dir_all(&blobs).unwrap();
        fs::create_dir_all(&snapshots).unwrap();
        let victim = base.join("victim.bin");
        fs::write(&victim, b"precious").unwrap();
        let pointer = snapshots.join("model.onnx");
        std::os::unix::fs::symlink("../../victim.bin", &pointer).unwrap();

        evict_cached_artifact(&pointer).unwrap();

        let pointer_gone = pointer.symlink_metadata().is_err();
        let victim_survives = victim.exists();
        let _ = fs::remove_dir_all(&base);
        assert!(pointer_gone, "the escaping symlink itself should be gone");
        assert!(
            victim_survives,
            "a target outside the cache boundary must never be deleted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn evicting_a_dangling_symlink_removes_the_link() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codesage-evict-dangling-test-{}-{unique}",
            std::process::id()
        ));
        let blobs = root.join("blobs");
        let snapshots = root.join("snapshots");
        fs::create_dir_all(&blobs).unwrap();
        fs::create_dir_all(&snapshots).unwrap();
        let pointer = snapshots.join("tokenizer.json");
        std::os::unix::fs::symlink("../blobs/never-written", &pointer).unwrap();

        evict_cached_artifact(&pointer).unwrap();

        let pointer_gone = pointer.symlink_metadata().is_err();
        let _ = fs::remove_dir_all(&root);
        assert!(pointer_gone, "dangling snapshot symlink should be gone");
    }

    #[test]
    fn arbitrary_model_is_rejected_without_override() {
        let err = validate_model_allowed("evil/backdoored-model", false)
            .expect_err("repo-supplied model names outside the allowlist must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("evil/backdoored-model"),
            "error should name the rejected model: {msg}"
        );
        assert!(
            msg.contains("CODESAGE_ALLOW_ANY_MODEL"),
            "error should name the escape hatch: {msg}"
        );
    }

    #[test]
    fn allow_any_override_bypasses_allowlist() {
        assert!(validate_model_allowed("evil/backdoored-model", true).is_ok());
    }

    #[test]
    fn allow_any_needs_env_eligibility_and_project_opt_in() {
        let root = Path::new("/home/user/proj");
        let listed = "/home/user/proj\n";
        assert!(allow_any_for_project_root(Some(root), true, listed));
        // Env flag set, but a project that was never opted in: still refused.
        assert!(!allow_any_for_project_root(
            Some(root),
            true,
            "/home/user/other\n"
        ));
        // Opted-in project, but the env flag not set: still refused.
        assert!(!allow_any_for_project_root(Some(root), false, listed));
        assert!(!allow_any_for_project_root(None, true, listed));
        assert!(!allow_any_for_project_root(None, false, ""));
    }

    #[test]
    fn effective_bypass_allows_opted_in_project_and_refuses_other_projects() {
        let listed = "/home/user/trusted\n";
        let trusted = Path::new("/home/user/trusted");
        let cloned = Path::new("/home/user/cloned-evil");
        // The env flag is "set" (eligible) in both cases; only the opted-in
        // project activates the bypass, so a cloned repo cannot ride a
        // globally-exported flag.
        assert!(
            validate_model_allowed(
                "evil/backdoored-model",
                allow_any_for_project_root(Some(trusted), true, listed)
            )
            .is_ok(),
            "opted-in project loads unpinned"
        );
        let err = validate_model_allowed(
            "evil/backdoored-model",
            allow_any_for_project_root(Some(cloned), true, listed),
        )
        .expect_err("flag set without project opt-in must still refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("allowed-models"),
            "refusal should point at the opt-in file: {msg}"
        );
        assert!(
            msg.contains("CODESAGE_ALLOW_ANY_MODEL"),
            "refusal should still name the env half of the opt-in: {msg}"
        );
    }

    #[test]
    fn project_opt_in_ignores_comments_blanks_and_matches_trailing_slash() {
        let root = Path::new("/home/user/proj");
        let contents = "# trusted projects\n\n  /home/user/proj/  \n/home/user/other\n";
        assert!(project_listed_in_allowlist(root, contents));
        assert!(!project_listed_in_allowlist(
            Path::new("/home/user/eve"),
            contents
        ));
        assert!(
            !project_listed_in_allowlist(root, "# /home/user/proj\n"),
            "a commented-out entry must not opt a project in"
        );
    }

    #[test]
    fn project_root_discovery_walks_up_to_the_codesage_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".codesage")).unwrap();
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_project_root_from(&nested),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(
            find_project_root_from(dir.path()),
            Some(dir.path().to_path_buf())
        );

        let bare = tempfile::tempdir().unwrap();
        assert_eq!(find_project_root_from(bare.path()), None);
    }

    #[test]
    fn allowlist_path_prefers_xdg_over_home_and_fails_closed() {
        assert_eq!(
            user_allowlist_path_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/xdg/codesage/allowed-models"))
        );
        assert_eq!(
            user_allowlist_path_from(None, Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.config/codesage/allowed-models"))
        );
        // Empty values are the same as unset (mirrors the HF_HOME/HOME
        // handling): XDG falls back to HOME, and no home fails closed.
        assert_eq!(
            user_allowlist_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.config/codesage/allowed-models"))
        );
        assert_eq!(user_allowlist_path_from(None, None), None);
        assert_eq!(user_allowlist_path_from(None, Some(OsStr::new(""))), None);
    }

    #[test]
    fn unpinned_warning_names_model_and_resolved_revision() {
        let msg = unpinned_load_message("evil/backdoored-model", None);
        assert!(
            msg.contains("evil/backdoored-model"),
            "warning must name the model: {msg}"
        );
        assert!(
            msg.contains("unpinned"),
            "warning must say the load is unpinned: {msg}"
        );
        let msg = unpinned_load_message("evil/backdoored-model", Some("abc123"));
        assert!(
            msg.contains("evil/backdoored-model") && msg.contains("abc123"),
            "warning must name model and revision: {msg}"
        );
    }

    #[test]
    fn artifact_resolution_is_gated_on_the_allowlist_before_any_lookup() {
        // The fingerprint path resolves model files by the repo-supplied
        // name; an unvalidated name must fail here, offline, before hf-hub
        // is asked for anything (the environment leaves the override unset
        // for this test binary).
        if allow_any_model_from_env() {
            return;
        }
        let err = resolve_model_artifacts("evil/backdoored-model")
            .expect_err("unallowlisted model must not be resolved");
        assert!(
            err.to_string()
                .contains("not on CodeSage's validated-model allowlist"),
            "{err:#}"
        );
        assert_eq!(cached_model_artifacts("evil/backdoored-model"), None);
    }

    /// Creates `root/<artifact>` so a memo hit's existence check passes,
    /// returning the path the way a real fetch would.
    fn fake_fetch(root: &Path, artifact: &str) -> PathBuf {
        let path = root.join(artifact);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, artifact).unwrap();
        path
    }

    /// The sidecar probe for a call whose sidecar fetch succeeds: the file
    /// list is only consulted after a failed fetch.
    fn no_probe() -> Result<bool> {
        panic!("the file list must not be consulted when the sidecar fetch succeeds")
    }

    /// The error hf-hub's `get` returns when the snapshot path is a dangling
    /// symlink: `symlink_or_rename` sees `!dst.exists()`, then `symlink`
    /// fails with `EEXIST` on the occupied name.
    fn hf_dangling_symlink_error(artifact: &str) -> anyhow::Error {
        anyhow::Error::from(hf_hub::api::sync::ApiError::IoError(std::io::Error::from(
            std::io::ErrorKind::AlreadyExists,
        )))
        .context(format!("failed to download {artifact}"))
    }

    #[test]
    fn artifact_paths_are_fetched_once_per_hf_home() {
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let resolve = |root: &Path| {
            resolve_hf_artifact_paths(
                Some(root),
                "test/memoized-model",
                Some("rev"),
                SidecarExpectation::Unknown,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    Ok(fake_fetch(root, artifact))
                },
                no_probe,
            )
            .unwrap()
        };

        let first = resolve(root_a.path());
        assert_eq!(
            fetches.get(),
            3,
            "tokenizer, graph, and sidecar fetched once"
        );
        assert_eq!(first.0, root_a.path().join("tokenizer.json"));
        assert_eq!(first.2, Some(root_a.path().join("onnx/model.onnx_data")));

        let again = resolve(root_a.path());
        assert_eq!(again, first, "the memo serves the same paths");
        assert_eq!(fetches.get(), 3, "a repeat resolution fetches nothing");

        let other = resolve(root_b.path());
        assert_eq!(fetches.get(), 6, "another HF_HOME resolves independently");
        assert_ne!(other, first);
        assert!(other.0.starts_with(root_b.path()), "{}", other.0.display());
    }

    #[test]
    fn artifact_paths_are_keyed_by_model() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let resolve = |model: &str| {
            resolve_hf_artifact_paths(
                Some(root.path()),
                model,
                Some("rev"),
                SidecarExpectation::Unknown,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    Ok(fake_fetch(&root.path().join(model), artifact))
                },
                no_probe,
            )
            .unwrap()
        };

        let minilm = resolve("test/keyed-minilm");
        assert_eq!(fetches.get(), 3);
        let jina = resolve("test/keyed-jina");
        assert_eq!(
            fetches.get(),
            6,
            "a second model under the same root is its own key"
        );
        assert_ne!(jina, minilm);
        assert!(jina.0.starts_with(root.path().join("test/keyed-jina")));
        assert!(minilm.0.starts_with(root.path().join("test/keyed-minilm")));

        assert_eq!(resolve("test/keyed-minilm"), minilm);
        assert_eq!(resolve("test/keyed-jina"), jina);
        assert_eq!(fetches.get(), 6, "both keys are served from the memo");
    }

    #[test]
    fn artifact_paths_are_keyed_by_revision() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let resolve = |revision: Option<&str>| {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/keyed-revision",
                revision,
                SidecarExpectation::Unknown,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    Ok(fake_fetch(
                        &root.path().join(revision.unwrap_or("main")),
                        artifact,
                    ))
                },
                no_probe,
            )
            .unwrap()
        };

        let pinned = resolve(Some("rev"));
        assert_eq!(fetches.get(), 3);
        let unpinned = resolve(None);
        assert_eq!(
            fetches.get(),
            6,
            "the unpinned revision is a distinct key from the pinned one"
        );
        assert_ne!(unpinned, pinned);
        assert!(pinned.0.starts_with(root.path().join("rev")));
        assert!(unpinned.0.starts_with(root.path().join("main")));

        assert_eq!(resolve(Some("rev")), pinned);
        assert_eq!(resolve(None), unpinned);
        assert_eq!(fetches.get(), 6, "both keys are served from the memo");
    }

    #[test]
    fn artifact_paths_memo_is_dropped_when_a_file_disappears() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let graph_symlink_dangling = Cell::new(false);
        let resolve = || {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/evicted-model",
                Some("rev"),
                SidecarExpectation::Unknown,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    if artifact == "onnx/model.onnx" && graph_symlink_dangling.get() {
                        return Err(hf_dangling_symlink_error(artifact));
                    }
                    Ok(fake_fetch(root.path(), artifact))
                },
                no_probe,
            )
        };

        let first = resolve().unwrap();
        assert_eq!(fetches.get(), 3);
        assert_eq!(resolve().unwrap(), first);
        assert_eq!(fetches.get(), 3, "all files present: served from the memo");

        fs::remove_file(&first.1).unwrap();
        assert_eq!(
            resolve().unwrap(),
            first,
            "the refetch lands on the same paths"
        );
        assert_eq!(
            fetches.get(),
            6,
            "a missing graph invalidates the entry and all three are refetched"
        );

        fs::remove_file(first.2.as_ref().unwrap()).unwrap();
        assert_eq!(resolve().unwrap(), first);
        assert_eq!(
            fetches.get(),
            9,
            "a missing sidecar invalidates the entry too"
        );

        // A snapshot symlink over a deleted blob is a miss too: the entry
        // must be dropped. Recovery is not asserted because production has
        // none here: hf-hub's `symlink_or_rename` fails with EEXIST on the
        // dangling name, so the refetch surfaces that error and nothing is
        // memoized until the link is removed by hand.
        fs::remove_file(&first.1).unwrap();
        std::os::unix::fs::symlink(root.path().join("gone-blob"), &first.1).unwrap();
        graph_symlink_dangling.set(true);
        let err = resolve().expect_err("a dangling graph symlink surfaces the refetch failure");
        assert_eq!(
            fetches.get(),
            5 + 6,
            "the dangling symlink invalidated the entry: tokenizer and graph were re-attempted"
        );
        assert!(
            err.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
            }),
            "{err:?}"
        );
        let key = (
            Some(root.path().to_path_buf()),
            "test/evicted-model".to_string(),
            Some("rev".to_string()),
        );
        assert!(
            !artifact_paths_memo()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&key),
            "a failed graph fetch memoizes nothing"
        );
    }

    #[test]
    fn artifact_paths_memoize_a_sidecar_the_file_list_proves_absent() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let probes = Cell::new(0u32);
        let err = resolve_hf_artifact_paths(
            Some(root.path()),
            "test/small-model",
            None,
            SidecarExpectation::Unknown,
            |_| {
                fetches.set(fetches.get() + 1);
                Err(anyhow::anyhow!("network down"))
            },
            no_probe,
        )
        .expect_err("a failed tokenizer fetch must surface");
        assert!(err.to_string().contains("network down"));
        assert_eq!(fetches.get(), 1);

        let resolve = || {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/small-model",
                None,
                SidecarExpectation::Unknown,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    if artifact == ONNX_DATA_ARTIFACT {
                        return Err(anyhow::anyhow!("request error: status 404"));
                    }
                    Ok(fake_fetch(root.path(), artifact))
                },
                || {
                    probes.set(probes.get() + 1);
                    Ok(false)
                },
            )
            .unwrap()
        };

        let first = resolve();
        assert_eq!(
            fetches.get(),
            4,
            "the tokenizer failure was not memoized; all three fetched"
        );
        assert_eq!(
            probes.get(),
            1,
            "the file list is consulted once after the 404"
        );
        assert_eq!(first.2, None);

        assert_eq!(resolve(), first);
        assert_eq!(
            fetches.get(),
            4,
            "a proven-absent sidecar is not re-attempted"
        );
        assert_eq!(probes.get(), 1, "and the file list is not re-read");
    }

    #[test]
    fn artifact_paths_retry_a_sidecar_that_is_listed_or_unverifiable() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let sidecar_err = Cell::new(Some("request error: status 503"));
        let resolve = |probe: &dyn Fn() -> Result<bool>| {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/large-model",
                Some("rev"),
                SidecarExpectation::Unknown,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    if artifact == ONNX_DATA_ARTIFACT
                        && let Some(message) = sidecar_err.get()
                    {
                        return Err(anyhow::anyhow!(message));
                    }
                    Ok(fake_fetch(root.path(), artifact))
                },
                probe,
            )
            .unwrap()
        };

        // Listed upstream, fetch failed: a transient failure on a model that
        // does ship external weights. Memoizing absence here would break
        // every later session build until the daemon restarts.
        let first = resolve(&|| Ok(true));
        assert_eq!(fetches.get(), 3);
        assert_eq!(
            first.2, None,
            "an unresolved sidecar is reported as absent for this call"
        );

        // The file list itself unavailable: no answer either way.
        assert_eq!(
            resolve(&|| Err(anyhow::anyhow!("request error: timeout"))),
            first
        );
        assert_eq!(
            fetches.get(),
            4,
            "only the sidecar is re-attempted; tokenizer and graph come from the memo"
        );

        // The hub recovers: the sidecar lands and the memo is complete.
        sidecar_err.set(None);
        let resolved = resolve(&no_probe);
        assert_eq!(fetches.get(), 5);
        assert_eq!(
            resolved.2,
            Some(root.path().join(ONNX_DATA_ARTIFACT)),
            "the sidecar is picked up once its fetch succeeds"
        );
        assert_eq!((&resolved.0, &resolved.1), (&first.0, &first.1));

        assert_eq!(resolve(&no_probe), resolved);
        assert_eq!(
            fetches.get(),
            5,
            "a fetched sidecar is served from the memo"
        );
    }

    /// The sidecar probe for a pinned load: the pin already answered, so the
    /// file list must never be consulted.
    fn no_pinned_probe() -> Result<bool> {
        panic!("the file list must not be consulted when the pin declares the sidecar outcome")
    }

    #[test]
    fn artifact_paths_skip_a_sidecar_the_pin_declares_absent() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let resolve = || {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/pinned-small-model",
                Some("rev"),
                SidecarExpectation::Absent,
                |artifact| {
                    assert_ne!(
                        artifact, ONNX_DATA_ARTIFACT,
                        "a pin that declares no sidecar must not fetch one"
                    );
                    fetches.set(fetches.get() + 1);
                    Ok(fake_fetch(root.path(), artifact))
                },
                no_pinned_probe,
            )
            .unwrap()
        };

        let first = resolve();
        assert_eq!(fetches.get(), 2, "tokenizer and graph only; no sidecar GET");
        assert_eq!(first.2, None);

        // `fetches` cannot tell a memoized absence (`Some(None)`) from an
        // unresolved sidecar (`None`): neither fetches under `Absent`. The
        // memo entry itself is the discriminator.
        let key = (
            Some(root.path().to_path_buf()),
            "test/pinned-small-model".to_string(),
            Some("rev".to_string()),
        );
        let entry = artifact_paths_memo()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned()
            .expect("a pinned resolution memoizes its paths");
        assert_eq!(
            entry.sidecar,
            Some(None),
            "the declared absence is memoized as settled, not left unresolved"
        );

        assert_eq!(resolve(), first);
        assert_eq!(fetches.get(), 2, "a repeat resolution fetches nothing");
    }

    #[test]
    fn sidecar_expectation_derives_from_the_pin_the_loader_verifies() {
        let minilm = "sentence-transformers/all-MiniLM-L6-v2";
        assert_eq!(
            model_pin(minilm).unwrap().onnx_data_sha256,
            None,
            "this test assumes the MiniLM pin declares no sidecar"
        );
        assert_eq!(
            sidecar_expectation(minilm, false),
            SidecarExpectation::Absent,
            "an allowlisted pin with onnx_data_sha256: None declares the sidecar absent"
        );
        assert_eq!(
            sidecar_expectation(minilm, true),
            SidecarExpectation::Unknown,
            "CODESAGE_ALLOW_ANY_MODEL resolves the repository head, where the pin says nothing"
        );
        assert_eq!(
            sidecar_expectation("test/model-without-a-pin-entry", false),
            SidecarExpectation::Unknown,
            "no pin entry: the network has to answer"
        );

        let pinned_with_sidecar = ModelPin {
            model: "test/pinned-large-model",
            revision: "rev",
            tokenizer_sha256: "",
            onnx_sha256: "",
            onnx_data_sha256: Some(
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        };
        assert_eq!(
            SidecarExpectation::from_pin(Some(&pinned_with_sidecar)),
            SidecarExpectation::Required
        );
        assert_eq!(
            SidecarExpectation::from_pin(None),
            SidecarExpectation::Unknown
        );
    }

    #[test]
    fn artifact_paths_refuse_an_on_disk_sidecar_the_pin_declares_absent() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let resolve = |model: &str| {
            resolve_hf_artifact_paths(
                Some(root.path()),
                model,
                Some("rev"),
                SidecarExpectation::Absent,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    Ok(fake_fetch(&root.path().join(model), artifact))
                },
                no_pinned_probe,
            )
        };

        // Nothing on disk: resolution succeeds and is memoized.
        let first = resolve("test/stray-sidecar-later").unwrap();
        assert_eq!(fetches.get(), 2);
        assert_eq!(first.2, None);

        // A sidecar appears next to the graph after memoization (an external
        // `huggingface-cli download`): the memo hit refuses it, with the path
        // ONNX Runtime would have loaded it from.
        let stray = first.1.parent().unwrap().join("model.onnx_data");
        fs::write(&stray, b"unverified weights").unwrap();
        let err = resolve("test/stray-sidecar-later")
            .expect_err("a sidecar on disk under a pin that declares none must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains(&stray.display().to_string()),
            "error should name the refused path: {msg}"
        );
        assert!(
            msg.contains("declares no external-weights sidecar"),
            "error should say why the file is refused: {msg}"
        );
        assert_eq!(fetches.get(), 2, "the refusal costs no fetch");

        // Already on disk before the first resolution: refused before
        // anything is memoized.
        let early = root.path().join("test/stray-sidecar-first/onnx");
        fs::create_dir_all(&early).unwrap();
        fs::write(early.join("model.onnx_data"), b"unverified weights").unwrap();
        let err = resolve("test/stray-sidecar-first")
            .expect_err("a pre-existing sidecar is refused on the first resolution too");
        assert!(
            err.to_string()
                .contains(&early.join("model.onnx_data").display().to_string()),
            "{err}"
        );
        let key = (
            Some(root.path().to_path_buf()),
            "test/stray-sidecar-first".to_string(),
            Some("rev".to_string()),
        );
        assert!(
            !artifact_paths_memo()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&key),
            "a refused resolution memoizes nothing"
        );
    }

    #[test]
    fn artifact_paths_fetch_a_pinned_sidecar_without_the_file_list() {
        let root = tempfile::tempdir().unwrap();
        let fetches = Cell::new(0u32);
        let sidecar_err = Cell::new(Some("request error: status 503"));
        let resolve = || {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/pinned-large-model",
                Some("rev"),
                SidecarExpectation::Required,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    if artifact == ONNX_DATA_ARTIFACT
                        && let Some(message) = sidecar_err.get()
                    {
                        return Err(anyhow::anyhow!(message));
                    }
                    Ok(fake_fetch(root.path(), artifact))
                },
                no_pinned_probe,
            )
            .unwrap()
        };

        // The pin requires the sidecar and its fetch failed: unresolved, not
        // an error here. `verify_pinned_model_artifacts` reports the missing
        // pinned sidecar with its own message.
        let first = resolve();
        assert_eq!(
            fetches.get(),
            3,
            "tokenizer, graph, and the sidecar attempted"
        );
        assert_eq!(
            first.2, None,
            "an unresolved pinned sidecar is reported as absent for this call"
        );

        assert_eq!(resolve(), first);
        assert_eq!(
            fetches.get(),
            4,
            "only the sidecar is re-attempted; tokenizer and graph come from the memo"
        );

        sidecar_err.set(None);
        let resolved = resolve();
        assert_eq!(fetches.get(), 5);
        assert_eq!(
            resolved.2,
            Some(root.path().join(ONNX_DATA_ARTIFACT)),
            "the pinned sidecar is picked up once its fetch succeeds"
        );
        assert_eq!((&resolved.0, &resolved.1), (&first.0, &first.1));

        assert_eq!(resolve(), resolved);
        assert_eq!(
            fetches.get(),
            5,
            "a fetched sidecar is served from the memo"
        );
    }

    #[test]
    fn artifact_stat_key_tracks_size_and_mtime_without_reading_content() {
        let dir = tempfile::tempdir().unwrap();
        let tokenizer = dir.path().join("tokenizer.json");
        let onnx = dir.path().join("model.onnx");
        std::fs::write(&tokenizer, b"{}").unwrap();
        std::fs::write(&onnx, b"aaaa").unwrap();
        let fixed = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let touch = |path: &Path, at: std::time::SystemTime| {
            std::fs::File::open(path).unwrap().set_modified(at).unwrap();
        };
        touch(&tokenizer, fixed);
        touch(&onnx, fixed);
        let artifacts = ModelArtifacts {
            tokenizer: tokenizer.clone(),
            onnx: onnx.clone(),
            onnx_data: None,
            ort_runtime: None,
        };
        let base = artifacts.stat_key().unwrap();

        // Same size, same mtime, different bytes: the stat key cannot see it
        // (the content digest can); that is the trade the per-call key makes.
        std::fs::write(&onnx, b"bbbb").unwrap();
        touch(&onnx, fixed);
        assert_eq!(artifacts.stat_key().unwrap(), base);

        std::fs::write(&onnx, b"bbbbb").unwrap();
        touch(&onnx, fixed);
        assert_ne!(
            artifacts.stat_key().unwrap(),
            base,
            "a size change is visible"
        );

        std::fs::write(&onnx, b"aaaa").unwrap();
        touch(&onnx, fixed + Duration::from_secs(1));
        assert_ne!(
            artifacts.stat_key().unwrap(),
            base,
            "an mtime change is visible"
        );

        let missing = ModelArtifacts {
            tokenizer,
            onnx: dir.path().join("absent.onnx"),
            onnx_data: None,
            ort_runtime: None,
        };
        assert_eq!(missing.stat_key(), None);
    }

    #[test]
    fn artifact_stat_key_is_keyed_by_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(real.join("model.onnx"), b"aaaa").unwrap();
        let absolute = ModelArtifacts {
            tokenizer: real.join("tokenizer.json"),
            onnx: real.join("model.onnx"),
            onnx_data: None,
            ort_runtime: None,
        };
        // The same files spelled through `.` and `..` segments and through
        // a symlinked directory, as a relative HF_HOME resolves them.
        let dotted = ModelArtifacts {
            tokenizer: real.join(".").join("tokenizer.json"),
            onnx: real.join("sub").join("..").join("model.onnx"),
            onnx_data: None,
            ort_runtime: None,
        };
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let linked = ModelArtifacts {
            tokenizer: link.join("tokenizer.json"),
            onnx: link.join("model.onnx"),
            onnx_data: None,
            ort_runtime: None,
        };
        let base = absolute.stat_key().unwrap();
        assert_eq!(dotted.stat_key().unwrap(), base, "dotted spelling");
        assert_eq!(linked.stat_key().unwrap(), base, "symlinked spelling");
        assert!(
            !base.contains("/./") && !base.contains("/../") && !base.contains("/link/"),
            "the key records the canonical path: {base}"
        );

        // The directory moves: same names, same sizes, same mtimes, another
        // location — another key, and the old spelling keys nothing.
        let moved = dir.path().join("moved");
        std::fs::rename(&real, &moved).unwrap();
        let relocated = ModelArtifacts {
            tokenizer: moved.join("tokenizer.json"),
            onnx: moved.join("model.onnx"),
            onnx_data: None,
            ort_runtime: None,
        };
        assert_ne!(
            relocated.stat_key().unwrap(),
            base,
            "a moved dir keys differently"
        );
        assert_eq!(absolute.stat_key(), None);
    }

    #[test]
    fn hf_download_timeout_env_parser_rejects_unbounded_values() {
        assert_eq!(
            hf_download_timeout_from_env_value(None).unwrap(),
            HF_DOWNLOAD_TIMEOUT
        );
        assert_eq!(
            hf_download_timeout_from_env_value(Some("7")).unwrap(),
            Duration::from_secs(7)
        );
        assert!(hf_download_timeout_from_env_value(Some("0")).is_err());
        assert!(hf_download_timeout_from_env_value(Some("abc")).is_err());
    }

    #[test]
    fn a_forced_cpu_fallback_fingerprints_as_the_cpu_setup() {
        let cuda = EmbeddingConfig {
            device: "cuda".to_string(),
            ..EmbeddingConfig::default()
        };
        let configured = crate::fingerprint::configured_execution_provider(&cuda.device);
        assert_eq!(configured, "cuda");
        let as_configured =
            crate::fingerprint::SemanticFingerprint::with_artifact_digest(&cuda, 384, "d");

        // Libraries mapped: the session runs where it was asked to.
        let (provider, fell_back) =
            effective_execution_provider(configured, Ok(()), false).unwrap();
        assert_eq!((provider, fell_back), ("cuda", false));
        assert_eq!(
            as_configured.with_execution_provider(provider),
            as_configured
        );

        // Libraries missing, fallback refused: the load fails as before.
        let err =
            effective_execution_provider(configured, Err(anyhow::anyhow!("no libcuda")), false)
                .unwrap_err();
        assert!(err.to_string().contains("no libcuda"));

        // Libraries missing, fallback allowed: the session runs on the CPU
        // and its fingerprint is the CPU one, not the configured CUDA one.
        let (provider, fell_back) =
            effective_execution_provider(configured, Err(anyhow::anyhow!("no libcuda")), true)
                .unwrap();
        assert_eq!((provider, fell_back), ("cpu", true));
        let effective = as_configured.with_execution_provider(provider);
        assert_ne!(effective, as_configured);
        assert!(effective.as_str().contains("device=cpu"), "{effective}");
    }

    #[test]
    fn token_type_ids_detected_from_input_names() {
        assert!(wants_token_type_ids(
            ["input_ids", "token_type_ids", "attention_mask"].into_iter()
        ));
        assert!(!wants_token_type_ids(
            ["input_ids", "attention_mask"].into_iter()
        ));
        assert!(!wants_token_type_ids(std::iter::empty()));
    }

    #[cfg(all(feature = "cuda", target_os = "linux"))]
    #[test]
    fn cuda_maps_without_cudnn_or_cublas_are_rejected() {
        let maps = "\
7f0000000000-7f0000010000 r-xp 00000000 00:00 0 /usr/lib/libcuda.so.1
7f0000010000-7f0000020000 r-xp 00000000 00:00 0 /usr/lib/libcudart.so.12
";

        let err = verify_cuda_libs_mapped(maps)
            .expect_err("CUDA runtime without cuDNN/cuBLAS should be treated as CPU fallback");
        let msg = err.to_string();
        assert!(
            msg.contains("libcudnn") && msg.contains("libcublas"),
            "error should name the missing math libraries: {msg}"
        );
    }

    /// Serializes the env-seam tests below: `HF_HOME`, `HOME`, and
    /// `ORT_DYLIB_PATH` are process-global and the harness runs tests on
    /// threads. Every test below restores what it touches via [`SavedEnv`].
    fn env_lock() -> std::sync::LockResult<std::sync::MutexGuard<'static, ()>> {
        static LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));
        LOCK.lock()
    }

    /// Saves one env var and restores it on drop, so a serial env-seam test
    /// cannot leak into another test if it panics mid-mutation.
    struct SavedEnv {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl SavedEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.old {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn hf_cache_prefers_hf_home_over_home() {
        let _lock = env_lock().unwrap_or_else(PoisonError::into_inner);
        let hf_home = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _hf = SavedEnv::set("HF_HOME", hf_home.path().to_str().unwrap());
        let _home = SavedEnv::set("HOME", home.path().to_str().unwrap());

        let cache = hf_cache_from_env().expect("a set HF_HOME must resolve");
        assert_eq!(
            cache.path(),
            &hf_home.path().join("hub"),
            "HF_HOME wins over HOME, exactly as hf-hub reads it"
        );
        // The download client builds from that same root without touching
        // the network: construction is the seam under test, not the hub.
        hf_api().expect("client construction from the resolved cache must succeed");
    }

    #[test]
    fn hf_cache_falls_back_to_home_when_hf_home_unset() {
        let _lock = env_lock().unwrap_or_else(PoisonError::into_inner);
        let home = tempfile::tempdir().unwrap();
        let _hf = SavedEnv::remove("HF_HOME");
        let _home = SavedEnv::set("HOME", home.path().to_str().unwrap());

        let cache = hf_cache_from_env().expect("a set HOME must resolve");
        assert_eq!(
            cache.path(),
            &home.path().join(".cache").join("huggingface").join("hub")
        );
    }

    #[test]
    fn hf_cache_is_unresolvable_without_hf_home_or_home() {
        let _lock = env_lock().unwrap_or_else(PoisonError::into_inner);
        let _hf = SavedEnv::remove("HF_HOME");
        let _home = SavedEnv::remove("HOME");

        assert!(
            hf_cache_from_env().is_none(),
            "with neither variable set there is no cache root (and no HOME-less panic)"
        );
        let err = hf_api().expect_err("an unresolvable cache root must be an error, not a panic");
        let msg = err.to_string();
        assert!(
            msg.contains("HF_HOME") && msg.contains("HOME"),
            "the error must name both variables so the fix is actionable: {msg}"
        );

        let _empty_home = SavedEnv::set("HOME", "");
        assert!(
            hf_cache_from_env().is_none(),
            "an empty HOME resolves nothing either"
        );
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn empty_ort_dylib_path_counts_as_unset() {
        let _lock = env_lock().unwrap_or_else(PoisonError::into_inner);
        let _empty = SavedEnv::set("ORT_DYLIB_PATH", "");
        assert_eq!(
            ort_dylib_path_from_env(),
            None,
            "an empty ORT_DYLIB_PATH must fall through to discovery instead of \
             counting as caller-takes-control and hard-failing inside ORT"
        );
        {
            let _set = SavedEnv::set("ORT_DYLIB_PATH", "/tmp/libonnxruntime.so");
            assert_eq!(
                ort_dylib_path_from_env(),
                Some(PathBuf::from("/tmp/libonnxruntime.so"))
            );
        }
        let _unset = SavedEnv::remove("ORT_DYLIB_PATH");
        assert_eq!(ort_dylib_path_from_env(), None);
    }

    #[test]
    fn evicting_a_cached_artifact_purges_its_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokenizer.json");
        std::fs::write(&path, b"aaaaaaaaaaaaaaaaaaaa").unwrap();
        let before = crate::fingerprint::artifact_read_count(&path);
        let first = crate::fingerprint::cached_file_digest(&path).unwrap();
        assert_eq!(crate::fingerprint::artifact_read_count(&path), before + 1);
        // Same length, same mtime after the refetch: without a purge the
        // (size, mtime) key would serve the evicted bytes' hash and trip a
        // false supply-chain alarm in `verify_model_artifact_sha256`.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        evict_cached_artifact(&path).unwrap();
        assert!(!path.exists(), "eviction still removes the file");
        std::fs::write(&path, b"bbbbbbbbbbbbbbbbbbbb").unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let second = crate::fingerprint::cached_file_digest(&path).unwrap();
        assert_ne!(
            second, first,
            "the post-evict digest must reflect the refetched bytes, not the evicted ones"
        );
        assert_eq!(
            crate::fingerprint::artifact_read_count(&path),
            before + 2,
            "the refetch must be re-read from disk, not served from memory"
        );
    }

    #[test]
    fn coreml_provider_is_attested_from_configuration() {
        // There is no functional CoreML check (see the why-not at the
        // provider selection in `load_onnx_session_with_provider`): ORT
        // exposes no post-commit provider query, so the attested provider
        // is the configured one. Pin that mapping here.
        assert_eq!(
            crate::fingerprint::configured_execution_provider("coreml"),
            "coreml"
        );
        assert_eq!(
            crate::fingerprint::configured_execution_provider("CoreML"),
            "coreml"
        );
        assert_eq!(
            crate::fingerprint::configured_execution_provider("cpu"),
            "cpu"
        );
    }
}
