use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FileInfo, Language};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::detect::{detect_language_with_dialect, is_unambiguous_cpp_extension};

/// Skip files larger than this at discovery time. `HARD_EXCLUDE_PATTERNS`
/// catches the common offenders (lockfiles, minified JS, build outputs), but
/// a stray large generated SQL dump or vendored data file in the project
/// root would otherwise be `fs::read` into memory and OOM the indexer. 10MB
/// is well above any real source file (php-src's biggest .c hovers ~1.5MB)
/// while bounding worst-case allocation per file.
pub const MAX_INDEXABLE_FILE_BYTES: u64 = 10 * 1024 * 1024;

pub fn build_exclude_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

pub fn discover_files_with_excludes(
    root: &Path,
    exclude_patterns: &[String],
) -> Result<Vec<FileInfo>> {
    let mut files = Vec::new();
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(build_exclude_set(exclude_patterns)?)
    };

    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();

    // First pass: collect every relevant file with a tentative language. We
    // tentatively treat `.h` as C; a second pass flips `.h` to C++ when the
    // project also contains an unambiguous C++ extension. This single-walk
    // structure keeps FS pressure flat while still letting header routing
    // be project-aware.
    let mut saw_cpp = false;
    let mut h_file_indices = Vec::new();

    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();
        let Some(language) = detect_language_with_dialect(path, false) else {
            continue;
        };

        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        if let Some(ref exc) = excludes
            && exc.is_match(&rel_path)
        {
            continue;
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if is_unambiguous_cpp_extension(ext) {
            saw_cpp = true;
        }
        if ext == "h" {
            h_file_indices.push(files.len());
        }

        // Cap file size before `read` to bound worst-case allocation. A 400MB
        // SQL dump or vendored data file slipping past `HARD_EXCLUDE_PATTERNS`
        // would otherwise fill the indexer's heap.
        match entry.metadata() {
            Ok(meta) if meta.len() > MAX_INDEXABLE_FILE_BYTES => {
                tracing::warn!(
                    path = %rel_path,
                    bytes = meta.len(),
                    cap = MAX_INDEXABLE_FILE_BYTES,
                    "skipping oversized file"
                );
                continue;
            }
            _ => {}
        }

        let content = std::fs::read(path)?;
        let hash = hex::encode(Sha256::digest(&content));

        files.push(FileInfo {
            path: rel_path,
            language,
            content_hash: hash,
        });
    }

    if saw_cpp {
        for idx in h_file_indices {
            files[idx].language = Language::Cpp;
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Test and benchmark files. These ARE indexed structurally and semantically
/// (so `find_references` can see test callsites and `find_symbol` can find test
/// fixtures), but the search ranker demotes them via path-based penalties and
/// the git-history layer drops them from co-change pair generation (where they'd
/// pair with everything they cover and skew coupling rankings).
pub const TEST_LIKE_EXCLUDE_PATTERNS: &[&str] = &[
    "**/tests/**",
    "**/test/**",
    "**/__tests__/**",
    "**/Tests/**",
    "**/*Test.php",
    "**/*_test.php",
    "**/*.test.ts",
    "**/*.test.tsx",
    "**/*.test.js",
    "**/*.test.jsx",
    "**/*.spec.ts",
    "**/*.spec.tsx",
    "**/*.spec.js",
    "**/*.spec.jsx",
    "**/test_*.py",
    "**/*_test.py",
    "**/*_test.rs",
    "**/*_test.go",
    "**/*.phpt",
    "**/benches/**",
    "**/benchmarks/**",
];

/// Files that never enter any index — third-party code, build outputs, generated
/// artifacts, lockfiles, docs/changelogs, IDE state. Dropped at file discovery
/// time. Most live under project gitignore already; these defaults catch the
/// cases where they don't (vendored repos, sandboxed checkouts) and add
/// language/IDE/cache patterns that gitignore alone misses.
pub const HARD_EXCLUDE_PATTERNS: &[&str] = &[
    // ----- third-party / vendored code -----
    "**/vendor/**",
    "**/node_modules/**",
    "**/bower_components/**",
    "**/jspm_packages/**",
    "**/.bundle/**",
    // ----- compiled / bundled outputs -----
    "**/dist/**",
    "**/build/**",
    "**/out/**",
    "**/target/**", // Rust, Java/Maven
    "**/_build/**", // Erlang, Elixir, OCaml
    "**/.next/**",
    "**/.nuxt/**",
    "**/.svelte-kit/**",
    "**/.turbo/**",
    "**/.vercel/**",
    "**/.parcel-cache/**",
    "**/.angular/**",
    "**/.gradle/**",
    "**/cmake-build-*/**",
    "**/public/js/**",
    "**/public/build/**",
    "**/storage/framework/views/**",
    // ----- language caches / compiled artifacts -----
    "**/__pycache__/**",
    "**/*.pyc",
    "**/*.pyo",
    "**/*.class",
    "**/*.o",
    "**/*.obj",
    "**/*.a",
    "**/*.lib",
    "**/*.so",
    "**/*.dylib",
    "**/*.dll",
    "**/*.exe",
    "**/*.egg-info/**",
    // ----- coverage / test output -----
    "**/coverage/**",
    "**/.coverage",
    "**/htmlcov/**",
    "**/.nyc_output/**",
    "**/*.lcov",
    // ----- tool caches -----
    "**/.cache/**",
    "**/.pytest_cache/**",
    "**/.ruff_cache/**",
    "**/.mypy_cache/**",
    "**/.tox/**",
    "**/.eslintcache",
    // ----- minified / bundled JS/CSS -----
    "**/*.min.js",
    "**/*.min.css",
    "**/*.min.mjs",
    "**/*.bundle.js",
    "**/*.chunk.js",
    "**/*.generated.*",
    "**/*.gen.ts",
    "**/*.gen.tsx",
    "**/*.gen.js",
    "**/*.gen.go",
    "**/*embeddings*.json",
    // ----- lock files (huge, low signal) -----
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/bun.lockb",
    "**/composer.lock",
    "**/Cargo.lock",
    "**/poetry.lock",
    "**/Pipfile.lock",
    "**/uv.lock",
    "**/Gemfile.lock",
    "**/go.sum",
    "**/mix.lock",
    // ----- docs and changelogs (low signal; in git history, NEWS/UPGRADING co-change
    //       with everything because the team touches them on every commit) -----
    "**/docs/**",
    "**/doc/**",
    "**/site/**",  // mkdocs default output
    "**/_site/**", // jekyll
    "**/NEWS",
    "**/NEWS.md",
    "**/UPGRADING",
    "**/UPGRADING.md",
    "**/UPGRADING.INTERNALS",
    "**/CHANGELOG",
    "**/CHANGELOG.md",
    "**/CHANGELOG.txt",
    "**/CHANGES",
    "**/CHANGES.md",
    "**/HISTORY",
    "**/HISTORY.md",
    "**/RELEASE_NOTES",
    "**/RELEASE_NOTES.md",
    // ----- IDE / editor (most are dotfiles already filtered, but be explicit) -----
    "**/.idea/**",
    "**/.vscode/**",
    "**/.vs/**",
    "**/*.swp",
    "**/*.swo",
    "**/.DS_Store",
];

/// Excludes applied at file discovery time. User config in
/// `[index].exclude_patterns` extends this list. Equals `HARD_EXCLUDE_PATTERNS`;
/// `TEST_LIKE_EXCLUDE_PATTERNS` are intentionally NOT excluded here so that the
/// structural graph stays correct on test code (callsites, fixtures) and the
/// search ranker can demote them at rank time instead.
pub const DEFAULT_EXCLUDE_PATTERNS: &[&str] = HARD_EXCLUDE_PATTERNS;
