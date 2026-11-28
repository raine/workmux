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

/// The instructions of bundled skill `name`, as an agent invoking it with
/// `args` would read them: without frontmatter, `$ARGUMENTS` replaced.
///
/// A customized installed copy takes precedence over the bundled version:
/// the first copy in an agent's skills directory whose content differs.
pub fn instructions(name: &str, args: &str) -> Result<String> {
    let installed: Vec<PathBuf> = [
        Agent::Claude,
        Agent::Pi,
        Agent::Omp,
        Agent::OpenCode,
        Agent::Grok,
    ]
    .into_iter()
    .filter_map(skills_dir)
    .map(|dir| dir.join(name).join("SKILL.md"))
    .collect();
    instructions_from(name, args, &installed)
}

fn instructions_from(name: &str, args: &str, installed: &[PathBuf]) -> Result<String> {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| {
            let names: Vec<_> = BUNDLED_SKILLS.iter().map(|skill| skill.name).collect();
            anyhow::anyhow!(
                "no bundled skill named '{name}'; bundled skills: {}",
                names.join(", ")
            )
        })?;
    let customized = installed
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .find(|content| content != skill.content);
    let content = customized.as_deref().unwrap_or(skill.content);
    let body = strip_frontmatter(content).trim_start();
    Ok(body.replace("$ARGUMENTS", args.trim()))
}

/// `content` after its leading YAML frontmatter block, if it has one.
fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    match rest.find("\n---\n") {
        Some(end) => &rest[end + "\n---\n".len()..],
        None => content,
    }
}

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
        Agent::Grok => {
            let base = get_env("GROK_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".grok"));
            Some(base.join("skills"))
        }
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

/// Return the directory where bundled skills are also installed as commands,
/// for agents that invoke a command by name but have no prompt syntax for
/// skills.
pub fn commands_dir(agent: Agent) -> Option<PathBuf> {
    let home = home::home_dir()?;
    commands_dir_in(agent, &home)
}

fn commands_dir_in(agent: Agent, home: &Path) -> Option<PathBuf> {
    match agent {
        Agent::OpenCode => Some(home.join(".config/opencode/command")),
        _ => None,
    }
}

/// How a bundled skill is laid out in an install directory.
#[derive(Clone, Copy)]
enum Layout {
    /// `<name>/SKILL.md`.
    SkillDir,
    /// `<name>.md`: a command whose template is the skill.
    CommandFile,
}

impl Layout {
    fn noun(self) -> &'static str {
        match self {
            Layout::SkillDir => "Skills",
            Layout::CommandFile => "Commands",
        }
    }

    /// The file of skill `name`, relative to the install directory.
    fn file(self, name: &str) -> PathBuf {
        match self {
            Layout::SkillDir => Path::new(name).join("SKILL.md"),
            Layout::CommandFile => PathBuf::from(format!("{name}.md")),
        }
    }
}

/// The directories the given agent's bundled skills install to.
fn install_dirs(agent: Agent) -> Vec<(Layout, PathBuf)> {
    let skills = skills_dir(agent).map(|dir| (Layout::SkillDir, dir));
    let commands = commands_dir(agent).map(|dir| (Layout::CommandFile, dir));
    skills.into_iter().chain(commands).collect()
}

/// Check if any bundled skills are missing for the given agent.
pub fn needs_install(agent: Agent) -> bool {
    install_dirs(agent).iter().any(|(layout, base_dir)| {
        BUNDLED_SKILLS
            .iter()
            .any(|skill| !base_dir.join(layout.file(skill.name)).exists())
    })
}

enum InstallOutcome {
    Installed,
    AlreadyUpToDate,
    Updated,
    Skipped,
}

/// Install all bundled skills to the given agent's skills directory, and
/// as commands to its [`commands_dir`].
pub fn install_skills(agent: Agent) -> Result<String> {
    let dirs = install_dirs(agent);
    if dirs.is_empty() {
        return Ok(format!("{} does not support skills", agent.name()));
    }
    let summaries = dirs
        .iter()
        .map(|(layout, base_dir)| install_at(*layout, base_dir, agent.name()))
        .collect::<Result<Vec<_>>>()?;
    Ok(summaries.join("\n  "))
}

fn install_at(layout: Layout, base_dir: &Path, agent_name: &str) -> Result<String> {
    let mut installed = 0u32;
    let mut up_to_date = 0u32;
    let mut updated = 0u32;
    let mut skipped = 0u32;

    for skill in BUNDLED_SKILLS {
        let file = layout.file(skill.name);
        let label = file.display().to_string();
        let path = base_dir.join(&file);

        let outcome = if path.exists() {
            let existing = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;

            if existing == skill.content {
                InstallOutcome::AlreadyUpToDate
            } else {
                print_skill_diff(&label, &existing, skill.content);
                if confirm_overwrite(&label)? {
                    fs::write(&path, skill.content)
                        .with_context(|| format!("Failed to write {}", path.display()))?;
                    println!("  {} updated {}", style("✓").green(), label);
                    InstallOutcome::Updated
                } else {
                    InstallOutcome::Skipped
                }
            }
        } else {
            let dir = path.parent().unwrap_or(base_dir);
            fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
            fs::write(&path, skill.content)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            println!("  {} installed {}", style("✓").green(), label);
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
        "{} for {} ({}): {}",
        layout.noun(),
        agent_name,
        base_dir.display(),
        parts.join(", ")
    ))
}

fn print_skill_diff(label: &str, old: &str, new: &str) {
    println!();
    println!(
        "  {} {} differs from bundled version:",
        style("~").yellow(),
        label
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
/// Only removes skills whose contents match the bundled version exactly.
/// Modified skills are left in place.
pub fn remove_skills(agent: Agent) -> Result<String> {
    let dirs = install_dirs(agent);
    if dirs.is_empty() {
        return Ok(format!("{} does not support skills", agent.name()));
    }
    let summaries = dirs
        .into_iter()
        .map(|(layout, base_dir)| remove_at(layout, base_dir, agent.name()))
        .collect::<Result<Vec<_>>>()?;
    Ok(summaries.join("\n  "))
}

fn remove_at(layout: Layout, base_dir: PathBuf, agent_name: &str) -> Result<String> {
    if !base_dir.exists() {
        return Ok(format!(
            "No {} directory found at {}",
            layout.noun().to_lowercase(),
            base_dir.display()
        ));
    }

    let mut removed = 0u32;
    let mut skipped = 0u32;

    for skill in BUNDLED_SKILLS {
        let path = base_dir.join(layout.file(skill.name));
        if !path.exists() {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(content) if content == skill.content => {
                match layout {
                    Layout::SkillDir => fs::remove_dir_all(path.parent().unwrap())?,
                    Layout::CommandFile => fs::remove_file(&path)?,
                }
                removed += 1;
            }
            Ok(_) => {
                // User-modified skill, leave it
                skipped += 1;
            }
            Err(_) => continue,
        }
    }

    // Remove the base dir if empty
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
        "{} for {} ({}): {}",
        layout.noun(),
        agent_name,
        base_dir.display(),
        parts.join(", ")
    ))
}

fn confirm_overwrite(label: &str) -> Result<bool> {
    let prompt = format!(
        "  Overwrite {} with bundled version? {}{}{} ",
        label,
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

        let result = remove_at(Layout::SkillDir, skills_base, "Claude Code").unwrap();
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

        let result = remove_at(Layout::SkillDir, skills_base, "Claude Code").unwrap();
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

        let result = remove_at(Layout::SkillDir, skills_base, "Claude Code").unwrap();
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

        let result = remove_at(Layout::SkillDir, skills_base, "Claude Code").unwrap();
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
        let result1 = remove_at(Layout::SkillDir, skills_base.clone(), "Claude Code").unwrap();
        assert!(
            result1.contains("No skills directory found"),
            "result1: {result1}"
        );
        let result2 = remove_at(Layout::SkillDir, skills_base, "Claude Code").unwrap();
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

    #[test]
    fn test_commands_dir_only_for_opencode() {
        let home = Path::new("/home/test");
        assert_eq!(
            commands_dir_in(Agent::OpenCode, home),
            Some(PathBuf::from("/home/test/.config/opencode/command"))
        );
        assert_eq!(commands_dir_in(Agent::Claude, home), None);
        assert_eq!(commands_dir_in(Agent::Pi, home), None);
    }

    #[test]
    fn test_commands_install_and_remove_as_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("command");

        let result = install_at(Layout::CommandFile, &base, "OpenCode").unwrap();
        assert!(result.contains("6 installed"), "{result}");
        let merge = std::fs::read_to_string(base.join("merge.md")).unwrap();
        assert_eq!(merge, BUNDLED_SKILLS[0].content);

        let result = install_at(Layout::CommandFile, &base, "OpenCode").unwrap();
        assert!(result.contains("6 up to date"), "{result}");

        let result = remove_at(Layout::CommandFile, base.clone(), "OpenCode").unwrap();
        assert!(result.contains("6 removed"), "{result}");
        assert!(!base.exists());
    }

    #[test]
    fn test_instructions_drop_frontmatter_and_fill_arguments() {
        let merge = instructions_from("merge", "--keep", &[]).unwrap();
        assert!(!merge.starts_with("---"), "{merge}");
        assert!(!merge.contains("disable-model-invocation"));
        assert!(!merge.contains("$ARGUMENTS"));
        assert!(merge.contains("`--keep`"));

        for skill in BUNDLED_SKILLS {
            let body = instructions_from(skill.name, "", &[]).unwrap();
            assert!(!body.starts_with("---"), "{}", skill.name);
            assert!(!body.is_empty(), "{}", skill.name);
        }
    }

    #[test]
    fn test_instructions_prefer_a_customized_installed_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let unmodified = tmp.path().join("a/merge/SKILL.md");
        let customized = tmp.path().join("b/merge/SKILL.md");
        let missing = tmp.path().join("c/merge/SKILL.md");
        let bundled = BUNDLED_SKILLS.iter().find(|s| s.name == "merge").unwrap();
        fs::create_dir_all(unmodified.parent().unwrap()).unwrap();
        fs::create_dir_all(customized.parent().unwrap()).unwrap();
        fs::write(&unmodified, bundled.content).unwrap();
        fs::write(
            &customized,
            "---\nname: merge\n---\nSquash first. $ARGUMENTS\n",
        )
        .unwrap();

        let paths = [missing.clone(), unmodified.clone(), customized];
        assert_eq!(
            instructions_from("merge", "-k", &paths).unwrap(),
            "Squash first. -k\n"
        );
        assert_eq!(
            instructions_from("merge", "", &[missing, unmodified]).unwrap(),
            instructions_from("merge", "", &[]).unwrap()
        );
    }

    #[test]
    fn test_instructions_of_unknown_skill_lists_bundled_ones() {
        let err = instructions("nope", "").unwrap_err().to_string();
        assert!(err.contains("merge, rebase"), "{err}");
    }

    /// OpenCode runs `` !`cmd` `` in a command template as a shell command.
    #[test]
    fn test_bundled_skills_have_no_command_shell_interpolation() {
        for skill in BUNDLED_SKILLS {
            assert!(!skill.content.contains("!`"), "{}", skill.name);
        }
    }
}
