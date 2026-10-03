use std::path::Path;
use std::process::Command;

const REPOSITORY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_REFERENCE_BACKEND",
];

/// Construct a Git child whose repository is discovered from `root`, including
/// linked worktrees, rather than inherited repository or object-store selectors.
///
/// Configuration overrides remain inherited: `GIT_CONFIG`,
/// `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, its key/value pairs, and global
/// and system config controls. `GIT_NO_REPLACE_OBJECTS` and unrelated environment
/// also remain inherited. This selects a repository; it does not restrict Git's
/// execution configuration. Callers retain their deadlines and hardening flags.
pub fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(root);
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command
}
