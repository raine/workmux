//! Test-only helpers shared across modules.

use std::path::Path;
use std::process::Command;

pub const ISOLATED_TEST_ENV: &str = "WM_ISOLATED_TEST";

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
        stdout.contains("test result: ok. 1 passed; 0 failed;"),
        "isolated test {test_name} did not run exactly once\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
}
