//! In-memory comparison reuse for background Git collection.
//!
//! Dependencies are observed at lookup, not atomically. The non-sliding maximum
//! age bounds reuse for ignored attributes, installation-specific system attributes,
//! Git upgrades, and concurrent mutations not captured by those observations.
//! Expiry does not schedule refreshes or bound the age of a published display.

use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::security::RepositoryIdentity;

const MAX_AGE: Duration = Duration::from_secs(30);

pub(super) fn output(path: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut command = super::pinned_git(path)?;
    Ok(command.arg("--no-optional-locks").args(args).output()?)
}

pub(super) fn capture(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = output(path, args)?;
    if !output.status.success() {
        bail!("Git dependency query failed: {:?}", output.status.code());
    }
    Ok(output.stdout)
}

type FileContents = (PathBuf, Option<Vec<u8>>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Dependencies {
    identity: RepositoryIdentity,
    filesystem_identity: Vec<(u64, u64)>,
    config: Vec<u8>,
    global_attribute: Option<PathBuf>,
    files: Vec<FileContents>,
    attribute_index: Vec<u8>,
    branch: String,
    main_branch: Option<String>,
}

fn read_file(path: &Path, files: &mut Vec<FileContents>) -> Result<()> {
    let content = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    files.push((path.to_path_buf(), content));
    Ok(())
}

fn read_directory(path: &Path, files: &mut Vec<FileContents>) -> Result<()> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            files.push((path.to_path_buf(), None));
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            read_directory(&entry.path(), files)?;
        } else {
            read_file(&entry.path(), files)?;
        }
    }
    Ok(())
}

impl Dependencies {
    pub(super) fn observe(
        identity: RepositoryIdentity,
        branch: &str,
        main_branch: Option<&str>,
        untracked_attributes: &[PathBuf],
        previous: Option<&Self>,
    ) -> Result<Self> {
        let path = &identity.worktree;
        let config = capture(path, &["config", "--null", "--list", "--includes"])?;
        // Only attribute entries, not index stat data: ordinary index refreshes
        // must not invalidate committed comparisons.
        let attribute_index = capture(
            path,
            &[
                "ls-files",
                "--stage",
                "-z",
                "--",
                ".gitattributes",
                "**/.gitattributes",
            ],
        )?;
        let mut attributes: HashSet<PathBuf> = untracked_attributes.iter().cloned().collect();
        for entry in attribute_index.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                bail!("Malformed attribute index entry");
            };
            attributes.insert(PathBuf::from(OsString::from_vec(entry[tab + 1..].to_vec())));
        }
        let mut files = Vec::new();
        for attribute in attributes {
            read_file(&path.join(attribute), &mut files)?;
        }
        read_file(&identity.common_dir.join("info/attributes"), &mut files)?;
        // Path expansion is configuration-dependent; reuse it while the effective
        // configuration and worktree are unchanged, but always reread the file.
        let global_attribute = if let Some(previous) =
            previous.filter(|old| old.config == config && old.identity == identity)
        {
            previous.global_attribute.clone()
        } else {
            let global = output(path, &["config", "--path", "--get", "core.attributesFile"])?;
            match global.status.code() {
                Some(0) => {
                    let bytes = global.stdout.strip_suffix(b"\n").unwrap_or(&global.stdout);
                    Some(path.join(PathBuf::from(OsString::from_vec(bytes.to_vec()))))
                }
                Some(1) => std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
                    .map(|home| home.join("git/attributes")),
                _ => bail!("Failed to resolve global attributes"),
            }
        };
        if let Some(attribute) = &global_attribute {
            read_file(attribute, &mut files)?;
        }
        for name in ["packed-refs", "shallow", "info/grafts"] {
            read_file(&identity.common_dir.join(name), &mut files)?;
        }
        // Includes replacement refs and default-branch discovery inputs. Reftable
        // repositories store the same dependencies outside loose/packed refs.
        read_directory(&identity.common_dir.join("refs"), &mut files)?;
        read_directory(&identity.common_dir.join("reftable"), &mut files)?;
        if identity.admin_dir != identity.common_dir {
            read_directory(&identity.admin_dir.join("refs"), &mut files)?;
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let filesystem_identity = [
            &identity.worktree,
            &identity.admin_dir,
            &identity.common_dir,
            &identity.dot_git,
        ]
        .into_iter()
        .map(|p| {
            let metadata = std::fs::metadata(p)?;
            Ok((metadata.dev(), metadata.ino()))
        })
        .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            identity,
            filesystem_identity,
            config,
            global_attribute,
            files,
            attribute_index,
            branch: branch.to_owned(),
            main_branch: main_branch.map(str::to_owned),
        })
    }

    pub(super) fn merge_cache_eligible(&self, head: &str, base: &str) -> Result<bool> {
        // External drivers and renormalization filters can depend on outside inputs and
        // on branch labels. Keep their original arguments and execution frequency.
        for entry in self.config.split(|b| *b == 0) {
            let key = entry.split(|b| *b == b'\n').next().unwrap_or_default();
            if (key.starts_with(b"merge.") && key.ends_with(b".driver"))
                || key == b"merge.renormalize"
            {
                return Ok(false);
            }
        }
        // Submodule mergeability depends on subordinate repositories and history.
        for oid in [head, base] {
            let tree = capture(&self.identity.worktree, &["ls-tree", "-r", "-z", oid])?;
            if tree
                .split(|b| *b == 0)
                .any(|entry| entry.starts_with(b"160000 "))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub(super) struct Entry {
    pub dependencies: Dependencies,
    pub observed_at: Instant,
    pub base_branch: String,
    pub head: String,
    pub base: String,
    pub committed: Option<(usize, usize)>,
    pub conflict: Option<bool>,
    pub merge_eligible: bool,
}

/// Ephemeral successful Git computations, scoped to active worktrees.
#[derive(Default)]
pub struct GitStatusCache {
    pub(super) entries: HashMap<PathBuf, Entry>,
}

impl GitStatusCache {
    pub fn retain(&mut self, roots: &HashSet<PathBuf>) {
        self.entries.retain(|path, _| roots.contains(path));
    }

    pub(super) fn live(&self, root: &Path, now: Instant) -> Option<&Entry> {
        self.entries
            .get(root)
            .filter(|entry| now.duration_since(entry.observed_at) < MAX_AGE)
    }
}
