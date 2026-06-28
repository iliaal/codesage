# Patch: expose RRF_K and BM25_WEIGHT as env overrides

Behavior-preserving. Mirrors the existing `dir_saturation_*` idiom in the same
file (`OnceLock` + `get_or_init`, default = the current const). With no env var
set, fusion is byte-identical to today. Lets `ablation.py` sweep the two
headline fusion knobs without a recompile per arm (one rebuild total).

File: `crates/graph/src/query.rs`

## Edit 1 — rename the RRF_K const to a `_DEFAULT` (line 77)

The file already names env-overridable constants `*_DEFAULT`; match it.

```rust
// before
/// RRF constant. Standard value from the original paper; larger values
/// damp the influence of absolute rank position, smaller values amplify it.
const RRF_K: f64 = 60.0;

// after
/// RRF constant. Standard value from the original paper; larger values
/// damp the influence of absolute rank position, smaller values amplify it.
/// Override at runtime with `CODESAGE_RRF_K` (positive float).
const RRF_K_DEFAULT: f64 = 60.0;
```

## Edit 2 — rename the BM25_WEIGHT const to a `_DEFAULT` (line 262)

```rust
// before
const BM25_WEIGHT: f64 = 2.0;

// after
/// Override at runtime with `CODESAGE_BM25_WEIGHT` (positive float).
const BM25_WEIGHT_DEFAULT: f64 = 2.0;
```

## Edit 3 — add the cached env getters (insert just above `fn rrf_merge`, ~line 264)

```rust
// Env overrides for fusion tuning without rebuilds. Cached on first read,
// matching the dir_saturation_* pattern below. Both accept a positive float;
// non-positive or unparseable values fall back to the default.
static RRF_K_OVERRIDE: OnceLock<f64> = OnceLock::new();
static BM25_WEIGHT_OVERRIDE: OnceLock<f64> = OnceLock::new();

fn rrf_k() -> f64 {
    *RRF_K_OVERRIDE.get_or_init(|| {
        std::env::var("CODESAGE_RRF_K")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&v| v > 0.0)
            .unwrap_or(RRF_K_DEFAULT)
    })
}

fn bm25_weight() -> f64 {
    *BM25_WEIGHT_OVERRIDE.get_or_init(|| {
        std::env::var("CODESAGE_BM25_WEIGHT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&v| v > 0.0)
            .unwrap_or(BM25_WEIGHT_DEFAULT)
    })
}
```

## Edit 4 — use the getters in `rrf_merge` (lines 279 and 287)

```rust
// before
        let contrib = 1.0 / (RRF_K + rank as f64 + 1.0);
// after
        let contrib = 1.0 / (rrf_k() + rank as f64 + 1.0);
```

```rust
// before
        let contrib = BM25_WEIGHT / (RRF_K + rank as f64 + 1.0);
// after
        let contrib = bm25_weight() / (rrf_k() + rank as f64 + 1.0);
```

## Verify

```bash
cd ~/ai/codesage
grep -n "RRF_K\b\|BM25_WEIGHT\b" crates/graph/src/query.rs   # only _DEFAULT + getters remain
cargo build --release -p codesage
bash scripts/sanity-check.sh --fast                          # fmt + clippy -D warnings

# behavior-preserving check: default == shipped
CODESAGE_RRF_K= codesage search --json --limit 5 'parse url' >/tmp/a.json
codesage search --json --limit 5 'parse url' >/tmp/b.json
diff /tmp/a.json /tmp/b.json && echo "identical with no override — preserving"

# knob has effect:
CODESAGE_BM25_WEIGHT=1.0 codesage search --json --limit 5 'parse url' >/tmp/c.json
diff /tmp/b.json /tmp/c.json || echo "differs under override — wired"
```

If `RRF_K_DEFAULT` / `BM25_WEIGHT_DEFAULT` are referenced anywhere else in the
crate after edits 1–2, the build will tell you; both are currently used only in
`rrf_merge` (def at 77/262, uses at 279/287), so edits 3–4 cover every site.

> This is a benchmark/eval-harness affordance with no change to default output,
> so per the repo's changelog rule (`CLAUDE.md` → "No changelog entry for …
> benchmark/eval harnesses") it does **not** need a CHANGELOG entry. The new
> env vars are tuning-only and off by default; if you'd rather document them,
> add them next to `CODESAGE_DIR_SATURATION_*` wherever those are listed.
