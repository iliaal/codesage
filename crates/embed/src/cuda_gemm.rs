//! FP16 GEMM accumulation mode for ONNX Runtime's CUDA provider.
//!
//! The CUDA graphs are FP16, and ONNX Runtime accumulates their GEMMs in FP32
//! unless bit 0 of `ORT_CUDA_GEMM_OPTIONS` is set. GeForce RTX 20-40 cards run
//! FP16 tensor-core math at half rate when it accumulates in FP32, and full
//! rate when it accumulates in FP16. The provider reads the variable once
//! through `getenv` and has no session option for it, so the process
//! environment is the only control.

const VAR: &str = "ORT_CUDA_GEMM_OPTIONS";
const DEFAULT: &str = "1";

/// Set `ORT_CUDA_GEMM_OPTIONS` to the FP16-accumulate default unless the
/// environment already names a value, including an explicit `0`.
///
/// # Safety
///
/// No other thread may exist: `set_var` races every concurrent `getenv`.
pub unsafe fn apply_default() {
    if std::env::var_os(VAR).is_none() {
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::set_var(VAR, DEFAULT) };
    }
}

/// The GEMM option bits the CUDA provider will read: `None` for the provider
/// default (unset, empty, or `0`), otherwise the parsed value, or `"unparsed"`
/// when it is not a number.
pub fn effective() -> Option<String> {
    let raw = std::env::var(VAR).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<u32>() {
        Ok(0) => None,
        Ok(bits) => Some(bits.to_string()),
        Err(_) => Some("unparsed".to_string()),
    }
}
