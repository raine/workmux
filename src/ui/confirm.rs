use anyhow::Result;
use console::style;
use std::io::{self, Write};

pub enum ConfirmDefault {
    Yes,
    No,
}

/// Print `prompt` with a styled [Y/n] or [y/N] suffix, then read stdin for
/// a y/n answer. Returns `Ok(true)` for yes, `Ok(false)` for no. Loops on
/// invalid input. Default is used when the user enters empty input.
pub fn confirm(prompt: &str, default: ConfirmDefault) -> Result<bool> {
    let default_yes = matches!(default, ConfirmDefault::Yes);
    let suffix = format!(
        "{}{}{}",
        style("[").bold().cyan(),
        if default_yes {
            style("Y/n").bold()
        } else {
            style("y/N").bold()
        },
        style("]").bold().cyan(),
    );
    let full_prompt = format!("  {} {} ", prompt, suffix);

    loop {
        print!("{full_prompt}");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_lowercase();

        match answer.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            "" => return Ok(default_yes),
            _ => println!("    {}", style("Please enter y or n").dim()),
        }
    }
}
