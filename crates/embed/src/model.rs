use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "cuda", not(target_vendor = "apple")))]
use std::sync::Once;
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use hf_hub::{Repo, RepoType};
use ort::session::Session;
use sha2::{Digest, Sha256};
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

fn hf_get_model_file(
    model: &str,
    revision: Option<&str>,
    artifact: &'static str,
) -> Result<PathBuf> {
    let timeout = hf_download_timeout();
    let model = model.to_string();
    let revision = revision.map(str::to_string);
    let (tx, rx) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let result = (|| -> Result<PathBuf> {
            let api =
                hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
            let repo = if let Some(revision) = revision {
                api.repo(Repo::with_revision(model, RepoType::Model, revision))
            } else {
                api.model(model)
            };
            repo.get(artifact)
                .with_context(|| format!("failed to download {artifact}"))
        })();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!(
                "timed out after {}s downloading {artifact} from HuggingFace; \
                 set {HF_DOWNLOAD_TIMEOUT_ENV} to adjust the limit",
                timeout.as_secs()
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("HuggingFace download worker exited before fetching {artifact}")
        }
    }
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

/// Runtime ONNX Runtime dylib discovery (`ORT_DYLIB_PATH`, pip site-packages).
/// No-op on Apple targets: macOS builds statically link ORT with the CoreML EP.
#[cfg(not(target_vendor = "apple"))]
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

/// Embedding / reranker models this project has validated end-to-end. The
/// model name comes from the indexed repo's own `.codesage/config.toml`, so
/// it is attacker-controlled when indexing a cloned repo; passing it straight
/// to hf-hub would let that repo pick an arbitrary ONNX graph to download and
/// load into the native ONNX Runtime. Gate every load on this allowlist plus
/// pinned artifact hashes unless the user (not the repo) opts out via
/// `CODESAGE_ALLOW_ANY_MODEL=1`.
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

/// The pinned revision and ONNX file digest a validated model loads at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedModelIdentity {
    pub revision: &'static str,
    pub onnx_sha256: &'static str,
}

/// Identity of the files `model` is pinned to, `None` for a model outside the
/// pin table (loadable only under `CODESAGE_ALLOW_ANY_MODEL`). Static lookup;
/// never touches the network or the model cache.
pub fn pinned_model_identity(model: &str) -> Option<PinnedModelIdentity> {
    model_pin(model).map(|pin| PinnedModelIdentity {
        revision: pin.revision,
        onnx_sha256: pin.onnx_sha256,
    })
}

pub fn allow_any_model_from_env() -> bool {
    matches!(
        std::env::var("CODESAGE_ALLOW_ANY_MODEL").as_deref(),
        Ok("1") | Ok("true")
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
         another model you trust, set CODESAGE_ALLOW_ANY_MODEL=1."
    )
}

fn verify_model_artifact_sha256(
    model: &str,
    artifact: &str,
    path: &Path,
    expected: &str,
) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {artifact} for pinned model {model:?}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {artifact} for pinned model {model:?}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize());
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
    if let Ok(target) = std::fs::read_link(path) {
        let blob = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(Path::new("")).join(target)
        };
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
    let allow_any = allow_any_model_from_env();
    validate_model_allowed(model, allow_any)?;
    let pin = if allow_any { None } else { model_pin(model) };
    if !allow_any && pin.is_none() {
        anyhow::bail!(
            "validated model {model:?} has no pinned revision/hash metadata; refusing unpinned load"
        );
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

    let revision = pin.map(|pin| pin.revision);
    let tokenizer_path = hf_get_model_file(model, revision, "tokenizer.json")?;
    let model_path = hf_get_model_file(model, revision, "onnx/model.onnx")?;
    // External-weights sidecar (>2GB models like Jina v2 base, BGE-large).
    // Most models don't have this file — a 404 is the expected outcome and
    // not worth surfacing. A real failure (network, disk, permission) is
    // worth a debug-level breadcrumb so users running with RUST_LOG=debug
    // don't have to guess when commit_from_file later errors with an
    // opaque ORT external-data load failure.
    let onnx_data_path = match hf_get_model_file(model, revision, "onnx/model.onnx_data") {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::debug!(error = %e, "onnx/model.onnx_data not fetched (normal for small models)");
            None
        }
    };
    let (tokenizer_path, model_path) = if let Some(pin) = pin {
        verify_pinned_model_artifacts(model, pin, tokenizer_path, model_path, onnx_data_path)?
    } else {
        (tokenizer_path, model_path)
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

    let has_token_type_ids = wants_token_type_ids(session.inputs().iter().map(|i| i.name()));

    Ok((session, tokenizer, has_token_type_ids))
}

pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    dim: usize,
    pooling: PoolingStrategy,
    has_token_type_ids: bool,
    batch_size: NonZeroUsize,
}

impl Embedder {
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        tracing::info!(model = %config.model, "loading embedding model");
        let (session, tokenizer, has_token_type_ids) =
            load_onnx_session(&config.model, &config.device)?;
        let dim = detect_dim(&session)?;
        let pooling = config.pooling_strategy();
        let batch_size = config.effective_batch_size()?;

        tracing::info!(
            dim,
            pooling = ?pooling,
            token_type_ids = has_token_type_ids,
            batch_size = batch_size.get(),
            "embedding model loaded"
        );

        Ok(Self {
            session,
            tokenizer,
            dim,
            pooling,
            has_token_type_ids,
            batch_size,
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
