use crate::vcs;
use anyhow::{Context, Result};

pub fn run(name: &str) -> Result<()> {
    let vcs = vcs::detect_vcs()?;
    // Smart resolution: try handle first, then branch name
    let (path, _branch) = vcs.find_workspace(name).with_context(|| {
        format!(
            "No workspace found with name '{}'. Use 'workmux list' to see available workspaces.",
            name
        )
    })?;
    println!("{}", path.display());
    Ok(())
}
