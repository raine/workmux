//! Test-only helpers shared across modules.

use std::path::Path;
use std::process::Command;

pub const ISOLATED_TEST_ENV: &str = "WM_ISOLATED_TEST";
pub const ISOLATED_TEST_CANARY: &str = "WM_ISOLATED_TEST_EXECUTED";

pub fn is_isolated_child(test_name: &str) -> bool {
    std::env::var_os(ISOLATED_TEST_ENV).as_deref() == Some(std::ffi::OsStr::new(test_name))
}

pub fn run_isolated_test(test_name: &str, cwd: &Path, envs: &[(&str, &Path)]) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(cwd)
        .env(ISOLATED_TEST_ENV, test_name);
    clear_local_git_env(&mut command);

    for (key, value) in envs {
        command.env(key, value);
    }

    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated test {test_name} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );
    assert!(
        stdout.contains(ISOLATED_TEST_CANARY),
        "isolated test {test_name} did not execute\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
}

pub fn run_git(repo: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn init_repo(dir: &Path) {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .output()
        .expect("git init should run");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test User"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    run_git(dir, &["add", "README.md"]);
    run_git(dir, &["commit", "-m", "initial"]);
}

/// Initialize a "jj-only" repository in `dir`: a jj repo backed by git for
/// storage, but with the git repository hidden inside `.jj` (no `.git`
/// directory visible at the workspace root). Verified against jj 0.44.0:
/// `jj git init --no-colocate` is the invocation that satisfies this — the
/// plain `jj git init` (colocation is jj's default) and `jj git init
/// --colocate` both leave a `.git` directory at the workspace root, while
/// `--no-colocate` places the git repository under `.jj/repo/store/git`
/// instead. (`jj init` alone is not a valid subcommand in 0.44.0; jj always
/// requires an explicit backend via `jj git init`.)
pub fn init_jj_repo(dir: &Path) {
    let mut command = Command::new("jj");
    clear_local_jj_env(&mut command);
    let output = command
        .args(["git", "init", "--no-colocate"])
        .current_dir(dir)
        .output()
        .expect("jj git init should run");
    assert!(
        output.status.success(),
        "jj git init --no-colocate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialize a colocated jj+git repository in `dir`: both `.jj` and `.git`
/// are visible at the workspace root, and ordinary `git` commands work
/// alongside `jj` ones. Verified against jj 0.44.0: `jj git init --colocate`.
pub fn init_colocated_repo(dir: &Path) {
    let mut command = Command::new("jj");
    clear_local_jj_env(&mut command);
    let output = command
        .args(["git", "init", "--colocate"])
        .current_dir(dir)
        .output()
        .expect("jj git init --colocate should run");
    assert!(
        output.status.success(),
        "jj git init --colocate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Clear jj's ambient environment variables, mirroring
/// `clear_local_git_env`'s treatment of git's ambient env vars. Kept
/// separate from `crate::vcs::jj_security::clear_ambient_jj_env` so
/// `test_support` (used by both git and jj tests) does not need to depend on
/// `vcs` module internals to stay a plain fixture helper.
pub(crate) fn clear_local_jj_env(command: &mut Command) {
    for key in [
        "JJ_CONFIG",
        "JJ_USER",
        "JJ_EMAIL",
        "JJ_OP_HOSTNAME",
        "JJ_OP_USERNAME",
        "JJ_EDITOR",
        "JJ_DIFF_TOOL",
        "JJ_MERGE_TOOL",
        "EDITOR",
        "VISUAL",
        "PAGER",
    ] {
        command.env_remove(key);
    }
}

pub(crate) fn clear_local_git_env(command: &mut Command) {
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_DIR",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_SHALLOW_FILE",
        "GIT_WORK_TREE",
    ] {
        command.env_remove(key);
    }
    command.env_remove("GIT_CONFIG_COUNT");
    command.env_remove("GIT_CONFIG_PARAMETERS");
    for i in 0..32 {
        command.env_remove(format!("GIT_CONFIG_KEY_{i}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{i}"));
    }
}
