//! Sibling-test heuristics per language convention + `recommend_tests` entry
//! point. `risk::assess_risk` also consumes `test_sibling_exists` through the
//! `pub(super)` door.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_parser::detect::detect_language;
use codesage_protocol::{
    CoupledTestEntry, FileCategory, ImpactRequest, ImpactTarget, ReachableTestEntry,
    TestRecommendations,
};
use codesage_storage::Database;

use crate::impact::{WalkBudget, impact_analysis_walk_budgeted};

/// Per-level frontier for the reachability walk. `impact::MAX_FRONTIER` (512)
/// exists to bound an unbudgeted walk; here the step budget and the deadline
/// do that, and 512 truncates the depth-2 level of any widely-used class
/// (laravel's `Model` overflows it with a 100M-step budget and no deadline),
/// which would report every such file as capped regardless of budget.
const REACH_FRONTIER: usize = 8_192;

/// Directory segments whose contents are inputs to tests, not test entry
/// points. Matched case-insensitively as a whole path segment, the way
/// `FileCategory::classify` matches `tests/`.
const FIXTURE_SEGMENTS: [&str; 6] = [
    "fixtures",
    "fixture",
    "stubs",
    "testdata",
    "__fixtures__",
    "__snapshots__",
];

/// Test-shaped paths under a fixture directory are never something to run.
fn is_fixture(path: &str) -> bool {
    let lower = path.to_lowercase();
    FIXTURE_SEGMENTS
        .iter()
        .any(|seg| lower.contains(&format!("/{seg}/")) || lower.starts_with(&format!("{seg}/")))
}

/// The index-relative form of a user-supplied path: `.` components and
/// repeated separators are dropped (`.//x`, `././x` → `x`) and an absolute
/// path under `root` is made relative to it (a trailing slash on `root` is
/// irrelevant). The root itself, or `./`, normalizes to the empty string,
/// which the caller drops. Any other shape is returned as given; in
/// particular an absolute path with an interior `..` is not resolved (that
/// would need the filesystem), so it degrades to `unindexed_files`.
fn normalize_input_path(root: Option<&Path>, path: &str) -> String {
    use std::path::Component;
    let candidate = Path::new(path.trim());
    let relative_to_root = root.and_then(|root| {
        if candidate.is_absolute() {
            candidate.strip_prefix(root).ok()
        } else {
            None
        }
    });
    let source = relative_to_root.unwrap_or(candidate);
    let parts: Vec<String> = source
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // Absolute paths keep their leading separator: `RootDir` renders as "/"
    // and must not be joined with another one.
    if source.is_absolute() {
        let mut out = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 && part != "/" && !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(part);
        }
        return out;
    }
    parts.join("/")
}

/// All sibling test files for `file_path` that exist in the index, by language
/// convention, plus the tests that exist but were withheld from the list (a
/// `.phpt` directory above [`PHPT_LIST_CAP`]). Used by `recommend_tests` and by
/// `test_sibling_exists`, which must count withheld tests as existing — "too
/// many tests to list" and "no tests" are opposite claims.
fn test_sibling_paths(db: &Database, file_path: &str) -> Result<(Vec<String>, Vec<String>)> {
    let mut withheld = Vec::new();
    // First-dot stem: `foo.test.ts` stems to `foo`, so dotted basenames
    // still key the convention candidates. (Last-dot split left `foo.test`,
    // which no candidate pattern ever matched.)
    let stem = file_path
        .rsplit('/')
        .next()
        .map(|name| name.split('.').next().unwrap_or(name).to_string())
        .unwrap_or_default();
    if stem.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let dir = file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    let candidates: Vec<String> = vec![
        // PHP: FooTest.php in same dir or app/tests
        format!("{dir}/{stem}Test.php"),
        format!("tests/Unit/{stem}Test.php"),
        format!("tests/Feature/{stem}Test.php"),
        // Python: test_foo.py / foo_test.py in same dir or tests/
        format!("{dir}/test_{stem}.py"),
        format!("{dir}/{stem}_test.py"),
        format!("tests/test_{stem}.py"),
        // Go: foo_test.go
        format!("{dir}/{stem}_test.go"),
        // JS/TS: foo.test.ts(x), foo.spec.ts(x)
        format!("{dir}/{stem}.test.ts"),
        format!("{dir}/{stem}.test.tsx"),
        format!("{dir}/{stem}.test.js"),
        format!("{dir}/{stem}.spec.ts"),
        format!("{dir}/{stem}.spec.tsx"),
        format!("{dir}/{stem}.spec.js"),
        // Java: FooTest.java sibling (Maven mirror-tree handled below).
        format!("{dir}/{stem}Test.java"),
        // Rust: foo.rs uses inline #[cfg(test)] mod tests so often no separate
        // file. Skip the explicit rust check; absence here just means the rust
        // file relies on inline tests.
    ];

    let mut found = Vec::new();
    for c in &candidates {
        let normalized = c.trim_start_matches('/').to_string();
        // First-dot stemming means a test file passed as input names itself
        // (`foo.test.ts` stems to `foo`, regenerating `foo.test.ts`); the
        // edited file is never its own test.
        if normalized == file_path {
            continue;
        }
        if db.file_id_for_path(&normalized)?.is_some() {
            found.push(normalized);
        }
    }

    // Rust: integration tests live in `<crate_root>/tests/*.rs`, not as siblings
    // and not name-keyed to the source file. List every `.rs` file under the
    // crate's `tests/` directory; the agent can filter further if it has more
    // context. Skips fixture files since those aren't test entry points.
    if file_path.ends_with(".rs")
        && let Some(idx) = file_path.rfind("/src/")
    {
        let crate_root = &file_path[..idx];
        let tests_prefix = format!("{crate_root}/tests/");
        for path in db.indexed_files_with_prefix(&tests_prefix)? {
            if path.ends_with(".rs") && !is_fixture(&path) && !found.contains(&path) {
                found.push(path);
            }
        }
    }
    // Workspace-root case: src/foo.rs paired with tests/*.rs at the same level.
    // Guard against a nested `.../src/.../src/...` path that the crate-local
    // block above already resolved, so we don't list the root `tests/` for a
    // sub-crate source file too.
    if file_path.ends_with(".rs") && file_path.starts_with("src/") && !file_path.contains("/src/") {
        for path in db.indexed_files_with_prefix("tests/")? {
            if path.ends_with(".rs") && !is_fixture(&path) && !found.contains(&path) {
                found.push(path);
            }
        }
    }

    // PHP internals (.c/.h source): .phpt tests live in `<dir>/tests/*.phpt`.
    // The naming convention is loose (bug12345.phpt, gh21709.phpt, feature
    // descriptions) so we list the directory like Rust integration tests
    // rather than try to name-match. The agent or coupled-test signal can
    // narrow further. Skip if the tests dir would dump >PHPT_LIST_CAP files
    // (typical for ext/standard/tests) — too noisy as a "primary"
    // recommendation — but report the withheld files so callers don't read
    // the omission as "this file has no tests".
    if (file_path.ends_with(".c") || file_path.ends_with(".h"))
        && let Some((dir, _)) = file_path.rsplit_once('/')
    {
        let tests_prefix = format!("{dir}/tests/");
        let candidates: Vec<String> = db
            .indexed_files_with_prefix(&tests_prefix)?
            .into_iter()
            .filter(|p| p.ends_with(".phpt") && !found.contains(p))
            .collect();
        if candidates.len() <= PHPT_LIST_CAP {
            found.extend(candidates);
        } else {
            withheld = candidates;
        }
    }

    // Laravel mirror-tree: source at `app/<rest>/<file>.php` pairs with test at
    // `tests/{Unit,Feature,Integration,Browser}/<rest>/<file>Test.php`. This is
    // the convention most modern Laravel projects use; the flat
    // `tests/Unit/FooTest.php` candidates above only cover root-level sources.
    if file_path.ends_with(".php")
        && let Some(rest) = file_path.strip_prefix("app/")
        && let Some((rest_dir, stem_with_ext)) = rest.rsplit_once('/')
        && let Some((mirror_stem, _)) = stem_with_ext.rsplit_once('.')
    {
        for type_dir in ["Unit", "Feature", "Integration", "Browser"] {
            let candidate = format!("tests/{type_dir}/{rest_dir}/{mirror_stem}Test.php");
            if !found.contains(&candidate) && db.file_id_for_path(&candidate)?.is_some() {
                found.push(candidate);
            }
        }
    }

    // Symfony mirror-tree: source at `src/<rest>/<file>.php` pairs with test at
    // `tests/<rest>/<file>Test.php` (no Unit/Feature subdivisor; Symfony tests
    // mirror src/ directly).
    if file_path.ends_with(".php")
        && let Some(rest) = file_path.strip_prefix("src/")
        && let Some((rest_dir, stem_with_ext)) = rest.rsplit_once('/')
        && let Some((mirror_stem, _)) = stem_with_ext.rsplit_once('.')
    {
        let candidate = format!("tests/{rest_dir}/{mirror_stem}Test.php");
        if !found.contains(&candidate) && db.file_id_for_path(&candidate)?.is_some() {
            found.push(candidate);
        }
    }
    // Java Maven mirror-tree: source at `src/main/java/<rest>/Foo.java`
    // pairs with `src/test/java/<rest>/FooTest.java`.
    if file_path.ends_with(".java")
        && let Some(rest) = file_path.strip_prefix("src/main/java/")
        && let Some((rest_dir, _)) = rest.rsplit_once('/')
    {
        let candidate = format!("src/test/java/{rest_dir}/{stem}Test.java");
        if !found.contains(&candidate) && db.file_id_for_path(&candidate)?.is_some() {
            found.push(candidate);
        }
    }

    // C/C++: `foo_test.<ext>` / `test_foo.<ext>` next to the source or under
    // `tests/`, keeping the source extension (covers .c/.h/.cc/.cpp/.cxx/.hpp).
    if let Some(ext) = ["c", "h", "cc", "cpp", "cxx", "hpp"]
        .into_iter()
        .find(|ext| file_path.ends_with(&format!(".{ext}")))
    {
        for candidate in [
            format!("{dir}/{stem}_test.{ext}"),
            format!("{dir}/test_{stem}.{ext}"),
            format!("tests/{stem}_test.{ext}"),
            format!("tests/test_{stem}.{ext}"),
        ] {
            let candidate = candidate.trim_start_matches('/').to_string();
            if candidate != file_path
                && !found.contains(&candidate)
                && db.file_id_for_path(&candidate)?.is_some()
            {
                found.push(candidate);
            }
        }
    }

    Ok((found, withheld))
}

/// Largest `.phpt` directory listing surfaced verbatim in `primary`. Above
/// this the directory is named in a note instead of dumped file-by-file.
const PHPT_LIST_CAP: usize = 50;

/// Heuristic: do any indexed files look like tests for `file_path`? Exposed as
/// `pub(super)` so `risk::assess_risk` can consume it without re-implementing
/// sibling detection. A `.phpt` directory withheld from listing by
/// [`PHPT_LIST_CAP`] still counts: those tests exist.
pub(super) fn test_sibling_exists(db: &Database, file_path: &str) -> Result<bool> {
    let (paths, withheld) = test_sibling_paths(db, file_path)?;
    Ok(!paths.is_empty() || !withheld.is_empty())
}

/// Env override for the total resolution-step budget.
const REACH_BUDGET_ENV: &str = "CODESAGE_REACH_BUDGET";
/// Env override for the wall-clock deadline, in milliseconds.
const REACH_DEADLINE_MS_ENV: &str = "CODESAGE_REACH_DEADLINE_MS";

const REACH_BUDGET_DEFAULT: usize = 1_500_000;
const REACH_DEADLINE_MS_DEFAULT: u64 = 5_000;
const REACH_MIN_INPUT_BUDGET_DEFAULT: usize = 100_000;

/// Paths named verbatim by [`abbreviate_paths`] before the rest collapse to a
/// count.
const ABBREVIATE_CAP: usize = 5;

fn env_usize(var: &str) -> Option<usize> {
    std::env::var(var).ok()?.trim().parse().ok()
}

/// Knobs for [`recommend_tests_with_reachability`]. The defaults are the
/// shipped contract (two of them env-overridable: `CODESAGE_REACH_BUDGET`
/// and `CODESAGE_REACH_DEADLINE_MS`); tests override them to exercise caps
/// without building large fixtures.
#[derive(Debug, Clone)]
pub struct ReachabilityOptions {
    /// Reverse-dependency depth. The default matches the depth `assess_risk`
    /// walks for its blast-radius and test-reach signals so the two tools
    /// agree on what "reaches" means.
    pub depth: usize,
    /// Largest `reachable` listing returned verbatim. On a repo like php-src
    /// every test transitively reaches `zend_hash.c`; without the cap the
    /// bucket would dump the whole suite.
    pub list_cap: usize,
    /// Resolution steps (caller files × same-named candidate definitions per
    /// symbol; a unique name costs its caller-file count) the whole request
    /// may spend: one pool that inputs draw from in request order. Each input
    /// may spend everything left in the pool except one `min_input_budget`
    /// reserved for every input still queued behind it (see
    /// `min_input_budget`). Σ spent ≤ `work_budget` always. The order
    /// dependence is real: put the files you care about first, because the
    /// inputs after the pool runs dry are the ones that land in
    /// `unwalked_files`. At the measured ~1.5 µs per step the default is
    /// about two seconds of resolution work, which is why the deadline exists.
    pub work_budget: usize,
    /// Reserved per queued input. Input `i` of `n` draws
    /// `max(pool - min_input_budget × (n - i - 1), min(min_input_budget,
    /// pool))`: the whole remaining pool minus a floor held back for each
    /// input after it, never less than the floor while the pool can pay it.
    /// A file that completes alone completes unchanged when a cheap input is
    /// appended, since only the floor is set aside for it. With a long file
    /// list the floor lets the first `work_budget / min_input_budget` inputs
    /// each get a walk of useful size instead of every input getting a
    /// useless sliver; the inputs after them find the pool empty and land in
    /// `unwalked_files`. A cheap input's unspent share stays in the pool for
    /// the inputs after it.
    pub min_input_budget: usize,
    /// Wall-clock limit for the whole request. Checked between admitted
    /// symbols, at the start of pricing each level and every 256 symbols
    /// while pricing, and once before each level's caller lookup — never
    /// inside one symbol's resolution or inside that lookup, so the overrun
    /// is bounded by one admitted symbol's resolution plus one level's caller
    /// lookup plus one pricing window of up to 255 symbols (two COUNT queries
    /// each). Inputs whose turn comes after it passes are skipped and named
    /// in `unwalked_files`; the input it interrupts lands in `partial_files`.
    pub deadline: Duration,
    /// Project root, when the caller knows it. Lets an absolute input path
    /// under the root be matched against the index's repo-relative paths;
    /// a leading `./` is dropped either way.
    pub project_root: Option<PathBuf>,
}

impl Default for ReachabilityOptions {
    fn default() -> Self {
        Self {
            depth: 2,
            list_cap: 50,
            work_budget: env_usize(REACH_BUDGET_ENV).unwrap_or(REACH_BUDGET_DEFAULT),
            min_input_budget: REACH_MIN_INPUT_BUDGET_DEFAULT,
            deadline: Duration::from_millis(
                env_usize(REACH_DEADLINE_MS_ENV)
                    .map(|ms| ms as u64)
                    .unwrap_or(REACH_DEADLINE_MS_DEFAULT),
            ),
            project_root: None,
        }
    }
}

/// Sibling + co-change buckets before any note is written. Shared by the
/// cheap and the reachability-aware entry points.
struct BaseRecommendations {
    primary: Vec<String>,
    coupled: Vec<CoupledTestEntry>,
    /// Inputs whose `.phpt` directory exceeded [`PHPT_LIST_CAP`].
    suppressed_sources: Vec<String>,
    /// The withheld `.phpt` paths themselves: vouched for, just not listed.
    withheld: Vec<String>,
}

fn base_recommendations(db: &Database, file_paths: &[String]) -> Result<BaseRecommendations> {
    let mut primary: HashSet<String> = HashSet::new();
    let mut coupled: Vec<CoupledTestEntry> = Vec::new();
    let mut suppressed_sources: Vec<String> = Vec::new();
    let mut withheld: Vec<String> = Vec::new();

    // One batched co-change query for the whole file list instead of one
    // `co_changes_for` per file. Same per-file top-20 (weight DESC, name
    // tiebreak) — `co_changes_for_many` reproduces the per-file LIMIT
    // exactly — with a single round-trip.
    let path_refs: Vec<&str> = file_paths.iter().map(String::as_str).collect();
    let co_batched = db.co_changes_for_many(&path_refs, 20)?;
    for path in file_paths {
        let (siblings, withheld_here) = test_sibling_paths(db, path)?;
        if !withheld_here.is_empty() {
            suppressed_sources.push(path.clone());
            withheld.extend(withheld_here);
        }
        for sibling in siblings {
            primary.insert(sibling);
        }
        if let Some(rows) = co_batched.get(path.as_str()) {
            for entry in rows {
                if matches!(FileCategory::classify(&entry.file), FileCategory::Test)
                    && !is_fixture(&entry.file)
                {
                    coupled.push(CoupledTestEntry {
                        file: entry.file.clone(),
                        weight: entry.weight,
                        count: entry.count,
                        source: path.clone(),
                    });
                }
            }
        }
    }

    // Drop coupled entries that are also in primary; primary already says "run me".
    coupled.retain(|c| !primary.contains(&c.file));

    // Dedupe coupled entries by file, keeping the highest-weight pairing so the
    // agent sees the strongest signal. Source attribution refers to that pairing.
    coupled.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen: HashSet<String> = HashSet::new();
    coupled.retain(|e| seen.insert(e.file.clone()));

    let mut primary_sorted: Vec<String> = primary.into_iter().collect();
    primary_sorted.sort();

    Ok(BaseRecommendations {
        primary: primary_sorted,
        coupled,
        suppressed_sources,
        withheld,
    })
}

/// Why the "nothing found, add tests" advice is withheld even though every
/// bucket is empty. `None` means the advice stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AbsenceOverride {
    /// Some other route found tests, so nothing is absent.
    OtherSignal,
    /// The search was cut short; absence cannot be asserted.
    WalkCapped,
    /// The index holds no test files at all: the advice would be misdirected
    /// at the changed files when the cause is index configuration.
    NoTestFilesIndexed,
    /// No input has a parser: nothing was looked up, so nothing is absent.
    NoUsableInput,
}

/// Notes shared by both entry points. `sibling_count` is the number of
/// convention-resolved siblings, which may be smaller than `primary` once
/// changed test files are folded in.
fn base_notes(
    base: &BaseRecommendations,
    sibling_count: usize,
    absence_override: Option<AbsenceOverride>,
) -> Vec<String> {
    let mut notes = Vec::new();
    if sibling_count == 0
        && base.coupled.is_empty()
        && absence_override.is_none()
        && base.suppressed_sources.is_empty()
    {
        notes.push(
            "no test files found via sibling conventions or co-change history; \
             run `codesage git-index` if you haven't, or add tests for these files"
                .to_string(),
        );
        return notes;
    }
    if sibling_count > 0 {
        notes.push(format!(
            "{sibling_count} sibling test file(s) found by language convention"
        ));
    }
    if !base.coupled.is_empty() {
        notes.push(format!(
            "{} additional test file(s) suggested by co-change history",
            base.coupled.len()
        ));
    }
    if !base.suppressed_sources.is_empty() {
        // Withheld, not absent: without this note the cap would turn a
        // heavily-tested extension file into an "untested" report.
        notes.push(format!(
            "tests/ directory next to {} holds more than {PHPT_LIST_CAP} .phpt files — \
             omitted from `primary` to keep output bounded; run that directory's suite",
            base.suppressed_sources.join(", ")
        ));
    }
    notes
}

/// Tests an agent should run after editing the given files. Two layers:
/// sibling tests (high confidence, language convention) plus tests that
/// historically co-change (medium confidence, catches integration-style
/// tests that don't follow naming conventions). Empty result means no
/// matching test files in the index.
///
/// Cheap by construction: a handful of path lookups and one co-change query,
/// no graph walk. The pre-edit hook (`build_edit_brief`) runs this on every
/// Edit under a 10 s timeout, so the reachability bucket lives behind
/// [`recommend_tests_with_reachability`] instead.
pub fn recommend_tests(db: &Database, file_paths: &[String]) -> Result<TestRecommendations> {
    let base = base_recommendations(db, file_paths)?;
    let notes = base_notes(&base, base.primary.len(), None);
    Ok(TestRecommendations {
        primary: base.primary,
        coupled: base.coupled,
        notes,
        ..Default::default()
    })
}

/// Test file types with no tree-sitter grammar that are still runnable test
/// entry points (php-src's `.phpt`). A test-shaped path outside this list and
/// outside the parser's languages is data, not a test to run.
const RUNNABLE_TEST_EXTENSIONS: [&str; 1] = ["phpt"];

fn has_supported_language(path: &str) -> bool {
    let p = Path::new(path);
    detect_language(p).is_some()
        || p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| RUNNABLE_TEST_EXTENSIONS.contains(&e))
}

/// Inputs sorted by what the walk can do with them.
struct InputTriage {
    /// Indexed files the walk starts from, in request order. Changed tests
    /// are among them: a base class, trait, or helper under `tests/` is
    /// reached by the tests that extend or call it.
    walkable: Vec<String>,
    /// Supported-language paths the index does not hold, changed tests
    /// included; these cap the answer.
    unindexed: Vec<String>,
    /// Paths with no parser (and not a runnable test type): never indexed,
    /// so not a walk failure and not a test to run.
    unsupported: Vec<String>,
    /// Inputs that are tests themselves: recommended in `primary`, walked
    /// like any other input when the index holds them.
    changed_tests: Vec<String>,
    /// The subset of `changed_tests` with no grammar (`.phpt`): a runnable
    /// test the indexer never parses, so it has no edges to walk and its
    /// absence from the index is not a reason to cap.
    unparsed_changed_tests: Vec<String>,
}

/// Decide per input whether it can be walked, is itself a test, or is
/// nothing the tool can use. Every input passes the language gate first, so
/// `tests/lang/en.json` is data even though its directory says "tests".
fn triage_inputs(db: &Database, file_paths: &[String]) -> Result<InputTriage> {
    let mut triage = InputTriage {
        walkable: Vec::new(),
        unindexed: Vec::new(),
        unsupported: Vec::new(),
        changed_tests: Vec::new(),
        unparsed_changed_tests: Vec::new(),
    };
    for path in file_paths {
        if !has_supported_language(path) {
            triage.unsupported.push(path.clone());
            continue;
        }
        let test_like = FileCategory::classify(path) == FileCategory::Test && !is_fixture(path);
        if test_like {
            triage.changed_tests.push(path.clone());
            if detect_language(Path::new(path)).is_none() {
                triage.unparsed_changed_tests.push(path.clone());
                continue;
            }
        }
        if db.file_id_for_path(path)?.is_some() {
            triage.walkable.push(path.clone());
        } else {
            triage.unindexed.push(path.clone());
        }
    }
    Ok(triage)
}

struct ReachOutcome {
    reached: HashMap<String, ReachableTestEntry>,
    /// Inputs that received no walk (empty pool or deadline already passed).
    unwalked: Vec<String>,
    /// Inputs whose walk started and was cut short.
    partial: Vec<String>,
    /// Indexed inputs that define no symbols; nothing exists to reach.
    no_symbol: Vec<String>,
}

impl ReachOutcome {
    fn empty() -> Self {
        Self {
            reached: HashMap::new(),
            unwalked: Vec::new(),
            partial: Vec::new(),
            no_symbol: Vec::new(),
        }
    }
}

/// Test-category files that reach any of `walkable` through resolved
/// call/import edges within `opts.depth` hops. One entry per test file,
/// keeping the shortest distance (ties resolved toward the earlier input
/// file). Input files never list themselves. Inputs draw from one step pool
/// in request order; each takes what is left after a floor is reserved for
/// every input still queued behind it. The deadline spans the request.
fn reachable_test_files(
    db: &Database,
    walkable: &[String],
    inputs: &HashSet<&str>,
    opts: &ReachabilityOptions,
) -> Result<ReachOutcome> {
    let mut out = ReachOutcome::empty();
    let mut pool = opts.work_budget;
    let mut budget = WalkBudget::new(0, Some(Instant::now() + opts.deadline));
    for (i, path) in walkable.iter().enumerate() {
        // Only the floor is held back for the inputs still queued, so a
        // file that completes alone still completes when a cheap input is
        // appended after it.
        let queued = walkable.len() - i - 1;
        let reserved = opts.min_input_budget.saturating_mul(queued);
        let share = pool
            .saturating_sub(reserved)
            .max(opts.min_input_budget.min(pool));
        budget.reset(share);
        if budget.exhausted || budget.over_deadline() {
            out.unwalked.push(path.clone());
            continue;
        }
        let req = ImpactRequest {
            target: ImpactTarget::File { path: path.clone() },
            depth: opts.depth,
            source_only: false,
        };
        let outcome = impact_analysis_walk_budgeted(db, &req, REACH_FRONTIER, Some(&mut budget))?;
        // Only what this input spent leaves the pool; a cheap input's
        // remainder is available to the inputs after it.
        pool = pool.saturating_sub(share.saturating_sub(budget.remaining));
        if outcome.capped {
            out.partial.push(path.clone());
        } else if outcome.seed_count == 0 {
            out.no_symbol.push(path.clone());
        }
        for entry in outcome.entries {
            if entry.category != FileCategory::Test
                || is_fixture(&entry.file_path)
                || inputs.contains(entry.file_path.as_str())
            {
                continue;
            }
            let edge_count = *outcome
                .edge_counts
                .get(&entry.file_path)
                .expect("the walk records an edge count for every entry it returns");
            let candidate = ReachableTestEntry {
                path: entry.file_path,
                distance: entry.distance,
                edge_count,
                via: path.clone(),
            };
            match out.reached.get(&candidate.path) {
                Some(existing) if existing.distance <= candidate.distance => {}
                _ => {
                    out.reached.insert(candidate.path.clone(), candidate);
                }
            }
        }
    }
    Ok(out)
}

/// First five paths verbatim, the rest as "and N more". Shared by the notes,
/// the rehearsal summary, and the CLI printer so every surface abbreviates
/// the same way.
pub fn abbreviate_paths(paths: &[String]) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(ABBREVIATE_CAP)
        .map(String::as_str)
        .collect();
    let more = paths.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// The lower-bound disclosure for an incomplete answer: which inputs were
/// not walked, cut short, not indexed, or define no symbols. `None` when the
/// walk was complete.
pub(crate) fn reach_cap_clause(recs: &TestRecommendations) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !recs.unwalked_files.is_empty() {
        parts.push(format!(
            "not walked: {}",
            abbreviate_paths(&recs.unwalked_files)
        ));
    }
    if !recs.partial_files.is_empty() {
        parts.push(format!(
            "cut short: {}",
            abbreviate_paths(&recs.partial_files)
        ));
    }
    if !recs.unindexed_files.is_empty() {
        parts.push(format!(
            "not indexed: {}",
            abbreviate_paths(&recs.unindexed_files)
        ));
    }
    if !recs.no_symbol_files.is_empty() {
        parts.push(format!(
            "no symbols indexed: {}",
            abbreviate_paths(&recs.no_symbol_files)
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// [`recommend_tests`] plus a third bucket: tests that reach the changed
/// files through resolved call/import edges, and a count of the indexed
/// tests no bucket can vouch for. Costs a bounded reverse-dependency walk
/// per input plus one file-list scan; meant for the MCP tool, the CLI, and
/// the pre-commit rehearsal, not the per-edit hook.
pub fn recommend_tests_with_reachability(
    db: &Database,
    file_paths_in: &[String],
    opts: &ReachabilityOptions,
) -> Result<TestRecommendations> {
    // A blank input names nothing. The project root itself (`.`, `./`, or
    // the absolute root) normalizes to an empty path and names no file
    // either. Both are dropped and said so. Every other input is looked up
    // in its normalized form but reported back in the form the caller gave:
    // `/root/..` normalizes to `..`, which no caller would recognise. Two
    // spellings of one file (`./src/x.php`, `src/x.php`) are one input: the
    // first spelling is the one reported, and the file is walked once.
    let mut blank_inputs = 0usize;
    let mut root_inputs = 0usize;
    let mut duplicate_inputs = 0usize;
    let mut as_given: HashMap<String, String> = HashMap::new();
    let mut file_paths: Vec<String> = Vec::with_capacity(file_paths_in.len());
    for given in file_paths_in {
        if given.trim().is_empty() {
            blank_inputs += 1;
            continue;
        }
        let normalized = normalize_input_path(opts.project_root.as_deref(), given);
        if normalized.is_empty() {
            root_inputs += 1;
            continue;
        }
        if as_given.contains_key(&normalized) {
            duplicate_inputs += 1;
            continue;
        }
        as_given.insert(normalized.clone(), given.clone());
        file_paths.push(normalized);
    }
    let mut ignored_notes: Vec<String> = Vec::new();
    if blank_inputs > 0 {
        ignored_notes.push(format!("{blank_inputs} empty input(s) ignored"));
    }
    if root_inputs > 0 {
        ignored_notes.push(format!(
            "{root_inputs} input(s) ignored: the project root names no file"
        ));
    }
    if duplicate_inputs > 0 {
        ignored_notes.push(format!(
            "{duplicate_inputs} duplicate input(s) ignored: another spelling of the same path"
        ));
    }
    if file_paths.is_empty() {
        // Nothing was looked up, so there is nothing to vouch for or to
        // report absent; only the reason the inputs went unused is said.
        return Ok(TestRecommendations {
            notes: ignored_notes,
            ..Default::default()
        });
    }
    let restore = |paths: Vec<String>| -> Vec<String> {
        paths
            .into_iter()
            .map(|p| as_given.get(&p).cloned().unwrap_or(p))
            .collect()
    };
    let base = base_recommendations(db, &file_paths)?;
    let sibling_count = base.primary.len();

    // Triage before anything can short-circuit: a list of only brand-new
    // test files must still come back saying the index has not seen them.
    let triage = triage_inputs(db, &file_paths)?;

    // Every input lacks a parser (CHANGELOG.md, composer.json). Nothing was
    // walked and nothing could have been, so neither absence advice nor an
    // unmodelled count describes anything; the co-change bucket, which
    // needs no parser, is kept. An unindexed input keeps the ordinary path:
    // its absence from the index is a lower-bound disclosure, not a skip.
    if triage.walkable.is_empty() && triage.changed_tests.is_empty() && triage.unindexed.is_empty()
    {
        debug_assert_eq!(triage.unsupported.len(), file_paths.len());
        let mut notes = base_notes(&base, sibling_count, Some(AbsenceOverride::NoUsableInput));
        let unsupported_files = restore(triage.unsupported);
        notes.extend(ignored_notes);
        notes.push(unsupported_note(&unsupported_files));
        return Ok(TestRecommendations {
            primary: base.primary,
            coupled: base.coupled,
            unsupported_files,
            notes,
            ..Default::default()
        });
    }

    // A changed file that is itself a test is the highest-confidence
    // recommendation there is. It goes into `primary`, and stays out of the
    // graph-derived buckets and the unmodelled count; it is still walked, so
    // the tests that extend or call it land in `reachable` with it as `via`.
    // Fixture files and test-shaped data (`tests/lang/en.json`) are not
    // runnable, so the triage's language gate keeps them out.
    let inputs: HashSet<&str> = file_paths.iter().map(String::as_str).collect();
    let mut primary = base.primary.clone();
    let mut changed_tests = 0usize;
    for path in &triage.changed_tests {
        if !primary.contains(path) {
            primary.push(path.clone());
            changed_tests += 1;
        }
    }
    if changed_tests > 0 {
        primary.sort();
    }

    // `vouched` is the union used both to keep a test out of a second bucket
    // and to exclude it from the unmodelled count. Withheld `.phpt` files
    // are vouched for too: omitted from the listing, not unknown to the tool.
    let mut vouched: HashSet<String> = primary.iter().cloned().collect();
    vouched.extend(base.coupled.iter().map(|c| c.file.clone()));
    vouched.extend(base.withheld.iter().cloned());

    // The denominator: test-category paths in the index, plus the siblings
    // the conventions resolved that the category rule does not recognise
    // (C's `foo_test.c` is a sibling but classifies as source). An index with
    // no test files at all (tests excluded by config) has nothing for the
    // walk to find, so the walk is skipped.
    let mut test_files: HashSet<String> = db
        .all_file_paths()?
        .into_iter()
        .filter(|p| FileCategory::classify(p) == FileCategory::Test && !is_fixture(p))
        .collect();
    test_files.extend(base.primary.iter().cloned());
    test_files.extend(base.withheld.iter().cloned());
    let indexed_test_files = test_files.len();

    let reach = if indexed_test_files == 0 {
        ReachOutcome::empty()
    } else {
        reachable_test_files(db, &triage.walkable, &inputs, opts)?
    };
    let reach_walk_capped = !reach.unwalked.is_empty()
        || !reach.partial.is_empty()
        || !triage.unindexed.is_empty()
        || !reach.no_symbol.is_empty();
    let mut reachable: Vec<ReachableTestEntry> = reach
        .reached
        .into_values()
        .filter(|e| !vouched.contains(&e.path))
        .collect();
    // Most-connected first within a distance, so a cap keeps the tests whose
    // code touches the changed set most, not the alphabetically earliest.
    reachable.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| b.edge_count.cmp(&a.edge_count))
            .then_with(|| a.path.cmp(&b.path))
    });
    let reachable_total = reachable.len();
    let reachable_capped = reachable_total > opts.list_cap;
    vouched.extend(reachable.iter().map(|e| e.path.clone()));
    reachable.truncate(opts.list_cap);

    let unmodelled = test_files
        .iter()
        .filter(|p| !vouched.contains(*p) && !inputs.contains(p.as_str()))
        .count();

    let absence_override = if reachable_total > 0 || changed_tests > 0 {
        Some(AbsenceOverride::OtherSignal)
    } else if indexed_test_files == 0 {
        Some(AbsenceOverride::NoTestFilesIndexed)
    } else if reach_walk_capped {
        Some(AbsenceOverride::WalkCapped)
    } else {
        None
    };
    let mut notes = base_notes(&base, sibling_count, absence_override);
    if changed_tests > 0 {
        let unparsed = if triage.unparsed_changed_tests.is_empty() {
            String::new()
        } else {
            format!(
                " (not parsed by the indexer: {})",
                abbreviate_paths(&triage.unparsed_changed_tests)
            )
        };
        notes.push(format!(
            "{changed_tests} changed file(s) are tests themselves; listed in `primary`{unparsed}"
        ));
    }
    notes.extend(ignored_notes);

    let mut recs = TestRecommendations {
        primary,
        coupled: base.coupled,
        reachable,
        reachable_total,
        reachable_capped,
        reach_walk_capped,
        unwalked_files: restore(reach.unwalked),
        partial_files: restore(reach.partial),
        unindexed_files: restore(triage.unindexed),
        no_symbol_files: restore(reach.no_symbol),
        unsupported_files: restore(triage.unsupported),
        unmodelled,
        indexed_test_files,
        notes,
    };

    if indexed_test_files == 0 {
        recs.notes.push(
            "the index contains no test files (check `[index] exclude_patterns`); \
             reachability walk skipped"
                .to_string(),
        );
    }
    if reachable_total > 0 || unmodelled > 0 || reach_walk_capped {
        let cap_suffix = if reachable_capped {
            format!(", capped at {}", opts.list_cap)
        } else {
            String::new()
        };
        let mut note = format!(
            "{reachable_total} reachable test file(s) via call/import edges (distance ≤ {}){cap_suffix}",
            opts.depth
        );
        match reach_cap_clause(&recs) {
            // An incomplete answer cannot assert "no edge" about anything it
            // never visited, so the unmodelled sentence is withheld.
            Some(clause) => {
                note.push_str(&format!("; reachable is a lower bound ({clause})"));
            }
            None => note.push_str(&format!(
                "; {unmodelled} of {indexed_test_files} indexed test files have no resolved \
                 edge into the changed set and are not vouched for"
            )),
        }
        recs.notes.push(note);
    }
    if !recs.unsupported_files.is_empty() {
        recs.notes.push(unsupported_note(&recs.unsupported_files));
    }

    Ok(recs)
}

/// The skip note for inputs without a parser, `as_given` spelled as the
/// caller wrote them.
fn unsupported_note(as_given: &[String]) -> String {
    format!(
        "{} file(s) skipped, no parser for their type: {}",
        as_given.len(),
        abbreviate_paths(as_given)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, Symbol, SymbolKind};

    fn add_file(db: &Database, path: &str) -> i64 {
        db.upsert_file(&FileInfo {
            path: path.to_string(),
            language: Language::Php,
            content_hash: format!("hash-{path}"),
        })
        .unwrap()
    }

    fn symbol(name: &str, file_path: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: name.to_string(),
            kind: SymbolKind::Function,
            file_path: file_path.to_string(),
            line_start: 1,
            line_end: 10,
            col_start: 0,
            col_end: 0,
            rationale: vec![],
        }
    }

    fn call(from_file: &str, from_symbol: Option<&str>, to_name: &str, line: u32) -> Reference {
        Reference {
            from_file: from_file.to_string(),
            from_symbol: from_symbol.map(str::to_string),
            to_name: to_name.to_string(),
            kind: ReferenceKind::Call,
            line,
            col: 4,
        }
    }

    /// `Repository.php` defines `find`; `Service.php::run` calls it. Tests
    /// hang off these two as each case needs.
    fn graph() -> Database {
        let db = Database::open_in_memory().unwrap();
        let repo = add_file(&db, "Repository.php");
        db.insert_symbols(repo, &[symbol("find", "Repository.php")])
            .unwrap();
        let service = add_file(&db, "Service.php");
        db.insert_symbols(service, &[symbol("run", "Service.php")])
            .unwrap();
        db.insert_references(service, &[call("Service.php", Some("run"), "find", 5)])
            .unwrap();
        db
    }

    /// [`graph`] plus one test per hop: with both tests present, walking
    /// Repository.php costs 3 steps (`find`: 2 caller files, then `run`: 1)
    /// and walking Service.php costs 1.
    fn graph_with_hop_tests() -> Database {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_test_calling(&db, "tests/ServiceRunTest.php", &["run"]);
        db
    }

    fn add_test_calling(db: &Database, path: &str, to_names: &[&str]) {
        let id = add_file(db, path);
        let refs: Vec<Reference> = to_names
            .iter()
            .enumerate()
            .map(|(i, name)| call(path, None, name, 5 + i as u32))
            .collect();
        db.insert_references(id, &refs).unwrap();
    }

    /// Unlimited walk: no budget, no floor, no deadline pressure.
    fn unlimited() -> ReachabilityOptions {
        ReachabilityOptions {
            depth: 2,
            list_cap: 50,
            work_budget: usize::MAX,
            min_input_budget: 0,
            deadline: Duration::from_secs(60),
            project_root: None,
        }
    }

    fn recs(db: &Database, inputs: &[&str]) -> TestRecommendations {
        recs_with(db, inputs, &unlimited())
    }

    fn recs_with(
        db: &Database,
        inputs: &[&str],
        opts: &ReachabilityOptions,
    ) -> TestRecommendations {
        let inputs: Vec<String> = inputs.iter().map(|s| s.to_string()).collect();
        recommend_tests_with_reachability(db, &inputs, opts).unwrap()
    }

    fn shape(r: &TestRecommendations) -> Vec<(&str, u32, u32, &str)> {
        r.reachable
            .iter()
            .map(|e| (e.path.as_str(), e.distance, e.edge_count, e.via.as_str()))
            .collect()
    }

    fn no_files() -> Vec<String> {
        Vec::new()
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn reach_note(r: &TestRecommendations) -> &str {
        r.notes
            .iter()
            .find(|n| n.contains("reachable test file(s)"))
            .unwrap_or_else(|| panic!("no reach note in {:?}", r.notes))
    }

    #[test]
    fn defaults_come_from_constants_and_env_overrides_parse() {
        let d = ReachabilityOptions::default();
        assert_eq!(d.depth, 2);
        assert_eq!(d.list_cap, 50);
        assert_eq!(d.min_input_budget, REACH_MIN_INPUT_BUDGET_DEFAULT);
        assert!(d.project_root.is_none());
        // Only the parse path is pinned here: the env is process-global and
        // other tests run concurrently, so it is not mutated.
        assert_eq!(env_usize("CODESAGE_REACH_TEST_UNSET_VAR"), None);
    }

    #[test]
    fn abbreviate_paths_names_five_then_counts() {
        let five = files(&["a", "b", "c", "d", "e"]);
        assert_eq!(abbreviate_paths(&five), "a, b, c, d, e");
        let seven = files(&["a", "b", "c", "d", "e", "f", "g"]);
        assert_eq!(abbreviate_paths(&seven), "a, b, c, d, e and 2 more");
        assert_eq!(abbreviate_paths(&[]), "");
    }

    #[test]
    fn fixture_detection_is_segment_anchored_and_case_insensitive() {
        assert!(is_fixture("tests/Fixtures/CapitalTest.php"));
        assert!(is_fixture("tests/Database/stubs/X.php"));
        assert!(is_fixture("fixtures/a.php"));
        assert!(is_fixture("crates/parser/tests/fixtures/sample.php"));
        assert!(is_fixture("tests/__snapshots__/a.test.ts.snap"));
        assert!(is_fixture("pkg/testdata/in.go"));
        assert!(!is_fixture("tests/RepositoryTest.php"));
        assert!(!is_fixture("src/fixturesx/a.php"));
        assert!(!is_fixture("src/myfixtures/a.php"));
    }

    #[test]
    fn normalize_input_path_strips_dot_slash_and_root_prefix() {
        let root = Path::new("/proj");
        assert_eq!(normalize_input_path(None, "./src/x.php"), "src/x.php");
        assert_eq!(normalize_input_path(None, "././src/x.php"), "src/x.php");
        assert_eq!(
            normalize_input_path(Some(root), "/proj/src/x.php"),
            "src/x.php"
        );
        // Outside the root or without a root: left alone.
        assert_eq!(
            normalize_input_path(Some(root), "/elsewhere/x.php"),
            "/elsewhere/x.php"
        );
        assert_eq!(
            normalize_input_path(None, "/proj/src/x.php"),
            "/proj/src/x.php"
        );
        assert_eq!(normalize_input_path(None, "src/x.php"), "src/x.php");
        // Repeated separators and `.` components anywhere.
        assert_eq!(normalize_input_path(None, ".//src/x.php"), "src/x.php");
        assert_eq!(normalize_input_path(None, "src/./x.php"), "src/x.php");
        assert_eq!(normalize_input_path(None, "src//x.php"), "src/x.php");
        // Trailing slash on the root is irrelevant.
        assert_eq!(
            normalize_input_path(Some(Path::new("/proj/")), "/proj/src/x.php"),
            "src/x.php"
        );
        // The root itself, in every spelling, is the empty path.
        assert_eq!(normalize_input_path(Some(root), "/proj"), "");
        assert_eq!(normalize_input_path(Some(root), "/proj/"), "");
        assert_eq!(normalize_input_path(None, "./"), "");
        assert_eq!(normalize_input_path(None, "."), "");
        // Interior `..` is not resolved; the path is left as given.
        assert_eq!(
            normalize_input_path(Some(root), "/proj/src/../x.php"),
            "src/../x.php"
        );
    }

    #[test]
    fn dot_slash_and_absolute_inputs_resolve_against_the_index() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);

        let r = recs(&db, &["./Repository.php"]);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert_eq!(r.unindexed_files, no_files());
        assert!(!r.reach_walk_capped);

        let opts = ReachabilityOptions {
            project_root: Some(PathBuf::from("/proj")),
            ..unlimited()
        };
        let r = recs_with(&db, &["/proj/Repository.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert_eq!(r.unindexed_files, no_files());
        assert!(!r.reach_walk_capped);
    }

    #[test]
    fn cheap_variant_skips_the_walk_and_leaves_reachability_fields_default() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");

        let r = recommend_tests(&db, &["Repository.php".to_string()]).unwrap();
        assert!(r.primary.is_empty());
        assert!(r.reachable.is_empty());
        assert_eq!(r.reachable_total, 0);
        assert!(!r.reachable_capped);
        assert!(!r.reach_walk_capped);
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, no_files());
        assert_eq!(r.unindexed_files, no_files());
        assert_eq!(r.no_symbol_files, no_files());
        assert_eq!(r.unsupported_files, no_files());
        assert_eq!(r.unmodelled, 0);
        assert_eq!(r.indexed_test_files, 0);
        assert!(
            r.notes.iter().any(|n| n.contains("no test files found")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn reachable_lists_test_that_calls_input_directly() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);

        let r = recs(&db, &["Repository.php"]);
        assert!(r.primary.is_empty());
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert_eq!(r.reachable_total, 1);
        assert!(!r.reachable_capped);
        assert!(!r.reach_walk_capped);
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, no_files());
        assert_eq!(r.unindexed_files, no_files());
        assert_eq!(r.no_symbol_files, no_files());
        assert_eq!(r.unmodelled, 0);
        assert_eq!(r.indexed_test_files, 1);
        let note = reach_note(&r);
        assert!(
            note.starts_with("1 reachable test file(s)") && note.contains("0 of 1 indexed"),
            "{note}"
        );
    }

    #[test]
    fn reachable_reports_distance_two_through_intermediate_source() {
        let db = graph();
        add_test_calling(&db, "tests/ServiceRunTest.php", &["run"]);

        let r = recs(&db, &["Repository.php"]);
        assert_eq!(
            shape(&r),
            vec![("tests/ServiceRunTest.php", 2, 1, "Repository.php")]
        );
    }

    #[test]
    fn reachable_keeps_shortest_distance_across_inputs_and_sorts_by_distance() {
        let db = graph();
        add_test_calling(&db, "tests/ServiceRunTest.php", &["run"]);
        add_test_calling(&db, "tests/ZRepositoryTest.php", &["find"]);

        let r = recs(&db, &["Repository.php", "Service.php"]);
        assert_eq!(
            shape(&r),
            vec![
                ("tests/ServiceRunTest.php", 1, 1, "Service.php"),
                ("tests/ZRepositoryTest.php", 1, 1, "Repository.php"),
            ]
        );
    }

    #[test]
    fn equal_distance_tie_attributes_via_to_the_earlier_input() {
        let db = graph();
        add_test_calling(&db, "tests/BothTest.php", &["find", "run"]);

        // Walking from Repository.php records two edges for BothTest (`find`
        // at depth 1, `run` via Service.php at depth 2); from Service.php it
        // records one. The first input to reach the test at the shortest
        // distance owns the entry, edge count included.
        let r = recs(&db, &["Repository.php", "Service.php"]);
        assert_eq!(
            shape(&r),
            vec![("tests/BothTest.php", 1, 2, "Repository.php")]
        );
        let r = recs(&db, &["Service.php", "Repository.php"]);
        assert_eq!(shape(&r), vec![("tests/BothTest.php", 1, 1, "Service.php")]);
    }

    #[test]
    fn reachable_excludes_tests_already_in_primary() {
        let db = graph();
        // Sibling by convention *and* a direct caller: primary wins.
        add_test_calling(&db, "RepositoryTest.php", &["find"]);

        let r = recs(&db, &["Repository.php"]);
        assert_eq!(r.primary, vec!["RepositoryTest.php".to_string()]);
        assert!(r.reachable.is_empty(), "reachable: {:?}", r.reachable);
        assert_eq!(r.reachable_total, 0);
        assert_eq!(r.unmodelled, 0);
    }

    #[test]
    fn fixture_callers_and_fixture_co_changes_are_not_recommendations() {
        let db = graph();
        // A fixture that calls `find` is reachable by the graph but is not a
        // test to run; a real test with no edges makes the index non-empty.
        add_test_calling(&db, "tests/fixtures/FixtureCaller.php", &["find"]);
        add_file(&db, "tests/OtherTest.php");
        // A fixture that co-changes with the input is not a coupled test.
        db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_file("tests/fixtures/data.php", 0.5, 0, 5, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_co_change(
            "Repository.php",
            "tests/fixtures/data.php",
            4.2,
            8,
            Some(1_700_000_000),
        )
        .unwrap();

        let r = recs(&db, &["Repository.php"]);
        assert!(r.reachable.is_empty(), "reachable: {:?}", r.reachable);
        assert_eq!(r.reachable_total, 0);
        assert!(r.coupled.is_empty(), "coupled: {:?}", r.coupled);
        assert!(!r.reach_walk_capped);
        assert_eq!(r.indexed_test_files, 1);
        assert_eq!(r.unmodelled, 1);
        // Nothing vouched for anything: the absence advice stands.
        assert!(
            r.notes.iter().any(|n| n.contains("no test files found")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn reachable_caps_at_fifty_and_discloses_it() {
        let db = graph();
        for i in 0..60 {
            add_test_calling(&db, &format!("tests/Fan{i:02}Test.php"), &["find"]);
        }

        let r = recs(&db, &["Repository.php"]);
        assert_eq!(r.reachable.len(), 50);
        assert_eq!(r.reachable_total, 60);
        assert!(r.reachable_capped);
        assert!(!r.reach_walk_capped);
        assert_eq!(r.reachable[0].path, "tests/Fan00Test.php");
        assert_eq!(r.reachable[49].path, "tests/Fan49Test.php");
        // The ten dropped by the cap are still reachable, not unmodelled.
        assert_eq!(r.unmodelled, 0);
        assert_eq!(r.indexed_test_files, 60);
        let note = reach_note(&r);
        assert!(
            note.contains("60 reachable") && note.contains("capped at 50"),
            "{note}"
        );
    }

    #[test]
    fn list_cap_keeps_the_most_connected_tests_within_a_distance() {
        let db = graph();
        let repo = db.file_id_for_path("Repository.php").unwrap().unwrap();
        db.insert_symbols(repo, &[symbol("save", "Repository.php")])
            .unwrap();
        add_test_calling(&db, "tests/AThinTest.php", &["find"]);
        add_test_calling(&db, "tests/ZWideTest.php", &["find", "save"]);

        let opts = ReachabilityOptions {
            list_cap: 1,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![("tests/ZWideTest.php", 1, 2, "Repository.php")]
        );
        assert_eq!(r.reachable_total, 2);
        assert!(r.reachable_capped);
    }

    #[test]
    fn edge_count_is_uncapped_so_twelve_edges_outrank_eleven() {
        let db = graph();
        let repo = db.file_id_for_path("Repository.php").unwrap().unwrap();
        let names: Vec<String> = (1..=12).map(|i| format!("op{i:02}")).collect();
        let syms: Vec<Symbol> = names.iter().map(|n| symbol(n, "Repository.php")).collect();
        db.insert_symbols(repo, &syms).unwrap();
        let all: Vec<&str> = names.iter().map(String::as_str).collect();
        // Alphabetically first but one edge short: must lose the single slot.
        add_test_calling(&db, "tests/AElevenTest.php", &all[..11]);
        add_test_calling(&db, "tests/ZTwelveTest.php", &all);

        let opts = ReachabilityOptions {
            list_cap: 1,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![("tests/ZTwelveTest.php", 1, 12, "Repository.php")]
        );
        let full = recs(&db, &["Repository.php"]);
        assert_eq!(
            shape(&full),
            vec![
                ("tests/ZTwelveTest.php", 1, 12, "Repository.php"),
                ("tests/AElevenTest.php", 1, 11, "Repository.php"),
            ]
        );
    }

    #[test]
    fn work_budget_stops_the_walk_and_downgrades_the_note_to_a_lower_bound() {
        let db = graph_with_hop_tests();
        add_file(&db, "tests/UnrelatedTest.php");

        // `find` costs 2 steps and spends the budget, so the depth-2 hop to
        // ServiceRunTest is never taken.
        let opts = ReachabilityOptions {
            work_budget: 2,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert!(r.reach_walk_capped);
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, files(&["Repository.php"]));
        assert!(!r.reachable_capped);
        assert_eq!(r.indexed_test_files, 3);
        let note = reach_note(&r);
        assert!(
            note.contains("lower bound (cut short: Repository.php)"),
            "{note}"
        );
        assert!(!note.contains("no resolved edge"), "{note}");
    }

    #[test]
    fn pool_never_spends_past_the_budget_and_the_floor_serves_the_first_inputs() {
        let db = graph_with_hop_tests();

        // Budget 3, floor 3. Repository.php: pool 3 minus the 3 reserved for
        // Service.php is 0, so it draws the floor, 3, and spends all of it;
        // Service.php finds the pool empty and gets no walk. ⌊3 / 3⌋ = 1
        // input served, Σ spent = 3 = budget.
        let opts = ReachabilityOptions {
            work_budget: 3,
            min_input_budget: 3,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php", "Service.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/ServiceRunTest.php", 2, 1, "Repository.php"),
            ]
        );
        assert_eq!(r.partial_files, no_files());
        assert_eq!(r.unwalked_files, files(&["Service.php"]));
        assert!(r.reach_walk_capped);
    }

    #[test]
    fn pool_lets_a_cheap_inputs_leftover_finish_a_later_input() {
        let db = graph_with_hop_tests();

        // Budget 4 over 2 inputs, floor 3. Service.php may draw
        // max(4 - 3, min(3, 4)) = 3, the floor; it spends 1 of it and
        // completes; Repository.php, last in line with nothing
        // reserved behind it, draws the whole remaining 3 and finishes. A
        // fixed half-share of 2 would have cut Repository.php short.
        let opts = ReachabilityOptions {
            work_budget: 4,
            min_input_budget: 3,
            ..unlimited()
        };
        let r = recs_with(&db, &["Service.php", "Repository.php"], &opts);
        assert!(!r.reach_walk_capped, "partial: {:?}", r.partial_files);
        assert_eq!(r.partial_files, no_files());
        assert_eq!(r.reachable_total, 2);

        // Budget 3, floor 0: Service.php may draw all 3, spends 1, and
        // Repository.php still gets the remaining 2: `find` (2 caller
        // files) resolves, `run` behind it does not.
        let tight = ReachabilityOptions {
            work_budget: 3,
            min_input_budget: 0,
            ..unlimited()
        };
        let r = recs_with(&db, &["Service.php", "Repository.php"], &tight);
        assert_eq!(
            shape(&r),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/ServiceRunTest.php", 1, 1, "Service.php"),
            ]
        );
        assert_eq!(r.partial_files, files(&["Repository.php"]));
        assert_eq!(r.unwalked_files, no_files());
    }

    #[test]
    fn a_complete_input_stays_complete_when_a_cheap_input_is_appended() {
        let db = graph_with_hop_tests();
        let base = add_file(&db, "tests/TestCase.php");
        db.insert_symbols(base, &[symbol("TestCase", "tests/TestCase.php")])
            .unwrap();
        add_test_calling(&db, "tests/Unit/AlphaTest.php", &["TestCase"]);

        // Repository.php costs 3, TestCase.php costs 1. Budget 4, floor 1:
        // alone, Repository.php draws 4 and finishes. With the changed test
        // appended it draws 4 - 1 = 3 and still finishes; a cap of
        // budget / inputs = 2 would have flipped the same file to "cut
        // short" with byte-identical content.
        let opts = ReachabilityOptions {
            work_budget: 4,
            min_input_budget: 1,
            ..unlimited()
        };
        let alone = recs_with(&db, &["Repository.php"], &opts);
        assert!(!alone.reach_walk_capped, "{:?}", alone.notes);
        assert_eq!(
            shape(&alone),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/ServiceRunTest.php", 2, 1, "Repository.php"),
            ]
        );

        let both = recs_with(&db, &["Repository.php", "tests/TestCase.php"], &opts);
        assert!(!both.reach_walk_capped, "{:?}", both.notes);
        assert_eq!(both.partial_files, no_files());
        assert_eq!(both.unwalked_files, no_files());
        assert_eq!(both.primary, files(&["tests/TestCase.php"]));
        assert_eq!(
            shape(&both),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/Unit/AlphaTest.php", 1, 1, "tests/TestCase.php"),
                ("tests/ServiceRunTest.php", 2, 1, "Repository.php"),
            ]
        );
    }

    #[test]
    fn the_floor_reserved_for_later_inputs_keeps_the_first_from_starving_them() {
        let db = graph_with_hop_tests();

        // Budget 4, floor 2. Repository.php may draw 4 - 2 = 2, spends it on
        // `find` and is cut short before depth 2; Service.php draws the
        // reserved 2, and `run` costs 1 and completes. With no floor the
        // first input would take all 4 and the second would still get 1
        // here, but a first input costing 4 would leave it nothing.
        let opts = ReachabilityOptions {
            work_budget: 4,
            min_input_budget: 2,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php", "Service.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/ServiceRunTest.php", 1, 1, "Service.php"),
            ]
        );
        assert!(r.reach_walk_capped);
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, files(&["Repository.php"]));
    }

    #[test]
    fn passed_deadline_skips_every_input_and_names_them() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);

        let opts = ReachabilityOptions {
            deadline: Duration::ZERO,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php", "Service.php"], &opts);
        assert!(r.reachable.is_empty());
        assert!(r.reach_walk_capped);
        assert_eq!(r.unwalked_files, files(&["Repository.php", "Service.php"]));
        assert_eq!(r.partial_files, no_files());
        assert!(
            reach_note(&r).contains("not walked: Repository.php, Service.php"),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn capped_note_abbreviates_long_input_lists() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        for i in 0..7 {
            add_file(&db, &format!("Other{i}.php"));
        }
        let inputs: Vec<String> = (0..7).map(|i| format!("Other{i}.php")).collect();
        let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();

        let opts = ReachabilityOptions {
            deadline: Duration::ZERO,
            ..unlimited()
        };
        let r = recs_with(&db, &refs, &opts);
        assert_eq!(r.unwalked_files.len(), 7);
        assert!(
            reach_note(&r).contains(
                "not walked: Other0.php, Other1.php, Other2.php, Other3.php, Other4.php and 2 more"
            ),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn unindexed_input_is_disclosed_and_blocks_the_unmodelled_claim() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");

        // src/New.php has a parser but was never indexed: no edges, so no
        // test can be shown to reach it, and nothing can be called
        // unvouched-for.
        let r = recs(&db, &["Repository.php", "src/New.php"]);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert_eq!(r.unindexed_files, files(&["src/New.php"]));
        assert_eq!(r.unsupported_files, no_files());
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, no_files());
        assert!(r.reach_walk_capped);
        let note = reach_note(&r);
        assert!(note.contains("not indexed: src/New.php"), "{note}");
        assert!(!note.contains("indexed test files have"), "{note}");
    }

    #[test]
    fn unsupported_input_is_skipped_without_capping_the_answer() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");

        // CHANGELOG.md has no parser: it is never in the index, and that
        // says nothing about the walk over Repository.php.
        let r = recs(&db, &["CHANGELOG.md", "Repository.php"]);
        assert_eq!(r.unsupported_files, files(&["CHANGELOG.md"]));
        assert_eq!(r.unindexed_files, no_files());
        assert!(!r.reach_walk_capped);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        let note = reach_note(&r);
        assert!(
            note.contains("1 of 2 indexed test files have no resolved edge"),
            "{note}"
        );
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("1 file(s) skipped, no parser for their type: CHANGELOG.md")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn indexed_input_without_symbols_is_disclosed_without_a_reindex_remedy() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");
        add_file(&db, "src/Empty.php");

        let r = recs(&db, &["Repository.php", "src/Empty.php"]);
        assert_eq!(r.no_symbol_files, files(&["src/Empty.php"]));
        assert_eq!(r.unindexed_files, no_files());
        assert_eq!(r.partial_files, no_files());
        assert!(r.reach_walk_capped);
        let note = reach_note(&r);
        assert!(note.contains("no symbols indexed: src/Empty.php"), "{note}");
        assert!(!note.contains("not indexed:"), "{note}");
        assert!(!note.contains("indexed test files have"), "{note}");
    }

    #[test]
    fn a_list_of_only_new_test_files_is_still_disclosed() {
        let db = graph();

        // The index holds no test files, so the walk is skipped — but the
        // caller still learns that its one input is unknown to the index. A
        // changed test is walked like any input, so one the index has not
        // seen caps the answer like any unindexed input; it is still the
        // recommendation.
        let r = recs(&db, &["tests/NewTest.php"]);
        assert_eq!(r.primary, files(&["tests/NewTest.php"]));
        assert_eq!(r.indexed_test_files, 0);
        assert_eq!(r.unindexed_files, files(&["tests/NewTest.php"]));
        assert!(r.reach_walk_capped);
        assert!(
            r.notes.iter().any(|n| n.contains("contains no test files")),
            "notes: {:?}",
            r.notes
        );
        assert!(
            r.notes
                .iter()
                .any(|n| n == "1 changed file(s) are tests themselves; listed in `primary`"),
            "notes: {:?}",
            r.notes
        );
        let note = reach_note(&r);
        assert!(note.contains("not indexed: tests/NewTest.php"), "{note}");
        assert!(!note.contains("not yet indexed"), "{note}");
    }

    #[test]
    fn changed_base_test_class_reaches_its_subclasses() {
        let db = graph();
        let base = add_file(&db, "tests/TestCase.php");
        db.insert_symbols(base, &[symbol("TestCase", "tests/TestCase.php")])
            .unwrap();
        add_test_calling(&db, "tests/Unit/AlphaTest.php", &["TestCase"]);
        add_test_calling(&db, "tests/Unit/BetaTest.php", &["TestCase"]);

        let r = recs(&db, &["tests/TestCase.php"]);
        assert_eq!(r.primary, files(&["tests/TestCase.php"]));
        assert_eq!(
            shape(&r),
            vec![
                ("tests/Unit/AlphaTest.php", 1, 1, "tests/TestCase.php"),
                ("tests/Unit/BetaTest.php", 1, 1, "tests/TestCase.php"),
            ]
        );
        assert_eq!(r.reachable_total, 2);
        assert!(!r.reach_walk_capped, "{:?}", r.notes);
        assert_eq!(r.unindexed_files, no_files());
        assert_eq!(r.no_symbol_files, no_files());
        assert_eq!(r.indexed_test_files, 3);
        assert_eq!(r.unmodelled, 0);
        assert!(
            reach_note(&r).contains("2 reachable test file(s)"),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn changed_tests_draw_from_the_pool_in_request_order() {
        let db = graph_with_hop_tests();
        let base = add_file(&db, "tests/TestCase.php");
        db.insert_symbols(base, &[symbol("TestCase", "tests/TestCase.php")])
            .unwrap();
        add_test_calling(&db, "tests/Unit/AlphaTest.php", &["TestCase"]);

        // Repository.php costs 3 steps, TestCase.php costs 1 (one caller
        // file, unique name). A pool of 3 with a floor of 3 lets the first
        // input spend everything; the changed test after it finds the pool
        // empty and lands in `unwalked_files` like any later input.
        let opts = ReachabilityOptions {
            work_budget: 3,
            min_input_budget: 3,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php", "tests/TestCase.php"], &opts);
        assert_eq!(r.primary, files(&["tests/TestCase.php"]));
        assert_eq!(
            shape(&r),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/ServiceRunTest.php", 2, 1, "Repository.php"),
            ]
        );
        assert_eq!(r.partial_files, no_files());
        assert_eq!(r.unwalked_files, files(&["tests/TestCase.php"]));
        assert!(r.reach_walk_capped);

        // In the other order the changed test spends 1, and Repository.php
        // gets the remaining 2: enough for `find` (2 caller files), not for
        // `run` behind it.
        let r = recs_with(&db, &["tests/TestCase.php", "Repository.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![
                ("tests/RepositoryFindTest.php", 1, 1, "Repository.php"),
                ("tests/Unit/AlphaTest.php", 1, 1, "tests/TestCase.php"),
            ]
        );
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, files(&["Repository.php"]));
        assert!(r.reach_walk_capped);
    }

    #[test]
    fn skipped_inputs_are_reported_as_the_caller_wrote_them() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        let opts = ReachabilityOptions {
            project_root: Some(PathBuf::from("/proj")),
            ..unlimited()
        };

        // `/proj/..` normalizes to `..`, which no caller would recognise;
        // `./src/New.php` is looked up as `src/New.php` but named as given.
        let r = recs_with(
            &db,
            &["/proj/..", "./src/New.php", "./Repository.php"],
            &opts,
        );
        assert_eq!(r.unsupported_files, files(&["/proj/.."]));
        assert_eq!(r.unindexed_files, files(&["./src/New.php"]));
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        let note = reach_note(&r);
        assert!(note.contains("not indexed: ./src/New.php"), "{note}");
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("no parser for their type: /proj/..")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn blank_input_is_ignored_and_never_called_the_project_root() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);

        let r = recs(&db, &["", "  ", "Repository.php"]);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert_eq!(r.unsupported_files, no_files());
        assert_eq!(r.unindexed_files, no_files());
        assert!(!r.reach_walk_capped);
        assert!(
            r.notes.iter().any(|n| n == "2 empty input(s) ignored"),
            "notes: {:?}",
            r.notes
        );
        assert!(
            !r.notes.iter().any(|n| n.contains("project root")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn a_request_of_only_ignored_inputs_returns_just_that_note() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        let opts = ReachabilityOptions {
            project_root: Some(PathBuf::from("/proj")),
            ..unlimited()
        };

        for (inputs, expected) in [
            (
                vec!["./"],
                vec!["1 input(s) ignored: the project root names no file"],
            ),
            (vec![""], vec!["1 empty input(s) ignored"]),
            (
                vec!["", ".", "/proj/"],
                vec![
                    "1 empty input(s) ignored",
                    "2 input(s) ignored: the project root names no file",
                ],
            ),
        ] {
            let r = recs_with(&db, &inputs, &opts);
            assert_eq!(r.notes, files(&expected), "{inputs:?}");
            assert!(r.primary.is_empty(), "{inputs:?}");
            assert!(r.reachable.is_empty(), "{inputs:?}");
            assert_eq!(r.reachable_total, 0, "{inputs:?}");
            assert!(!r.reach_walk_capped, "{inputs:?}");
            assert_eq!(r.unmodelled, 0, "{inputs:?}");
            assert_eq!(r.indexed_test_files, 0, "{inputs:?}");
        }
    }

    #[test]
    fn test_shaped_data_files_are_unsupported_not_tests() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");

        // `tests/` in the path does not make a JSON fixture a test to run,
        // and its absence from the index says nothing about the walk.
        let r = recs(
            &db,
            &[
                "tests/Integration/Translation/lang/en.json",
                "Repository.php",
            ],
        );
        assert_eq!(
            r.unsupported_files,
            files(&["tests/Integration/Translation/lang/en.json"])
        );
        assert_eq!(r.primary, no_files());
        assert_eq!(r.unindexed_files, no_files());
        assert!(!r.reach_walk_capped);
        assert!(
            reach_note(&r).contains("1 of 2 indexed test files have no resolved edge"),
            "notes: {:?}",
            r.notes
        );
        assert!(
            !r.notes.iter().any(|n| n.contains("tests themselves")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn phpt_is_a_runnable_test_without_a_grammar() {
        let db = graph();
        add_file(&db, "ext/foo/tests/bar.phpt");

        // Indexed or not, a `.phpt` has no grammar: nothing to walk, and
        // its absence from the index is permanent, not pending.
        for db in [&db, &graph()] {
            let r = recs(db, &["ext/foo/tests/bar.phpt", "Repository.php"]);
            assert_eq!(r.primary, files(&["ext/foo/tests/bar.phpt"]));
            assert_eq!(r.unsupported_files, no_files());
            assert_eq!(r.unindexed_files, no_files());
            assert_eq!(r.unwalked_files, no_files());
            assert_eq!(r.no_symbol_files, no_files());
            assert!(!r.reach_walk_capped, "{:?}", r.notes);
            assert!(
                r.notes.iter().any(|n| n
                    == "1 changed file(s) are tests themselves; listed in `primary` \
                        (not parsed by the indexer: ext/foo/tests/bar.phpt)"),
                "notes: {:?}",
                r.notes
            );
        }
    }

    #[test]
    fn the_project_root_itself_is_dropped_with_a_note() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        let opts = ReachabilityOptions {
            project_root: Some(PathBuf::from("/proj")),
            ..unlimited()
        };

        for root_form in ["./", ".", "/proj", "/proj/"] {
            let r = recs_with(&db, &[root_form, "Repository.php"], &opts);
            assert_eq!(r.unsupported_files, no_files(), "{root_form}");
            assert_eq!(r.unindexed_files, no_files(), "{root_form}");
            assert!(!r.reach_walk_capped, "{root_form}");
            assert_eq!(
                shape(&r),
                vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")],
                "{root_form}"
            );
            assert!(
                r.notes
                    .iter()
                    .any(|n| n.contains("1 input(s) ignored: the project root names no file")),
                "{root_form}: {:?}",
                r.notes
            );
        }
    }

    #[test]
    fn root_with_trailing_slash_and_subdirectory_root_behave_as_documented() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);

        // Trailing slash on the root is irrelevant.
        let slashed = ReachabilityOptions {
            project_root: Some(PathBuf::from("/proj/")),
            ..unlimited()
        };
        let r = recs_with(&db, &["/proj/Repository.php"], &slashed);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert!(!r.reach_walk_capped);

        // A root that is a subdirectory of the index root cannot relativize
        // an absolute path outside it; the path stays absolute and is
        // reported as unindexed. The MCP layer resolves the true root
        // before calling, so this only reaches a caller that passed the
        // wrong root directly.
        let subdir = ReachabilityOptions {
            project_root: Some(PathBuf::from("/proj/src")),
            ..unlimited()
        };
        let r = recs_with(&db, &["/proj/Repository.php"], &subdir);
        assert_eq!(r.unindexed_files, files(&["/proj/Repository.php"]));
        assert!(r.reach_walk_capped);
        assert!(r.reachable.is_empty());
    }

    #[test]
    fn capped_walk_never_emits_the_add_tests_absence_note() {
        let db = graph();
        add_file(&db, "tests/UnrelatedTest.php");

        // A zero budget means an empty pool: the input is never walked.
        let opts = ReachabilityOptions {
            work_budget: 0,
            ..unlimited()
        };
        let r = recs_with(&db, &["Repository.php"], &opts);
        assert!(r.primary.is_empty() && r.coupled.is_empty() && r.reachable.is_empty());
        assert!(r.reach_walk_capped);
        assert_eq!(r.unwalked_files, files(&["Repository.php"]));
        assert_eq!(r.partial_files, no_files());
        assert!(
            !r.notes.iter().any(|n| n.contains("no test files found")),
            "an incomplete search must not claim absence: {:?}",
            r.notes
        );
        assert!(
            r.notes.iter().any(|n| n.contains("lower bound")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn unmodelled_counts_indexed_tests_with_no_edge_into_the_changed_set() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");
        add_file(&db, "tests/OtherTest.php");
        add_file(&db, "Helper.php");

        let r = recs(&db, &["Repository.php"]);
        assert_eq!(r.reachable.len(), 1);
        assert_eq!(r.unmodelled, 2);
        assert_eq!(r.indexed_test_files, 3);
        assert!(
            reach_note(&r).contains("2 of 3 indexed test files have no resolved edge"),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn index_without_test_files_skips_the_walk_and_blames_the_index() {
        let db = graph();
        let opts = ReachabilityOptions {
            work_budget: 0,
            ..unlimited()
        };
        // A zero budget would cap any walk that ran; none runs.
        let r = recs_with(&db, &["Repository.php"], &opts);
        assert_eq!(r.indexed_test_files, 0);
        assert!(!r.reach_walk_capped);
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, no_files());
        assert!(
            r.notes.iter().any(|n| n
                .contains("the index contains no test files (check `[index] exclude_patterns`)")),
            "notes: {:?}",
            r.notes
        );
        assert!(
            !r.notes
                .iter()
                .any(|n| n.contains("add tests for these files")),
            "absence advice is misdirected when the index has no tests: {:?}",
            r.notes
        );
    }

    #[test]
    fn c_sibling_counts_as_an_indexed_test_even_though_classify_says_source() {
        let db = Database::open_in_memory().unwrap();
        let foo = add_file(&db, "src/foo.c");
        db.insert_symbols(foo, &[symbol("foo", "src/foo.c")])
            .unwrap();
        add_file(&db, "src/foo_test.c");
        assert_eq!(
            FileCategory::classify("src/foo_test.c"),
            FileCategory::Source,
            "premise: the category rule does not know the C affix"
        );

        let r = recs(&db, &["src/foo.c"]);
        assert_eq!(r.primary, files(&["src/foo_test.c"]));
        assert_eq!(r.indexed_test_files, 1);
        assert_eq!(r.unmodelled, 0);
        assert!(!r.reach_walk_capped, "{:?}", r.notes);
        assert!(
            r.notes.iter().any(|n| n.contains("1 sibling test file(s)")),
            "notes: {:?}",
            r.notes
        );
        assert!(
            !r.notes.iter().any(|n| n.contains("contains no test files")),
            "a resolved sibling contradicts 'no test files': {:?}",
            r.notes
        );
    }

    #[test]
    fn fixture_paths_are_neither_primary_nor_counted() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "crates/parser/tests/fixtures/sample.php");
        add_file(&db, "tests/fixtures/other.php");

        let r = recs(
            &db,
            &["Repository.php", "crates/parser/tests/fixtures/sample.php"],
        );
        assert!(r.primary.is_empty(), "primary: {:?}", r.primary);
        assert!(
            !r.notes.iter().any(|n| n.contains("tests themselves")),
            "notes: {:?}",
            r.notes
        );
        // Neither fixture counts toward the denominator. The fixture input is
        // an ordinary indexed file to the walk, and it defines nothing.
        assert_eq!(r.indexed_test_files, 1);
        assert_eq!(r.unmodelled, 0);
        assert_eq!(
            r.no_symbol_files,
            files(&["crates/parser/tests/fixtures/sample.php"])
        );
    }

    #[test]
    fn withheld_phpt_siblings_are_vouched_not_unmodelled() {
        let db = Database::open_in_memory().unwrap();
        add_file(&db, "ext/foo/foo.c");
        for i in 0..(PHPT_LIST_CAP + 1) {
            add_file(&db, &format!("ext/foo/tests/case{i:03}.phpt"));
        }
        add_file(&db, "ext/bar/tests/other.phpt");

        let r = recs(&db, &["ext/foo/foo.c"]);
        assert!(r.primary.is_empty(), "primary: {:?}", r.primary);
        assert_eq!(r.indexed_test_files, PHPT_LIST_CAP + 2);
        assert_eq!(r.unmodelled, 1);
        assert!(
            r.notes.iter().any(|n| n.contains("holds more than")),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn changed_test_file_lands_in_primary_and_nowhere_else() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        let test_id = db
            .file_id_for_path("tests/RepositoryFindTest.php")
            .unwrap()
            .unwrap();
        db.insert_symbols(
            test_id,
            &[symbol("RepositoryFindTest", "tests/RepositoryFindTest.php")],
        )
        .unwrap();

        // The changed test is walked like any input: its class is reached
        // by nothing, which is a complete answer, and it never lists itself.
        let r = recs(&db, &["Repository.php", "tests/RepositoryFindTest.php"]);
        assert_eq!(r.primary, files(&["tests/RepositoryFindTest.php"]));
        assert!(r.reachable.is_empty(), "reachable: {:?}", r.reachable);
        assert!(!r.reach_walk_capped, "{:?}", r.notes);
        assert_eq!(r.no_symbol_files, no_files());
        assert_eq!(r.unmodelled, 0);
        assert_eq!(r.indexed_test_files, 1);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("1 changed file(s) are tests themselves")),
            "notes: {:?}",
            r.notes
        );
        assert!(
            !r.notes.iter().any(|n| n.contains("sibling test file(s)")),
            "changed tests are not sibling matches: {:?}",
            r.notes
        );
    }

    #[test]
    fn a_request_of_only_unsupported_inputs_emits_just_the_skip_note() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/UnrelatedTest.php");

        // Nothing was walked and nothing could have been: no absence advice,
        // no unmodelled count, no cap. The two indexed tests are not
        // "not vouched for"; they were never asked about.
        let r = recs(&db, &["CHANGELOG.md", "composer.json"]);
        assert_eq!(
            r.notes,
            files(&["2 file(s) skipped, no parser for their type: CHANGELOG.md, composer.json"])
        );
        assert_eq!(
            r.unsupported_files,
            files(&["CHANGELOG.md", "composer.json"])
        );
        assert!(r.primary.is_empty() && r.coupled.is_empty() && r.reachable.is_empty());
        assert!(!r.reach_walk_capped);
        assert_eq!(r.unmodelled, 0);
        assert_eq!(r.indexed_test_files, 0);

        // An unindexed input alongside keeps the lower-bound disclosure: its
        // absence from the index is a finding about the walk, not a skip.
        let r = recs(&db, &["CHANGELOG.md", "src/New.php"]);
        assert_eq!(r.unindexed_files, files(&["src/New.php"]));
        assert!(r.reach_walk_capped);
        assert!(
            reach_note(&r).contains("not indexed: src/New.php"),
            "notes: {:?}",
            r.notes
        );
    }

    #[test]
    fn two_spellings_of_one_path_are_one_input() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);

        // Budget 3 covers exactly one walk of Repository.php; a second walk
        // would find the pool empty and land in `unwalked_files`.
        let opts = ReachabilityOptions {
            work_budget: 3,
            min_input_budget: 3,
            ..unlimited()
        };
        let r = recs_with(&db, &["./Repository.php", "Repository.php"], &opts);
        assert_eq!(
            shape(&r),
            vec![("tests/RepositoryFindTest.php", 1, 1, "Repository.php")]
        );
        assert!(!r.reach_walk_capped, "{:?}", r.notes);
        assert_eq!(r.unwalked_files, no_files());
        assert!(
            r.notes
                .iter()
                .any(|n| n == "1 duplicate input(s) ignored: another spelling of the same path"),
            "notes: {:?}",
            r.notes
        );

        // The first spelling is the one reported; a duplicate is listed once.
        let r = recs_with(&db, &["./src/New.php", "src/New.php"], &opts);
        assert_eq!(r.unindexed_files, files(&["./src/New.php"]));
    }

    #[test]
    fn a_changed_test_without_symbols_is_walked_and_disclosed_as_no_symbol() {
        let db = graph();
        add_test_calling(&db, "tests/RepositoryFindTest.php", &["find"]);
        add_file(&db, "tests/EmptyTest.php");

        // The chosen trade-off: an indexed test that defines nothing is
        // still listed in `primary` (it is runnable), but its walk has no
        // seed, so it lands in `no_symbol_files` and caps the answer like
        // any other symbol-free input. Nothing can be said about what
        // extends or calls it.
        let r = recs(&db, &["tests/EmptyTest.php"]);
        assert_eq!(r.primary, files(&["tests/EmptyTest.php"]));
        assert_eq!(r.no_symbol_files, files(&["tests/EmptyTest.php"]));
        assert!(r.reach_walk_capped);
        assert_eq!(r.unwalked_files, no_files());
        assert_eq!(r.partial_files, no_files());
        assert_eq!(r.unindexed_files, no_files());
        assert!(r.reachable.is_empty());
        assert!(
            reach_note(&r).contains("lower bound (no symbols indexed: tests/EmptyTest.php)"),
            "notes: {:?}",
            r.notes
        );
    }
}
