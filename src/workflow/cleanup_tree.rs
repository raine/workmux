//! Descriptor-relative recursive deletion that continues after individual failures.

use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{Mode, fstat, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};

use super::cleanup::{DirectoryIdentity, metadata_matches};

const MAX_DEPTH: usize = 64;
const MAX_ERROR_BYTES: usize = 16 * 1024;

fn directory_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

/// Open and validate the quarantine root before touching its contents. All child
/// traversal and deletion is relative to held descriptors, never joined paths.
pub(super) fn remove(path: &Path, expected: DirectoryIdentity) -> io::Result<()> {
    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing quarantine parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("Missing quarantine filename"))?;
    let parent = match open(parent_path, directory_flags(), Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let directory = match openat(&parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata_matches(&directory.metadata()?, expected) {
        return Err(io::Error::other(
            "Quarantined worktree identity changed before deletion",
        ));
    }
    let mut failures = Failures::default();
    clear_directory(&directory, path, 0, &mut failures);
    failures.record(path, unlink_directory(&parent, name, &directory));
    failures.finish()
}

fn clear_directory(directory: &File, path: &Path, depth: usize, failures: &mut Failures) {
    if depth >= MAX_DEPTH {
        failures.record(
            path,
            Err(io::Error::other("Directory nesting exceeds cleanup limit")),
        );
        return;
    }
    // A separate stream descriptor avoids borrowing the traversal handle while
    // readdir advances. Dot entries must never participate in deletion.
    let entries = match Dir::openat(directory, c".", directory_flags(), Mode::empty()) {
        Ok(entries) => entries,
        Err(error) => {
            failures.record(path, Err(error.into()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.record(path, Err(error.into()));
                break;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        // Joined paths are diagnostic labels only, never filesystem operands.
        let child_path = path.join(OsStr::from_bytes(name.to_bytes()));
        remove_entry(directory, name, &child_path, depth + 1, failures);
    }
}

fn remove_entry(parent: &File, name: &CStr, path: &Path, depth: usize, failures: &mut Failures) {
    match openat(parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => {
            let directory = File::from(fd);
            clear_directory(&directory, path, depth, failures);
            failures.record(
                path,
                unlink_directory(parent, OsStr::from_bytes(name.to_bytes()), &directory),
            );
        }
        Err(Errno::ENOTDIR | Errno::ELOOP) => {
            // unlinkat without RemoveDir removes the entry itself, not its target.
            failures.record(
                path,
                unlinkat(parent, name, UnlinkatFlags::NoRemoveDir).map_err(Into::into),
            );
        }
        Err(error) => failures.record(path, Err(error.into())),
    }
}

fn unlink_directory(parent: &File, name: &OsStr, directory: &File) -> io::Result<()> {
    let opened = fstat(directory)?;
    let current = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if opened.st_dev != current.st_dev || opened.st_ino != current.st_ino {
        return Err(io::Error::other(
            "Directory identity changed before removal",
        ));
    }
    #[cfg(test)]
    before_rmdir::fire(name);
    unlinkat(parent, name, UnlinkatFlags::RemoveDir).map_err(Into::into)
}

/// Keep diagnostics bounded while remembering whether any non-transient error
/// occurred. Ancestor ENOTEMPTY errors must not hide a child's permanent error.
#[derive(Default)]
struct Failures {
    count: usize,
    permanent_kind: Option<io::ErrorKind>,
    details: String,
}

impl Failures {
    fn record(&mut self, path: &Path, result: io::Result<()>) {
        let Err(error) = result else { return };
        if error.kind() == io::ErrorKind::NotFound {
            return;
        }
        self.count += 1;
        if error.kind() != io::ErrorKind::DirectoryNotEmpty {
            self.permanent_kind.get_or_insert(error.kind());
        }
        if self.details.len() < MAX_ERROR_BYTES {
            let line = format!("\n{path:?}: {error} (errno={:?})", error.raw_os_error());
            let mut end = line.len().min(MAX_ERROR_BYTES - self.details.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            self.details.push_str(&line[..end]);
        }
    }

    fn finish(self) -> io::Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        let truncated = if self.details.len() >= MAX_ERROR_BYTES {
            " [error details truncated]"
        } else {
            ""
        };
        Err(io::Error::new(
            self.permanent_kind
                .unwrap_or(io::ErrorKind::DirectoryNotEmpty),
            format!(
                "Recursive deletion encountered {} errors:{}{}",
                self.count, self.details, truncated
            ),
        ))
    }
}

/// Coordinate late file creation after enumeration and before the real rmdir.
/// Thread-local guards keep parallel tests isolated and restore hooks on panic.
#[cfg(test)]
pub(super) mod before_rmdir {
    use std::cell::RefCell;
    use std::ffi::OsStr;

    type Hook = Box<dyn FnMut(&OsStr)>;
    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub struct Guard(Option<Hook>);

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }

    pub fn install(hook: impl FnMut(&OsStr) + 'static) -> Guard {
        Guard(HOOK.with(|slot| slot.replace(Some(Box::new(hook)))))
    }

    pub(super) fn fire(name: &OsStr) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(name);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn identity(path: &Path) -> DirectoryIdentity {
        let metadata = std::fs::metadata(path).unwrap();
        DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[test]
    fn removes_nested_files_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tree = root.path().join("trash with 'quotes' and $dollars");
        std::fs::create_dir_all(tree.join("a/b")).unwrap();
        std::fs::write(tree.join("a/b/file"), "remove").unwrap();
        std::fs::write(outside.path().join("sentinel"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), tree.join("link")).unwrap();
        std::os::unix::fs::symlink("missing", tree.join("broken-link")).unwrap();
        remove(&tree, identity(&tree)).unwrap();
        assert!(!tree.exists());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn rejects_a_different_root_and_accepts_an_absent_root() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sentinel"), "keep").unwrap();
        assert!(remove(root.path(), identity(other.path())).is_err());
        assert!(root.path().join("sentinel").exists());
        remove(&root.path().join("missing"), identity(root.path())).unwrap();
    }

    #[test]
    fn nesting_failure_does_not_prevent_sibling_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        let mut deep = tree.clone();
        for _ in 0..=MAX_DEPTH {
            deep.push("nested");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir(tree.join("unrelated-build")).unwrap();
        std::fs::write(tree.join("unrelated-build/artifact"), "remove").unwrap();
        let error = remove(&tree, identity(&tree)).unwrap_err();
        assert!(error.to_string().contains("nesting exceeds cleanup limit"));
        assert!(deep.exists());
        assert!(!tree.join("unrelated-build").exists());
    }

    #[test]
    fn rejects_a_symlink_quarantine_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel"), "keep").unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(remove(&link, identity(outside.path())).is_err());
        assert!(link.is_symlink());
        assert!(outside.path().join("sentinel").exists());
    }

    #[test]
    fn reports_permanent_errors_without_masking_them_with_nonempty_ancestors() {
        let mut failures = Failures::default();
        failures.record(Path::new("missing"), Err(io::ErrorKind::NotFound.into()));
        failures.record(
            Path::new("busy"),
            Err(io::ErrorKind::DirectoryNotEmpty.into()),
        );
        failures.record(
            Path::new("protected"),
            Err(io::ErrorKind::PermissionDenied.into()),
        );
        let error = failures.finish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("2 errors"));
        assert!(error.to_string().contains("protected"));
    }

    #[test]
    fn bounds_failure_details() {
        let mut failures = Failures::default();
        for _ in 0..1000 {
            failures.record(
                Path::new("entry"),
                Err(io::ErrorKind::DirectoryNotEmpty.into()),
            );
        }
        assert!(failures.details.len() <= MAX_ERROR_BYTES);
        assert!(
            failures
                .finish()
                .unwrap_err()
                .to_string()
                .contains("1000 errors")
        );
    }
}
