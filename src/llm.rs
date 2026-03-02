use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

const DEFAULT_SYSTEM_PROMPT: &str = r#"Generate a short, valid git branch name (kebab-case) based on the user's input.
Output ONLY the branch name."#;

pub fn generate_branch_name(
    prompt: &str,
    model: Option<&str>,
    system_prompt: Option<&str>,
    command: Option<&str>,
) -> Result<String> {
    let system = system_prompt.unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let full_prompt = format!("{}\n\nUser Input:\n{}", system, prompt);

    let raw = run_generator_command(&full_prompt, model, command)?;
    let branch_name = sanitize_branch_name(raw.trim());

    if branch_name.is_empty() {
        return Err(anyhow!("LLM returned empty branch name"));
    }

    Ok(branch_name)
}

fn run_generator_command(
    full_prompt: &str,
    model: Option<&str>,
    command: Option<&str>,
) -> Result<String> {
    let configured_command = command.and_then(|c| {
        let trimmed = c.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });

    if let Some(command_line) = configured_command {
        return run_custom_command(full_prompt, command_line);
    }

    run_llm_command(full_prompt, model)
}

fn run_custom_command(full_prompt: &str, command_line: &str) -> Result<String> {
    let (program, rest) = crate::config::split_first_token(command_line)
        .ok_or_else(|| anyhow!("auto_name.command cannot be empty"))?;

    let mut cmd = Command::new(program);
    if !rest.trim().is_empty() {
        cmd.args(rest.split_whitespace());
    }
    cmd.arg(full_prompt);

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run '{}' command. Is it installed?", program))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{} command failed: {}", program, stderr));
    }

    Ok(String::from_utf8(output.stdout)?)
}

fn run_llm_command(full_prompt: &str, model: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("llm");
    if let Some(m) = model {
        cmd.args(["-m", m]);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to run 'llm' command. Is it installed? (pipx install llm)")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(full_prompt.as_bytes())?;
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("llm command failed: {}", stderr));
    }

    Ok(String::from_utf8(output.stdout)?)
}

fn sanitize_branch_name(raw: &str) -> String {
    // Remove markdown code blocks if present
    let cleaned = raw
        .trim_matches('`')
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim();

    // Use slug to ensure valid format
    slug::slugify(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn write_executable_script(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn custom_command_supports_claude_style_prompt_flag() {
        let tmp = TempDir::new().unwrap();
        let script_path = tmp.path().join("fake-claude");
        let received_path = tmp.path().join("received_prompt.txt");
        write_executable_script(
            &script_path,
            &format!(
                "#!/bin/sh\nset -e\n[ \"$1\" = \"-p\" ]\nprintf '%s' \"$2\" > \"{}\"\nprintf '%s' 'branch from claude'\n",
                received_path.display()
            ),
        );

        let command = format!("{} -p", script_path.display());
        let generated =
            generate_branch_name("Add billing retry logic", None, None, Some(&command)).unwrap();

        assert_eq!(generated, "branch-from-claude");
        let captured_prompt = fs::read_to_string(received_path).unwrap();
        assert!(captured_prompt.contains("User Input:\nAdd billing retry logic"));
    }

    #[test]
    #[cfg(unix)]
    fn custom_command_supports_opencode_run_style_invocation() {
        let tmp = TempDir::new().unwrap();
        let script_path = tmp.path().join("fake-opencode");
        write_executable_script(
            &script_path,
            "#!/bin/sh\nset -e\n[ \"$1\" = \"run\" ]\nprintf '%s' 'opencode-branch'\n",
        );

        let command = format!("{} run", script_path.display());
        let generated =
            generate_branch_name("Refactor auth middleware", None, None, Some(&command)).unwrap();

        assert_eq!(generated, "opencode-branch");
    }

    #[test]
    fn sanitize_branch_name_simple() {
        assert_eq!(sanitize_branch_name("add-user-auth"), "add-user-auth");
    }

    #[test]
    fn sanitize_branch_name_with_backticks() {
        assert_eq!(sanitize_branch_name("`add-user-auth`"), "add-user-auth");
    }

    #[test]
    fn sanitize_branch_name_with_triple_backticks() {
        assert_eq!(
            sanitize_branch_name("```\nadd-user-auth\n```"),
            "add-user-auth"
        );
    }

    #[test]
    fn sanitize_branch_name_multiline() {
        assert_eq!(
            sanitize_branch_name("add-user-auth\nsome explanation"),
            "add-user-auth"
        );
    }

    #[test]
    fn sanitize_branch_name_with_spaces() {
        assert_eq!(sanitize_branch_name("add user auth"), "add-user-auth");
    }

    #[test]
    fn sanitize_branch_name_with_special_chars() {
        assert_eq!(sanitize_branch_name("Add User Auth!"), "add-user-auth");
    }

    #[test]
    fn sanitize_branch_name_empty() {
        assert_eq!(sanitize_branch_name(""), "");
    }

    #[test]
    fn sanitize_branch_name_whitespace_only() {
        assert_eq!(sanitize_branch_name("   "), "");
    }
}
