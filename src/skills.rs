//! Bundled skill installation.
//!
//! Embeds all workmux SKILL.md files at compile time and writes them
//! to the appropriate platform-specific skills directories.

use anyhow::{Context, Result};
use console::style;
use similar::{ChangeTag, TextDiff};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    skills_dir_with_env(agent, &home, |key| std::env::var_os(key))
}

fn skills_dir_with_env(
    agent: Agent,
    home: &Path,
    get_env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    match agent {
        Agent::Claude => {
            let base = get_env("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"));
            Some(base.join("skills"))
        }
        Agent::OpenCode => Some(home.join(".config/opencode/skills")),
        Agent::Pi => {
            let pi_dir = get_env("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".pi/agent"));
            Some(pi_dir.join("skills"))
        }
        Agent::Omp => {
            Some(crate::agent_setup::omp::omp_agent_dir_with_env(home, get_env).join("skills"))
        }
        Agent::Antigravity | Agent::Codex | Agent::Copilot | Agent::Gemini => None,
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

/// Remove bundled skills for the given agent.
///
/// Only removes skill directories whose contents match the bundled
/// version exactly. Modified skills are left in place.
pub fn remove_skills(agent: Agent) -> Result<String> {
    let Some(base_dir) = skills_dir(agent) else {
        return Ok(format!("{} does not support skills", agent.name()));
    };
    remove_skills_at(base_dir, agent.name())
}

fn remove_skills_at(base_dir: PathBuf, agent_name: &str) -> Result<String> {
    if !base_dir.exists() {
        return Ok(format!(
            "No skills directory found at {}",
            base_dir.display()
        ));
    }

    let mut removed = 0u32;
    let mut skipped = 0u32;

    for skill in BUNDLED_SKILLS {
        let path = base_dir.join(skill.name).join("SKILL.md");
        if !path.exists() {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(content) if content == skill.content => {
                let dir = path.parent().unwrap();
                fs::remove_dir_all(dir)?;
                removed += 1;
            }
            Ok(_) => {
                // User-modified skill, leave it
                skipped += 1;
            }
            Err(_) => continue,
        }
    }

    // Remove skills base dir if empty
    if base_dir.read_dir().is_ok_and(|mut it| it.next().is_none()) {
        let _ = fs::remove_dir(&base_dir);
    }

    let mut parts = Vec::new();
    if removed > 0 {
        parts.push(format!("{removed} removed"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped (modified)"));
    }
    if removed == 0 && skipped == 0 {
        parts.push("none found".to_string());
    }

    Ok(format!(
        "Skills for {} ({}): {}",
        agent_name,
        base_dir.display(),
        parts.join(", ")
    ))
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
        // Safety: serial within this test; we restore the original value.
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        // SAFETY: tests in this module that read CLAUDE_CONFIG_DIR are
        // intentionally isolated; cargo test runs may interleave, but no
        // other test in this crate mutates this var.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/workmux-test-claude-cfg");
        }
        let dir = skills_dir(Agent::Claude).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/workmux-test-claude-cfg/skills"));
        match prev {
            Some(v) => unsafe {
                std::env::set_var("CLAUDE_CONFIG_DIR", v);
            },
            None => unsafe {
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            },
        }
    }

    #[test]
    fn test_skills_dir_opencode() {
        let dir = skills_dir(Agent::OpenCode);
        assert!(dir.is_some());
    }

    #[test]
    fn test_skills_dir_pi() {
        let dir = skills_dir_with_env(Agent::Pi, Path::new("/home/test"), |_| None).unwrap();

        assert_eq!(dir, PathBuf::from("/home/test/.pi/agent/skills"));
    }

    #[test]
    fn test_skills_dir_omp() {
        let dir = skills_dir_with_env(Agent::Omp, Path::new("/home/test"), |_| None).unwrap();

        assert_eq!(dir, PathBuf::from("/home/test/.omp/agent/skills"));
    }

    #[test]
    fn test_skills_dir_omp_respects_pi_config_dir() {
        let dir = skills_dir_with_env(Agent::Omp, Path::new("/home/test"), |key| {
            (key == "PI_CONFIG_DIR").then(|| OsString::from("custom-omp"))
        })
        .unwrap();

        assert_eq!(dir, PathBuf::from("/home/test/custom-omp/agent/skills"));
    }

    #[test]
    fn test_skills_dir_omp_respects_pi_coding_agent_dir() {
        let dir = skills_dir_with_env(Agent::Omp, Path::new("/home/test"), |key| {
            (key == "PI_CODING_AGENT_DIR").then(|| OsString::from("/tmp/omp-agent"))
        })
        .unwrap();

        assert_eq!(dir, PathBuf::from("/tmp/omp-agent/skills"));
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

    #[test]
    fn test_remove_skills_none_found() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_base = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_base).unwrap();

        let result = remove_skills_at(skills_base, "Claude Code").unwrap();
        assert!(result.contains("none found"));
    }

    #[test]
    fn test_remove_skills_removes_bundled_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_base = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_base).unwrap();

        // Write one bundled skill with exact content
        let skill = &BUNDLED_SKILLS[0];
        let skill_dir = skills_base.join(skill.name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill.content).unwrap();

        let result = remove_skills_at(skills_base, "Claude Code").unwrap();
        assert!(result.contains("1 removed"));
        assert!(!skill_dir.exists());
    }

    #[test]
    fn test_remove_skills_skips_modified_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_base = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_base).unwrap();

        // Write a modified version of a bundled skill
        let skill = &BUNDLED_SKILLS[0];
        let skill_dir = skills_base.join(skill.name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "modified content").unwrap();

        let result = remove_skills_at(skills_base, "Claude Code").unwrap();
        assert!(result.contains("1 skipped (modified)"));
        assert!(skill_dir.exists());
    }

    #[test]
    fn test_remove_skills_mixed_bundled_and_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_base = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_base).unwrap();

        // Write first bundled skill exactly (will be removed)
        let s1 = &BUNDLED_SKILLS[0];
        let d1 = skills_base.join(s1.name);
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::write(d1.join("SKILL.md"), s1.content).unwrap();

        // Write second bundled skill modified (will be skipped)
        let s2 = &BUNDLED_SKILLS[1];
        let d2 = skills_base.join(s2.name);
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d2.join("SKILL.md"), "modified content").unwrap();

        let result = remove_skills_at(skills_base, "Claude Code").unwrap();
        assert!(result.contains("1 removed"));
        assert!(result.contains("1 skipped (modified)"));
        assert!(!d1.exists());
        assert!(d2.exists());
    }

    #[test]
    fn test_remove_skills_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_base = tmp.path().join("skills");
        // Dir doesn't exist -- both calls return "No skills directory found"
        let result1 = remove_skills_at(skills_base.clone(), "Claude Code").unwrap();
        assert!(
            result1.contains("No skills directory found"),
            "result1: {result1}"
        );
        let result2 = remove_skills_at(skills_base, "Claude Code").unwrap();
        assert!(
            result2.contains("No skills directory found"),
            "result2: {result2}"
        );
    }

    #[test]
    fn test_remove_skills_unsupported_agent() {
        let result = remove_skills(Agent::Codex).unwrap();
        assert!(result.contains("does not support skills"));
    }
}
