//! Bundled skill installation.
//!
//! Embeds all workmux SKILL.md files at compile time and writes them
//! to the appropriate platform-specific skills directories.

use anyhow::{Context, Result};
use console::style;
use similar::{ChangeTag, TextDiff};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::agent_setup::Agent;

pub struct BundledSkill {
    pub name: &'static str,
    pub content: &'static str,
}

pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "merge",
        content: include_str!("../skills/merge/SKILL.md"),
    },
    BundledSkill {
        name: "rebase",
        content: include_str!("../skills/rebase/SKILL.md"),
    },
    BundledSkill {
        name: "worktree",
        content: include_str!("../skills/worktree/SKILL.md"),
    },
    BundledSkill {
        name: "coordinator",
        content: include_str!("../skills/coordinator/SKILL.md"),
    },
    BundledSkill {
        name: "open-pr",
        content: include_str!("../skills/open-pr/SKILL.md"),
    },
    BundledSkill {
        name: "workmux",
        content: include_str!("../skills/workmux/SKILL.md"),
    },
];

/// Return the skills base directory for a given agent.
/// Returns None if the agent doesn't support skills.
pub fn skills_dir(agent: Agent) -> Option<PathBuf> {
    let home = home::home_dir()?;
    match agent {
        Agent::Claude => {
            let base = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"));
            Some(base.join("skills"))
        }
        Agent::OpenCode => Some(home.join(".config/opencode/skills")),
        Agent::Pi => {
            let pi_dir = if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
                PathBuf::from(dir)
            } else {
                home.join(".pi/agent")
            };
            Some(pi_dir.join("skills"))
        }
        Agent::Codex | Agent::Copilot | Agent::Gemini => None,
    }
}

/// Check if any bundled skills are missing for the given agent.
pub fn needs_install(agent: Agent) -> bool {
    let Some(base_dir) = skills_dir(agent) else {
        return false;
    };

    BUNDLED_SKILLS
        .iter()
        .any(|skill| !base_dir.join(skill.name).join("SKILL.md").exists())
}

enum InstallOutcome {
    Installed,
    AlreadyUpToDate,
    Updated,
    Skipped,
}

/// Install all bundled skills to the given agent's skills directory.
pub fn install_skills(agent: Agent) -> Result<String> {
    let Some(base_dir) = skills_dir(agent) else {
        return Ok(format!("{} does not support skills", agent.name()));
    };

    let mut installed = 0u32;
    let mut up_to_date = 0u32;
    let mut updated = 0u32;
    let mut skipped = 0u32;

    for skill in BUNDLED_SKILLS {
        let dir = base_dir.join(skill.name);
        let path = dir.join("SKILL.md");

        let outcome = if path.exists() {
            let existing = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;

            if existing == skill.content {
                InstallOutcome::AlreadyUpToDate
            } else {
                print_skill_diff(skill.name, &existing, skill.content);
                if confirm_overwrite(skill.name)? {
                    fs::write(&path, skill.content)
                        .with_context(|| format!("Failed to write {}", path.display()))?;
                    println!("  {} updated {}/SKILL.md", style("✓").green(), skill.name);
                    InstallOutcome::Updated
                } else {
                    InstallOutcome::Skipped
                }
            }
        } else {
            fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
            fs::write(&path, skill.content)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            println!("  {} installed {}/SKILL.md", style("✓").green(), skill.name);
            InstallOutcome::Installed
        };

        match outcome {
            InstallOutcome::Installed => installed += 1,
            InstallOutcome::AlreadyUpToDate => up_to_date += 1,
            InstallOutcome::Updated => updated += 1,
            InstallOutcome::Skipped => skipped += 1,
        }
    }

    let mut parts = Vec::new();
    if installed > 0 {
        parts.push(format!("{installed} installed"));
    }
    if updated > 0 {
        parts.push(format!("{updated} updated"));
    }
    if up_to_date > 0 {
        parts.push(format!("{up_to_date} up to date"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }

    Ok(format!(
        "Skills for {} ({}): {}",
        agent.name(),
        base_dir.display(),
        parts.join(", ")
    ))
}

fn print_skill_diff(name: &str, old: &str, new: &str) {
    println!();
    println!(
        "  {} {}/SKILL.md differs from bundled version:",
        style("~").yellow(),
        name
    );
    println!();

    let diff = TextDiff::from_lines(old, new);
    for (idx, group) in diff.grouped_ops(3).iter().enumerate() {
        if idx > 0 {
            println!("    {}", style("~~~").dim());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = change.value().trim_end_matches('\n');
                match change.tag() {
                    ChangeTag::Insert => {
                        println!("    {}", style(format!("+{line}")).green());
                    }
                    ChangeTag::Delete => {
                        println!("    {}", style(format!("-{line}")).red());
                    }
                    ChangeTag::Equal => {
                        println!("    {}", style(format!(" {line}")).dim());
                    }
                }
            }
        }
    }
    println!();
}

fn confirm_overwrite(name: &str) -> Result<bool> {
    let prompt = format!(
        "  Overwrite {}/SKILL.md with bundled version? {}{}{} ",
        name,
        style("[").bold().cyan(),
        style("y/N").bold(),
        style("]").bold().cyan(),
    );

    loop {
        print!("{}", prompt);
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_lowercase();

        match answer.as_str() {
            "" | "n" | "no" => return Ok(false),
            "y" | "yes" => return Ok(true),
            _ => println!("    {}", style("Please enter y or n").dim()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_skills_not_empty() {
        assert_eq!(BUNDLED_SKILLS.len(), 6);
        for skill in BUNDLED_SKILLS {
            assert!(!skill.name.is_empty(), "skill name should not be empty");
            assert!(
                !skill.content.is_empty(),
                "skill {} content should not be empty",
                skill.name
            );
            assert!(
                skill.content.starts_with("---"),
                "skill {} should start with YAML frontmatter",
                skill.name
            );
        }
    }

    #[test]
    fn test_skills_dir_claude() {
        // Without CLAUDE_CONFIG_DIR, this resolves to $HOME/.claude/skills.
        // With it set, the env var should win. We can't safely mutate process
        // env in parallel tests, so just exercise the unset-or-set branch
        // generically and assert the trailing component.
        let dir = skills_dir(Agent::Claude);
        assert!(dir.is_some());
        let path = dir.unwrap();
        assert!(path.ends_with("skills"));
    }

    #[test]
    fn test_skills_dir_claude_respects_env() {
        let mut process = crate::test_support::process_state().unwrap();
        process.set_env("CLAUDE_CONFIG_DIR", "/tmp/workmux-test-claude-cfg");

        let dir = skills_dir(Agent::Claude).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/workmux-test-claude-cfg/skills"));
    }

    #[test]
    fn test_skills_dir_opencode() {
        let dir = skills_dir(Agent::OpenCode);
        assert!(dir.is_some());
    }

    #[test]
    fn test_skills_dir_pi() {
        let dir = skills_dir(Agent::Pi);
        assert!(dir.is_some());
        let path = dir.unwrap();
        assert!(path.ends_with(".pi/agent/skills"));
    }

    #[test]
    fn test_skills_dir_codex_none() {
        assert!(skills_dir(Agent::Codex).is_none());
    }

    #[test]
    fn test_skills_dir_copilot_none() {
        assert!(skills_dir(Agent::Copilot).is_none());
    }

    #[test]
    fn test_bundled_skill_names() {
        let names: Vec<_> = BUNDLED_SKILLS.iter().map(|s| s.name).collect();
        assert!(names.contains(&"merge"));
        assert!(names.contains(&"rebase"));
        assert!(names.contains(&"worktree"));
        assert!(names.contains(&"coordinator"));
        assert!(names.contains(&"open-pr"));
        assert!(names.contains(&"workmux"));
    }
}
