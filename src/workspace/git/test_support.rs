use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Detach spawned `git` from developer and system configuration.
///
/// A global `trace2.eventTarget` (git-ai installs one) makes every git invocation notify a
/// daemon that then writes into the repository asynchronously — `ai/` directories and
/// fast-import objects appearing after the foreground command has already exited. That racing
/// writer makes the tests' `remove_dir_all` teardown fail with `DirectoryNotEmpty`, and the
/// same inherited config could equally change refstorage, gc, or default-branch behaviour
/// underneath a fixture. Tests must depend only on the repositories they build themselves.
///
/// Process-wide rather than per-`Command` so it also covers the production code under test,
/// which spawns its own git. Safe because nextest runs each test in its own process.
fn isolate_git_from_developer_config() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
}

pub(super) fn temp_test_dir(name: &str) -> PathBuf {
    isolate_git_from_developer_config();
    let unique = format!(
        "herdr-workspace-tests-{}-{}-{}",
        name,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&path).unwrap();
    path
}

pub(super) fn write_fake_tracked_repo(root: &Path) {
    let head_oid = "1111111111111111111111111111111111111111";
    let upstream_oid = "2222222222222222222222222222222222222222";
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join(".git/refs/heads/main"), format!("{head_oid}\n")).unwrap();
    std::fs::write(
        root.join(".git/refs/remotes/origin/main"),
        format!("{upstream_oid}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    )
    .unwrap();
}

pub(super) fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
