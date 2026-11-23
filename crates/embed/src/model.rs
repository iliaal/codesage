use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
#[cfg(not(target_vendor = "apple"))]
use std::sync::LazyLock;
#[cfg(feature = "cuda")]
use std::sync::Once;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use hf_hub::{Repo, RepoType};
use ort::environment::Environment;
use ort::session::{Session, builder::GraphOptimizationLevel};
pub use tokenizers::{Encoding, Tokenizer};
use wait_timeout::ChildExt;

use crate::config::{EmbeddingConfig, MAX_SEQ_LENGTH, PoolingStrategy, wants_coreml, wants_cuda};

#[cfg(not(target_vendor = "apple"))]
static ORT_DYLIB: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| ort_dylib_path_from_env().or_else(discover_ort_dylib));
#[cfg(feature = "cuda")]
static CUDA_PRELOAD: Once = Once::new();
/// Fallback preload set, used only when the ONNX Runtime CUDA provider next to
/// the selected runtime is absent or unparseable: the CUDA 12 sonames pip's
/// `onnxruntime-gpu` 1.24 provider links against. ort's `ep::cuda::CUDA_DYLIBS`
/// follows the CUDA major of ort's own prebuilt binaries (CUDA 13 since rc.14),
/// not the runtime this crate loads.
#[cfg(feature = "cuda")]
const CUDA_DYLIBS: &[&str] = &[
    "libcudart.so.12",
    "libcublasLt.so.12",
    "libcublas.so.12",
    "libnvrtc.so.12",
    "libcurand.so.10",
    "libcufft.so.11",
];
static ORT_ENVIRONMENT: Mutex<Option<Environment>> = Mutex::new(None);

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

pub(crate) fn collect_nvidia_lib_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
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
            let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            children.sort();
            for child in children {
                let lib_dir = child.join("lib");
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
    codesage_protocol::work::checkpoint()?;
    let control = codesage_protocol::work::current();
    let controlled = control.is_some();
    let lease = control.as_ref().and_then(|control| control.worker_lease());
    let timeout = hf_download_timeout();
    let started = std::time::Instant::now();
    let (tx, rx) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let _lease = lease;
        let _scope = control.as_ref().map(|control| control.enter());
        let result = (|| {
            codesage_protocol::work::checkpoint()?;
            let result = work()?;
            codesage_protocol::work::checkpoint()?;
            Ok(result)
        })();
        let _ = tx.send(result);
    });

    loop {
        codesage_protocol::work::checkpoint()?;
        let remaining = timeout.saturating_sub(started.elapsed());
        let wait = if controlled {
            remaining.min(Duration::from_millis(25))
        } else {
            remaining
        };
        match rx.recv_timeout(wait) {
            Ok(result) => {
                codesage_protocol::work::checkpoint()?;
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() < timeout => {}
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
}

/// The HuggingFace client every download and file-list probe shares, built
/// from the same [`hf_cache_from_env`] root the cached-only probe
/// ([`cached_model_artifacts`]) consults. Never `Api::new`, whose
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

/// ORT can report CUDA registration success while running on CPU.
/// Require mapped CUDA runtime and math libraries after session creation.
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
             (2) install the nvidia-* pip packages for the CUDA major onnxruntime-gpu was built \
             against (cuda-runtime, cudnn, cublas, cudart, \
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
             Fixes: install the nvidia cuDNN and cuBLAS pip packages for onnxruntime-gpu's CUDA major, set \
             CODESAGE_NVIDIA_LIBS to the directory containing them, and check \
             `codesage doctor` for nvidia-lib discovery details. \
             To override (e.g. for tests), set CODESAGE_ALLOW_CPU_FALLBACK=1."
        );
    }
    Ok(())
}

/// Resolve native-library locations ahead of the first session, without loading
/// the runtime or changing the process environment. Safe after threads start.
/// Session loading and fingerprinting also resolve these locations lazily, so
/// library consumers do not need to call this startup optimization.
pub fn init_for_main() {
    #[cfg(not(target_vendor = "apple"))]
    let _ = selected_ort_dylib();
    #[cfg(feature = "cuda")]
    let _ = discover_nvidia_lib_dirs();
}

/// dlopen the CUDA/cuDNN stack from the discovered NVIDIA library
/// directories, once per process. Maps ~200 MB RSS and ~1.9 GB virtual, so
/// it runs only from the session loader when a session actually requests the
/// CUDA execution provider, never at startup.
///
/// The sonames are the CUDA/cuDNN `DT_NEEDED` entries of the
/// `libonnxruntime_providers_cuda.so` beside the selected ONNX Runtime, so the
/// preload follows the CUDA major that provider was built against; the static
/// list applies only when that file is absent or unparseable. Directory choice
/// follows `cuda_preload::resolve_dir`.
///
/// Writes no environment variables: this runs from whatever thread builds the
/// session, where a `set_var` would race every concurrent `getenv` in the
/// process. Discovery and absolute-path preloading do not require a startup hook.
#[cfg(feature = "cuda")]
pub fn preload_native_libs() {
    CUDA_PRELOAD.call_once(|| {
        let lib_dirs = discover_nvidia_lib_dirs();
        if lib_dirs.is_empty() {
            return;
        }

        let fallback: Vec<&str> = CUDA_DYLIBS
            .iter()
            .chain(ort::ep::cuda::CUDNN_DYLIBS.iter())
            .copied()
            .collect();
        let provider = crate::cuda_preload::provider_path(selected_ort_dylib());
        let plan = crate::cuda_preload::plan(provider.as_deref(), &fallback, lib_dirs);
        if let crate::cuda_preload::PlanSource::Fallback(reason) = &plan.source {
            tracing::warn!(%reason, "CUDA provider dependencies not derived; preloading the static CUDA 12 list");
        }
        let cuda_major = crate::cuda_preload::cuda_major(&plan.sonames);

        // ort::ep::cuda::preload_dylibs accepts one CUDA root and one cuDNN root,
        // but pip's nvidia-* packages place the libraries in separate directories.
        let mut chosen = Vec::with_capacity(plan.sonames.len());
        for lib_name in &plan.sonames {
            let dir = crate::cuda_preload::resolve_dir(lib_name, cuda_major, lib_dirs);
            if let Some(dir) = dir
                && let Err(e) = ort::util::preload_dylib(dir.join(lib_name))
            {
                tracing::warn!(lib = %lib_name, error = %e, "CUDA preload failed");
            }
            chosen.push(format!(
                "{lib_name}={}",
                dir.map_or_else(|| "<not found>".to_string(), |d| d.display().to_string())
            ));
        }
        tracing::debug!(
            source = ?plan.source,
            cuda_major = ?cuda_major,
            preload = %chosen.join(", "),
            "CUDA preload plan"
        );
    });
}

/// Locate ONNX Runtime in pip site-packages, then standard system locations.
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

/// Treat an empty ORT_DYLIB_PATH as unset so discovery and fingerprints agree.
#[cfg(not(target_vendor = "apple"))]
fn ort_dylib_path_from_env() -> Option<PathBuf> {
    std::env::var_os("ORT_DYLIB_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Select one runtime path for both fingerprinting and session loading. Keep it
/// process-local: publishing discovery through `set_var` is unsound once other
/// threads may be reading the native environment.
#[cfg(not(target_vendor = "apple"))]
fn selected_ort_dylib() -> Option<&'static Path> {
    ORT_DYLIB.as_deref()
}

/// Load the configured or discovered ONNX Runtime without changing environment
/// variables. Safe to call after other threads have started. If discovery finds
/// nothing, leave ort's normal platform-soname fallback available to sessions.
/// Call before using other ort APIs: ort retains the first runtime loaded in the
/// process, including one loaded by the embedding application's own ort calls.
#[cfg(not(target_vendor = "apple"))]
pub fn init_ort_dylib() -> Result<()> {
    if let Some(path) = selected_ort_dylib() {
        // ort loads the library inside init_from; the returned builder is
        // dropped unbuilt because ort_environment builds the process's one
        // Environment after this load.
        drop(
            ort::init_from(path).with_context(|| {
                format!("loading ONNX Runtime shared library {}", path.display())
            })?,
        );
    }
    Ok(())
}

/// The process's ONNX Runtime environment, built on first use after loading the
/// selected runtime. ort allows one Environment per process and refuses a second
/// build even after the first is dropped, so it is retained for the process
/// lifetime and shared by every session. An embedding application that already
/// built its own Environment makes this fail.
pub fn ort_environment() -> Result<Environment> {
    let mut slot = ORT_ENVIRONMENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(environment) = slot.as_ref() {
        return Ok(environment.clone());
    }
    #[cfg(not(target_vendor = "apple"))]
    init_ort_dylib()?;
    let environment = ort::init()
        .build()
        .context("creating the ONNX Runtime environment")?;
    *slot = Some(environment.clone());
    Ok(environment)
}

/// Repo-local model names are untrusted: require an allowlisted model and
/// pinned hashes before native ONNX loading. Bypass requires both env eligibility
/// and a canonical project root in the user-owned allowlist; see
/// [`allow_any_model_from_env`].
const ALLOWED_MODELS: &[&str] = &[
    "sentence-transformers/all-MiniLM-L6-v2",
    "cross-encoder/ms-marco-MiniLM-L6-v2",
    "jinaai/jina-embeddings-v2-base-code",
];

struct ModelPin {
    model: &'static str,
    /// Hugging Face repository the artifacts are fetched from. Differs from
    /// `model` where CodeSage loads graphs derived from the upstream model
    /// (`scripts/derive-jina-onnx.py`); the config keeps the upstream name.
    repo: &'static str,
    revision: &'static str,
    tokenizer_sha256: &'static str,
    onnx_sha256: &'static str,
    /// Required when this revision uses external weights. `None` forbids an
    /// on-disk sidecar: ORT loads adjacent weights independently of our fetches,
    /// so [`refuse_undeclared_sidecar`] must reject unverified leftovers.
    onnx_data_sha256: Option<&'static str>,
    /// Graph at the same revision that replaces `onnx/model.onnx` when the
    /// device selects the CUDA execution provider.
    cuda_onnx: Option<GraphPin>,
    /// Tokens an embedding input is truncated to; bounded by the model's
    /// position embeddings.
    max_seq_length: usize,
}

struct GraphPin {
    file: &'static str,
    sha256: &'static str,
}

/// Commit of the derived all-MiniLM-L6-v2 graphs (portable fp32 pooled plus an
/// ORT-fused fp16 CUDA graph), built from upstream revision
/// c9745ed1d9f207416be6d2e6f8de32d1f16199bf.
pub(crate) const MINILM_DERIVED_REVISION: &str = "292b973de823b96bb970cd40b233c8075427110d";

/// Commit of the derived jina-embeddings-v2-base-code graphs, built from
/// upstream revision 516f4baf13dec4ddddda8631e019b5737c8bc250.
pub(crate) const JINA_CODE_DERIVED_REVISION: &str = "211266c1a2fdf1191f617611af2cbef9b7a96944";

/// Commit of the ms-marco-MiniLM-L6-v2 graphs (upstream plus a fused fp16
/// CUDA graph), built from upstream revision
/// c5ee24cb16019beea0893ab7796b1df96625c6b8.
const MS_MARCO_DERIVED_REVISION: &str = "3a8dae18b7a92308d7d63b834f3bfa01bab02d32";

const MODEL_PINS: &[ModelPin] = &[
    ModelPin {
        model: "sentence-transformers/all-MiniLM-L6-v2",
        repo: "IA0x00/all-MiniLM-L6-v2-codesage",
        revision: MINILM_DERIVED_REVISION,
        tokenizer_sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
        onnx_sha256: "99154eb07d13eb3de6b2132a7f043195f53cc944f957f69710eed3dfcea62858",
        onnx_data_sha256: None,
        cuda_onnx: Some(GraphPin {
            file: "onnx/model_cuda_fp16.onnx",
            sha256: "f46f5f118a6473f8da23c6b2856c97680abd2836f993b16d8fa3e2499f3eee88",
        }),
        max_seq_length: MAX_SEQ_LENGTH,
    },
    ModelPin {
        model: "cross-encoder/ms-marco-MiniLM-L6-v2",
        repo: "IA0x00/ms-marco-MiniLM-L6-v2-codesage",
        revision: MS_MARCO_DERIVED_REVISION,
        tokenizer_sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        onnx_sha256: "5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a",
        onnx_data_sha256: None,
        cuda_onnx: Some(GraphPin {
            file: "onnx/model_cuda_fp16.onnx",
            sha256: "32eef6d63e978aba96b6c4134b93b252eab2eb6ae3f8b1b5d15c96b4473f7eb2",
        }),
        max_seq_length: MAX_SEQ_LENGTH,
    },
    ModelPin {
        model: "jinaai/jina-embeddings-v2-base-code",
        repo: "IA0x00/jina-embeddings-v2-base-code-codesage",
        revision: JINA_CODE_DERIVED_REVISION,
        tokenizer_sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
        onnx_sha256: "b38da98ce812d0f2c1057e734c138e058f3c4f0f2c7060855ef02f03bfb6f066",
        onnx_data_sha256: None,
        cuda_onnx: Some(GraphPin {
            file: "onnx/model_cuda_fp16.onnx",
            sha256: "cc6736d11d8a73c174359e9263216ce55e98682763206ca3369860e8159dd59e",
        }),
        // ALiBi has no learned positions. Real C code in php-src runs a median
        // of 611 tokens per 1500-byte chunk; data tables are dropped by the
        // chunker before they reach the cap.
        max_seq_length: 1024,
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
    /// The path component is canonical (absolute, symlinks resolved), so a
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
/// load: the configured or discovered path passed directly to ort. `Ok(None)`
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
        match selected_ort_dylib() {
            Some(path) => Ok(Some(path.to_path_buf())),
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

/// The Hugging Face repository a load of `model` fetches from: the pin's for
/// a validated model, the model name itself under the per-project unpinned
/// bypass.
fn load_repo(model: &str, allow_any: bool) -> &str {
    load_pin(model, allow_any).map_or(model, |pin| pin.repo)
}

/// The graph file a load of `model` on `device` opens: the pin's CUDA graph
/// when the device selects CUDA and the pin ships one, `onnx/model.onnx`
/// otherwise. The fingerprint and the loader both resolve through this, so a
/// device switch digests the graph that actually runs.
fn load_graph(model: &str, allow_any: bool, device: &str) -> &'static str {
    graph_pin(load_pin(model, allow_any), device).map_or(ONNX_ARTIFACT, |graph| graph.file)
}

/// Tokens an embedding input for `model` is truncated to.
pub fn max_seq_length(model: &str) -> usize {
    load_pin(model, allow_any_model_from_env()).map_or(MAX_SEQ_LENGTH, |pin| pin.max_seq_length)
}

fn graph_pin(pin: Option<&'static ModelPin>, device: &str) -> Option<&'static GraphPin> {
    pin.and_then(|pin| pin.cuda_onnx.as_ref())
        .filter(|_| wants_cuda(device))
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
pub fn resolve_model_artifacts(model: &str, device: &str) -> Result<ModelArtifacts> {
    let allow_any = allow_any_model_from_env();
    validate_model_allowed(model, allow_any)?;
    resolve_model_artifacts_at(
        load_repo(model, allow_any),
        load_revision(model, allow_any),
        load_graph(model, allow_any, device),
        sidecar_expectation(model, allow_any),
    )
}

/// The artifacts for `model` already present in the local hf-hub cache, at
/// the revision a load would open. Never touches the network: `None` when
/// the model is not allowlisted or the tokenizer or the graph is not cached
/// yet. For a caller that must not block on a download (a per-call key, a
/// status line), where a later load changing the answer is acceptable.
pub fn cached_model_artifacts(model: &str, device: &str) -> Option<ModelArtifacts> {
    let allow_any = allow_any_model_from_env();
    validate_model_allowed(model, allow_any).ok()?;
    let revision = load_revision(model, allow_any);
    let repo_id = load_repo(model, allow_any).to_string();
    let repo = match revision {
        Some(revision) => Repo::with_revision(repo_id, RepoType::Model, revision.to_string()),
        None => Repo::model(repo_id),
    };
    let cache = hf_cache_from_env()?.repo(repo);
    let ort_runtime = ort_runtime_dylib().ok()?;
    // Match resolution's artifact set or every pass re-embeds. Resolution
    // rejects undeclared sidecars; this non-failing probe omits them.
    let onnx_data = match sidecar_expectation(model, allow_any) {
        SidecarExpectation::Absent => None,
        SidecarExpectation::Required | SidecarExpectation::Unknown => cache.get(ONNX_DATA_ARTIFACT),
    };
    Some(ModelArtifacts {
        tokenizer: cache.get("tokenizer.json")?,
        onnx: cache.get(load_graph(model, allow_any, device))?,
        onnx_data,
        ort_runtime,
    })
}

/// Match hf-hub's HF_HOME precedence without Cache::default's HOME-less panic.
/// Non-UTF-8 HF_HOME falls back to HOME; no passwd lookup is attempted.
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

const ONNX_ARTIFACT: &str = "onnx/model.onnx";
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
type ArtifactKey = (Option<PathBuf>, String, Option<String>, &'static str);

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

// Memoize snapshot paths to avoid repeated hub-client construction on queries.
// Refetches preserve paths; content digests separately track size and mtime.
// Follow symlinks when checking existence so deleted blobs invalidate the memo.
// hf-hub cannot replace dangling snapshot links (EEXIST); those require removal.
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
            // Invalidate on any stat error; log the cause to distinguish
            // eviction from persistent access failures.
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

/// Memoize paths while their files exist; tokenizer/graph failures cache nothing.
/// Pinned absence is checked on disk even on memo hits. Failed required-sidecar
/// fetches remain unresolved and retry on the next call.
///
/// Unpinned fetch errors cannot prove absence: hf-hub conflates HTTP and network
/// failures. Only the revision's file list can settle absence. An unavailable
/// list or a listed-but-unfetched sidecar leaves that sidecar retryable.
/// Never hold the memo lock across network calls; racing first calls may refetch.
fn resolve_hf_artifact_paths(
    hf_home: Option<&Path>,
    model: &str,
    revision: Option<&str>,
    graph: &'static str,
    expectation: SidecarExpectation,
    mut fetch: impl FnMut(&'static str) -> Result<PathBuf>,
    sidecar_listed: impl FnOnce() -> Result<bool>,
) -> Result<ArtifactPaths> {
    let key = (
        hf_home.map(Path::to_path_buf),
        model.to_string(),
        revision.map(str::to_string),
        graph,
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
        None => (fetch("tokenizer.json")?, fetch(graph)?),
    };
    // Keep fetch diagnostics: ORT's later external-data error omits this cause.
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
/// path alone (`commit_from_file`), so a sidecar already at that path, left
/// by an earlier run or an external `huggingface-cli download`, would be
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
    repo: &str,
    revision: Option<&str>,
    graph: &'static str,
    expectation: SidecarExpectation,
) -> Result<ModelArtifacts> {
    let hf_home = std::env::var_os("HF_HOME").map(PathBuf::from);
    let (tokenizer, onnx, onnx_data) = resolve_hf_artifact_paths(
        hf_home.as_deref(),
        repo,
        revision,
        graph,
        expectation,
        |artifact| hf_get_model_file(repo, revision, artifact),
        || hf_sidecar_listed(repo, revision),
    )?;
    Ok(ModelArtifacts {
        tokenizer,
        onnx,
        onnx_data,
        ort_runtime: ort_runtime_dylib()?,
    })
}

/// A request's model policy, resolved from its canonical project rather than the
/// daemon's working directory. Resolve again for each operation so revocation
/// also applies to resident sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelAuthorization {
    root: Option<PathBuf>,
    allow_any: bool,
}

thread_local! {
    static MODEL_AUTHORIZATION: std::cell::RefCell<Option<ModelAuthorization>> = const {
        std::cell::RefCell::new(None)
    };
}

impl ModelAuthorization {
    pub fn for_project(root: &Path) -> Self {
        let root = std::fs::canonicalize(root).ok();
        let allow_any = allow_any_eligible_from_env()
            && root.as_deref().is_some_and(project_allow_any_opted_in);
        Self { root, allow_any }
    }

    /// Validate before looking up a resident session, including after revocation.
    /// Pinned allowlisted models share one resident session across projects;
    /// unpinned bypasses stay isolated per canonical project root so revoking
    /// one project never shares a session with another.
    pub fn pool_key(&self, model: &str) -> Result<String> {
        validate_model_allowed(model, self.allow_any)?;
        if self.allow_any {
            Ok(format!("{:?}|unpinned:true", self.root))
        } else {
            Ok("pinned".to_string())
        }
    }

    /// Bind all nested synchronous model/artifact/fingerprint operations to this
    /// request. Never span an async suspension or expect propagation to a spawned
    /// thread: establish a fresh scope there. Restores the outer scope on unwind.
    pub fn scope<T>(&self, operation: impl FnOnce() -> T) -> T {
        struct Restore(Option<ModelAuthorization>);
        impl Drop for Restore {
            fn drop(&mut self) {
                MODEL_AUTHORIZATION.with(|slot| {
                    slot.replace(self.0.take());
                });
            }
        }
        let _restore = Restore(MODEL_AUTHORIZATION.with(|slot| slot.replace(Some(self.clone()))));
        operation()
    }

    /// Policy of the current synchronous operation; cwd discovery is reserved
    /// for standalone callers that did not supply a requested project.
    pub fn current() -> Self {
        MODEL_AUTHORIZATION
            .with(|slot| slot.borrow().clone())
            .unwrap_or_else(|| {
                current_project_root().map_or(
                    Self {
                        root: None,
                        allow_any: false,
                    },
                    |root| Self::for_project(&root),
                )
            })
    }
}

/// Bypass requires CODESAGE_ALLOW_ANY_MODEL plus the project's canonical root
/// in the user-owned allowlist (one path per line; blanks and # comments ignored).
/// Repo-local opt-in would let a cloned repo authorize its own ONNX graph.
/// An explicit request scope wins; only standalone callers fall back to cwd.
/// See [`user_allowlist_path`] for the external configuration location.
pub fn allow_any_model_from_env() -> bool {
    ModelAuthorization::current().allow_any
}

/// Raw eligibility signal behind [`allow_any_model_from_env`]: the env var
/// alone. Process-global: never consult it directly on a load path; loads
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

fn find_project_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".codesage").is_dir())
        .map(Path::to_path_buf)
}

/// Whether `root` is opted in: its canonical path is listed in the
/// user-owned allowlist file. A missing or unreadable file, or an
/// unresolvable config home (notably HOME-less daemon environments; see
/// [`hf_cache_from_env`]), is "not listed": fail closed, never panic.
fn project_allow_any_opted_in(root: &Path) -> bool {
    let Ok(canonical) = std::fs::canonicalize(root) else {
        return false;
    };
    let Some(path) = user_allowlist_path() else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
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

/// Require both env eligibility and an explicitly allowlisted project.
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
/// head; the bypass never pins).
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
pub fn user_allowlist_display() -> String {
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
/// cache boundary: a crafted or corrupted link (`../../../../some-file`)
/// must not get an arbitrary external file deleted on the eviction path.
/// On containment failure only the symlink itself is removed.
fn evict_cached_artifact(path: &Path) -> Result<()> {
    // Purge before refetch: identical size/mtime must not reuse the old digest.
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
    repo: &str,
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
        || hf_get_model_file(repo, Some(revision), artifact),
    )
}

/// Verify every pinned artifact, tolerating one locally-corrupted cache file
/// per artifact via [`verify_with_refetch`]. Returns the verified artifacts
/// (with re-downloaded paths when the cache was evicted).
fn verify_pinned_model_artifacts(
    model: &str,
    pin: &ModelPin,
    graph: Option<&GraphPin>,
    mut artifacts: ModelArtifacts,
) -> Result<ModelArtifacts> {
    artifacts.tokenizer = verify_or_refetch_artifact(
        model,
        pin.repo,
        pin.revision,
        "tokenizer.json",
        artifacts.tokenizer,
        pin.tokenizer_sha256,
    )?;
    let (graph_file, graph_sha256) =
        graph.map_or((ONNX_ARTIFACT, pin.onnx_sha256), |g| (g.file, g.sha256));
    artifacts.onnx = verify_or_refetch_artifact(
        model,
        pin.repo,
        pin.revision,
        graph_file,
        artifacts.onnx,
        graph_sha256,
    )?;
    artifacts.onnx_data = match (pin.onnx_data_sha256, artifacts.onnx_data) {
        (Some(expected), Some(path)) => Some(verify_or_refetch_artifact(
            model,
            pin.repo,
            pin.revision,
            "onnx/model.onnx_data",
            path,
            expected,
        )?),
        (Some(_), None) => {
            anyhow::bail!(
                "pinned model {model:?} requires onnx/model.onnx_data at revision {}, but it was not downloaded",
                pin.revision
            );
        }
        (None, Some(path)) => {
            // Resolution normally rejects this in refuse_undeclared_sidecar;
            // retain the verification guard if another resolver supplies a path.
            anyhow::bail!(
                "pinned model {model:?} unexpectedly resolved onnx/model.onnx_data at {}; refusing unpinned sidecar",
                path.display()
            );
        }
        (None, None) => None,
    };
    Ok(artifacts)
}

/// Whether a model's declared input names include `token_type_ids`, i.e. the
/// inference call must supply that third tensor alongside ids + mask.
pub(crate) fn wants_token_type_ids<'a>(mut input_names: impl Iterator<Item = &'a str>) -> bool {
    input_names.any(|name| name == "token_type_ids")
}

/// Shared model-loading path for [`Embedder`] and [`Reranker`]: downloads the
/// tokenizer + ONNX model (and the optional external-weights sidecar), builds a
/// padding/truncating tokenizer, creates an ORT session (registering the CUDA
/// execution provider when `device` requests the GPU), and asserts the CUDA
/// libraries actually mapped (the silent-CPU-fallback guard). Returns the
/// session, tokenizer, and whether the model takes a `token_type_ids` input.
pub(crate) fn load_onnx_session(model: &str, device: &str) -> Result<(Session, Tokenizer, bool)> {
    let loaded = load_onnx_session_with_provider(model, device, false)?;
    Ok((loaded.session, loaded.tokenizer, loaded.has_token_type_ids))
}

/// What [`load_onnx_session_with_provider`] produced, including the
/// execution provider the session actually runs on and the artifacts it opened.
pub(crate) struct LoadedSession {
    pub(crate) session: Session,
    pub(crate) tokenizer: Tokenizer,
    pub(crate) has_token_type_ids: bool,
    /// `cpu`, `cuda`, or `coreml`: the provider that succeeded, which under
    /// `CODESAGE_ALLOW_CPU_FALLBACK=1` may differ from the configured one.
    pub(crate) execution_provider: &'static str,
    /// The verified model artifacts this session actually opened.
    pub(crate) artifacts: ModelArtifacts,
}

/// The provider a session runs on after the CUDA-library check. `configured`
/// is the provider the device string selects; `cuda_check` is the result of
/// the mapped-libraries guard (only meaningful when CUDA was requested). A
/// failed check is fatal unless the fallback is allowed, in which case the
/// session runs on the CPU and the second element says so.
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

struct CudaSessionContext<'a> {
    model: &'a str,
    device: &'a str,
    configured: &'static str,
    has_cuda_graph: bool,
    allow_cpu_fallback: bool,
}

/// Resolve the final session, execution provider, and loaded artifacts after a
/// CUDA session-commit attempt.
///
/// When CUDA libraries are absent and `allow_cpu_fallback` is true:
/// - If the initial load targeted a CUDA-specific derived graph (whether
///   `commit_result` failed on a CUDA-only op such as Jina's `BiasSplitGelu` or
///   succeeded on CPU such as MiniLM's fp16 graph), drop any initial session
///   and load the verified portable `onnx/model.onnx` graph on CPU instead.
/// - Otherwise (the initial load already targeted `onnx/model.onnx`), keep the
///   committed session on CPU or propagate its commit error.
///
/// When CUDA libraries are absent and `allow_cpu_fallback` is false, surface
/// the actionable `cuda_check` error even when `commit_result` failed first.
fn resolve_cuda_session<S, A>(
    ctx: CudaSessionContext<'_>,
    commit_result: Result<S>,
    initial_artifacts: A,
    cuda_check: impl FnOnce() -> Result<()>,
    load_portable_cpu: impl FnOnce() -> Result<(S, A)>,
) -> Result<(S, &'static str, A)> {
    let (provider, fell_back) =
        effective_execution_provider(ctx.configured, cuda_check(), ctx.allow_cpu_fallback)?;
    if fell_back {
        if ctx.has_cuda_graph {
            drop(commit_result);
            tracing::warn!(
                model = ctx.model,
                configured_device = ctx.device,
                execution_provider = provider,
                fallback_graph = ONNX_ARTIFACT,
                "CODESAGE_ALLOW_CPU_FALLBACK: CUDA was requested but its libraries are not \
                 mapped; loading the verified portable graph on CPU and fingerprinting as a CPU setup"
            );
            let (cpu_session, portable_artifacts) = load_portable_cpu()?;
            return Ok((cpu_session, provider, portable_artifacts));
        }
        let session = commit_result?;
        tracing::warn!(
            model = ctx.model,
            configured_device = ctx.device,
            execution_provider = provider,
            "CODESAGE_ALLOW_CPU_FALLBACK: CUDA was requested but its libraries are not \
             mapped; this session runs on the CPU and fingerprints as a CPU setup"
        );
        return Ok((session, provider, initial_artifacts));
    }
    let session = commit_result?;
    Ok((session, provider, initial_artifacts))
}

#[cfg(all(feature = "cuda", target_os = "linux"))]
fn cpu_fallback_allowed_from_env() -> bool {
    matches!(
        std::env::var("CODESAGE_ALLOW_CPU_FALLBACK").as_deref(),
        Ok("1") | Ok("true")
    )
}

struct SessionWiring {
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    same_as_requested_arena: bool,
    #[cfg_attr(not(any(feature = "cuda", target_vendor = "apple")), allow(dead_code))]
    register_execution_providers: bool,
    enforce_platform_gates: bool,
}

struct ArtifactResolution<'a> {
    model: &'a str,
    repo: &'a str,
    revision: Option<&'static str>,
    graph: &'static str,
    expectation: SidecarExpectation,
    pin: Option<&'static ModelPin>,
    graph_pin: Option<&'static GraphPin>,
}

type CudaLibsCheck = fn() -> Result<()>;

/// [`load_onnx_session`], also reporting the execution provider the session
/// initialised on. Pass `same_as_requested_arena = true` only when the caller
/// pairs CUDA inference with peak-first batching and idle arena shrinkage.
pub(crate) fn load_onnx_session_with_provider(
    model: &str,
    device: &str,
    same_as_requested_arena: bool,
) -> Result<LoadedSession> {
    let wiring = SessionWiring {
        same_as_requested_arena,
        register_execution_providers: true,
        enforce_platform_gates: true,
    };
    let mut resolve = |req: ArtifactResolution<'_>| -> Result<ModelArtifacts> {
        let artifacts =
            resolve_model_artifacts_at(req.repo, req.revision, req.graph, req.expectation)?;
        if let Some(pin) = req.pin {
            verify_pinned_model_artifacts(req.model, pin, req.graph_pin, artifacts)
        } else {
            Ok(artifacts)
        }
    };
    #[cfg(all(feature = "cuda", target_os = "linux"))]
    let cuda_fallback: Option<(CudaLibsCheck, bool)> =
        Some((require_cuda_libs_mapped, cpu_fallback_allowed_from_env()));
    #[cfg(not(all(feature = "cuda", target_os = "linux")))]
    let cuda_fallback: Option<(CudaLibsCheck, bool)> = None;

    load_onnx_session_with_wiring(model, device, wiring, &mut resolve, cuda_fallback)
}

fn load_onnx_session_with_wiring<FCheck>(
    model: &str,
    device: &str,
    wiring: SessionWiring,
    resolve_artifacts: &mut impl FnMut(ArtifactResolution<'_>) -> Result<ModelArtifacts>,
    cuda_fallback: Option<(FCheck, bool)>,
) -> Result<LoadedSession>
where
    FCheck: FnOnce() -> Result<()>,
{
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
        let unpinned = unpinned_load_message(model, load_revision(model, allow_any));
        tracing::warn!("{unpinned}");
    }

    #[cfg(not(target_vendor = "apple"))]
    init_ort_dylib()?;

    crate::config::validate_device(device)?;
    let want_cuda = wants_cuda(device);
    if want_cuda && wiring.enforce_platform_gates {
        #[cfg(not(feature = "cuda"))]
        {
            anyhow::bail!(
                "GPU requested but binary built without cuda feature. Rebuild with: cargo build --features cuda"
            );
        }
    }

    let want_coreml = wants_coreml(device);
    if want_coreml && wiring.enforce_platform_gates && !cfg!(target_vendor = "apple") {
        anyhow::bail!(
            "CoreML requested in .codesage/config.toml but this binary is not running on Apple hardware"
        );
    }

    let artifacts = resolve_artifacts(ArtifactResolution {
        model,
        repo: load_repo(model, allow_any),
        revision: pin.map(|pin| pin.revision),
        graph: load_graph(model, allow_any, device),
        expectation: sidecar_expectation(model, allow_any),
        pin,
        graph_pin: graph_pin(pin, device),
    })?;

    let mut tokenizer =
        Tokenizer::from_file(&artifacts.tokenizer).map_err(|e| anyhow::anyhow!("{e}"))?;
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: pin.map_or(MAX_SEQ_LENGTH, |pin| pin.max_seq_length),
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // ONNX Runtime's memory-efficient attention takes an additive bias only
    // when the sequence length is a multiple of 8 tokens. Preserve any
    // model-configured pad ID/token for unpinned tokenizers.
    let existing_pad = tokenizer.get_padding().cloned();
    tokenizer.with_padding(Some(tokenizers::PaddingParams {
        strategy: tokenizers::PaddingStrategy::BatchLongest,
        pad_to_multiple_of: Some(8),
        pad_id: existing_pad.as_ref().map_or(0, |p| p.pad_id),
        pad_type_id: existing_pad.as_ref().map_or(0, |p| p.pad_type_id),
        pad_token: existing_pad.map_or_else(|| "[PAD]".to_string(), |p| p.pad_token),
        ..Default::default()
    }));

    let mut builder = Session::builder(&ort_environment()?)?;

    if graph_pin(pin, device).is_some() {
        // ORT_ENABLE_ALL adds CPU layout passes whose LayerNorm fusion fails on
        // the fp16 graph's inserted casts; CUDA kernels gain nothing from them.
        builder = builder
            .with_optimization_level(GraphOptimizationLevel::Level2)
            .map_err(|e| anyhow::anyhow!("setting the graph optimization level: {e}"))?;
    }

    if want_cuda && wiring.register_execution_providers {
        #[cfg(feature = "cuda")]
        {
            preload_native_libs();
            let mut cuda = ort::ep::CUDA::default();
            if wiring.same_as_requested_arena {
                // Paired with peak-first batch execution in `BatchPlan`: allocating
                // the largest batch first lets SameAsRequested split one peak block
                // across all later smaller batches instead of doubling via power-of-two.
                cuda =
                    cuda.with_arena_extend_strategy(ort::ep::ArenaExtendStrategy::SameAsRequested);
            }
            builder = builder
                .with_execution_providers([cuda.build().error_on_failure()])
                .map_err(|e| anyhow::anyhow!("CUDA provider failed to register: {e}"))?;
        }
    }

    if want_coreml && wiring.register_execution_providers {
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

    // Linux CUDA fallback must load the portable graph and fingerprint as CPU.
    let configured = crate::fingerprint::configured_execution_provider(device);
    let (session, execution_provider, artifacts) = if let Some((cuda_check, allow_cpu_fallback)) =
        cuda_fallback
        && want_cuda
    {
        let commit_result = builder
            .commit_from_file(&artifacts.onnx)
            .map_err(anyhow::Error::from);
        resolve_cuda_session(
            CudaSessionContext {
                model,
                device,
                configured,
                has_cuda_graph: graph_pin(pin, device).is_some(),
                allow_cpu_fallback,
            },
            commit_result,
            artifacts,
            cuda_check,
            || {
                let portable_artifacts = resolve_artifacts(ArtifactResolution {
                    model,
                    repo: load_repo(model, allow_any),
                    revision: pin.map(|pin| pin.revision),
                    graph: ONNX_ARTIFACT,
                    expectation: sidecar_expectation(model, allow_any),
                    pin,
                    graph_pin: None,
                })?;
                let cpu_session = Session::builder(&ort_environment()?)?
                    .commit_from_file(&portable_artifacts.onnx)?;
                Ok((cpu_session, portable_artifacts))
            },
        )?
    } else {
        // CoreML registration errors are fatal, but ORT exposes no post-commit
        // provider query or mapping check. Attest the configured provider.
        (
            builder.commit_from_file(&artifacts.onnx)?,
            configured,
            artifacts,
        )
    };

    let has_token_type_ids = wants_token_type_ids(session.inputs().iter().map(|i| i.name()));

    Ok(LoadedSession {
        session,
        tokenizer,
        has_token_type_ids,
        execution_provider,
        artifacts,
    })
}

/// Minimum padded token count in a CUDA batch that marks the BFC arena dirty.
/// Inclusive (`>=`) so a single-row max-length Jina chunk (1x1024) triggers
/// idle arena shrinkage while ordinary search queries stay below the threshold.
const ARENA_SHRINK_TOKEN_THRESHOLD: usize = 1024;

/// Stop retrying idle arena shrinkage for this session after this many
/// consecutive failures so a broken shrink path does not spin every daemon tick.
const MAX_ARENA_SHRINK_FAILURES: u32 = 3;

fn should_mark_arena_dirty(execution_provider: &str, total_elems: usize) -> bool {
    execution_provider == "cuda" && total_elems >= ARENA_SHRINK_TOKEN_THRESHOLD
}

pub struct Embedder {
    session: Session,
    tokenizer: Arc<Tokenizer>,
    pad_token_id: i64,
    dim: usize,
    pooling: PoolingStrategy,
    /// The graph emits one pooled vector per input ([batch, dim]) rather
    /// than token-level hidden states, so no host-side pooling runs.
    pre_pooled: bool,
    has_token_type_ids: bool,
    batch_size: NonZeroUsize,
    execution_provider: &'static str,
    artifacts: ModelArtifacts,
    /// ORT `memory.enable_memory_arena_shrinkage` target list.
    arena_shrink_spec: &'static str,
    /// True after a large CUDA batch expanded the BFC arena beyond single-query needs.
    arena_dirty: bool,
    /// Consecutive shrink failures; reaching [`MAX_ARENA_SHRINK_FAILURES`] disables shrinking.
    arena_shrink_failures: u32,
    /// Monotonic timestamp when the most recent inference call completed.
    last_inference_completed: Option<Instant>,
}

impl Embedder {
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        // Reject an invalid override before resolving artifacts or creating a
        // native session. This is the private-query constructor as well as the
        // indexing constructor, so cache state cannot change the error.
        let batch_size = config.effective_batch_size()?;
        tracing::info!(model = %config.model, "loading embedding model");
        let LoadedSession {
            session,
            mut tokenizer,
            has_token_type_ids,
            execution_provider,
            artifacts,
        } = load_onnx_session_with_provider(&config.model, &config.device, true)?;
        // Pad directly when assembling tensor buffers so `BatchPlanner` can
        // tokenize once without per-encoding padding allocations and plan on
        // exact token counts.
        let pad_token_id = tokenizer.get_padding().map_or(0i64, |p| p.pad_id as i64);
        tokenizer.with_padding(None);
        let dim = detect_dim(&session)?;
        let pooling = config.pooling_strategy();
        let pre_pooled = output_rank(&session) == Some(2);
        // Every derived graph folds in mean pooling; CLS cannot be recovered
        // from its output.
        if pre_pooled && pooling != PoolingStrategy::Mean {
            anyhow::bail!(
                "model {:?} emits mean-pooled [batch, {dim}] vectors, but pooling {pooling:?} \
                 is configured; remove [embedding].pooling or set it to \"mean\"",
                config.model
            );
        }

        tracing::info!(
            dim,
            pooling = ?pooling,
            pre_pooled,
            token_type_ids = has_token_type_ids,
            batch_size = batch_size.get(),
            execution_provider,
            "embedding model loaded"
        );

        Ok(Self {
            session,
            tokenizer: Arc::new(tokenizer),
            pad_token_id,
            dim,
            pooling,
            pre_pooled,
            has_token_type_ids,
            batch_size,
            execution_provider,
            artifacts,
            arena_shrink_spec: "gpu:0;cpu:0",
            arena_dirty: false,
            arena_shrink_failures: 0,
            last_inference_completed: None,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The execution provider this session actually initialised on (`cpu`,
    /// `cuda`, `coreml`): the one a fingerprint over its vectors must name.
    pub fn execution_provider(&self) -> &'static str {
        self.execution_provider
    }

    /// The verified model artifacts this session actually opened and runs.
    pub fn artifacts(&self) -> &ModelArtifacts {
        &self.artifacts
    }

    /// The semantic fingerprint of the vectors this session actually produces,
    /// digesting the graph it loaded and naming its actual execution provider.
    pub fn fingerprint(
        &self,
        config: &EmbeddingConfig,
    ) -> Result<crate::fingerprint::SemanticFingerprint> {
        Ok(crate::fingerprint::SemanticFingerprint::for_artifacts(
            config,
            self.dim,
            &self.artifacts,
        )?
        .with_execution_provider(self.execution_provider))
    }

    /// When the most recent inference call on this session completed.
    pub fn last_inference_completed(&self) -> Option<Instant> {
        self.last_inference_completed
    }

    /// Whether repeated shrink failures have disabled automatic arena shrinkage.
    pub fn arena_shrink_disabled(&self) -> bool {
        self.arena_shrink_failures >= MAX_ARENA_SHRINK_FAILURES
    }

    /// Whether a large CUDA batch has expanded the device arena since the last shrink.
    pub fn needs_arena_shrink(&self) -> bool {
        self.execution_provider == "cuda" && self.arena_dirty && !self.arena_shrink_disabled()
    }

    /// Release unused CUDA and CPU BFC arena blocks back to the device and OS
    /// while keeping the resident session warm for subsequent queries.
    pub fn shrink_arena(&mut self) -> Result<()> {
        if !self.needs_arena_shrink() {
            return Ok(());
        }
        const SHRINK_SEQ_LEN: usize = 8;
        let input_ids = vec![self.pad_token_id; SHRINK_SEQ_LEN];
        let mut attention_mask = vec![0i64; SHRINK_SEQ_LEN];
        attention_mask[0] = 1;
        let ids_tensor = ort::value::Tensor::from_array(([1, SHRINK_SEQ_LEN], input_ids))?;
        let mask_tensor = ort::value::Tensor::from_array(([1, SHRINK_SEQ_LEN], attention_mask))?;
        let inputs = if self.has_token_type_ids {
            let type_tensor =
                ort::value::Tensor::from_array(([1, SHRINK_SEQ_LEN], vec![0i64; SHRINK_SEQ_LEN]))?;
            ort::inputs![
                "input_ids" => ids_tensor,
                "token_type_ids" => type_tensor,
                "attention_mask" => mask_tensor,
            ]
        } else {
            ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
            ]
        };
        match crate::run_control::run_session_shrinking(
            &mut self.session,
            inputs,
            self.arena_shrink_spec,
            |_| Ok(()),
        ) {
            Ok(()) => {
                self.arena_dirty = false;
                self.arena_shrink_failures = 0;
                Ok(())
            }
            Err(error) => {
                self.arena_shrink_failures = self.arena_shrink_failures.saturating_add(1);
                if self.arena_shrink_disabled() {
                    self.arena_dirty = false;
                }
                Err(error)
            }
        }
    }

    /// Cheap lock-free planner sharing this embedder's tokenizer and batch cap.
    pub fn planner(&self) -> BatchPlanner {
        self.planner_with_limit(self.batch_size)
    }

    /// [`Embedder::planner`] with an explicit per-call `batch_size` cap, so
    /// projects sharing one pooled session still honor their own batch limits.
    pub fn planner_with_limit(&self, batch_size: NonZeroUsize) -> BatchPlanner {
        BatchPlanner::for_dim(Arc::clone(&self.tokenizer), self.dim, batch_size)
    }

    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
        let mut stats = EmbedStats::default();
        let batch = self.embed_encoded_batch(&[&encoding], &mut stats)?;
        Ok(batch.into_iter().next().unwrap())
    }

    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_batch_with_stats(texts, &mut EmbedStats::default())
    }

    /// [`Embedder::embed_batch`], recording the work done into `stats`. The
    /// counts stay valid when the call fails or is stopped part way, so a
    /// caller can attribute a slow or abandoned run.
    pub fn embed_batch_with_stats(
        &mut self,
        texts: &[&str],
        stats: &mut EmbedStats,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let plan = self.plan_batches(texts)?;
        plan.run_encoded(stats, |batch, stats| self.embed_encoded_batch(batch, stats))
    }

    /// Tokenize and split `texts` the way [`Embedder::embed_batch`] does, for
    /// callers driving the batches through [`BatchPlan::run_encoded`] and
    /// [`Embedder::embed_encoded_batch`].
    pub fn plan_batches(&self, texts: &[&str]) -> Result<BatchPlan> {
        self.planner().plan(texts)
    }

    /// Run inference on one raw-text batch of a [`BatchPlan`], in the order given.
    pub fn embed_planned_batch(
        &mut self,
        texts: &[&str],
        stats: &mut EmbedStats,
    ) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
        let refs: Vec<&tokenizers::Encoding> = encodings.iter().collect();
        self.embed_encoded_batch(&refs, stats)
    }

    /// Run inference on one pre-tokenized batch of a [`BatchPlan`].
    pub fn embed_encoded_batch(
        &mut self,
        encodings: &[&tokenizers::Encoding],
        stats: &mut EmbedStats,
    ) -> Result<Vec<Vec<f32>>> {
        if encodings.is_empty() {
            return Ok(Vec::new());
        }
        let batch_size = encodings.len();
        let (seq_len, input_ids, attention_mask, tokens) =
            assemble_encoded_batch(encodings, self.pad_token_id)?;
        let total_elems = input_ids.len();

        let ids_tensor = ort::value::Tensor::from_array(([batch_size, seq_len], input_ids))?;
        let mask_tensor = ort::value::Tensor::from_array(([batch_size, seq_len], attention_mask))?;

        let inputs = if self.has_token_type_ids {
            let token_type_ids = vec![0i64; total_elems];
            let type_tensor =
                ort::value::Tensor::from_array(([batch_size, seq_len], token_type_ids))?;
            ort::inputs![
                "input_ids" => ids_tensor,
                "token_type_ids" => type_tensor,
                "attention_mask" => mask_tensor,
            ]
        } else {
            ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
            ]
        };

        // Mark dirty before inference so failed or cancelled large CUDA runs
        // still release any arena blocks they allocated once idle.
        if should_mark_arena_dirty(self.execution_provider, total_elems) {
            self.arena_dirty = true;
        }
        let (dim, pooling, pre_pooled) = (self.dim, self.pooling, self.pre_pooled);
        let result = crate::run_control::run_session(&mut self.session, inputs, |outputs| {
            let (_shape, hidden) = outputs[0].try_extract_tensor::<f32>()?;
            pool_hidden(hidden, encodings, seq_len, dim, pooling, pre_pooled)
        });
        self.last_inference_completed = Some(Instant::now());
        let embeddings = result?;
        stats.tokens += tokens;
        stats.padded_tokens += total_elems;
        Ok(embeddings)
    }
}

fn assemble_encoded_batch(
    encodings: &[&tokenizers::Encoding],
    pad_token_id: i64,
) -> Result<(usize, Vec<i64>, Vec<i64>, usize)> {
    let batch_size = encodings.len();
    let max_len = encodings.iter().map(|e| e.len()).max().unwrap_or(0);
    // ONNX Runtime's memory-efficient attention takes an additive bias only
    // when the sequence length is a multiple of 8 tokens.
    let seq_len = round_up_8(max_len);
    ensure!(
        seq_len > 0,
        "cannot embed a batch whose tokenized encodings are all empty"
    );

    let total_elems = batch_size
        .checked_mul(seq_len)
        .context("embedding batch tensor size overflow")?;
    let mut input_ids = Vec::with_capacity(total_elems);
    let mut attention_mask = Vec::with_capacity(total_elems);
    let mut tokens = 0usize;

    for enc in encodings {
        let ids = enc.get_ids();
        let mask = enc.get_attention_mask();
        ensure!(
            ids.len() == mask.len() && ids.len() <= seq_len,
            "tokenizer encoding lengths mismatch: ids={} mask={} seq_len={seq_len}",
            ids.len(),
            mask.len()
        );
        input_ids.extend(ids.iter().map(|&id| id as i64));
        input_ids.resize(input_ids.len() + (seq_len - ids.len()), pad_token_id);
        attention_mask.extend(mask.iter().map(|&m| m as i64));
        attention_mask.resize(attention_mask.len() + (seq_len - mask.len()), 0);
        tokens += mask.iter().filter(|&&m| m != 0).count();
    }

    Ok((seq_len, input_ids, attention_mask, tokens))
}

/// Lock-free batch planner sharing an [`Embedder`]'s tokenizer and token budget.
#[derive(Clone)]
pub struct BatchPlanner {
    tokenizer: Arc<Tokenizer>,
    batch_size: usize,
    max_batch_tokens: usize,
}

impl BatchPlanner {
    /// Construct a planner with the model-scaled token budget for `dim` and `batch_size`.
    pub fn for_dim(tokenizer: Arc<Tokenizer>, dim: usize, batch_size: NonZeroUsize) -> Self {
        let limit = batch_size.get();
        Self {
            tokenizer,
            batch_size: limit,
            max_batch_tokens: max_batch_tokens_for(dim, limit),
        }
    }

    /// Tokenize `texts` once and group them into length-tight, descending batches.
    pub fn plan(&self, texts: &[&str]) -> Result<BatchPlan> {
        if texts.is_empty() {
            return Ok(BatchPlan::empty());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
        Ok(BatchPlan::from_encodings(
            encodings,
            self.batch_size,
            self.max_batch_tokens,
        ))
    }
}

fn round_up_8(len: usize) -> usize {
    len.div_ceil(8).saturating_mul(8)
}

/// Activation memory scales with hidden dimension: 8,192 tokens at dim>=768
/// (Jina) and 16,384 tokens at dim<=384 (MiniLM) for default batch_size=64.
fn max_batch_tokens_for(dim: usize, batch_size: usize) -> usize {
    let tokens_per_slot = if dim >= 768 { 128 } else { 256 };
    batch_size.saturating_mul(tokens_per_slot)
}

/// Work one [`Embedder::embed_batch_with_stats`] call performed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EmbedStats {
    /// Internal batches the texts were split into.
    pub planned_batches: usize,
    /// Internal batches whose inference completed.
    pub completed_batches: usize,
    /// Non-padding tokens across completed batches.
    pub tokens: usize,
    /// Rows times padded sequence length across completed batches: the
    /// tokens inference actually paid for.
    pub padded_tokens: usize,
}

fn pool_hidden(
    hidden: &[f32],
    encodings: &[&tokenizers::Encoding],
    seq_len: usize,
    dim: usize,
    pooling: PoolingStrategy,
    pre_pooled: bool,
) -> Result<Vec<Vec<f32>>> {
    let batch_size = encodings.len();
    if pre_pooled {
        let expected = batch_size
            .checked_mul(dim)
            .context("pooled output size overflow")?;
        ensure!(
            hidden.len() == expected,
            "model output has {} values but a pooled [batch={batch_size}, dim={}] tensor \
             holds {expected}",
            hidden.len(),
            dim
        );
        return Ok(hidden.chunks_exact(dim).map(l2_normalized).collect());
    }

    // detect_dim accepts pre-pooled [batch, dim] outputs, but these loops
    // require token-level [batch, seq, dim] data to avoid out-of-bounds access.
    let expected = batch_size
        .checked_mul(seq_len)
        .and_then(|n| n.checked_mul(dim))
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
            dim,
            expected
        );
    }

    let mut embeddings = Vec::with_capacity(batch_size);

    for (i, enc) in encodings.iter().enumerate() {
        let pooled = match pooling {
            PoolingStrategy::Mean => {
                let mask = enc.get_attention_mask();
                let mut vec = vec![0.0f32; dim];
                let mut mask_sum = 0.0f32;
                for (j, &m) in mask.iter().enumerate() {
                    if m == 0 {
                        continue;
                    }
                    let m = m as f32;
                    mask_sum += m;
                    let offset = (i * seq_len + j) * dim;
                    for k in 0..dim {
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
                ensure!(seq_len > 0, "cannot CLS-pool an empty token sequence");
                let offset = i * seq_len * dim;
                hidden[offset..offset + dim].to_vec()
            }
        };

        embeddings.push(l2_normalized(&pooled));
    }

    Ok(embeddings)
}

/// How [`Embedder::embed_batch`] splits a set of texts into internal batches.
/// Batches are grouped in ascending length order (minimizing internal padding)
/// and executed in descending order (heaviest batch first) so CUDA's
/// `SameAsRequested` BFC arena allocates one peak block and reuses it across
/// all later smaller batches.
#[derive(Debug, Clone)]
pub struct BatchPlan {
    batches: Vec<Vec<usize>>,
    texts: usize,
    encodings: Option<Vec<tokenizers::Encoding>>,
}

fn plan_descending_batches(
    lengths: &[usize],
    batch_size: usize,
    max_item_len: usize,
    max_batch_budget: usize,
    round_up_multiple_of_8: bool,
) -> Vec<Vec<usize>> {
    let batch_size = batch_size.max(1);
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    order.sort_by_key(|&i| lengths[i]);

    // Ascending grouping minimizes padding spread within each batch.
    let mut batches: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    let mut start = 0;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() {
            let rows = end + 1 - start;
            let raw = lengths[order[end]].min(max_item_len);
            let longest = if round_up_multiple_of_8 {
                round_up_8(raw)
            } else {
                raw
            };
            if rows > batch_size || rows.saturating_mul(longest) > max_batch_budget {
                break;
            }
            end += 1;
        }
        let raw_max = lengths[order[end - 1]].min(max_item_len);
        let seq_len = if round_up_multiple_of_8 {
            round_up_8(raw_max)
        } else {
            raw_max
        };
        let footprint = (end - start).saturating_mul(seq_len);
        batches.push((footprint, seq_len, order[start..end].to_vec()));
        start = end;
    }
    // Execute the largest padded-token footprint first (tie-breaking by longer
    // sequences, whose O(S^2) attention bias is heavier) rather than merely
    // reversing, so a small partial remainder at the longest tier never runs
    // ahead of a full peak batch.
    batches.sort_by_key(|(footprint, seq_len, _)| std::cmp::Reverse((*footprint, *seq_len)));
    batches.into_iter().map(|(_, _, batch)| batch).collect()
}

impl BatchPlan {
    pub fn empty() -> Self {
        Self {
            batches: Vec::new(),
            texts: 0,
            encodings: Some(Vec::new()),
        }
    }

    pub fn new(
        texts: &[&str],
        batch_size: usize,
        max_text_bytes: usize,
        max_batch_bytes: usize,
    ) -> Self {
        let lengths: Vec<usize> = texts.iter().map(|t| t.len()).collect();
        let batches =
            plan_descending_batches(&lengths, batch_size, max_text_bytes, max_batch_bytes, false);
        Self {
            batches,
            texts: texts.len(),
            encodings: None,
        }
    }

    pub fn from_encodings(
        encodings: Vec<tokenizers::Encoding>,
        batch_size: usize,
        max_batch_tokens: usize,
    ) -> Self {
        let lengths: Vec<usize> = encodings.iter().map(tokenizers::Encoding::len).collect();
        let batches =
            plan_descending_batches(&lengths, batch_size, usize::MAX, max_batch_tokens, true);
        Self {
            batches,
            texts: encodings.len(),
            encodings: Some(encodings),
        }
    }

    pub fn len(&self) -> usize {
        self.batches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// Embed every batch through `embed`, returning vectors in input order.
    /// `texts` must be the slice the plan was made from. A stopped
    /// `WorkControl` ends the loop before the next batch.
    pub fn run(
        &self,
        texts: &[&str],
        stats: &mut EmbedStats,
        mut embed: impl FnMut(&[&str], &mut EmbedStats) -> Result<Vec<Vec<f32>>>,
    ) -> Result<Vec<Vec<f32>>> {
        ensure!(
            texts.len() == self.texts,
            "batch plan covers {} texts, {} were supplied",
            self.texts,
            texts.len()
        );
        stats.planned_batches += self.batches.len();
        let mut slots: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for batch in &self.batches {
            codesage_protocol::work::checkpoint()?;
            let batch_texts: Vec<&str> = batch.iter().map(|&i| texts[i]).collect();
            let embeddings = embed(&batch_texts, stats)?;
            stats.completed_batches += 1;
            ensure!(
                embeddings.len() == batch.len(),
                "embedder produced {} vectors for {} texts",
                embeddings.len(),
                batch.len()
            );
            for (&i, embedding) in batch.iter().zip(embeddings) {
                slots[i] = Some(embedding);
            }
        }
        Ok(slots
            .into_iter()
            .map(|v| v.expect("every text belongs to exactly one batch"))
            .collect())
    }

    /// Embed every pre-tokenized batch through `embed`, returning vectors in
    /// input order. Requires a plan built by [`BatchPlanner::plan`] or
    /// [`BatchPlan::from_encodings`].
    pub fn run_encoded(
        &self,
        stats: &mut EmbedStats,
        mut embed: impl FnMut(&[&tokenizers::Encoding], &mut EmbedStats) -> Result<Vec<Vec<f32>>>,
    ) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .encodings
            .as_deref()
            .context("batch plan carries no pre-tokenized encodings")?;
        ensure!(
            encodings.len() == self.texts,
            "batch plan covers {} texts, {} encodings are attached",
            self.texts,
            encodings.len()
        );
        stats.planned_batches += self.batches.len();
        let mut slots: Vec<Option<Vec<f32>>> = vec![None; self.texts];
        for batch in &self.batches {
            codesage_protocol::work::checkpoint()?;
            let batch_encodings: Vec<&tokenizers::Encoding> =
                batch.iter().map(|&i| &encodings[i]).collect();
            let embeddings = embed(&batch_encodings, stats)?;
            stats.completed_batches += 1;
            ensure!(
                embeddings.len() == batch.len(),
                "embedder produced {} vectors for {} texts",
                embeddings.len(),
                batch.len()
            );
            for (&i, embedding) in batch.iter().zip(embeddings) {
                slots[i] = Some(embedding);
            }
        }
        Ok(slots
            .into_iter()
            .map(|v| v.expect("every text belongs to exactly one batch"))
            .collect())
    }
}

#[cfg(test)]
fn embed_in_length_order(
    texts: &[&str],
    batch_size: usize,
    max_text_bytes: usize,
    max_batch_bytes: usize,
    stats: &mut EmbedStats,
    embed: impl FnMut(&[&str], &mut EmbedStats) -> Result<Vec<Vec<f32>>>,
) -> Result<Vec<Vec<f32>>> {
    BatchPlan::new(texts, batch_size, max_text_bytes, max_batch_bytes).run(texts, stats, embed)
}

fn l2_normalized(vector: &[f32]) -> Vec<f32> {
    let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter().map(|v| v / norm).collect()
    } else {
        vector.to_vec()
    }
}

/// Rank of the first output tensor when the graph declares it.
fn output_rank(session: &Session) -> Option<usize> {
    match session.outputs()[0].dtype() {
        ort::value::ValueType::Tensor { shape, .. } => Some(shape.len()),
        _ => None,
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
    anyhow::bail!(
        "could not infer embedding dimension from model output shape; \
         the model's first output tensor has no static last-dimension. \
         Refusing to fall back to a default — index would store \
         wrong-dimension vectors. Set the model explicitly in \
         .codesage/config.toml to one with a static output shape."
    )
}

/// Test-only ONNX and `Embedder` builders shared with downstream crate unit
/// tests via the `test-support` Cargo feature (enabled only in dev-dependencies).
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use super::*;

    pub const MINILM_DERIVED_REVISION: &str = super::MINILM_DERIVED_REVISION;
    pub const JINA_CODE_DERIVED_REVISION: &str = super::JINA_CODE_DERIVED_REVISION;
    pub const TEST_WORD_TOKENIZER_JSON: &str = r#"{"version":"1.0","model":{"type":"WordLevel","vocab":{"[UNK]":0,"a":1,"b":2,"c":3},"unk_token":"[UNK]"},"pre_tokenizer":{"type":"Whitespace"}}"#;

    pub fn test_word_tokenizer() -> Tokenizer {
        Tokenizer::from_bytes(TEST_WORD_TOKENIZER_JSON.as_bytes()).unwrap()
    }

    fn pb_varint(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n != 0 {
                out.push(b | 0x80);
            } else {
                out.push(b);
                return out;
            }
        }
    }

    fn pb_field_varint(num: u64, val: u64) -> Vec<u8> {
        let mut out = pb_varint(num << 3);
        out.extend(pb_varint(val));
        out
    }

    fn pb_field_bytes(num: u64, data: &[u8]) -> Vec<u8> {
        let mut out = pb_varint((num << 3) | 2);
        out.extend(pb_varint(data.len() as u64));
        out.extend_from_slice(data);
        out
    }

    fn onnx_value_info(name: &str, elem_type: u64, dims: &[Vec<u8>]) -> Vec<u8> {
        let mut shape = Vec::new();
        for dim in dims {
            shape.extend(pb_field_bytes(1, dim));
        }
        let mut tensor_type = pb_field_varint(1, elem_type);
        tensor_type.extend(pb_field_bytes(2, &shape));
        let type_proto = pb_field_bytes(1, &tensor_type);
        let mut out = pb_field_bytes(1, name.as_bytes());
        out.extend(pb_field_bytes(2, &type_proto));
        out
    }

    fn onnx_attr_int(name: &str, val: i64) -> Vec<u8> {
        let mut out = pb_field_bytes(1, name.as_bytes());
        out.extend(pb_field_varint(3, val as u64));
        out.extend(pb_field_varint(20, 2)); // AttributeProto::INT
        out
    }

    fn onnx_attr_ints(name: &str, vals: &[i64]) -> Vec<u8> {
        let mut out = pb_field_bytes(1, name.as_bytes());
        for &v in vals {
            out.extend(pb_field_varint(7, v as u64));
        }
        out.extend(pb_field_varint(20, 7)); // AttributeProto::INTS
        out
    }

    fn onnx_node(
        op: &str,
        domain: &str,
        inputs: &[&str],
        outputs: &[&str],
        attrs: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for i in inputs {
            out.extend(pb_field_bytes(1, i.as_bytes()));
        }
        for o in outputs {
            out.extend(pb_field_bytes(2, o.as_bytes()));
        }
        out.extend(pb_field_bytes(4, op.as_bytes()));
        if !domain.is_empty() {
            out.extend(pb_field_bytes(7, domain.as_bytes()));
        }
        for attr in attrs {
            out.extend(pb_field_bytes(5, attr));
        }
        out
    }

    /// Build a tiny valid ONNX embedding graph emitting `[batch, dim]` floats,
    /// or a CUDA-only op graph when `unsupported_op` is `Some(...)`.
    pub fn tiny_onnx_bytes(
        dim: usize,
        with_token_type_ids: bool,
        unsupported_op: Option<&str>,
    ) -> Vec<u8> {
        const FLOAT: u64 = 1;
        const INT64: u64 = 7;
        let dim_batch = pb_field_bytes(2, b"batch");
        let dim_seq = pb_field_bytes(2, b"seq");
        let dim_out = pb_field_varint(1, dim as u64);

        let mut nodes = Vec::new();
        let ids_src = if with_token_type_ids {
            nodes.push(onnx_node(
                "Add",
                "",
                &["input_ids", "token_type_ids"],
                &["ids_sum"],
                &[],
            ));
            "ids_sum"
        } else {
            "input_ids"
        };
        nodes.push(onnx_node(
            "Cast",
            "",
            &[ids_src],
            &["ids_f"],
            &[onnx_attr_int("to", FLOAT as i64)],
        ));
        nodes.push(onnx_node(
            "Cast",
            "",
            &["attention_mask"],
            &["mask_f"],
            &[onnx_attr_int("to", FLOAT as i64)],
        ));
        nodes.push(onnx_node("Mul", "", &["ids_f", "mask_f"], &["masked"], &[]));
        let pooled_in = if let Some(op) = unsupported_op {
            nodes.push(onnx_node(
                op,
                "com.microsoft",
                &["masked"],
                &["cuda_only"],
                &[],
            ));
            "cuda_only"
        } else {
            "masked"
        };
        nodes.push(onnx_node(
            "ReduceMean",
            "",
            &[pooled_in],
            &["m"],
            &[onnx_attr_ints("axes", &[1]), onnx_attr_int("keepdims", 1)],
        ));
        let concat_inputs = vec!["m"; dim];
        nodes.push(onnx_node(
            "Concat",
            "",
            &concat_inputs,
            &["embeddings"],
            &[onnx_attr_int("axis", 1)],
        ));

        let mut graph = Vec::new();
        for node in nodes {
            graph.extend(pb_field_bytes(1, &node));
        }
        let graph_name = format!("tiny_dim{dim}_tti{with_token_type_ids}");
        graph.extend(pb_field_bytes(2, graph_name.as_bytes()));
        graph.extend(pb_field_bytes(
            11,
            &onnx_value_info("input_ids", INT64, &[dim_batch.clone(), dim_seq.clone()]),
        ));
        if with_token_type_ids {
            graph.extend(pb_field_bytes(
                11,
                &onnx_value_info(
                    "token_type_ids",
                    INT64,
                    &[dim_batch.clone(), dim_seq.clone()],
                ),
            ));
        }
        graph.extend(pb_field_bytes(
            11,
            &onnx_value_info("attention_mask", INT64, &[dim_batch.clone(), dim_seq]),
        ));
        graph.extend(pb_field_bytes(
            12,
            &onnx_value_info("embeddings", FLOAT, &[dim_batch, dim_out]),
        ));

        let mut opset = pb_field_bytes(1, b"");
        opset.extend(pb_field_varint(2, 14));
        let mut model = pb_field_varint(1, 7);
        model.extend(pb_field_bytes(7, &graph));
        model.extend(pb_field_bytes(8, &opset));
        if unsupported_op.is_some() {
            let mut ms_opset = pb_field_bytes(1, b"com.microsoft");
            ms_opset.extend(pb_field_varint(2, 1));
            model.extend(pb_field_bytes(8, &ms_opset));
        }
        model
    }

    pub fn tiny_embedder(
        artifacts: ModelArtifacts,
        execution_provider: &'static str,
        arena_shrink_spec: &'static str,
        batch_size: usize,
    ) -> Embedder {
        let session = Session::builder(&ort_environment().unwrap())
            .unwrap()
            .commit_from_file(&artifacts.onnx)
            .unwrap();
        let dim = detect_dim(&session).unwrap();
        let pre_pooled = output_rank(&session) == Some(2);
        let has_token_type_ids = wants_token_type_ids(session.inputs().iter().map(|i| i.name()));
        Embedder {
            session,
            tokenizer: Arc::new(test_word_tokenizer()),
            pad_token_id: 0,
            dim,
            pooling: PoolingStrategy::Mean,
            pre_pooled,
            has_token_type_ids,
            batch_size: NonZeroUsize::new(batch_size).unwrap(),
            execution_provider,
            artifacts,
            arena_shrink_spec,
            arena_dirty: false,
            arena_shrink_failures: 0,
            last_inference_completed: None,
        }
    }

    pub fn set_last_inference_completed(embedder: &mut Embedder, at: Option<Instant>) {
        embedder.last_inference_completed = at;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn embedder_rejects_oversized_batch_before_model_access() {
        use std::num::NonZeroUsize;

        use crate::config::{EmbeddingConfig, MAX_BATCH_SIZE};

        let mut config = EmbeddingConfig {
            model: "not-a-real-model".into(),
            device: "cpu".into(),
            ..EmbeddingConfig::default()
        };
        config.batch_size_override = NonZeroUsize::new(MAX_BATCH_SIZE + 1);
        let error = match super::Embedder::new(&config) {
            Ok(_) => panic!("oversized batch unexpectedly constructed an embedder"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("exceeds max supported batch size")
        );
    }

    #[test]
    fn hf_cancelled_caller_retains_nested_worker_lease() {
        use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};

        let control = WorkControl::new(None);
        let lease: Arc<dyn Send + Sync> = Arc::new(());
        let weak_lease = Arc::downgrade(&lease);
        control.set_work_lease(weak_lease.clone());
        let caller_control = control.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let caller = std::thread::spawn(move || {
            let _lease = lease;
            let _scope = caller_control.enter();
            super::hf_with_deadline("controlled test worker", move || {
                assert!(codesage_protocol::work::current().is_some());
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        control.cancel(StopReason::ClientCancelled);
        let error = caller.join().unwrap().unwrap_err();
        assert_eq!(
            error.downcast_ref::<WorkStopped>().unwrap().reason,
            StopReason::ClientCancelled
        );
        assert!(weak_lease.upgrade().is_some());
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while weak_lease.upgrade().is_some() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(weak_lease.upgrade().is_none());
    }

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
    fn prefix_requiring_nomic_model_is_not_allowlisted_but_bypassable() {
        let nomic = "nomic-ai/nomic-embed-text-v1.5";
        assert!(!ALLOWED_MODELS.contains(&nomic));
        assert!(model_pin(nomic).is_none());
        let err = validate_model_allowed(nomic, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("CODESAGE_ALLOW_ANY_MODEL=1"), "{err}");
        assert!(validate_model_allowed(nomic, true).is_ok());
        assert!(validate_model_allowed("jinaai/jina-embeddings-v2-base-code", false).is_ok());
        assert!(validate_model_allowed("cross-encoder/ms-marco-MiniLM-L6-v2", false).is_ok());
    }

    #[test]
    fn derived_artifacts_load_from_their_own_repository_only_when_pinned() {
        let jina = "jinaai/jina-embeddings-v2-base-code";
        assert_eq!(
            load_repo(jina, false),
            "IA0x00/jina-embeddings-v2-base-code-codesage"
        );
        assert_eq!(
            load_repo(jina, true),
            jina,
            "the unpinned bypass resolves the name the config gives"
        );
        let minilm = "sentence-transformers/all-MiniLM-L6-v2";
        assert_eq!(load_repo(minilm, false), "IA0x00/all-MiniLM-L6-v2-codesage");
        assert_eq!(load_repo(minilm, true), minilm);
    }

    #[test]
    fn cuda_devices_load_the_pinned_cuda_graph_and_others_the_portable_one() {
        for model in [
            "jinaai/jina-embeddings-v2-base-code",
            "sentence-transformers/all-MiniLM-L6-v2",
            "cross-encoder/ms-marco-MiniLM-L6-v2",
        ] {
            for device in ["gpu", "cuda", "GPU"] {
                assert_eq!(
                    load_graph(model, false, device),
                    "onnx/model_cuda_fp16.onnx",
                    "{model} on {device}"
                );
            }
            for device in ["cpu", "coreml"] {
                assert_eq!(
                    load_graph(model, false, device),
                    ONNX_ARTIFACT,
                    "{model} on {device}"
                );
            }
            assert_eq!(
                load_graph(model, true, "gpu"),
                ONNX_ARTIFACT,
                "the unpinned bypass knows no derived graphs ({model})"
            );
        }
    }

    #[test]
    fn only_alibi_jina_raises_the_sequence_cap() {
        assert_eq!(max_seq_length("jinaai/jina-embeddings-v2-base-code"), 1024);
        for model in ALLOWED_MODELS
            .iter()
            .filter(|m| **m != "jinaai/jina-embeddings-v2-base-code")
        {
            assert_eq!(max_seq_length(model), MAX_SEQ_LENGTH, "{model}");
        }
        assert_eq!(max_seq_length("test/unpinned"), MAX_SEQ_LENGTH);
    }

    #[test]
    fn artifact_paths_are_memoized_per_graph() {
        let root = tempfile::tempdir().unwrap();
        let resolve = |graph: &'static str| {
            resolve_hf_artifact_paths(
                Some(root.path()),
                "test/two-graph-model",
                Some("rev"),
                graph,
                SidecarExpectation::Absent,
                |artifact| Ok(fake_fetch(root.path(), artifact)),
                no_pinned_probe,
            )
            .unwrap()
        };
        let portable = resolve(ONNX_ARTIFACT);
        let cuda = resolve("onnx/model_cuda_fp16.onnx");
        assert_eq!(portable.1, root.path().join(ONNX_ARTIFACT));
        assert_eq!(cuda.1, root.path().join("onnx/model_cuda_fp16.onnx"));
        assert_eq!(resolve(ONNX_ARTIFACT).1, portable.1);
    }

    #[test]
    fn length_ordered_batches_return_vectors_in_input_order() {
        let texts = ["ccc", "a", "eeeee", "bb", "dddd"];
        let mut batches = Vec::new();
        let mut stats = EmbedStats::default();
        let out =
            embed_in_length_order(&texts, 2, usize::MAX, usize::MAX, &mut stats, |batch, _| {
                batches.push(batch.iter().map(|t| t.to_string()).collect::<Vec<_>>());
                Ok(batch.iter().map(|t| vec![t.len() as f32]).collect())
            })
            .unwrap();
        assert_eq!(
            batches,
            [vec!["ccc", "dddd"], vec!["eeeee"], vec!["a", "bb"]]
        );
        assert_eq!(out, [vec![3.0], vec![1.0], vec![5.0], vec![2.0], vec![4.0]]);
        assert_eq!((stats.planned_batches, stats.completed_batches), (3, 3));
    }

    #[test]
    fn length_ordered_batches_shrink_to_the_byte_budget() {
        let long = "x".repeat(40);
        let texts = ["aaaa", "bbbb", "cccc", "dddd", long.as_str(), long.as_str()];
        let mut sizes = Vec::new();
        embed_in_length_order(&texts, 4, 30, 60, &mut EmbedStats::default(), |batch, _| {
            sizes.push(batch.len());
            Ok(batch.iter().map(|_| vec![0.0]).collect())
        })
        .unwrap();
        // Long texts run first (descending order) capped at 2 by the 60-byte
        // budget; short texts run next filling a batch of 4 by count.
        assert_eq!(sizes, [2, 4]);

        let mut sizes = Vec::new();
        embed_in_length_order(
            &[long.as_str()],
            4,
            100,
            10,
            &mut EmbedStats::default(),
            |batch, _| {
                sizes.push(batch.len());
                Ok(vec![vec![0.0]])
            },
        )
        .unwrap();
        assert_eq!(sizes, [1], "a text over the budget still embeds alone");
    }

    #[test]
    fn token_batch_planner_rounds_to_eight_and_executes_heaviest_first() {
        let lengths = [12, 500, 14, 510, 10, 490];
        let batches = plan_descending_batches(&lengths, 4, usize::MAX, 1024, true);
        // Ascending groups: [4, 0, 2] (3 * 16 = 48), [5, 1] (2 * 504 = 1008),
        // [3] (1 * 512 = 512). Sorted by descending padded-token footprint so
        // the full peak batch runs ahead of a small partial tail remainder.
        assert_eq!(batches, vec![vec![5, 1], vec![3], vec![4, 0, 2]]);
        assert_eq!(max_batch_tokens_for(768, 64), 8_192);
        assert_eq!(max_batch_tokens_for(384, 64), 16_384);
        assert_eq!(max_batch_tokens_for(768, 10), 1_280);
        assert_eq!(max_batch_tokens_for(384, 10), 2_560);

        // 17 long chunks with budget 8192 pack into [16 x 504 = 8064] and [1 x 504 = 504];
        // the full 16-chunk peak batch must execute before the 1-chunk remainder.
        let long_tier = vec![500; 17];
        let long_batches = plan_descending_batches(&long_tier, 64, usize::MAX, 8_192, true);
        assert_eq!(long_batches.len(), 2);
        assert_eq!(long_batches[0].len(), 16);
        assert_eq!(long_batches[1].len(), 1);
    }

    #[test]
    fn batch_plan_from_encodings_preserves_order_and_enforces_contracts() {
        let tok = test_word_tokenizer();
        let texts = ["a b c", "a", "a b c a b c a b c", "a b"];
        let encodings = tok.encode_batch(texts.to_vec(), false).unwrap();
        let plan = BatchPlan::from_encodings(encodings, 2, 16);

        let mut seen_lens = Vec::new();
        let mut stats = EmbedStats::default();
        let out = plan
            .run_encoded(&mut stats, |batch, _| {
                seen_lens.push(batch.iter().map(|e| e.len()).collect::<Vec<_>>());
                Ok(batch.iter().map(|e| vec![e.len() as f32]).collect())
            })
            .unwrap();
        assert_eq!(out, vec![vec![3.0], vec![1.0], vec![9.0], vec![2.0]]);
        assert_eq!(stats.planned_batches, seen_lens.len());
        assert_eq!(stats.completed_batches, seen_lens.len());
        // The 9-token encoding (padded to 16) runs before or alongside smaller groups.
        assert_eq!(seen_lens[0], vec![9]);

        // Mismatched vector count is rejected.
        let err = plan
            .run_encoded(&mut EmbedStats::default(), |_, _| Ok(vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("produced 0 vectors"));

        // Cancelled WorkControl stops before running any batch.
        let control = codesage_protocol::work::WorkControl::new(None);
        control.cancel(codesage_protocol::work::StopReason::ClientCancelled);
        let _scope = control.enter();
        let stopped = plan
            .run_encoded(&mut EmbedStats::default(), |_, _| {
                panic!("cancelled plan must not invoke embedder")
            })
            .unwrap_err();
        drop(_scope);
        assert!(
            stopped
                .downcast_ref::<codesage_protocol::work::WorkStopped>()
                .is_some()
        );

        // A byte-only BatchPlan::new has no encodings and errors on run_encoded.
        let unencoded = BatchPlan::new(&["x"], 2, 16, 32);
        let err = unencoded
            .run_encoded(&mut EmbedStats::default(), |_, _| Ok(vec![vec![1.0]]))
            .unwrap_err();
        assert!(err.to_string().contains("no pre-tokenized encodings"));
    }

    #[test]
    fn assemble_encoded_batch_pads_to_multiple_of_8_and_guards_empty() {
        let tok = test_word_tokenizer();
        let e1 = tok.encode("a b c", false).unwrap();
        let e2 = tok.encode("a", false).unwrap();
        let (seq_len, ids, mask, tokens) = assemble_encoded_batch(&[&e1, &e2], 99).unwrap();
        assert_eq!(seq_len, 8);
        assert_eq!(tokens, 4);
        assert_eq!(
            ids,
            vec![1, 2, 3, 99, 99, 99, 99, 99, 1, 99, 99, 99, 99, 99, 99, 99]
        );
        assert_eq!(mask, vec![1, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);

        // Empty encodings (special-tokenless custom tokenizer) must fail before ORT.
        let empty = tok.encode("", false).unwrap();
        assert_eq!(empty.len(), 0);
        let err = assemble_encoded_batch(&[&empty], 0).unwrap_err();
        assert!(err.to_string().contains("all empty"));
    }

    #[test]
    fn pool_hidden_handles_unpadded_mean_cls_and_pre_pooled() {
        let tok = test_word_tokenizer();
        let e1 = tok.encode("a b", false).unwrap();
        let e2 = tok.encode("a", false).unwrap();
        let seq_len = 8;
        let dim = 2;
        // Row 0: token0=(2,0), token1=(0,2), padding=(999,999) -> mean=(1,1) -> norm=(1/sqrt2, 1/sqrt2)
        // Row 1: token0=(0,4), padding=(999,999) -> mean=(0,4) -> norm=(0,1)
        let mut hidden = vec![999.0f32; 2 * seq_len * dim];
        hidden[0] = 2.0;
        hidden[1] = 0.0;
        hidden[2] = 0.0;
        hidden[3] = 2.0;
        let row1 = seq_len * dim;
        hidden[row1] = 0.0;
        hidden[row1 + 1] = 4.0;

        let mean = pool_hidden(
            &hidden,
            &[&e1, &e2],
            seq_len,
            dim,
            PoolingStrategy::Mean,
            false,
        )
        .unwrap();
        assert!((mean[0][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!((mean[0][1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!((mean[1][0]).abs() < 1e-5);
        assert!((mean[1][1] - 1.0).abs() < 1e-5);

        let cls = pool_hidden(
            &hidden,
            &[&e1, &e2],
            seq_len,
            dim,
            PoolingStrategy::Cls,
            false,
        )
        .unwrap();
        assert!((cls[0][0] - 1.0).abs() < 1e-5);
        assert!((cls[1][1] - 1.0).abs() < 1e-5);

        let pre = pool_hidden(
            &[3.0, 4.0, 0.0, 5.0],
            &[&e1, &e2],
            seq_len,
            dim,
            PoolingStrategy::Mean,
            true,
        )
        .unwrap();
        assert!((pre[0][0] - 0.6).abs() < 1e-5);
        assert!((pre[0][1] - 0.8).abs() < 1e-5);
        assert!((pre[1][1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn pinned_pool_key_shares_across_projects_while_unpinned_stays_per_project() {
        let pinned_a = ModelAuthorization {
            root: Some(PathBuf::from("/proj-a")),
            allow_any: false,
        };
        let pinned_b = ModelAuthorization {
            root: Some(PathBuf::from("/proj-b")),
            allow_any: false,
        };
        let unpinned_a = ModelAuthorization {
            root: Some(PathBuf::from("/proj-a")),
            allow_any: true,
        };
        let unpinned_b = ModelAuthorization {
            root: Some(PathBuf::from("/proj-b")),
            allow_any: true,
        };
        let model = "jinaai/jina-embeddings-v2-base-code";
        assert_eq!(
            pinned_a.pool_key(model).unwrap(),
            pinned_b.pool_key(model).unwrap(),
            "pinned allowlisted models must share one resident session across projects"
        );
        assert_ne!(
            unpinned_a.pool_key(model).unwrap(),
            pinned_a.pool_key(model).unwrap(),
            "unpinned bypass must never share a pinned session"
        );
        assert_ne!(
            unpinned_a.pool_key(model).unwrap(),
            unpinned_b.pool_key(model).unwrap(),
            "unpinned bypasses stay isolated per project root"
        );
    }

    #[test]
    fn length_ordered_batches_reject_a_short_batch() {
        let err = embed_in_length_order(
            &["a", "b"],
            2,
            usize::MAX,
            usize::MAX,
            &mut EmbedStats::default(),
            |_, _| Ok(vec![vec![0.0]]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("1 vectors for 2 texts"), "{err}");
    }

    #[test]
    fn length_ordered_batches_stop_when_the_work_control_is_cancelled() {
        use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};

        let control = WorkControl::new(None);
        let _scope = control.enter();
        let mut stats = EmbedStats::default();
        let mut calls = 0;
        let err = embed_in_length_order(
            &["a", "bb", "ccc", "dddd"],
            2,
            usize::MAX,
            usize::MAX,
            &mut stats,
            |batch, _| {
                calls += 1;
                control.cancel(StopReason::ClientCancelled);
                Ok(batch.iter().map(|_| vec![0.0]).collect())
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1, "the batch after the cancellation must not run");
        assert_eq!(
            err.downcast_ref::<WorkStopped>().unwrap().reason,
            StopReason::ClientCancelled
        );
        assert_eq!((stats.planned_batches, stats.completed_batches), (2, 1));
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
            if let Some(graph) = &pin.cuda_onnx {
                assert_eq!(
                    graph.sha256.len(),
                    64,
                    "{model} CUDA graph hash should be sha256 hex"
                );
                assert_ne!(graph.file, ONNX_ARTIFACT);
            }
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
        let err = resolve_model_artifacts("evil/backdoored-model", "cpu")
            .expect_err("unallowlisted model must not be resolved");
        assert!(
            err.to_string()
                .contains("not on CodeSage's validated-model allowlist"),
            "{err:#}"
        );
        assert_eq!(cached_model_artifacts("evil/backdoored-model", "cpu"), None);
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
                ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
            ONNX_ARTIFACT,
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
            ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
            ONNX_ARTIFACT,
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
            repo: "test/pinned-large-model",
            revision: "rev",
            tokenizer_sha256: "",
            onnx_sha256: "",
            onnx_data_sha256: Some(
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
            cuda_onnx: None,
            max_seq_length: MAX_SEQ_LENGTH,
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
                ONNX_ARTIFACT,
                SidecarExpectation::Absent,
                |artifact| {
                    fetches.set(fetches.get() + 1);
                    Ok(fake_fetch(&root.path().join(model), artifact))
                },
                no_pinned_probe,
            )
        };

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
            ONNX_ARTIFACT,
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
                ONNX_ARTIFACT,
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
        // location: another key, and the old spelling keys nothing.
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

        let (provider, fell_back) =
            effective_execution_provider(configured, Ok(()), false).unwrap();
        assert_eq!((provider, fell_back), ("cuda", false));
        assert_eq!(
            as_configured.with_execution_provider(provider),
            as_configured
        );

        let err =
            effective_execution_provider(configured, Err(anyhow::anyhow!("no libcuda")), false)
                .unwrap_err();
        assert!(err.to_string().contains("no libcuda"));

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

    #[cfg(feature = "cuda")]
    #[test]
    fn cuda_preload_covers_the_cu12_provider_dependencies() {
        // NEEDED entries of onnxruntime-gpu 1.24's libonnxruntime_providers_cuda.so.
        for soname in [
            "libcublasLt.so.12",
            "libcublas.so.12",
            "libcurand.so.10",
            "libcufft.so.11",
            "libcudart.so.12",
        ] {
            assert!(CUDA_DYLIBS.contains(&soname), "{soname} is not preloaded");
        }
        assert!(ort::ep::cuda::CUDNN_DYLIBS.contains(&"libcudnn.so.9"));
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

    /// Serializes the env-seam tests below for `HF_HOME` and `HOME`.
    /// Every test below restores what it touches via [`SavedEnv`].
    pub(crate) fn env_lock() -> std::sync::LockResult<std::sync::MutexGuard<'static, ()>> {
        static LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));
        LOCK.lock()
    }

    /// Saves one env var and restores it on drop, so a serial env-seam test
    /// cannot leak into another test if it panics mid-mutation.
    pub(crate) struct SavedEnv {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl SavedEnv {
        pub(crate) fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        pub(crate) fn remove(key: &'static str) -> Self {
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

    use super::test_support::{
        TEST_WORD_TOKENIZER_JSON, test_word_tokenizer, tiny_embedder, tiny_onnx_bytes,
    };

    fn build_test_embedder(
        dir: &Path,
        execution_provider: &'static str,
        arena_shrink_spec: &'static str,
        with_token_type_ids: bool,
        batch_size: usize,
    ) -> Embedder {
        let tokenizer_path = dir.join("tokenizer.json");
        let onnx_path = dir.join(format!("model_tti_{with_token_type_ids}.onnx"));
        fs::write(&tokenizer_path, TEST_WORD_TOKENIZER_JSON).unwrap();
        fs::write(&onnx_path, tiny_onnx_bytes(4, with_token_type_ids, None)).unwrap();
        let emb = tiny_embedder(
            ModelArtifacts {
                tokenizer: tokenizer_path,
                onnx: onnx_path,
                onnx_data: None,
                ort_runtime: None,
            },
            execution_provider,
            arena_shrink_spec,
            batch_size,
        );
        assert_eq!(emb.has_token_type_ids, with_token_type_ids);
        emb
    }

    #[test]
    fn load_onnx_session_wiring_reloads_portable_graph_for_pinned_cuda_models() {
        let dir = tempfile::tempdir().unwrap();
        let tokenizer_path = dir.path().join("tokenizer.json");
        let portable_onnx = dir.path().join("portable_model.onnx");
        let minilm_fp16_onnx = dir.path().join("minilm_cuda_fp16.onnx");
        let jina_cuda_only_onnx = dir.path().join("jina_cuda_fp16.onnx");
        fs::write(&tokenizer_path, TEST_WORD_TOKENIZER_JSON).unwrap();
        fs::write(&portable_onnx, tiny_onnx_bytes(4, false, None)).unwrap();
        fs::write(&minilm_fp16_onnx, tiny_onnx_bytes(2, false, None)).unwrap();
        fs::write(
            &jina_cuda_only_onnx,
            tiny_onnx_bytes(2, false, Some("BiasSplitGelu")),
        )
        .unwrap();

        let cases = [
            ("sentence-transformers/all-MiniLM-L6-v2", &minilm_fp16_onnx),
            ("jinaai/jina-embeddings-v2-base-code", &jina_cuda_only_onnx),
        ];

        for (model, cuda_onnx) in cases {
            let mut requested_graphs = Vec::new();
            let mut resolve = |req: ArtifactResolution<'_>| -> Result<ModelArtifacts> {
                requested_graphs.push(req.graph);
                let onnx = if req.graph == ONNX_ARTIFACT {
                    portable_onnx.clone()
                } else {
                    cuda_onnx.clone()
                };
                Ok(ModelArtifacts {
                    tokenizer: tokenizer_path.clone(),
                    onnx,
                    onnx_data: None,
                    ort_runtime: None,
                })
            };
            let loaded = load_onnx_session_with_wiring(
                model,
                "gpu",
                SessionWiring {
                    same_as_requested_arena: true,
                    register_execution_providers: false,
                    enforce_platform_gates: false,
                },
                &mut resolve,
                Some((
                    || anyhow::bail!("CUDA runtime libraries are not loaded"),
                    true,
                )),
            )
            .unwrap();
            assert_eq!(
                loaded.execution_provider, "cpu",
                "{model}: fallback must report cpu provider"
            );
            assert_eq!(
                requested_graphs,
                vec!["onnx/model_cuda_fp16.onnx", ONNX_ARTIFACT],
                "{model}: wiring must reload portable onnx/model.onnx after CUDA graph fallback"
            );
            assert_eq!(
                loaded.artifacts.onnx, portable_onnx,
                "{model}: LoadedSession must retain portable artifacts"
            );
            assert_eq!(
                detect_dim(&loaded.session).unwrap(),
                4,
                "{model}: fallback session must run the portable graph (dim=4)"
            );
        }
    }

    #[test]
    fn embedder_shrink_arena_marks_dirty_at_boundary_and_clears_on_shrink() {
        assert!(!should_mark_arena_dirty("cuda", 1016));
        assert!(!should_mark_arena_dirty("cuda", 1023));
        assert!(should_mark_arena_dirty("cuda", 1024));
        assert!(should_mark_arena_dirty("cuda", 1025));
        assert!(!should_mark_arena_dirty("cpu", 1024));
        assert!(!should_mark_arena_dirty("coreml", 2048));

        let dir = tempfile::tempdir().unwrap();
        let tok = test_word_tokenizer();
        let below = tok.encode(vec!["a"; 1016].join(" "), false).unwrap();
        let exact = tok.encode(vec!["a"; 1024].join(" "), false).unwrap();
        assert_eq!(below.len(), 1016);
        assert_eq!(exact.len(), 1024);

        for with_token_type_ids in [false, true] {
            let mut emb = build_test_embedder(dir.path(), "cuda", "cpu:0", with_token_type_ids, 8);
            assert!(!emb.needs_arena_shrink());
            assert!(emb.last_inference_completed().is_none());

            let mut stats = EmbedStats::default();
            let out = emb.embed_encoded_batch(&[&below], &mut stats).unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].len(), 4);
            assert!(
                !emb.needs_arena_shrink(),
                "1016 tokens is below the 1024-token boundary (tti={with_token_type_ids})"
            );
            assert!(emb.last_inference_completed().is_some());

            let out = emb.embed_encoded_batch(&[&exact], &mut stats).unwrap();
            assert_eq!(out.len(), 1);
            assert!(
                emb.needs_arena_shrink(),
                "a single-row 1024-token batch must mark the arena dirty (tti={with_token_type_ids})"
            );

            emb.shrink_arena().unwrap();
            assert!(
                !emb.needs_arena_shrink(),
                "successful shrink_arena must clear the dirty flag (tti={with_token_type_ids})"
            );
            assert!(!emb.arena_shrink_disabled());

            // A later large batch can mark dirty and shrink again.
            emb.embed_encoded_batch(&[&exact], &mut stats).unwrap();
            assert!(emb.needs_arena_shrink());
            emb.shrink_arena().unwrap();
            assert!(!emb.needs_arena_shrink());
        }

        // On CPU, even a 1024-token batch never marks the arena dirty and
        // shrink_arena returns Ok without touching ORT (gpu:0 would fail on CPU).
        let mut cpu_emb = build_test_embedder(dir.path(), "cpu", "gpu:0;cpu:0", false, 8);
        cpu_emb
            .embed_encoded_batch(&[&exact], &mut EmbedStats::default())
            .unwrap();
        assert!(!cpu_emb.needs_arena_shrink());
        cpu_emb.shrink_arena().unwrap();
    }

    #[test]
    fn embedder_shrink_arena_gives_up_after_repeated_failures_and_resets_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let tok = test_word_tokenizer();
        let exact = tok.encode(vec!["a"; 1024].join(" "), false).unwrap();

        // Real CPU ORT rejects "gpu:0;cpu:0" with INVALID_ARGUMENT.
        let mut emb = build_test_embedder(dir.path(), "cuda", "gpu:0;cpu:0", false, 8);
        emb.embed_encoded_batch(&[&exact], &mut EmbedStats::default())
            .unwrap();
        assert!(emb.needs_arena_shrink());

        for attempt in 1..MAX_ARENA_SHRINK_FAILURES {
            assert!(
                emb.shrink_arena().is_err(),
                "attempt {attempt} should surface the ORT shrink error"
            );
            assert!(
                emb.needs_arena_shrink(),
                "dirty flag stays set while retries remain"
            );
            assert!(!emb.arena_shrink_disabled());
        }
        assert!(
            emb.shrink_arena().is_err(),
            "final failure should still return Err so the caller logs one warning"
        );
        assert!(
            emb.arena_shrink_disabled(),
            "reaching MAX_ARENA_SHRINK_FAILURES must disable automatic shrinking"
        );
        assert!(
            !emb.needs_arena_shrink(),
            "disabled session must not request further shrinks"
        );
        // Subsequent shrink_arena calls are no-ops even after another large batch.
        emb.shrink_arena().unwrap();
        emb.embed_encoded_batch(&[&exact], &mut EmbedStats::default())
            .unwrap();
        assert!(!emb.needs_arena_shrink());
        emb.shrink_arena().unwrap();

        // A transient failure followed by a success resets the failure counter.
        let mut recovering = build_test_embedder(dir.path(), "cuda", "gpu:0;cpu:0", false, 8);
        recovering
            .embed_encoded_batch(&[&exact], &mut EmbedStats::default())
            .unwrap();
        assert!(recovering.shrink_arena().is_err());
        assert_eq!(recovering.arena_shrink_failures, 1);
        recovering.arena_shrink_spec = "cpu:0";
        recovering.shrink_arena().unwrap();
        assert_eq!(recovering.arena_shrink_failures, 0);
        assert!(!recovering.needs_arena_shrink());
        assert!(!recovering.arena_shrink_disabled());
    }

    #[test]
    fn embedder_planner_with_limit_caps_batches_below_session_default() {
        let dir = tempfile::tempdir().unwrap();
        let emb = build_test_embedder(dir.path(), "cpu", "cpu:0", false, 8);
        let texts = ["a", "b b", "c c c", "a b", "b c a b"];

        let default_plan = emb.planner().plan(&texts).unwrap();
        assert_eq!(default_plan.len(), 1, "default cap 8 fits all 5 texts");

        let capped_plan = emb
            .planner_with_limit(NonZeroUsize::new(2).unwrap())
            .plan(&texts)
            .unwrap();
        assert_eq!(capped_plan.len(), 3, "cap 2 splits 5 texts into [2, 2, 1]");
        let mut batch_sizes = Vec::new();
        let out = capped_plan
            .run_encoded(&mut EmbedStats::default(), |batch, _| {
                batch_sizes.push(batch.len());
                Ok(batch.iter().map(|e| vec![e.len() as f32]).collect())
            })
            .unwrap();
        assert_eq!(batch_sizes, vec![2, 2, 1]);
        assert_eq!(
            out,
            vec![vec![1.0], vec![2.0], vec![3.0], vec![2.0], vec![4.0]]
        );
    }

    #[test]
    fn cuda_fallback_loads_portable_graph_on_commit_failure_and_minilm_cpu_commit() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let tokenizer = dir.path().join("tokenizer.json");
        let cuda_only_onnx = dir.path().join("jina_cuda_fp16.onnx");
        let minilm_fp16_onnx = dir.path().join("minilm_cuda_fp16.onnx");
        let portable_onnx = dir.path().join("portable_model.onnx");
        fs::write(&tokenizer, b"{}").unwrap();
        fs::write(
            &cuda_only_onnx,
            tiny_onnx_bytes(2, false, Some("BiasSplitGelu")),
        )
        .unwrap();
        fs::write(&minilm_fp16_onnx, tiny_onnx_bytes(2, false, None)).unwrap();
        fs::write(&portable_onnx, tiny_onnx_bytes(4, false, None)).unwrap();

        let make_artifacts = |onnx: &Path| ModelArtifacts {
            tokenizer: tokenizer.clone(),
            onnx: onnx.to_path_buf(),
            onnx_data: None,
            ort_runtime: None,
        };
        let env = ort_environment().unwrap();

        // 1. Jina-style CUDA-only op fails commit_from_file on CPU: fallback loads portable graph.
        let jina_commit = Session::builder(&env)
            .unwrap()
            .commit_from_file(&cuda_only_onnx)
            .map_err(anyhow::Error::from);
        assert!(
            jina_commit.is_err(),
            "CUDA-only BiasSplitGelu graph must fail CPU commit_from_file"
        );
        let portable_loads = Cell::new(0u32);
        let (session, provider, loaded) = resolve_cuda_session(
            CudaSessionContext {
                model: "jinaai/jina-embeddings-v2-base-code",
                device: "gpu",
                configured: "cuda",
                has_cuda_graph: true,
                allow_cpu_fallback: true,
            },
            jina_commit,
            make_artifacts(&cuda_only_onnx),
            || anyhow::bail!("CUDA runtime libraries are not loaded"),
            || {
                portable_loads.set(portable_loads.get() + 1);
                let s = Session::builder(&env)?.commit_from_file(&portable_onnx)?;
                Ok((s, make_artifacts(&portable_onnx)))
            },
        )
        .unwrap();
        assert_eq!(provider, "cpu");
        assert_eq!(portable_loads.get(), 1);
        assert_eq!(loaded.onnx, portable_onnx);
        assert_eq!(
            detect_dim(&session).unwrap(),
            4,
            "fallback session must run the portable graph (dim=4)"
        );

        // 2. MiniLM-style fp16 graph commits on CPU: fallback must drop it and load portable graph.
        struct DropWatchedSession {
            session: Session,
            dropped: Arc<AtomicBool>,
        }
        impl Drop for DropWatchedSession {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }
        let fp16_dropped = Arc::new(AtomicBool::new(false));
        let minilm_commit = Session::builder(&env)
            .unwrap()
            .commit_from_file(&minilm_fp16_onnx)
            .map(|session| DropWatchedSession {
                session,
                dropped: Arc::clone(&fp16_dropped),
            })
            .map_err(anyhow::Error::from);
        assert!(
            minilm_commit.is_ok(),
            "MiniLM fp16 fixture commits on CPU just like upstream MiniLM"
        );
        let portable_loads = Cell::new(0u32);
        let dropped_before_reload = Cell::new(false);
        let (session, provider, loaded) = resolve_cuda_session(
            CudaSessionContext {
                model: "sentence-transformers/all-MiniLM-L6-v2",
                device: "gpu",
                configured: "cuda",
                has_cuda_graph: true,
                allow_cpu_fallback: true,
            },
            minilm_commit,
            make_artifacts(&minilm_fp16_onnx),
            || anyhow::bail!("CUDA runtime libraries are not loaded"),
            || {
                dropped_before_reload.set(fp16_dropped.load(Ordering::SeqCst));
                portable_loads.set(portable_loads.get() + 1);
                let s = Session::builder(&env)?.commit_from_file(&portable_onnx)?;
                Ok((
                    DropWatchedSession {
                        session: s,
                        dropped: Arc::new(AtomicBool::new(false)),
                    },
                    make_artifacts(&portable_onnx),
                ))
            },
        )
        .unwrap();
        assert_eq!(provider, "cpu");
        assert_eq!(portable_loads.get(), 1);
        assert!(
            dropped_before_reload.get(),
            "CPU-committed fp16 session must be dropped before loading the portable graph"
        );
        assert_eq!(loaded.onnx, portable_onnx);
        assert_eq!(
            detect_dim(&session.session).unwrap(),
            4,
            "MiniLM CPU fallback must run the portable graph (dim=4), not the fp16 graph (dim=2)"
        );

        // 3. Disallowed fallback surfaces the actionable CUDA error in both commit Ok and Err.
        for commit in [
            Err(anyhow::anyhow!("BiasSplitGelu unsupported on CPU")),
            Ok(()),
        ] {
            let err = resolve_cuda_session(
                CudaSessionContext {
                    model: "jinaai/jina-embeddings-v2-base-code",
                    device: "gpu",
                    configured: "cuda",
                    has_cuda_graph: true,
                    allow_cpu_fallback: false,
                },
                commit,
                "cuda-graph",
                || anyhow::bail!("CUDA runtime libraries are not loaded"),
                || panic!("portable loader must not run when fallback is disabled"),
            )
            .unwrap_err();
            assert!(
                err.to_string()
                    .contains("CUDA runtime libraries are not loaded"),
                "expected actionable CUDA check error, got: {err:#}"
            );
        }

        // 4. When CUDA libs are mapped, genuine commit errors propagate even with fallback allowed.
        let err = resolve_cuda_session(
            CudaSessionContext {
                model: "jinaai/jina-embeddings-v2-base-code",
                device: "gpu",
                configured: "cuda",
                has_cuda_graph: true,
                allow_cpu_fallback: true,
            },
            Err::<(), _>(anyhow::anyhow!("corrupted protobuf")),
            "cuda-graph",
            || Ok(()),
            || panic!("portable loader must not run when CUDA libraries are present"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("corrupted protobuf"));

        // 5. Without a separate CUDA graph, fallback keeps the committed portable session.
        let (sess, provider, art) = resolve_cuda_session(
            CudaSessionContext {
                model: "custom/unpinned",
                device: "gpu",
                configured: "cuda",
                has_cuda_graph: false,
                allow_cpu_fallback: true,
            },
            Ok("already-portable"),
            "onnx/model.onnx",
            || anyhow::bail!("no cuda"),
            || panic!("single-graph model must not reload"),
        )
        .unwrap();
        assert_eq!(
            (sess, provider, art),
            ("already-portable", "cpu", "onnx/model.onnx")
        );
    }

    #[test]
    fn cpu_fallback_embedder_fingerprint_digests_portable_graph_not_cuda_graph() {
        let dir = tempfile::tempdir().unwrap();
        let tokenizer = dir.path().join("tokenizer.json");
        let cuda_onnx = dir.path().join("model_cuda_fp16.onnx");
        let portable_onnx = dir.path().join("model.onnx");
        fs::write(&tokenizer, b"{}").unwrap();
        fs::write(&cuda_onnx, tiny_onnx_bytes(2, false, None)).unwrap();
        fs::write(&portable_onnx, tiny_onnx_bytes(4, false, None)).unwrap();

        let cuda_artifacts = ModelArtifacts {
            tokenizer: tokenizer.clone(),
            onnx: cuda_onnx,
            onnx_data: None,
            ort_runtime: None,
        };
        let portable_artifacts = ModelArtifacts {
            tokenizer,
            onnx: portable_onnx.clone(),
            onnx_data: None,
            ort_runtime: None,
        };
        let session = Session::builder(&ort_environment().unwrap())
            .unwrap()
            .commit_from_file(&portable_onnx)
            .unwrap();
        let embedder = Embedder {
            session,
            tokenizer: Arc::new(test_word_tokenizer()),
            pad_token_id: 0,
            dim: 4,
            pooling: PoolingStrategy::Mean,
            pre_pooled: true,
            has_token_type_ids: false,
            batch_size: NonZeroUsize::new(8).unwrap(),
            execution_provider: "cpu",
            artifacts: portable_artifacts.clone(),
            arena_shrink_spec: "gpu:0;cpu:0",
            arena_dirty: false,
            arena_shrink_failures: 0,
            last_inference_completed: None,
        };

        let gpu_config = EmbeddingConfig {
            model: "jinaai/jina-embeddings-v2-base-code".into(),
            device: "gpu".into(),
            ..EmbeddingConfig::default()
        };
        let cpu_config = EmbeddingConfig {
            device: "cpu".into(),
            ..gpu_config.clone()
        };

        let fallback_fp = embedder.fingerprint(&gpu_config).unwrap();
        let native_cpu_fp = crate::fingerprint::SemanticFingerprint::for_artifacts(
            &cpu_config,
            4,
            &portable_artifacts,
        )
        .unwrap();
        let stale_cuda_graph_rebound =
            crate::fingerprint::SemanticFingerprint::for_artifacts(&gpu_config, 4, &cuda_artifacts)
                .unwrap()
                .with_execution_provider("cpu");

        assert_eq!(
            fallback_fp, native_cpu_fp,
            "CPU fallback session fingerprint must match native CPU over onnx/model.onnx"
        );
        assert_eq!(
            fallback_fp.artifact_digest(),
            native_cpu_fp.artifact_digest()
        );
        assert_eq!(fallback_fp.execution_provider(), "cpu");
        assert!(
            !fallback_fp.as_str().contains("cudagemm"),
            "CPU fallback must not emit CUDA GEMM options: {fallback_fp}"
        );
        assert_ne!(
            fallback_fp, stale_cuda_graph_rebound,
            "fallback fingerprint must not digest the CUDA fp16 graph"
        );
        assert_ne!(
            fallback_fp.artifact_digest(),
            stale_cuda_graph_rebound.artifact_digest()
        );
    }
}
