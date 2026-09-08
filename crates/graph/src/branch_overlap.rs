use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use codesage_protocol::{BranchOverlap, BranchOverlapReport, ReviewObjection, ReviewSeverity};

const MAX_REFS: usize = 50;
const MAX_ROWS: usize = 3;
const MAX_FILES: usize = 5;
const MAX_GIT_BYTES: usize = 4 * 1024 * 1024;

pub fn branch_overlap(root: &Path, files: &[String], budget: Duration) -> BranchOverlapReport {
    let mut report = BranchOverlapReport::default();
    let deadline = Instant::now() + budget;
    if let Err(error) = scan(root, files, deadline, &mut report) {
        report.note = Some(format!("Branch overlap incomplete: {error:#}"));
    }
    report
}

fn scan(
    root: &Path,
    files: &[String],
    deadline: Instant,
    report: &mut BranchOverlapReport,
) -> Result<()> {
    let head = git(root, &["rev-parse", "--verify", "HEAD^{commit}"], deadline)?;
    let head = std::str::from_utf8(&head)?.trim();
    report.current_commit = Some(head.to_string());
    let raw = git(
        root,
        &[
            "for-each-ref",
            "--sort=refname",
            "--sort=-committerdate",
            "--format=%(objectname)%09%(refname)%09%(symref)",
            "refs/heads/",
            "refs/remotes/",
        ],
        deadline,
    )?;
    let raw = std::str::from_utf8(&raw)?;
    let refs: Vec<_> = raw
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let sha = parts.next()?;
            let name = parts.next()?;
            let symbolic = parts.next()?;
            (symbolic.is_empty() && sha != head).then_some((sha, name))
        })
        .collect();
    report.total_refs = Some(refs.len());
    let targets: BTreeSet<_> = files
        .iter()
        .filter(|file| !noise_path(file))
        .map(String::as_str)
        .collect();
    let mut seen = BTreeSet::new();
    for (sha, branch) in refs.iter().take(MAX_REFS) {
        if Instant::now() >= deadline {
            bail!("time budget exhausted");
        }
        if !seen.insert(*sha) {
            report.scanned_refs += 1;
            continue;
        }
        let bases = git(root, &["merge-base", "--all", head, sha], deadline)?;
        let bases = std::str::from_utf8(&bases)?;
        let bases: Vec<_> = bases.lines().collect();
        if bases.len() != 1 {
            report.note = Some("Some refs have no unique merge base and were skipped.".to_string());
            report.scanned_refs += 1;
            continue;
        }
        let base = bases[0];
        if base == head || base == *sha {
            report.scanned_refs += 1;
            continue;
        }
        let range = format!("{head}...{sha}");
        let diff = git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--relative",
                "--name-only",
                "-z",
                &range,
                "--",
            ],
            deadline,
        )?;
        let mut matching = BTreeSet::new();
        for path in diff
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = std::str::from_utf8(path).context("non-UTF-8 Git path")?;
            if targets.contains(path) {
                matching.insert(path.to_string());
            }
        }
        report.scanned_refs += 1;
        if !matching.is_empty() {
            report.matching_branches += 1;
            if report.branches.len() < MAX_ROWS {
                report.branches.push(BranchOverlap {
                    branch: branch.to_string(),
                    commit: sha.to_string(),
                    merge_base: base.to_string(),
                    files_total: matching.len(),
                    files: matching.into_iter().take(MAX_FILES).collect(),
                    basis: "same-file: HEAD...branch (three-dot diff from merge base)".to_string(),
                });
            }
        }
    }
    report.complete = report.scanned_refs == refs.len() && report.note.is_none();
    if report.scanned_refs < refs.len() {
        let note = report.note.get_or_insert_with(String::new);
        if !note.is_empty() {
            note.push(' ');
        }
        note.push_str("Only the newest 50 refs were considered.");
    }
    Ok(())
}

fn noise_path(path: &str) -> bool {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if matches!(
        name,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "bun.lockb"
            | "composer.json"
            | "composer.lock"
            | "go.mod"
            | "go.sum"
            | "pyproject.toml"
            | "poetry.lock"
            | "uv.lock"
            | "Pipfile"
            | "Pipfile.lock"
            | "requirements.txt"
            | "Gemfile"
            | "Gemfile.lock"
            | "setup.py"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "npm-shrinkwrap.json"
            | ".gitlab-ci.yml"
            | "azure-pipelines.yml"
            | "Jenkinsfile"
            | "CHANGELOG.md"
            | "NEWS"
            | ".DS_Store"
    ) {
        return true;
    }
    if name.to_ascii_lowercase().starts_with("changelog")
        || [
            ".min.js",
            ".min.css",
            ".map",
            ".pb.go",
            "_pb2.py",
            "_pb2_grpc.py",
            ".po",
            ".mo",
            ".prefab",
            ".meta",
            ".asset",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return true;
    }
    path.components().any(|part| {
        matches!(
            part.as_os_str()
                .to_str()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some(
                ".git"
                    | ".github"
                    | ".circleci"
                    | "node_modules"
                    | "vendor"
                    | "target"
                    | "dist"
                    | "build"
                    | "__pycache__"
                    | "generated"
                    | "__generated__"
                    | "locales"
                    | "locale"
                    | "localization"
                    | "i18n"
                    | "translations"
            )
        )
    })
}

fn git(root: &Path, args: &[&str], deadline: Instant) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .args(["--no-pager", "--literal-pathspecs"])
        .args(args);
    run_git(command, args[0], deadline)
}

fn run_git(mut command: Command, operation: &str, deadline: Instant) -> Result<Vec<u8>> {
    ensure!(Instant::now() < deadline, "time budget exhausted");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Lazy-fetch helpers can inherit stdout and outlive the Git leader.
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("starting Git")?;
    let stdout = child.stdout.take().context("opening Git output")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_GIT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let mut reaped = false;
    let result = (|| {
        let bytes = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Git time budget exhausted")??;
        ensure!(bytes.len() <= MAX_GIT_BYTES, "Git output exceeds 4 MiB");
        loop {
            if let Some(status) = child.try_wait()? {
                reaped = true;
                if operation == "merge-base" && status.code() == Some(1) {
                    return Ok(Vec::new());
                }
                ensure!(status.success(), "Git {operation} failed ({status})");
                return Ok(bytes);
            }
            ensure!(Instant::now() < deadline, "Git time budget exhausted");
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    if result.is_err() && !reaped {
        #[cfg(unix)]
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_child(&child),
            rustix::process::Signal::KILL,
        );
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = reader.join();
    result
}

pub fn branch_overlap_summary(report: &BranchOverlapReport) -> String {
    let total = report
        .total_refs
        .map_or_else(|| "unknown".to_string(), |n| n.to_string());
    let mut summary = format!(
        "Branch overlap: {} matching branch(es); scanned {}/{total} refs (local and remote-tracking refs; same-tip matches counted once; not a liveness check).",
        report.matching_branches, report.scanned_refs
    );
    if report.matching_branches > report.branches.len() {
        summary.push_str(&format!(" Showing {} branches.", report.branches.len()));
    }
    if let Some(note) = &report.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

pub fn branch_overlap_objection(report: &BranchOverlapReport) -> Option<ReviewObjection> {
    if report.branches.is_empty() {
        return None;
    }
    let mut evidence = vec![branch_overlap_summary(report)];
    let mut files = BTreeSet::new();
    for row in &report.branches {
        evidence.push(format!(
            "{:?} at {}: {} ({}; merge base {}; showing {} of {} files)",
            row.branch,
            row.commit,
            quoted_paths(&row.files),
            row.basis,
            row.merge_base,
            row.files.len(),
            row.files_total
        ));
        files.extend(row.files.iter().cloned());
    }
    Some(ReviewObjection {
        severity: ReviewSeverity::Low,
        category: "branch-overlap".to_string(),
        title: "Other branches edit the same files; coordinate or rebase before merging"
            .to_string(),
        evidence,
        files: files.into_iter().collect(),
    })
}

pub fn quoted_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| format!("{path:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn timeout_stops_descendants_holding_git_stdout_open() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2 & wait"]);
        let start = Instant::now();
        let error = run_git(command, "probe", start + Duration::from_millis(100)).unwrap_err();
        assert!(format!("{error:#}").contains("budget exhausted"));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "descendant held the stdout pipe open for {:?}",
            start.elapsed()
        );
    }

    fn command(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn fixture() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        command(dir.path(), &["init", "-b", "base"]);
        std::fs::write(dir.path().join("shared.rs"), "base\n").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "base\n").unwrap();
        command(dir.path(), &["add", "."]);
        command(dir.path(), &["commit", "-m", "base"]);
        let base = command(dir.path(), &["rev-parse", "HEAD"]);
        command(dir.path(), &["checkout", "-b", "current"]);
        std::fs::write(dir.path().join("current.rs"), "current\n").unwrap();
        command(dir.path(), &["add", "."]);
        command(dir.path(), &["commit", "-m", "current"]);
        let head = command(dir.path(), &["rev-parse", "HEAD"]);
        (dir, base, head)
    }

    fn side(root: &Path, base: &str, name: &str, files: &[&str]) -> String {
        command(root, &["checkout", "-b", name, base]);
        for file in files {
            std::fs::write(root.join(file), format!("{name}\n")).unwrap();
        }
        command(root, &["add", "."]);
        command(root, &["commit", "-m", name]);
        let sha = command(root, &["rev-parse", "HEAD"]);
        command(root, &["checkout", "current"]);
        sha
    }

    #[test]
    fn three_dot_excludes_stacked_refs_aliases_and_manifest_noise() {
        let (dir, base, head) = fixture();
        let sha = side(
            dir.path(),
            &base,
            "sibling",
            &["shared.rs", "Cargo.toml", "name\nwith-tab\t.rs"],
        );
        command(
            dir.path(),
            &["update-ref", "refs/remotes/origin/sibling", &sha],
        );
        side(dir.path(), &head, "stacked", &["shared.rs"]);
        let report = branch_overlap(
            dir.path(),
            &[
                "shared.rs".into(),
                "current.rs".into(),
                "Cargo.toml".into(),
                "name\nwith-tab\t.rs".into(),
            ],
            Duration::from_secs(5),
        );
        assert!(report.complete, "{report:?}");
        assert_eq!(report.matching_branches, 1);
        assert_eq!(report.scanned_refs, 4);
        assert_eq!(report.total_refs, Some(4));
        assert_eq!(report.branches[0].merge_base, base);
        assert_eq!(
            report.branches[0].files,
            ["name\nwith-tab\t.rs", "shared.rs"]
        );
        assert!(report.branches[0].basis.contains("three-dot"));
        let manifest = branch_overlap(dir.path(), &["Cargo.toml".into()], Duration::from_secs(5));
        assert_eq!(manifest.matching_branches, 0);
        assert!(branch_overlap_objection(&manifest).is_none());
        let db = codesage_storage::Database::open_in_memory().unwrap();
        let indexed =
            crate::build_review_rehearsal(dir.path(), &db, &["shared.rs".into()]).unwrap();
        assert!(
            indexed
                .objections
                .iter()
                .any(|row| row.category == "branch-overlap")
        );
        let unindexed = crate::build_branch_only_rehearsal(dir.path(), &["shared.rs".into()]);
        assert_eq!(unindexed.objections[0].category, "branch-overlap");
        assert!(unindexed.summary_notes[0].contains("did not run"));
    }

    #[test]
    fn row_and_file_caps_preserve_totals() {
        let (dir, base, _) = fixture();
        for i in 0..5 {
            side(
                dir.path(),
                &base,
                &format!("sibling-{i}"),
                &["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"],
            );
        }
        let files = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"].map(String::from);
        let report = branch_overlap(dir.path(), &files, Duration::from_secs(5));
        assert!(report.complete, "{report:?}");
        assert_eq!(report.matching_branches, 5);
        assert_eq!(report.branches.len(), 3);
        assert!(
            report
                .branches
                .iter()
                .all(|row| row.files_total == 6 && row.files.len() == 5)
        );
    }

    #[test]
    fn newest_fifty_ref_limit_and_timeout_disclose_incomplete_evidence() {
        let (dir, base, _) = fixture();
        let side = side(dir.path(), &base, "sibling", &["shared.rs"]);
        for i in 0..60 {
            command(
                dir.path(),
                &["update-ref", &format!("refs/heads/alias-{i:02}"), &side],
            );
        }
        let report = branch_overlap(dir.path(), &["shared.rs".into()], Duration::from_secs(5));
        assert_eq!(report.scanned_refs, 50);
        assert_eq!(report.total_refs, Some(62));
        assert!(!report.complete);
        assert!(report.note.unwrap().contains("newest 50"));
        let expired = branch_overlap(dir.path(), &["shared.rs".into()], Duration::ZERO);
        assert_eq!(expired.scanned_refs, 0);
        assert_eq!(expired.total_refs, None);
        assert!(!expired.complete);
        assert!(expired.note.unwrap().contains("budget exhausted"));
    }

    #[test]
    fn newest_commit_precedes_alphabetically_earlier_refs() {
        let (dir, base, _) = fixture();
        let old = side(dir.path(), &base, "old", &["shared.rs"]);
        for i in 0..60 {
            command(
                dir.path(),
                &["update-ref", &format!("refs/heads/aaa-{i:02}"), &old],
            );
        }
        side(dir.path(), &base, "zzz-newest", &["new.rs"]);
        let tree = command(dir.path(), &["rev-parse", "zzz-newest^{tree}"]);
        let output = Command::new("git")
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_DATE", "2000000000 +0000")
            .args(["commit-tree", &tree, "-p", &base, "-m", "newest"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let sha = String::from_utf8(output.stdout).unwrap();
        command(
            dir.path(),
            &["update-ref", "refs/heads/zzz-newest", sha.trim()],
        );
        let report = branch_overlap(dir.path(), &["new.rs".into()], Duration::from_secs(5));
        assert_eq!(report.scanned_refs, 50);
        assert_eq!(report.branches[0].branch, "refs/heads/zzz-newest");
        assert_eq!(report.branches[0].files, ["new.rs"]);
    }

    #[test]
    fn noise_paths_and_display_controls_do_not_become_evidence() {
        for path in [
            "generated/api.rs",
            "nested/LOCALes/messages.json",
            "bundle.min.js",
            "widget.pb.go",
            "module/pom.xml",
            ".github/workflows/ci.yml",
            "changelog.txt",
            "Gemfile.lock",
            "../escape.rs",
            "/outside.rs",
        ] {
            assert!(noise_path(path), "{path}");
        }
        for path in [
            "src/server.rs",
            "Makefile",
            "tests/test_api.py",
            "app/Repository.php",
        ] {
            assert!(!noise_path(path), "{path}");
        }
        assert_eq!(quoted_paths(&["name\n\t.rs".into()]), "\"name\\n\\t.rs\"");
    }

    #[test]
    fn nested_project_paths_are_relative_to_the_evidence_root() {
        let (dir, base, _) = fixture();
        std::fs::create_dir(dir.path().join("module")).unwrap();
        side(dir.path(), &base, "sibling", &["module/shared.rs"]);
        let root = dir.path().join("module");
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let report = branch_overlap(&root, &["shared.rs".into()], Duration::from_secs(5));
        assert!(report.complete, "{report:?}");
        assert_eq!(report.matching_branches, 1, "{report:?}");
        assert_eq!(report.branches[0].files, ["shared.rs"]);
        let indexed = crate::build_review_rehearsal(
            &root,
            &codesage_storage::Database::open_in_memory().unwrap(),
            &["shared.rs".into()],
        )
        .unwrap();
        assert!(
            indexed
                .objections
                .iter()
                .any(|row| row.category == "branch-overlap")
        );
    }
}
