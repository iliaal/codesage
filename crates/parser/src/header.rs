use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, ensure};
use codesage_protocol::{FileInfo, Language};

use crate::detect::is_unambiguous_cpp_extension;
use crate::parse::parse_file_tolerant;
use crate::preproc::DeadRegions;
use crate::source::read_indexable_file;

/// One-hop evidence only: a C-compatible header included from C++ does not
/// turn its own include tree into C++. Missing build include paths are resolved
/// only when the indexed suffix is unique. For C-degraded ambiguous headers,
/// retain the legacy project dialect, also for otherwise clean C trees with
/// macro calls in function declarators. Error counts alone do not establish
/// that C preserves the symbols and ranges recovered by the C++ grammar.
pub fn cpp_headers(root: &Path, files: &[FileInfo]) -> Result<HashSet<String>> {
    cpp_headers_with_context(root, files, &[])
}

/// Reuse unchanged header decisions only when the admitted header set and all
/// C++ evidence are unchanged. Callers must check structural interpretation
/// before supplying persisted decisions.
pub fn cpp_headers_with_context(
    root: &Path,
    files: &[FileInfo],
    previous: &[FileInfo],
) -> Result<HashSet<String>> {
    let headers: HashSet<_> = files
        .iter()
        .filter(|file| file.path.ends_with(".h"))
        .map(|file| file.path.as_str())
        .collect();
    let mut cpp = HashSet::new();
    if headers.is_empty() {
        return Ok(cpp);
    }
    let prior_headers: HashMap<_, _> = previous
        .iter()
        .filter(|file| file.path.ends_with(".h"))
        .map(|file| (file.path.as_str(), file))
        .collect();
    let cpp_evidence = |files: &[FileInfo]| {
        files
            .iter()
            .filter(|file| is_cpp_evidence(Path::new(&file.path)))
            .map(|file| (file.path.clone(), file.content_hash.clone()))
            .collect::<HashMap<_, _>>()
    };
    let stable_evidence = headers.len() == prior_headers.len()
        && headers
            .iter()
            .all(|header| prior_headers.contains_key(header))
        && cpp_evidence(files) == cpp_evidence(previous);
    let reusable: HashMap<_, _> = files
        .iter()
        .filter(|file| stable_evidence && file.path.ends_with(".h"))
        .filter_map(|file| {
            prior_headers.get(file.path.as_str()).and_then(|prior| {
                (prior.content_hash == file.content_hash)
                    .then_some((file.path.as_str(), prior.language))
            })
        })
        .collect();
    cpp.extend(
        reusable
            .iter()
            .filter(|(_, language)| **language == Language::Cpp)
            .map(|(path, _)| (*path).to_owned()),
    );
    let cpp_sources: Vec<_> = files
        .iter()
        .filter(|file| is_cpp_evidence(Path::new(&file.path)))
        .collect();
    let cpp_directories: HashSet<_> = cpp_sources
        .iter()
        .filter_map(|file| Path::new(&file.path).parent())
        .collect();
    cpp.extend(
        headers
            .iter()
            .filter(|header| {
                Path::new(header)
                    .parent()
                    .is_some_and(|parent| cpp_directories.contains(parent))
            })
            .map(|header| (*header).to_owned()),
    );
    let mut pending: HashSet<_> = headers
        .iter()
        .copied()
        .filter(|header| !cpp.contains(*header) && !reusable.contains_key(header))
        .collect();
    for file in &cpp_sources {
        codesage_protocol::work::checkpoint()?;
        let parent = Path::new(&file.path).parent().unwrap_or(Path::new(""));
        let Some(source) = read_indexable_file(root, Path::new(&file.path))? else {
            continue;
        };
        ensure!(
            file.content_hash.is_empty() || source.content_hash == file.content_hash,
            "{} changed while establishing header routing; retry discovery",
            file.path
        );
        if pending.is_empty() {
            continue;
        }
        let directives = crate::export_macro::directives_only(&source.bytes);
        let parsed = parse_file_tolerant(&directives, Language::C)?;
        let dead = DeadRegions::scan(parsed.tree.root_node(), &source.bytes, Language::Cpp);
        let mut cursor = parsed.tree.walk();
        loop {
            codesage_protocol::work::checkpoint()?;
            let node = cursor.node();
            if node.kind() == "preproc_include"
                && !dead.covers(&node)
                && let Some(path) = node.child_by_field_name("path")
                && matches!(path.kind(), "string_literal" | "system_lib_string")
                && let Ok(text) = path.utf8_text(&source.bytes)
                && text.len() >= 2
            {
                let include = &text[1..text.len() - 1];
                let quoted = path.kind() == "string_literal";
                if let Some(header) = resolve_include(parent, include, quoted, &headers) {
                    cpp.insert(header.to_owned());
                    pending.remove(header);
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node() == parsed.tree.root_node() {
                break;
            }
        }
    }
    if !cpp_sources.is_empty() {
        let candidates: Vec<_> = files
            .iter()
            .filter(|file| file.path.ends_with(".h"))
            .filter(|file| {
                !cpp.contains(file.path.as_str()) && !reusable.contains_key(file.path.as_str())
            })
            .collect();
        cpp.extend(probe_headers(root, &candidates)?);
    }
    Ok(cpp)
}

/// Upper bound on header probe workers per call. Each concurrent daemon
/// overlay refresh spawns its own probe workers, so the per-call count stays
/// at the `.min(4)` that `verify_membership` uses for its walk.
const MAX_HEADER_PROBE_WORKERS: usize = 4;

/// Workers for one probe call: the available parallelism, capped at
/// `MAX_HEADER_PROBE_WORKERS` and at the number of candidates.
fn probe_worker_count(available: usize, candidates: usize) -> usize {
    available.min(MAX_HEADER_PROBE_WORKERS).min(candidates)
}

/// The headers among `candidates` whose C parse routes them to C++, in
/// `candidates` order. Each header is probed independently, so up to
/// `MAX_HEADER_PROBE_WORKERS` scoped workers share the list, largest file first
/// so one large header does not start last; each worker enters the caller's
/// work control. Every candidate is probed and results are read back in
/// `candidates` order, so the first failing header in that order supplies the
/// error, as in a serial loop.
fn probe_headers(root: &Path, candidates: &[&FileInfo]) -> Result<Vec<String>> {
    let probe = |file: &FileInfo| -> Result<bool> {
        codesage_protocol::work::checkpoint()?;
        let header = file.path.as_str();
        let Some(source) = read_indexable_file(root, Path::new(header))? else {
            return Ok(false);
        };
        ensure!(
            file.content_hash.is_empty() || file.content_hash == source.content_hash,
            "{header} changed while establishing header routing; retry discovery"
        );
        Ok(!crate::parse::c_header_prefers_c(&source.bytes)?)
    };
    let workers = probe_worker_count(
        std::thread::available_parallelism().map_or(1, usize::from),
        candidates.len(),
    );
    let results: Vec<Result<bool>> = if workers <= 1 {
        candidates.iter().map(|file| probe(file)).collect()
    } else {
        // Scheduling order only; a failed stat just sorts the header last.
        let mut order: Vec<(u64, usize)> = candidates
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let size = std::fs::metadata(root.join(&file.path)).map_or(0, |meta| meta.len());
                (size, index)
            })
            .collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        let next = AtomicUsize::new(0);
        let control = codesage_protocol::work::current();
        let mut slots: Vec<Option<Result<bool>>> = Vec::new();
        slots.resize_with(candidates.len(), || None);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let _scope = control.as_ref().map(|control| control.enter());
                        let mut done = Vec::new();
                        while let Some(&(_, index)) =
                            order.get(next.fetch_add(1, Ordering::Relaxed))
                        {
                            done.push((index, probe(candidates[index])));
                        }
                        done
                    })
                })
                .collect();
            for handle in handles {
                let done = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                for (index, result) in done {
                    slots[index] = Some(result);
                }
            }
        });
        slots
            .into_iter()
            .map(|slot| slot.expect("every header index is claimed exactly once"))
            .collect()
    };
    let mut routed = Vec::new();
    for (file, result) in candidates.iter().zip(results) {
        if result? {
            routed.push(file.path.clone());
        }
    }
    Ok(routed)
}

pub fn is_cpp_evidence(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(is_unambiguous_cpp_extension)
}

pub(crate) fn unchanged_context(
    files: &[FileInfo],
    previous: &[FileInfo],
) -> Option<HashSet<String>> {
    if previous.is_empty() {
        return None;
    }
    let relevant =
        |file: &&FileInfo| file.path.ends_with(".h") || is_cpp_evidence(Path::new(&file.path));
    let current: HashMap<_, _> = files
        .iter()
        .filter(relevant)
        .map(|file| (file.path.as_str(), file.content_hash.as_str()))
        .collect();
    let prior: HashMap<_, _> = previous
        .iter()
        .filter(relevant)
        .map(|file| (file.path.as_str(), file.content_hash.as_str()))
        .collect();
    (current == prior).then(|| {
        previous
            .iter()
            .filter(|file| file.path.ends_with(".h") && file.language == Language::Cpp)
            .map(|file| file.path.clone())
            .collect()
    })
}

/// Overlays cannot add files. New header-routing evidence requires persistent
/// discovery so suffix ambiguity and the admissible source set stay identical.
pub fn verify_membership(root: &Path, files: &[FileInfo], excludes: &[String]) -> Result<()> {
    let known: HashSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    let observed = Mutex::new(HashSet::new());
    let excludes = crate::discover::build_exclude_set(excludes)?;
    let root_owned = root.to_path_buf();
    let mut walk = ignore::WalkBuilder::new(root);
    walk.threads(
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4),
    )
    .hidden(true)
    .git_ignore(true)
    .filter_entry(move |entry| {
        let Ok(relative) = entry.path().strip_prefix(&root_owned) else {
            return false;
        };
        relative.as_os_str().is_empty()
            || !crate::discover::exclude_matches_path(
                &excludes,
                &relative.to_string_lossy(),
                entry.file_type().is_some_and(|kind| kind.is_dir()),
            )
    });
    let context_entries = AtomicUsize::new(0);
    let first_error = Mutex::new(None);
    let control = codesage_protocol::work::current();
    walk.build_parallel().run(|| {
        Box::new(|entry| {
            let result = (|| -> Result<()> {
                // The caller's thread-local deadline is not inherited by walk workers.
                if let Some(control) = &control {
                    control.check()?;
                }
                let entry = entry?;
                let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
                if !is_directory {
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        return Ok(());
                    }
                    let path = entry.path();
                    if !is_cpp_evidence(path) && path.extension().is_none_or(|ext| ext != "h") {
                        return Ok(());
                    }
                    if entry.metadata()?.len() > crate::discover::MAX_INDEXABLE_FILE_BYTES {
                        return Ok(());
                    }
                }
                ensure!(
                    context_entries.fetch_add(1, Ordering::Relaxed) < 16_384,
                    "header dialect context exceeds 16384 routing candidates and directories; run codesage index"
                );
                if !is_directory {
                    let relative = entry.path().strip_prefix(root)?.to_string_lossy();
                    ensure!(
                        known.contains(relative.as_ref()),
                        "header dialect requires codesage index: new candidate {relative}"
                    );
                    observed.lock().unwrap().insert(relative.into_owned());
                }
                Ok(())
            })();
            match result {
                Ok(()) => ignore::WalkState::Continue,
                Err(error) => {
                    first_error.lock().unwrap().get_or_insert(error);
                    ignore::WalkState::Quit
                }
            }
        })
    });
    if let Some(error) = first_error.into_inner().unwrap() {
        return Err(error);
    }
    codesage_protocol::work::checkpoint()?;
    let observed = observed.into_inner().unwrap();
    for file in files
        .iter()
        .filter(|file| file.path.ends_with(".h") || is_cpp_evidence(Path::new(&file.path)))
    {
        if !observed.contains(&file.path) && root.join(&file.path).try_exists()? {
            anyhow::bail!(
                "header dialect requires codesage index: candidate {} is no longer admitted",
                file.path
            );
        }
    }
    Ok(())
}

fn resolve_include<'a>(
    parent: &Path,
    include: &str,
    quoted: bool,
    headers: &HashSet<&'a str>,
) -> Option<&'a str> {
    if quoted
        && let Some(relative) = normalize(&parent.join(include))
        && let Some(header) = headers.get(relative.to_str()?)
    {
        return Some(header);
    }
    let normalized = normalize(Path::new(include))?;
    let normalized = normalized.to_str()?;
    if let Some(header) = headers.get(normalized) {
        return Some(header);
    }
    let suffix = format!("/{normalized}");
    let mut candidates = headers.iter().filter(|header| header.ends_with(&suffix));
    let first = *candidates.next()?;
    candidates.next().is_none().then_some(first)
}

fn normalize(path: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::{MAX_HEADER_PROBE_WORKERS, probe_headers, probe_worker_count};
    use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};
    use codesage_protocol::{FileInfo, Language};

    fn header(root: &std::path::Path, path: &str, body: &str, content_hash: &str) -> FileInfo {
        std::fs::write(root.join(path), body).unwrap();
        FileInfo {
            path: path.to_owned(),
            language: Language::C,
            content_hash: content_hash.to_owned(),
            is_test: false,
        }
    }

    #[test]
    fn probe_worker_count_never_exceeds_the_cap() {
        assert_eq!(MAX_HEADER_PROBE_WORKERS, 4);
        for available in [1, 2, 4, 5, 8, 64, usize::MAX] {
            for candidates in [0, 1, 2, 3, 4, 5, 240, usize::MAX] {
                assert_eq!(
                    probe_worker_count(available, candidates),
                    available.min(candidates).min(4),
                    "available={available} candidates={candidates}"
                );
            }
        }
    }

    #[test]
    fn probe_workers_see_the_callers_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..8)
            .map(|n| header(root.path(), &format!("h{n}.h"), "int clean(void);\n", ""))
            .collect();
        let candidates: Vec<_> = files.iter().collect();
        assert!(probe_headers(root.path(), &candidates).unwrap().is_empty());

        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let _scope = control.enter();
        let error = probe_headers(root.path(), &candidates).unwrap_err();
        assert_eq!(
            error.downcast_ref::<WorkStopped>().map(|stop| stop.reason),
            Some(StopReason::ClientCancelled),
            "{error:#}"
        );
    }

    /// Differential against the serial loop this replaced: probing every
    /// candidate in order with `c_header_prefers_c` must route the same
    /// headers, over generated C, C++ and ambiguous headers of mixed sizes.
    #[test]
    fn parallel_probe_routes_the_same_headers_as_a_serial_probe() {
        const FRAGMENTS: [&str; 8] = [
            "int plain(int x);\n",
            "typedef struct point { int x, y; } point;\n",
            "class Widget { public: int x; };\n",
            "namespace ns { int f(); }\n",
            "template <typename T> T id(T value);\n",
            "RULE(EQ(A, B))\n{ emit(); }\n",
            "#define MAX(a, b) ((a) > (b) ? (a) : (b))\n",
            "extern \"C\" { int c_api(void); }\n",
        ];
        let root = tempfile::tempdir().unwrap();
        let mut state = 0x2545_f491_u32;
        let files: Vec<_> = (0..240)
            .map(|n| {
                let mut body = String::new();
                for _ in 0..1 + n % 7 {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    body.push_str(
                        &FRAGMENTS[(state >> 24) as usize % FRAGMENTS.len()].repeat(1 + n % 13),
                    );
                }
                header(root.path(), &format!("h{n}.h"), &body, "")
            })
            .collect();
        let candidates: Vec<_> = files.iter().collect();
        let serial: Vec<_> = files
            .iter()
            .filter(|file| {
                let bytes = std::fs::read(root.path().join(&file.path)).unwrap();
                !crate::parse::c_header_prefers_c(&bytes).unwrap()
            })
            .map(|file| file.path.clone())
            .collect();
        assert!(
            serial.len() > 20 && serial.len() < files.len() - 20,
            "{}",
            serial.len()
        );
        assert_eq!(probe_headers(root.path(), &candidates).unwrap(), serial);
    }

    #[test]
    fn the_first_failing_candidate_supplies_the_error() {
        let root = tempfile::tempdir().unwrap();
        // The second header is larger, so it is probed first.
        let first = header(root.path(), "first.h", "int a(void);\n", "stale");
        let second = header(
            root.path(),
            "second.h",
            &"int b(void);\n".repeat(4096),
            "stale",
        );
        let error = probe_headers(root.path(), &[&first, &second]).unwrap_err();
        assert!(
            error.to_string().starts_with("first.h changed"),
            "{error:#}"
        );
        let error = probe_headers(root.path(), &[&second, &first]).unwrap_err();
        assert!(
            error.to_string().starts_with("second.h changed"),
            "{error:#}"
        );
    }
}
