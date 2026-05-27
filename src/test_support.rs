//! Test-only helpers shared across modules.

use std::sync::Mutex;

/// Process-wide lock used to serialize tests that mutate the process's
/// current working directory via `std::env::set_current_dir`.
///
/// Several tests need to verify that workmux code consults an explicit
/// repo path argument and does not fall back to the process cwd. Those
/// tests temporarily change cwd to a known-bad value, run the code under
/// test, then restore the original cwd. Because cargo runs tests in
/// parallel by default, every such test **must** acquire this single
/// crate-wide lock; otherwise concurrent cwd mutations from sibling test
/// modules will race and cause spurious failures.
pub static CWD_LOCK: Mutex<()> = Mutex::new(());
