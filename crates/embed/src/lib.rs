pub mod chunk;
pub mod config;
pub mod model;
pub mod reranker;

/// Expose the NVIDIA library-directory discovery result to the doctor command without
/// leaking the internal cache type.
pub fn nvidia_lib_dirs() -> Vec<std::path::PathBuf> {
    model::public_nvidia_lib_dirs()
}
