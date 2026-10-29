use anyhow::{Context, Result, anyhow};

use crate::{config, git};

pub fn run(name: Option<&str>) -> Result<()> {
    let name_to_rebase = super::resolve_name(name)?;
    let config = config::Config::load(None)?;
    if let Some(record) = crate::remote::cache::find_here(&name_to_rebase) {
        return crate::remote::homecoming::rebase(&config, record);
    }

    let (worktree_path, branch_to_rebase) = git::find_worktree(&name_to_rebase).map_err(|_| {
        anyhow!(
            "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
            name_to_rebase
        )
    })?;

    let main_branch = if let Some(ref branch) = config.main_branch {
        branch.clone()
    } else {
        let main_root = git::get_main_worktree_root_in(Some(&worktree_path))
            .context("Could not find the main git worktree")?;
        git::get_default_branch_in(Some(&main_root))
            .context("Failed to determine the main branch")?
    };

    let base_branch = match git::get_branch_base_in(&branch_to_rebase, Some(&worktree_path)) {
        Ok(base) if git::local_branch_exists_in(&base, Some(&worktree_path))? => base,
        _ => main_branch,
    };

    if branch_to_rebase == base_branch {
        return Err(anyhow!(
            "Cannot rebase branch '{}' onto itself.",
            branch_to_rebase
        ));
    }

    if !git::local_branch_exists_in(&base_branch, Some(&worktree_path))? {
        return Err(anyhow!("Base branch '{}' does not exist", base_branch));
    }

    println!("Rebasing '{}' onto '{}'...", branch_to_rebase, base_branch);
    git::rebase_branch_onto_base(&worktree_path, &base_branch).with_context(|| {
        format!(
            "Rebase failed, likely due to conflicts.\n\n\
            Please resolve them manually inside the worktree at '{}'.\n\
            Then, run 'git rebase --continue' to proceed or 'git rebase --abort' to cancel.",
            worktree_path.display()
        )
    })?;
    println!("✓ Rebased '{}' onto '{}'", branch_to_rebase, base_branch);

    Ok(())
}
