//! jj-backed implementation of [`WorkmuxMetaStore`].
//!
//! `GitConfigMetaStore` (see `crate::vcs::git_backend`) stores per-worktree
//! metadata as `workmux.worktree.<handle>.<key>` entries in `git config
//! --local`, which is inherently per-repository-copy: git config lives
//! alongside the `.git` directory workmux resolves for a given workdir. jj
//! has no equivalent "local config" concept scoped the same way, so
//! `JjMetaStore` instead keeps a single TOML file at
//! `<jj-common-store-dir>/workmux/metadata.toml`, where `<jj-common-store-dir>`
//! is the directory containing the *primary* workspace's `.jj/repo` (resolved
//! via [`JjRepositoryIdentity`], not assumed from a fixed relative path) —
//! this is the one location shared by every workspace of the same repository,
//! mirroring how git config on the main worktree is shared by every linked
//! worktree.
//!
//! Reads and writes go through `toml_edit` so the file stays hand-editable
//! and round-trips comments/formatting; writes are guarded by
//! `meta_lock::FileLock` on a `<file>.lock` sibling to serialize concurrent
//! workmux processes' read-modify-write cycles.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, value};

use super::WorkmuxMetaStore;
use super::jj_security::JjRepositoryIdentity;
use super::meta_lock::FileLock;

/// [`WorkmuxMetaStore`] implementation backed by a single TOML file in the
/// jj repository's common store directory. Stateless.
pub struct JjMetaStore;

fn resolve_workdir(workdir: Option<&Path>) -> Result<PathBuf> {
    match workdir {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_dir().context("Failed to determine current directory"),
    }
}

/// The directory containing the primary workspace's `.jj/repo`, i.e. the
/// stable location shared by every workspace of this repository, regardless
/// of which workspace `identity` was resolved from.
fn common_store_dir(identity: &JjRepositoryIdentity) -> Result<PathBuf> {
    let primary_dot_jj = identity.repo_dir.parent().ok_or_else(|| {
        anyhow!(
            "jj repo directory has no parent: {}",
            identity.repo_dir.display()
        )
    })?;
    let primary_root = primary_dot_jj.parent().ok_or_else(|| {
        anyhow!(
            "jj .jj directory has no parent: {}",
            primary_dot_jj.display()
        )
    })?;
    Ok(primary_root.to_path_buf())
}

/// The metadata file path for a given jj common-store directory.
fn metadata_file_in(common_store_dir: &Path) -> PathBuf {
    common_store_dir.join("workmux").join("metadata.toml")
}

/// Resolve the metadata file path for `workdir` (or the current directory)
/// by discovering its jj repository identity.
fn metadata_path_for_workdir(workdir: Option<&Path>) -> Result<PathBuf> {
    let dir = resolve_workdir(workdir)?;
    let identity = JjRepositoryIdentity::discover(&dir)?;
    let store_dir = common_store_dir(&identity)?;
    Ok(metadata_file_in(&store_dir))
}

fn lock_path_for(metadata_path: &Path) -> PathBuf {
    let mut os_string = metadata_path.as_os_str().to_owned();
    os_string.push(".lock");
    PathBuf::from(os_string)
}

/// Load the metadata document, defaulting to an empty document with
/// `schema_version = 1` if the file doesn't exist yet.
fn load_document(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        let mut doc = DocumentMut::new();
        doc["schema_version"] = value(1_i64);
        return Ok(doc);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read metadata file: {}", path.display()))?;
    content
        .parse::<DocumentMut>()
        .with_context(|| format!("Failed to parse metadata file: {}", path.display()))
}

fn save_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("Failed to write metadata file: {}", path.display()))
}

fn ensure_subtable<'a>(parent: &'a mut Table, key: &str) -> &'a mut Table {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .expect("just-inserted entry is a table")
}

/// Run `body` with a lock held on the metadata file at `path`, creating the
/// file's parent directory first if needed.
fn with_locked_document<T>(
    path: &Path,
    body: impl FnOnce(&mut DocumentMut) -> Result<T>,
) -> Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    let _lock = FileLock::acquire(&lock_path_for(path))?;
    let mut doc = load_document(path)?;
    let result = body(&mut doc)?;
    save_document(path, &doc)?;
    Ok(result)
}

impl WorkmuxMetaStore for JjMetaStore {
    fn get(&self, handle: &str, key: &str, workdir: Option<&Path>) -> Option<String> {
        let path = metadata_path_for_workdir(workdir).ok()?;
        if !path.exists() {
            return None;
        }
        // Take the lock for reads too, so a concurrent writer's
        // read-modify-write cycle is never observed half-written.
        let _lock = FileLock::acquire(&lock_path_for(&path)).ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let doc = content.parse::<DocumentMut>().ok()?;
        doc.get("worktree")?
            .as_table()?
            .get(handle)?
            .as_table()?
            .get(key)?
            .as_str()
            .map(str::to_string)
    }

    fn set(&self, handle: &str, key: &str, value: &str, workdir: Option<&Path>) -> Result<()> {
        let path = metadata_path_for_workdir(workdir)?;
        with_locked_document(&path, |doc| {
            let worktree = ensure_subtable(doc.as_table_mut(), "worktree");
            let handle_table = ensure_subtable(worktree, handle);
            handle_table[key] = toml_edit::value(value);
            Ok(())
        })
    }

    fn remove_all_at(&self, handle: &str, common_dir: &Path) -> Result<()> {
        let path = metadata_file_in(common_dir);
        if !path.exists() {
            return Ok(());
        }
        with_locked_document(&path, |doc| {
            if let Some(worktree) = doc.get_mut("worktree").and_then(Item::as_table_mut) {
                worktree.remove(handle);
            }
            Ok(())
        })
    }

    fn migrate(&self, old_handle: &str, new_handle: &str, workdir: Option<&Path>) -> Result<()> {
        if old_handle == new_handle {
            return Ok(());
        }
        let path = metadata_path_for_workdir(workdir)?;
        with_locked_document(&path, |doc| {
            if let Some(worktree) = doc.get_mut("worktree").and_then(Item::as_table_mut)
                && let Some(old_item) = worktree.remove(old_handle)
                && let Ok(old_table) = old_item.into_table()
            {
                // Match git's `migrate_worktree_meta` semantics: copy/overwrite
                // only the keys present in `old_handle` into `new_handle`,
                // leaving any pre-existing `new_handle` keys not present in
                // `old_handle` untouched (no wholesale table replacement).
                let new_table = ensure_subtable(worktree, new_handle);
                for (key, value) in old_table.iter() {
                    new_table.insert(key, value.clone());
                }
            }
            Ok(())
        })
    }

    fn get_branch_base(&self, branch: &str, workdir: Option<&Path>) -> Option<String> {
        let path = metadata_path_for_workdir(workdir).ok()?;
        if !path.exists() {
            return None;
        }
        let _lock = FileLock::acquire(&lock_path_for(&path)).ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let doc = content.parse::<DocumentMut>().ok()?;
        doc.get("branch_base")?
            .as_table()?
            .get(branch)?
            .as_table()?
            .get("base")?
            .as_str()
            .map(str::to_string)
    }

    fn set_branch_base(&self, branch: &str, base: &str, workdir: Option<&Path>) -> Result<()> {
        let path = metadata_path_for_workdir(workdir)?;
        with_locked_document(&path, |doc| {
            let branch_base = ensure_subtable(doc.as_table_mut(), "branch_base");
            let branch_table = ensure_subtable(branch_base, branch);
            branch_table["base"] = value(base);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{init_colocated_repo, init_jj_repo};
    use std::sync::Arc;

    fn store() -> JjMetaStore {
        JjMetaStore
    }

    #[test]
    fn get_returns_none_when_never_set() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        assert_eq!(store.get("handle", "attachment", Some(temp.path())), None);
    }

    #[test]
    fn set_get_round_trip_jj_only() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        store
            .set("feature", "attachment", "headless", Some(temp.path()))
            .unwrap();
        store
            .set("feature", "window-token", "abc123", Some(temp.path()))
            .unwrap();
        assert_eq!(
            store.get("feature", "attachment", Some(temp.path())),
            Some("headless".to_string())
        );
        assert_eq!(
            store.get("feature", "window-token", Some(temp.path())),
            Some("abc123".to_string())
        );
        // Unset key on a handle that does have other keys set.
        assert_eq!(store.get("feature", "mode", Some(temp.path())), None);
    }

    #[test]
    fn set_get_round_trip_colocated() {
        let temp = tempfile::tempdir().unwrap();
        init_colocated_repo(temp.path());
        let store = store();
        store
            .set("feature", "mode", "session", Some(temp.path()))
            .unwrap();
        assert_eq!(
            store.get("feature", "mode", Some(temp.path())),
            Some("session".to_string())
        );
    }

    #[test]
    fn metadata_file_lives_under_common_store_dir() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        store
            .set("feature", "attachment", "headless", Some(temp.path()))
            .unwrap();
        let expected = temp.path().join("workmux").join("metadata.toml");
        assert!(
            expected.is_file(),
            "expected {} to exist",
            expected.display()
        );
        let content = std::fs::read_to_string(&expected).unwrap();
        assert!(content.contains("schema_version"));
        assert!(
            content.contains("[worktree.feature]") || content.contains("[worktree.\"feature\"]")
        );
    }

    #[test]
    fn remove_all_at_removes_only_target_handle() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        store
            .set("feature-a", "attachment", "headless", Some(temp.path()))
            .unwrap();
        store
            .set("feature-b", "attachment", "multiplexer", Some(temp.path()))
            .unwrap();

        let identity = JjRepositoryIdentity::discover(temp.path()).unwrap();
        let common_dir = common_store_dir(&identity).unwrap();
        store.remove_all_at("feature-a", &common_dir).unwrap();

        assert_eq!(
            store.get("feature-a", "attachment", Some(temp.path())),
            None
        );
        assert_eq!(
            store.get("feature-b", "attachment", Some(temp.path())),
            Some("multiplexer".to_string())
        );
    }

    #[test]
    fn remove_all_at_is_noop_when_file_missing() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let identity = JjRepositoryIdentity::discover(temp.path()).unwrap();
        let common_dir = common_store_dir(&identity).unwrap();
        let store = store();
        store.remove_all_at("nonexistent", &common_dir).unwrap();
    }

    #[test]
    fn migrate_preserves_values_under_new_handle() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        store
            .set("old-handle", "attachment", "headless", Some(temp.path()))
            .unwrap();
        store
            .set("old-handle", "window-token", "tok", Some(temp.path()))
            .unwrap();
        store
            .set("other", "attachment", "multiplexer", Some(temp.path()))
            .unwrap();

        store
            .migrate("old-handle", "new-handle", Some(temp.path()))
            .unwrap();

        assert_eq!(
            store.get("old-handle", "attachment", Some(temp.path())),
            None
        );
        assert_eq!(
            store.get("new-handle", "attachment", Some(temp.path())),
            Some("headless".to_string())
        );
        assert_eq!(
            store.get("new-handle", "window-token", Some(temp.path())),
            Some("tok".to_string())
        );
        assert_eq!(
            store.get("other", "attachment", Some(temp.path())),
            Some("multiplexer".to_string())
        );
    }

    #[test]
    fn migrate_merges_into_existing_new_handle_keys() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        // Pre-existing metadata at the destination handle, under keys that
        // are NOT present in the source handle's table.
        store
            .set(
                "new-handle",
                "window-token",
                "preexisting-token",
                Some(temp.path()),
            )
            .unwrap();
        store
            .set("new-handle", "mode", "session", Some(temp.path()))
            .unwrap();
        // Source handle has an overlapping key ("mode") and a non-overlapping
        // key ("attachment").
        store
            .set("old-handle", "attachment", "headless", Some(temp.path()))
            .unwrap();
        store
            .set("old-handle", "mode", "detached", Some(temp.path()))
            .unwrap();

        store
            .migrate("old-handle", "new-handle", Some(temp.path()))
            .unwrap();

        // old-handle is gone.
        assert_eq!(
            store.get("old-handle", "attachment", Some(temp.path())),
            None
        );
        assert_eq!(store.get("old-handle", "mode", Some(temp.path())), None);

        // new-handle has old-handle's keys, with "mode" overwritten by
        // old-handle's value...
        assert_eq!(
            store.get("new-handle", "attachment", Some(temp.path())),
            Some("headless".to_string())
        );
        assert_eq!(
            store.get("new-handle", "mode", Some(temp.path())),
            Some("detached".to_string())
        );
        // ...but the pre-existing new-handle-only key is untouched.
        assert_eq!(
            store.get("new-handle", "window-token", Some(temp.path())),
            Some("preexisting-token".to_string())
        );
    }

    #[test]
    fn migrate_same_handle_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        store
            .set("handle", "attachment", "headless", Some(temp.path()))
            .unwrap();
        store
            .migrate("handle", "handle", Some(temp.path()))
            .unwrap();
        assert_eq!(
            store.get("handle", "attachment", Some(temp.path())),
            Some("headless".to_string())
        );
    }

    #[test]
    fn migrate_missing_old_handle_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        store
            .set("other", "attachment", "headless", Some(temp.path()))
            .unwrap();
        store
            .migrate("missing", "also-missing", Some(temp.path()))
            .unwrap();
        assert_eq!(
            store.get("other", "attachment", Some(temp.path())),
            Some("headless".to_string())
        );
    }

    #[test]
    fn branch_base_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = store();
        assert_eq!(store.get_branch_base("feature", Some(temp.path())), None);
        store
            .set_branch_base("feature", "main", Some(temp.path()))
            .unwrap();
        assert_eq!(
            store.get_branch_base("feature", Some(temp.path())),
            Some("main".to_string())
        );
    }

    #[test]
    fn concurrent_sets_do_not_corrupt_or_lose_writes() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let store = Arc::new(store());
        let root = temp.path().to_path_buf();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let store = Arc::clone(&store);
                let root = root.clone();
                std::thread::spawn(move || {
                    let handle = format!("handle-{i}");
                    let key = format!("key-{i}");
                    let value = format!("value-{i}");
                    store.set(&handle, &key, &value, Some(&root)).unwrap();
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 0..8 {
            let handle = format!("handle-{i}");
            let key = format!("key-{i}");
            let value = format!("value-{i}");
            assert_eq!(
                store.get(&handle, &key, Some(&root)),
                Some(value),
                "lost or corrupted write for {handle}"
            );
        }
    }
}
