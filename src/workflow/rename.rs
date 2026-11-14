use anyhow::{Context, Result, anyhow};
use regex::Regex;
use tracing::{info, warn};

use crate::config::MuxMode;
use crate::multiplexer::util::prefixed;
use crate::state::StateStore;
use crate::util::canon_or_self;
use crate::{git, naming};

use super::context::WorkflowContext;
use super::types::RenameResult;

/// Rename a worktree, its tmux window/session, per-worktree git metadata,
/// agent state files, and (optionally) the branch.
pub fn rename(
    user_target: &str,
    new_name: &str,
    rename_branch: bool,
    context: &WorkflowContext,
) -> Result<RenameResult> {
    // 1. Resolve source worktree. `user_target` may be a handle OR a branch;
    //    `find_worktree` handles both. Always derive the authoritative handle
    //    from the worktree's directory basename to keep metadata/tmux/state
    //    migrations consistent regardless of what the user typed.
    let (old_path, branch_name) = git::find_worktree(user_target).with_context(|| {
        format!(
            "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
            user_target
        )
    })?;

    let old_handle = old_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Could not derive handle from worktree path: {}",
                old_path.display()
            )
        })?
        .to_string();

    // 2. Reject main worktree
    if old_path == context.main_worktree_root {
        return Err(anyhow!("Cannot rename the main worktree"));
    }

    // 3. Derive new handle (slugify + validate). Treat new_name as an explicit
    //    handle override, matching `add --name` semantics: prefix is bypassed,
    //    slugify still runs to keep it filesystem/tmux safe.
    let new_handle = naming::derive_handle(&branch_name, Some(new_name), &context.config)?;

    if new_handle == old_handle && !rename_branch {
        return Err(anyhow!(
            "Nothing to rename: new name '{}' matches current handle",
            new_handle
        ));
    }

    // 4. Detached HEAD + branch rename is nonsensical
    if rename_branch && branch_name == "(detached)" {
        return Err(anyhow!(
            "Cannot rename the branch of a detached HEAD worktree. \
             Omit --branch to rename only the worktree and tmux window."
        ));
    }

    // 5. Determine new worktree path
    let parent = old_path
        .parent()
        .ok_or_else(|| anyhow!("Cannot determine parent directory of worktree"))?;
    let new_path = parent.join(&new_handle);

    let new_branch = if rename_branch {
        // Keep it simple: the new branch name equals the new handle.
        // Users wanting asymmetric names can run `git branch -m` separately.
        Some(new_handle.clone())
    } else {
        None
    };

    // 6. Collision preflight
    if new_handle != old_handle {
        if new_path.exists() {
            return Err(anyhow!(
                "Target path already exists: {}",
                new_path.display()
            ));
        }
        if git::find_worktree(&new_handle).is_ok() {
            return Err(anyhow!(
                "Another worktree with handle '{}' already exists",
                new_handle
            ));
        }
    }

    if let Some(ref b) = new_branch
        && b != &branch_name
        && git::branch_exists(b).unwrap_or(false)
    {
        return Err(anyhow!("Branch '{}' already exists", b));
    }

    // 7. tmux target collision check (only if handle is changing)
    let mode = git::get_worktree_mode(&old_handle);
    let attachment = git::get_worktree_attachment_in(&old_handle, Some(&context.execution_dir));
    let old_full = prefixed(&context.prefix, &old_handle);
    let new_full = prefixed(&context.prefix, &new_handle);
    let mux_running = context.mux.is_running().unwrap_or(false);

    if mux_running && attachment.manages_mux() && new_handle != old_handle {
        match mode {
            MuxMode::Session => {
                if context.mux.session_exists(&new_full)? {
                    return Err(anyhow!("tmux session '{}' already exists", new_full));
                }
            }
            MuxMode::Window => {
                let all = context.mux.get_all_window_names()?;
                let re = duplicate_name_regex(&new_full);
                if all.iter().any(|w| re.is_match(w)) {
                    return Err(anyhow!(
                        "tmux window '{}' (or a numbered duplicate) already exists",
                        new_full
                    ));
                }
            }
        }
    }

    info!(
        old_handle = %old_handle,
        new_handle = %new_handle,
        rename_branch,
        "rename:starting"
    );

    // 8. Capture the old canonical path before we move it. After `git worktree
    //    move`, `canonicalize(old_path)` would fail and we'd be unable to
    //    match stored agent workdirs (which are usually canonicalized).
    let old_canonical = canon_or_self(&old_path);

    // 9. Change to safe CWD before filesystem ops. If we're running from inside
    //    the worktree being moved, we'd otherwise lose our CWD.
    context.chdir_to_main_worktree()?;

    // 10. Migrate metadata first so a successful path move leaves attachment
    // state keyed by the resulting handle.
    if new_handle != old_handle {
        git::migrate_worktree_meta(&old_handle, &new_handle)
            .context("Failed to migrate worktree metadata")?;
    }

    // 11. Execute: git worktree move
    if new_handle != old_handle
        && let Err(error) = git::move_worktree(&old_path, &new_path)
    {
        return match git::migrate_worktree_meta(&new_handle, &old_handle) {
            Ok(()) => Err(error).context("Failed to move worktree (is the directory in use?)"),
            Err(rollback_error) => Err(error).context(format!(
                "Failed to move worktree and restore its metadata: {rollback_error:#}"
            )),
        };
    }
    if new_handle != old_handle {
        info!(from = %old_path.display(), to = %new_path.display(), "rename:worktree moved");
    }

    // 12. Execute: git branch rename
    if let Some(ref nb) = new_branch
        && nb != &branch_name
    {
        git::rename_branch(&branch_name, nb)?;
        info!(old = branch_name, new = nb, "rename:branch renamed");
    }

    // 13. Rename tmux window(s)/session
    let mut tmux_renamed = 0;
    if mux_running && attachment.manages_mux() && new_handle != old_handle {
        match mode {
            MuxMode::Session => {
                if context.mux.session_exists(&old_full).unwrap_or(false) {
                    match context.mux.rename_session(&old_full, &new_full) {
                        Ok(()) => {
                            tmux_renamed += 1;
                            info!(old = %old_full, new = %new_full, "rename:session renamed");
                        }
                        Err(e) => {
                            warn!(error = %e, "rename:failed to rename tmux session");
                        }
                    }
                }
            }
            MuxMode::Window => {
                let all = context.mux.get_all_window_names().unwrap_or_default();
                let re = duplicate_name_regex(&old_full);
                let matches: Vec<String> = all.into_iter().filter(|w| re.is_match(w)).collect();
                for old_name in &matches {
                    let new_name = remap_duplicate_name(old_name, &old_full, &new_full);
                    if let Err(e) = context.mux.rename_window(old_name, &new_name) {
                        warn!(window = old_name, error = %e, "rename:tmux rename_window failed");
                    } else {
                        tmux_renamed += 1;
                        info!(old = old_name, new = new_name, "rename:window renamed");
                    }
                }
            }
        }
    }

    // 14. Migrate agent state files + container markers (best-effort)
    let agents_migrated = match StateStore::new() {
        Ok(store) => {
            let migrated = store
                .migrate_worktree_paths(&old_canonical, &new_path, &old_full, &new_full)
                .unwrap_or_else(|e| {
                    warn!(error = %e, "rename:failed to migrate agent state");
                    0
                });
            if new_handle != old_handle
                && let Err(e) = store.migrate_container_handle(&old_handle, &new_handle)
            {
                warn!(error = %e, "rename:failed to migrate container markers");
            }
            migrated
        }
        Err(e) => {
            warn!(error = %e, "rename:state store unavailable, skipping state migration");
            0
        }
    };

    Ok(RenameResult {
        old_path,
        new_path,
        old_handle,
        new_handle,
        old_branch: branch_name,
        new_branch,
        tmux_renamed,
        agents_migrated,
    })
}

/// Build a regex that matches `base` or `base-<digits>`.
fn duplicate_name_regex(base: &str) -> Regex {
    let pattern = format!(r"^{}(-\d+)?$", regex::escape(base));
    Regex::new(&pattern).expect("static regex pattern")
}

/// Rename a window name that may carry a numeric `-N` duplicate suffix.
fn remap_duplicate_name(name: &str, old_base: &str, new_base: &str) -> String {
    if name == old_base {
        return new_base.to_string();
    }
    if let Some(suffix) = name.strip_prefix(&format!("{}-", old_base))
        && !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}-{}", new_base, suffix);
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_exact_match() {
        assert_eq!(remap_duplicate_name("wm-old", "wm-old", "wm-new"), "wm-new");
    }

    #[test]
    fn remap_numeric_suffix() {
        assert_eq!(
            remap_duplicate_name("wm-old-2", "wm-old", "wm-new"),
            "wm-new-2"
        );
        assert_eq!(
            remap_duplicate_name("wm-old-42", "wm-old", "wm-new"),
            "wm-new-42"
        );
    }

    #[test]
    fn remap_non_matching_unchanged() {
        assert_eq!(remap_duplicate_name("other", "wm-old", "wm-new"), "other");
        // Non-numeric suffix: not a duplicate pattern
        assert_eq!(
            remap_duplicate_name("wm-old-abc", "wm-old", "wm-new"),
            "wm-old-abc"
        );
    }

    #[test]
    fn duplicate_regex_matches_base_and_suffixes() {
        let re = duplicate_name_regex("wm-feature");
        assert!(re.is_match("wm-feature"));
        assert!(re.is_match("wm-feature-2"));
        assert!(re.is_match("wm-feature-99"));
        assert!(!re.is_match("wm-feature-abc"));
        assert!(!re.is_match("wm-feature-x"));
        assert!(!re.is_match("wm-feature2"));
        assert!(!re.is_match("other"));
    }
}
