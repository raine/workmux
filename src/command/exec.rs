//! Hidden `_exec` subcommand for running commands in worktree panes.
//!
//! This is invoked by `workmux run` in a split pane to execute the command
//! while capturing output to files.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use anyhow::{Context, Result};

use crate::state::run::{RunResult, read_spec, write_result};

pub fn run(run_dir: &Path) -> Result<()> {
    let result = try_run(run_dir);

    // If execution failed before writing result, write a failure marker
    // so the coordinator doesn't hang waiting forever
    if let Err(e) = &result {
        eprintln!("Execution failed: {:#}", e);
        let fail_result = RunResult {
            exit_code: Some(1),
            signal: None,
        };
        let _ = write_result(run_dir, &fail_result);
    }

    result
}

fn try_run(run_dir: &Path) -> Result<()> {
    let spec = read_spec(run_dir)?;

    let stdout_path = run_dir.join("stdout");
    let stderr_path = run_dir.join("stderr");

    // Open output files for appending
    let stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_path)
        .context("Failed to open stdout file")?;

    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .context("Failed to open stderr file")?;

    // Spawn the command
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(&spec.command)
        .current_dir(&spec.worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn command")?;

    let child_pid = child.id();
    let running = Arc::new(AtomicBool::new(true));

    // Setup signal handler to forward SIGINT to child
    #[cfg(unix)]
    {
        let r = running.clone();
        let _ = ctrlc::set_handler(move || {
            if r.load(Ordering::SeqCst) {
                unsafe {
                    libc::kill(child_pid as i32, libc::SIGINT);
                }
            }
        });
    }

    // Take ownership of child's stdout/stderr
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    // Spawn thread to pump stdout (move owned handles into thread)
    let stdout_handle = thread::spawn(move || {
        pump_output(child_stdout, stdout_file, std::io::stdout());
    });

    // Spawn thread to pump stderr (move owned handles into thread)
    let stderr_handle = thread::spawn(move || {
        pump_output(child_stderr, stderr_file, std::io::stderr());
    });

    // Wait for child to complete
    let status = child.wait().context("Failed to wait for command")?;
    running.store(false, Ordering::SeqCst);

    // Wait for IO threads to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Write result
    #[cfg(unix)]
    let signal = status.signal();
    #[cfg(not(unix))]
    let signal = None;

    let result = RunResult {
        exit_code: status.code(),
        signal,
    };
    write_result(run_dir, &result)?;

    // Exit with same code as child
    std::process::exit(status.code().unwrap_or(1));
}

fn pump_output<R: Read, F: Write, T: Write>(mut reader: R, mut file: F, mut terminal: T) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let data = &buf[..n];
                let _ = file.write_all(data);
                let _ = file.flush();
                let _ = terminal.write_all(data);
                let _ = terminal.flush();
            }
            Err(_) => break,
        }
    }
}
