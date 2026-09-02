//! `install-hooks`: git hook installation and the hook-body templates.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::find_project_root;
use crate::util::{self, git_common_dir};

pub(crate) fn cmd_install_hooks(with_leak_check: bool) -> Result<()> {
    let root = find_project_root()?;
    if !root.join(".git").exists() {
        bail!("not a git repository (no .git at project root)");
    }

    let (hooks_dir, is_husky) = resolve_hooks_dir(&root)?;
    std::fs::create_dir_all(&hooks_dir)?;

    let codesage_bin =
        std::env::current_exe().context("resolving current_exe for git hook body")?;
    let codesage_path = codesage_bin
        .to_str()
        .with_context(|| {
            format!(
                "codesage binary path is not valid UTF-8: {}",
                codesage_bin.display()
            )
        })?
        .to_owned();

    // Background indexers run niced + ionice'd so they can't soak the foreground.
    // `nice` is portable; `ionice` is Linux-only (util-linux), gated on `command -v`
    // so the hook stays a no-op on macOS / *BSD instead of failing.
    let hook_body = generate_post_commit_hook_body(&codesage_path);

    // post-rewrite fires on amend/rebase. It reshapes history, so the stored last_sha may
    // no longer be an ancestor of HEAD — incremental mode detects this and falls back to
    // full automatically, so we can safely reuse the same body here.
    let hook_names = ["post-commit", "post-merge", "post-checkout", "post-rewrite"];
    let mut installed: Vec<PathBuf> = Vec::new();
    for name in &hook_names {
        let path = hooks_dir.join(name);
        if path.exists() {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains("codesage install-hooks") {
                println!(
                    "skip: {} already exists and is not a codesage hook — \
                     codesage will NOT auto-reindex on {name}",
                    path.display()
                );
                println!(
                    "      to wire it up, chain `codesage index --lock-wait 60` and \
                     `codesage git-index --incremental --lock-wait 60` into your existing \
                     hook (backgrounded), or move it aside and re-run `codesage install-hooks`"
                );
                continue;
            }
        }

        std::fs::write(&path, &hook_body)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }

        println!("installed: {}", path.display());
        installed.push(path);
    }

    if is_husky && !installed.is_empty() {
        exclude_husky_hook_paths(&root, &installed)?;
    }

    install_leak_check_hook(&root, &hooks_dir, is_husky, with_leak_check, &mut installed)?;

    Ok(())
}

/// Generate the post-commit/post-merge/post-checkout/post-rewrite hook body
/// that runs structural+semantic index then git-history index sequentially
/// in one background subshell. Both subcommands take the same project
/// lock; if launched in parallel one would silently skip on lock
/// contention. Each pass runs regardless of the other's outcome, and each
/// pass's output and exit status append to `.codesage/hooks.log` (truncated
/// once it grows past 1 MB) so a failing hook leaves a trace instead of
/// vanishing into /dev/null.
///
/// The daemon's filesystem watcher is a third lock contender: it
/// debounce-indexes ~1s after an edit, i.e. right around commit time,
/// but covers only structural+semantic — never feature mapping or git
/// history. `--lock-wait` makes each hook pass poll the lock for up to
/// 60s instead of skipping, so a watcher pass in flight delays the hook
/// by seconds rather than silently starving the hook-only passes.
///
/// Before launching anything the hook compares HEAD plus a `cksum` of
/// `git status --porcelain` (with `.codesage/` itself excluded, so the log
/// and stamp this hook writes never perturb the digest in a clone whose
/// `.codesage/.gitignore` is missing) against `.codesage/hook-state`, which the
/// previous hook run wrote after both passes exited 0. A match means
/// nothing the hook indexes has changed (a release commit that touched
/// only ignored files, a checkout back to the same tree, a rewrite that
/// kept HEAD), and the binary is not invoked at all. The digest covers
/// CONTENT — `git diff HEAD --binary` for every tracked change plus a
/// checksum of every untracked file — not `git status`, whose output is
/// status letters and paths: a second edit to an already-modified file left
/// that unchanged and a same-HEAD hook skipped the reindex. Every stage's
/// exit status is checked; any failure leaves the stamp empty, which never
/// matches and is never written, so the binary runs. The stamp is taken
/// before the run and written after it, so a worktree that moves during the
/// run never matches the next time. A pass that exits nonzero — including
/// `EXIT_LOCK_HELD` when another indexer held the lock for the whole wait —
/// withholds the stamp, so the next hook retries.
///
/// The index pass carries `--device cpu`: an incremental pass embeds a
/// handful of changed files, and on that count a CUDA context bring-up costs
/// more than the embedding. The flag applies only within the binary's
/// `--device-max-files` bound (default 32), so a checkout that touches a
/// whole subtree still embeds on the configured device, and a full rebuild
/// is never run by the hook.
pub(crate) fn generate_post_commit_hook_body(bin: &str) -> String {
    let bin = shell_single_quote(bin);
    format!(
        "#!/bin/sh\n\
         # installed by codesage install-hooks\n\
         root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\n\
         [ -d \"$root/.codesage\" ] || exit 0\n\
         # `-d` follows symlinks, so a hostile repo can ship .codesage as a\n\
         # directory symlink and redirect every state path below it.\n\
         [ -L \"$root/.codesage\" ] && exit 0\n\
         NICE=\"nice -n 19\"\n\
         IONICE=\"\"\n\
         command -v ionice >/dev/null 2>&1 && IONICE=\"ionice -c 3\"\n\
         log=\"$root/.codesage/hooks.log\"\n\
         # Refuse a symlinked or otherwise non-regular log target: a hostile\n\
         # repo could ship .codesage/hooks.log as a symlink and turn every\n\
         # commit's append (and >1MB truncate) into a write on an arbitrary\n\
         # file. Keep running, just without a log.\n\
         if [ -L \"$log\" ] || {{ [ -e \"$log\" ] && [ ! -f \"$log\" ]; }}; then log=/dev/null; fi\n\
         # Keep the log bounded: truncate before this run once it passes 1MB.\n\
         [ -f \"$log\" ] && [ \"$(wc -c <\"$log\")\" -gt 1048576 ] && : >\"$log\"\n\
         # Early exit: the previous successful run recorded the HEAD it indexed\n\
         # and a digest of the worktree status. When both still match, nothing\n\
         # this hook indexes has changed and the binary is not started at all.\n\
         # Same symlink guard as the log: a planted state file must never turn\n\
         # the stamp write into a write elsewhere, nor a forged stamp into a\n\
         # permanent skip.\n\
         state=\"$root/.codesage/hook-state\"\n\
         if [ -L \"$state\" ] || {{ [ -e \"$state\" ] && [ ! -f \"$state\" ]; }}; then state=/dev/null; fi\n\
         # Content digest of the worktree relative to HEAD: every tracked\n\
         # change as a binary patch, plus name and checksum of every untracked\n\
         # file. Each stage's status is checked; one failure fails the digest.\n\
         worktree_digest() {{\n\
           tracked=\"$(git diff HEAD --binary --no-ext-diff --no-color -- . ':(exclude).codesage')\" || return 1\n\
           untracked=\"$(git ls-files -o --exclude-standard -- . ':(exclude).codesage')\" || return 1\n\
           sums=\"$(printf '%s\\n' \"$untracked\" | while IFS= read -r f; do\n\
             [ -n \"$f\" ] || continue\n\
             if [ -L \"$f\" ]; then printf '%s -> %s\\n' \"$f\" \"$(readlink \"$f\")\" || exit 1\n\
             elif [ -f \"$f\" ]; then cksum \"$f\" || exit 1\n\
             else printf '%s (special)\\n' \"$f\"\n\
             fi\n\
           done)\" || return 1\n\
           printf '%s\\n%s\\n' \"$tracked\" \"$sums\" | cksum\n\
         }}\n\
         head=\"$(git rev-parse HEAD 2>/dev/null)\" || head=\"\"\n\
         stamp=\"\"\n\
         if [ -n \"$head\" ]; then\n\
           digest=\"$(worktree_digest 2>/dev/null)\" && stamp=\"$head $digest\"\n\
         fi\n\
         if [ -n \"$stamp\" ] && [ -f \"$state\" ] && [ \"$(cat \"$state\" 2>/dev/null)\" = \"$stamp\" ]; then\n\
           echo \"[$(date)] $(basename \"$0\") hook skip: HEAD and worktree unchanged since last index\" >>\"$log\"\n\
           exit 0\n\
         fi\n\
         # Run structural+semantic index then git-history index sequentially\n\
         # in one background subshell. Both subcommands take the same\n\
         # project lock and would silently skip on contention if launched in\n\
         # parallel. Each pass runs regardless of the other's outcome; its\n\
         # output and exit status append to the log. The daemon's filesystem\n\
         # watcher contends for the same lock around commit time but never\n\
         # runs feature mapping or git-history indexing, so --lock-wait\n\
         # polls it out instead of skipping. --device cpu keeps a few changed\n\
         # files off the GPU; the binary ignores it past --device-max-files.\n\
         # The stamp is recorded only after both passes exit 0, so a failed\n\
         # run — or one that exited 75 because another indexer held the lock\n\
         # for the whole wait — is retried by the next hook. An empty stamp\n\
         # (no HEAD, or a digest stage failed) is never recorded.\n\
         ( cd \"$root\" || exit 0\n\
           echo \"[$(date)] $(basename \"$0\") hook start\" >>\"$log\"\n\
           $IONICE $NICE {bin} index --lock-wait 60 --device cpu >>\"$log\" 2>&1; rc=$?\n\
           echo \"[$(date)] index exit=$rc\" >>\"$log\"\n\
           index_rc=$rc\n\
           $IONICE $NICE {bin} git-index --incremental --lock-wait 60 >>\"$log\" 2>&1; rc=$?\n\
           echo \"[$(date)] git-index exit=$rc\" >>\"$log\"\n\
           [ -n \"$stamp\" ] && [ \"$index_rc\" -eq 0 ] && [ \"$rc\" -eq 0 ] && printf '%s\\n' \"$stamp\" >\"$state\" ) >>\"$log\" 2>&1 &\n\
         exit 0\n",
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Install a pre-commit leak-check hook wrapping the repo's `scripts/leak-check.sh`.
/// Keeps the hook a thin wrapper that invokes the repo's own script so the pattern
/// list and script logic can be iterated without re-running install-hooks.
///
/// Opt-in only (`--with-leak-check`): unlike the other hooks, which exec the
/// trusted codesage binary, this one execs a repo-shipped script — auto-wiring
/// it would hand a fresh clone of a malicious repo code execution on the
/// user's next commit.
fn install_leak_check_hook(
    root: &std::path::Path,
    hooks_dir: &std::path::Path,
    is_husky: bool,
    enabled: bool,
    installed: &mut Vec<PathBuf>,
) -> Result<()> {
    let script = root.join("scripts/leak-check.sh");
    if !script.exists() {
        if enabled {
            println!("skip: --with-leak-check passed but no scripts/leak-check.sh in repo");
        }
        return Ok(());
    }
    if !enabled {
        println!(
            "notice: scripts/leak-check.sh found; rerun with --with-leak-check to install it as a pre-commit hook"
        );
        return Ok(());
    }

    let path = hooks_dir.join("pre-commit");
    if path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if !existing.contains("codesage install-hooks") {
            println!(
                "skip: {} already exists and is not a codesage hook — \
                 the leak-check will NOT run on commit",
                path.display()
            );
            println!(
                "      to wire it up, exec scripts/leak-check.sh from your existing \
                 pre-commit hook, or move it aside and re-run \
                 `codesage install-hooks --with-leak-check`"
            );
            return Ok(());
        }
    }

    let body = "#!/bin/sh\n\
                # installed by codesage install-hooks\n\
                root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\n\
                script=\"$root/scripts/leak-check.sh\"\n\
                [ -x \"$script\" ] || exit 0\n\
                exec \"$script\"\n";
    std::fs::write(&path, body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("installed: {} (leak-check)", path.display());
    installed.push(path.clone());

    if is_husky {
        exclude_husky_hook_paths(root, std::slice::from_ref(&path))?;
    }

    Ok(())
}

fn resolve_hooks_dir(root: &std::path::Path) -> Result<(PathBuf, bool)> {
    let configured = std::process::Command::new("git")
        .arg("config")
        .arg("--get")
        .arg("core.hooksPath")
        .current_dir(root)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        });

    match configured {
        None => {
            let common = git_common_dir(root)
                .ok_or_else(|| anyhow::anyhow!("unable to resolve git common dir"))?;
            Ok((common.join("hooks"), false))
        }
        Some(raw) => {
            let path = std::path::Path::new(&raw);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            // A `core.hooksPath` that resolves to the default `<git_common>/hooks`
            // is a no-op redundancy; treat it like an unset value rather than
            // refusing. Seen in the wild on PHP-extension repos that share a
            // config template.
            if let Some(common) = git_common_dir(root) {
                let default_hooks = common.join("hooks");
                if util::paths_resolve_same(&resolved, &default_hooks) {
                    return Ok((default_hooks, false));
                }
            }
            if resolved.join("h").is_file() || resolved.join("husky.sh").is_file() {
                let user_dir = resolved
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("husky hooks dir has no parent"))?
                    .to_path_buf();
                Ok((user_dir, true))
            } else {
                bail!(
                    "core.hooksPath is set to {} but it does not look like a Husky setup; \
                     refusing to install hooks. Install manually or clear core.hooksPath.",
                    resolved.display()
                );
            }
        }
    }
}

fn exclude_husky_hook_paths(root: &std::path::Path, hooks: &[PathBuf]) -> Result<()> {
    let Some(exclude) = git_local_exclude_path(root) else {
        return Ok(());
    };
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut to_add: Vec<String> = Vec::new();
    for hook in hooks {
        let Ok(rel) = hook.strip_prefix(root) else {
            continue;
        };
        let line = format!("/{}", rel.display());
        if !existing.lines().any(|l| l.trim() == line.trim()) {
            to_add.push(line);
        }
    }
    if to_add.is_empty() {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&exclude)?;
    use std::io::Write;
    writeln!(f, "\n# codesage husky hooks")?;
    for line in &to_add {
        writeln!(f, "{line}")?;
    }
    println!(
        "    added {} husky hook path(s) to .git/info/exclude",
        to_add.len()
    );
    Ok(())
}

fn git_local_exclude_path(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    Some(git_common_dir(cwd)?.join("info").join("exclude"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A concurrent test's fork can hold a just-written script's fd until its
    // exec, so the first spawn may hit ETXTBSY; retry briefly.
    fn run_script(hook: &std::path::Path, root: &std::path::Path) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match std::process::Command::new(hook).current_dir(root).status() {
                Ok(status) => return status,
                Err(e)
                    if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("hook spawn failed: {e}"),
            }
        }
    }

    // ---------- post-commit hook body contract ----------

    #[test]
    fn post_commit_hook_runs_both_passes_independently() {
        // Both subcommands run inside ONE background subshell (parallel
        // launch would race on the project lock), and neither pass is
        // conditioned on the other: an `&&` chain would let an index
        // failure silently starve git-index of every incremental update.
        let body = generate_post_commit_hook_body("/usr/local/bin/codesage");
        assert!(
            body.contains(
                "'/usr/local/bin/codesage' index --lock-wait 60 --device cpu >>\"$log\" 2>&1; rc=$?"
            ),
            "expected logged `index --lock-wait --device cpu` invocation, got:\n{body}"
        );
        assert!(
            body.contains(
                "'/usr/local/bin/codesage' git-index --incremental --lock-wait 60 >>\"$log\" 2>&1; rc=$?"
            ),
            "expected logged `git-index --incremental --lock-wait` invocation, got:\n{body}"
        );
        assert!(
            !body.contains("--lock-wait 60 &&"),
            "passes must not be `&&`-chained (index failure would skip git-index):\n{body}"
        );
        assert!(
            body.contains("index exit=$rc") && body.contains("git-index exit=$rc"),
            "each pass's exit status must be logged, got:\n{body}"
        );
        // Only one background `&` should appear at the top level —
        // we sequence inside one subshell, then background the whole
        // group. Two `&` (one per command) would re-introduce the race.
        let backgrounded_lines: Vec<&str> = body
            .lines()
            .filter(|l| l.trim_end().ends_with(" &"))
            .collect();
        assert_eq!(
            backgrounded_lines.len(),
            1,
            "expected exactly one background `&` line, got {} lines:\n{}",
            backgrounded_lines.len(),
            backgrounded_lines.join("\n")
        );
    }

    #[test]
    fn post_commit_hook_logs_to_bounded_hooks_log() {
        let body = generate_post_commit_hook_body("/x");
        assert!(
            body.contains("log=\"$root/.codesage/hooks.log\""),
            "hook output must land in .codesage/hooks.log, got:\n{body}"
        );
        assert!(
            body.contains("1048576"),
            "expected the 1MB truncation guard, got:\n{body}"
        );
        assert!(
            !body.contains(") >/dev/null"),
            "hook output must not be discarded to /dev/null:\n{body}"
        );
        // Installer idempotence and doctor's hook detection both key on
        // this marker string; template changes must preserve it.
        assert!(
            body.contains("codesage install-hooks"),
            "marker string must survive template changes:\n{body}"
        );
    }

    #[test]
    fn post_commit_hook_shell_quotes_codesage_path() {
        let body = generate_post_commit_hook_body("/tmp/cod\"e$age`bin's/codesage");

        assert!(
            body.contains("'/tmp/cod\"e$age`bin'\"'\"'s/codesage' index --lock-wait 60"),
            "expected shell-quoted binary path, got:\n{body}"
        );
        assert!(
            !body.contains("\"/tmp/cod\"e$age`bin's/codesage\""),
            "double-quoted binary path would still allow shell expansion:\n{body}"
        );
    }

    #[test]
    fn shell_single_quote_escapes_single_quotes() {
        assert_eq!(
            shell_single_quote("/tmp/a'b/codesage"),
            "'/tmp/a'\"'\"'b/codesage'"
        );
        assert_eq!(shell_single_quote("/tmp/codesage"), "'/tmp/codesage'");
    }

    #[test]
    fn post_commit_hook_early_exit_keys_on_head_and_content_digest() {
        let body = generate_post_commit_hook_body("/x");
        assert!(
            body.contains("state=\"$root/.codesage/hook-state\""),
            "expected the hook-state path, got:\n{body}"
        );
        // Content, not status: the tracked side is a binary patch against
        // HEAD and the untracked side checksums each file. `git status
        // --porcelain` must be gone — its output is unchanged by a second
        // edit to an already-modified file.
        assert!(
            !body.contains("git status"),
            "the stamp must not be keyed on `git status` output:\n{body}"
        );
        assert!(
            body.contains(
                "tracked=\"$(git diff HEAD --binary --no-ext-diff --no-color -- . ':(exclude).codesage')\" || return 1"
            ),
            "tracked changes must be digested as content with the stage checked, got:\n{body}"
        );
        assert!(
            body.contains(
                "untracked=\"$(git ls-files -o --exclude-standard -- . ':(exclude).codesage')\" || return 1"
            ),
            "untracked listing must be stage-checked, got:\n{body}"
        );
        assert!(
            body.contains("elif [ -f \"$f\" ]; then cksum \"$f\" || exit 1"),
            "untracked file content must be checksummed with the stage checked, got:\n{body}"
        );
        assert!(
            body.contains("digest=\"$(worktree_digest 2>/dev/null)\" && stamp=\"$head $digest\""),
            "a failed digest must leave the stamp empty, got:\n{body}"
        );
        assert!(
            body.contains("if [ -n \"$stamp\" ] && [ -f \"$state\" ] && [ \"$(cat \"$state\" 2>/dev/null)\" = \"$stamp\" ]; then"),
            "expected the skip comparison gated on a non-empty stamp, got:\n{body}"
        );
        // The skip must be decided before the binary is launched, and the
        // stamp written only after both passes exited 0.
        let skip = body.find("hook skip:").unwrap();
        let launch = body.find("hook start").unwrap();
        let record = body.find("printf '%s\\n' \"$stamp\" >\"$state\"").unwrap();
        assert!(skip < launch && launch < record, "{body}");
        assert!(
            body.contains(
                "[ -n \"$stamp\" ] && [ \"$index_rc\" -eq 0 ] && [ \"$rc\" -eq 0 ] && printf"
            ),
            "stamp must require a computed digest and both passes exiting 0, got:\n{body}"
        );
        // Same symlink guard as the log, before the first use of $state.
        assert!(
            body.contains(
                "if [ -L \"$state\" ] || { [ -e \"$state\" ] && [ ! -f \"$state\" ]; }; then state=/dev/null; fi"
            ),
            "expected the state symlink guard, got:\n{body}"
        );
        let guard = body.find("[ -L \"$state\" ]").unwrap();
        let first_read = body.find("cat \"$state\"").unwrap();
        assert!(guard < first_read, "{body}");
    }

    #[cfg(unix)]
    #[test]
    fn post_commit_hook_skips_the_binary_when_head_and_worktree_are_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        let status = std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "one"])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        let stub = root.join("codesage-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho \"stub ran: $1\" >> \"$(git rev-parse --show-toplevel)/.codesage/stub.mark\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hook = root.join(".git/hooks/post-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            generate_post_commit_hook_body(stub.to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = root.join(".codesage/hooks.log");
        let mark = root.join(".codesage/stub.mark");
        let wait_for = |needle: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let content = std::fs::read_to_string(&log).unwrap_or_default();
                if content.contains(needle) {
                    return content;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "log never recorded {needle:?}:\n{content}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        };

        // First run: no stamp yet, both passes run, stamp recorded.
        assert!(run_script(&hook, root).success());
        wait_for("] git-index exit=0");
        let state = root.join(".codesage/hook-state");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !state.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "hook-state never written"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let stamp = std::fs::read_to_string(&state).unwrap();
        assert!(
            stamp.starts_with(&format!("{} ", head_sha(root))),
            "{stamp:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&mark).unwrap(),
            "stub ran: index\nstub ran: git-index\n"
        );

        // Second run with the same HEAD and worktree: skipped before the binary.
        assert!(run_script(&hook, root).success());
        let content = wait_for("hook skip:");
        assert_eq!(content.matches("hook start").count(), 1, "{content}");
        assert_eq!(
            std::fs::read_to_string(&mark).unwrap(),
            "stub ran: index\nstub ran: git-index\n",
            "the binary must not run on an unchanged tree"
        );

        // A worktree change with the same HEAD defeats the skip.
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        assert!(run_script(&hook, root).success());
        let starts_after = |n: usize, why: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let content = std::fs::read_to_string(&log).unwrap_or_default();
                if content.matches("hook start").count() == n
                    && content.matches("] git-index exit=0").count() == n
                {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "{why}:\n{content}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        starts_after(2, "dirty worktree must re-run the passes");
        let wait_stamp_change = |previous: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let now = std::fs::read_to_string(&state).unwrap_or_default();
                if !now.is_empty() && now != previous {
                    return now;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "stamp never moved past {previous:?}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let stamp_modified = wait_stamp_change(&stamp);

        // The file is still "modified" in `git status` terms, so a status
        // digest would be identical here; the content digest must not be.
        std::fs::write(root.join("a.txt"), "changed again\n").unwrap();
        assert!(run_script(&hook, root).success());
        starts_after(3, "a second edit to an already-modified file must re-run");
        let stamp_modified_again = wait_stamp_change(&stamp_modified);

        // Untracked content is part of the digest too: creating the file and
        // then editing it are two changes, and both must run the passes.
        std::fs::write(root.join("u.txt"), "u1\n").unwrap();
        assert!(run_script(&hook, root).success());
        starts_after(4, "a new untracked file must re-run");
        let stamp_untracked = wait_stamp_change(&stamp_modified_again);
        std::fs::write(root.join("u.txt"), "u2\n").unwrap();
        assert!(run_script(&hook, root).success());
        starts_after(5, "an edit to an untracked file must re-run");
        wait_stamp_change(&stamp_untracked);

        // Same tree once more: skipped.
        assert!(run_script(&hook, root).success());
        let content = wait_for("hook skip:");
        assert_eq!(content.matches("hook skip:").count(), 2, "{content}");
        assert_eq!(content.matches("hook start").count(), 5, "{content}");
    }

    #[cfg(unix)]
    fn git_repo_with_one_commit(root: &std::path::Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        for args in [vec!["add", "a.txt"], vec!["commit", "-q", "-m", "one"]] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
    }

    #[cfg(unix)]
    fn install_hook_with_stub(root: &std::path::Path, stub_body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let stub = root.join("codesage-stub");
        std::fs::write(&stub, stub_body).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook = root.join(".git/hooks/post-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            generate_post_commit_hook_body(stub.to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        hook
    }

    #[cfg(unix)]
    fn wait_for_log(log: &std::path::Path, needle: &str) -> String {
        use std::time::Duration;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let content = std::fs::read_to_string(log).unwrap_or_default();
            if content.contains(needle) {
                return content;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "log never recorded {needle:?}:\n{content}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    #[test]
    fn post_commit_hook_runs_the_binary_and_writes_no_stamp_when_git_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_repo_with_one_commit(root);
        let hook = install_hook_with_stub(
            root,
            "#!/bin/sh\necho \"stub ran: $1\" >> \"$(git rev-parse --show-toplevel)/.codesage/stub.mark\"\nexit 0\n",
        );

        // A `git` whose `diff` fails, ahead of the real one on PATH. The old
        // stamp piped this failure into a successful `cksum`, so a broken
        // digest still matched itself and the binary was skipped.
        let real_git = String::from_utf8(
            std::process::Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let shim_dir = root.join("shim");
        std::fs::create_dir_all(&shim_dir).unwrap();
        let shim = shim_dir.join("git");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nif [ \"$1\" = diff ]; then echo 'simulated git failure' >&2; exit 128; fi\nexec '{}' \"$@\"\n",
                real_git.trim()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            shim_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let run_with_shim = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match std::process::Command::new(&hook)
                    .current_dir(root)
                    .env("PATH", &path)
                    .status()
                {
                    Ok(status) => return status,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("hook spawn failed: {e}"),
                }
            }
        };

        let log = root.join(".codesage/hooks.log");
        let state = root.join(".codesage/hook-state");
        for run in 1..=2 {
            assert!(run_with_shim().success(), "hook must still exit 0");
            let content = {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let content = std::fs::read_to_string(&log).unwrap_or_default();
                    if content.matches("] git-index exit=0").count() == run {
                        break content;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "run {run}: passes never completed:\n{content}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            };
            assert_eq!(
                content.matches("hook start").count(),
                run,
                "run {run}: the binary must run when the digest cannot be computed:\n{content}"
            );
            assert!(
                !content.contains("hook skip:"),
                "run {run}: a failed digest must never match a stamp:\n{content}"
            );
            // Both passes exited 0, yet nothing may be recorded: an empty
            // stamp would otherwise be written and match itself forever.
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(
                !state.exists(),
                "run {run}: no stamp may be written when a digest stage failed"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join(".codesage/stub.mark")).unwrap(),
            "stub ran: index\nstub ran: git-index\nstub ran: index\nstub ran: git-index\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_commit_hook_withholds_the_stamp_when_index_exits_lock_held() {
        // `codesage index` exits EXIT_LOCK_HELD (75) when another indexer
        // held the lock for the whole wait: nothing was indexed. The hook
        // must not record the tree as indexed, or every later run on this
        // HEAD would skip and the index would stay stale for good.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_repo_with_one_commit(root);
        let hook = install_hook_with_stub(
            root,
            &format!(
                "#!/bin/sh\necho \"stub ran: $1\" >> \"$(git rev-parse --show-toplevel)/.codesage/stub.mark\"\n[ \"$1\" = index ] && exit {}\nexit 0\n",
                crate::EXIT_LOCK_HELD
            ),
        );
        let log = root.join(".codesage/hooks.log");
        let state = root.join(".codesage/hook-state");

        assert!(run_script(&hook, root).success());
        let content = wait_for_log(&log, "] git-index exit=0");
        assert!(
            content.contains(&format!("] index exit={}", crate::EXIT_LOCK_HELD)),
            "{content}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !state.exists(),
            "a lock-held index pass indexed nothing; the stamp must be withheld"
        );

        // The next hook on the same tree retries instead of skipping.
        assert!(run_script(&hook, root).success());
        let content = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let content = std::fs::read_to_string(&log).unwrap_or_default();
                if content.matches("hook start").count() == 2 {
                    break content;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "second run must retry the passes:\n{content}"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        assert!(!content.contains("hook skip:"), "{content}");
    }

    #[cfg(unix)]
    fn head_sha(root: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn post_commit_hook_skips_when_no_codesage_dir() {
        let body = generate_post_commit_hook_body("/x");
        assert!(
            body.contains("[ -d \"$root/.codesage\" ]"),
            "expected guard on .codesage directory presence"
        );
    }

    #[test]
    fn post_commit_hook_refuses_a_symlinked_codesage_dir() {
        // `-d` follows symlinks, so the `-L` guard must sit beside it.
        let body = generate_post_commit_hook_body("/x");
        assert!(
            body.contains("[ -L \"$root/.codesage\" ] && exit 0"),
            "expected the .codesage directory-symlink guard, got:\n{body}"
        );
        let dir_guard = body.find("[ -L \"$root/.codesage\" ]").unwrap();
        let log_assign = body.find("log=\"$root/.codesage/hooks.log\"").unwrap();
        assert!(
            dir_guard < log_assign,
            "the directory guard must precede the log path assignment:\n{body}"
        );
    }

    #[test]
    fn post_commit_hook_refuses_non_regular_log_target() {
        // A hostile repo can ship `.codesage/hooks.log` as a symlink (or a
        // fifo); appending — and the >1MB truncation — would then hit the
        // symlink's target. The template must drop to /dev/null instead.
        let body = generate_post_commit_hook_body("/x");
        assert!(
            body.contains(
                "if [ -L \"$log\" ] || { [ -e \"$log\" ] && [ ! -f \"$log\" ]; }; then log=/dev/null; fi"
            ),
            "expected the symlink/non-regular log guard, got:\n{body}"
        );
        // The guard must run before the first use of $log (the truncation).
        let guard_pos = body.find("[ -L \"$log\" ]").unwrap();
        let truncate_pos = body.find("wc -c").unwrap();
        assert!(
            guard_pos < truncate_pos,
            "log guard must precede the truncation check:\n{body}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_commit_hook_does_not_write_through_symlinked_log() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let status = std::process::Command::new("git")
            .arg("init")
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        // Victim file a hostile repo would target via a shipped symlink.
        let victim = root.join("victim.txt");
        std::fs::write(&victim, "").unwrap();
        std::os::unix::fs::symlink(&victim, root.join(".codesage/hooks.log")).unwrap();

        // Stub records its runs in a separate marker file since the log is
        // (correctly) discarded in this scenario.
        let stub = root.join("codesage-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho \"ran $1\" >> \"$(git rev-parse --show-toplevel)/stub.mark\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hook = root.join(".git/hooks/post-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            generate_post_commit_hook_body(stub.to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let status = run_script(&hook, root);
        assert!(status.success(), "hook must exit 0");

        // Wait for both backgrounded passes to run.
        let mark = root.join("stub.mark");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let content = std::fs::read_to_string(&mark).unwrap_or_default();
            if content.contains("ran index") && content.contains("ran git-index") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stub marker never recorded both passes: {content:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        let victim_content = std::fs::read_to_string(&victim).unwrap();
        assert_eq!(
            victim_content, "",
            "hook must not write through a symlinked hooks.log"
        );
        assert!(
            root.join(".codesage/hooks.log").is_symlink(),
            "the planted symlink must be left untouched, not replaced"
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_commit_hook_executes_and_logs_both_passes() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let status = std::process::Command::new("git")
            .arg("init")
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        // Stub binary fails on `index` and succeeds on `git-index`, proving
        // the second pass runs even when the first one breaks.
        let stub = root.join("codesage-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho \"stub ran: $1\"\n[ \"$1\" = index ] && exit 7\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hook = root.join(".git/hooks/post-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            generate_post_commit_hook_body(stub.to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let status = run_script(&hook, root);
        assert!(status.success(), "hook must exit 0");

        // The indexing subshell is backgrounded; poll the log it writes.
        let log = root.join(".codesage/hooks.log");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let content = std::fs::read_to_string(&log).unwrap_or_default();
            if content.contains("] git-index exit=0") {
                assert!(
                    content.contains("] index exit=7"),
                    "index failure status must be logged:\n{content}"
                );
                assert!(
                    content.contains("stub ran: index") && content.contains("stub ran: git-index"),
                    "both passes must have run:\n{content}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "hook log never recorded git-index completion:\n{content}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ---------- leak-check hook opt-in gate ----------

    fn leak_check_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/leak-check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        let hooks_dir = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        (dir, root, hooks_dir)
    }

    #[test]
    fn leak_check_hook_requires_explicit_opt_in() {
        // The leak-check hook execs a repo-shipped script. Auto-installing it
        // on `install-hooks` would give a fresh clone of a malicious repo
        // code execution on the user's next commit, so without
        // --with-leak-check nothing may be written.
        let (_dir, root, hooks_dir) = leak_check_fixture();
        let mut installed = Vec::new();

        install_leak_check_hook(&root, &hooks_dir, false, false, &mut installed).unwrap();

        assert!(
            !hooks_dir.join("pre-commit").exists(),
            "pre-commit hook must not be installed without --with-leak-check"
        );
        assert!(installed.is_empty());
    }

    #[test]
    fn leak_check_hook_installed_with_opt_in() {
        let (_dir, root, hooks_dir) = leak_check_fixture();
        let mut installed = Vec::new();

        install_leak_check_hook(&root, &hooks_dir, false, true, &mut installed).unwrap();

        let hook = hooks_dir.join("pre-commit");
        let body = std::fs::read_to_string(&hook).expect("pre-commit hook written");
        assert!(
            body.contains("codesage install-hooks"),
            "hook must carry the codesage marker for idempotent reinstall:\n{body}"
        );
        assert!(
            body.contains("scripts/leak-check.sh"),
            "hook must exec the repo's leak-check script:\n{body}"
        );
        assert_eq!(installed, vec![hook.clone()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "hook must be executable");
        }
    }

    #[test]
    fn leak_check_hook_never_overwrites_foreign_pre_commit() {
        let (_dir, root, hooks_dir) = leak_check_fixture();
        let foreign = "#!/bin/sh\n# user's own hook\nexit 0\n";
        std::fs::write(hooks_dir.join("pre-commit"), foreign).unwrap();
        let mut installed = Vec::new();

        install_leak_check_hook(&root, &hooks_dir, false, true, &mut installed).unwrap();

        let body = std::fs::read_to_string(hooks_dir.join("pre-commit")).unwrap();
        assert_eq!(body, foreign, "foreign pre-commit hook must be untouched");
        assert!(installed.is_empty());
    }

    #[test]
    fn leak_check_hook_noop_without_script() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let hooks_dir = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let mut installed = Vec::new();

        install_leak_check_hook(&root, &hooks_dir, false, true, &mut installed).unwrap();

        assert!(!hooks_dir.join("pre-commit").exists());
        assert!(installed.is_empty());
    }
}
