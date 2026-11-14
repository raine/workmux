//! One-time notice that introduces remote servers to CLI users.

use std::io::{self, IsTerminal, Write};

use console::style;

use crate::multiplexer::BackendType;
use crate::tips::REMOTE_GUIDE_URL;

/// Print the remote servers notice to stderr once, after an interactive
/// command on the local machine. Output that a script or agent reads never
/// carries it.
pub fn show_once(remotes_configured: bool) {
    let interactive = io::stdout().is_terminal() && io::stderr().is_terminal();
    let eligible = eligible(
        interactive,
        crate::remote::node::operation::in_session(),
        crate::sandbox::guest::is_sandbox_guest(),
        crate::multiplexer::detect_backend(),
    );
    if eligible && crate::tips::take_remote_notice_once(remotes_configured) {
        let _ = write_notice(&mut io::stderr().lock());
    }
}

/// Remote agents run in tmux windows on the local machine, so the notice
/// only makes sense in an interactive local tmux setup.
fn eligible(
    interactive: bool,
    remote_session: bool,
    sandbox_guest: bool,
    backend: BackendType,
) -> bool {
    interactive && !remote_session && !sandbox_guest && backend == BackendType::Tmux
}

fn write_notice(output: &mut impl Write) -> io::Result<()> {
    writeln!(output)?;
    writeln!(output, "{}", style("New: remote servers").bold().cyan())?;
    writeln!(
        output,
        "  Run an agent on a Linux server you reach over SSH. It keeps working while"
    )?;
    writeln!(
        output,
        "  this machine sleeps, and shows up in the dashboard like a local agent."
    )?;
    writeln!(output, "  Guide: {}", style(REMOTE_GUIDE_URL).cyan())?;
    writeln!(output, "  {}", style("This note is shown once.").dim())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_links_the_guide_without_a_version() {
        let mut output = Vec::new();
        write_notice(&mut output).unwrap();
        let text = console::strip_ansi_codes(&String::from_utf8(output).unwrap()).to_string();
        assert!(text.contains("New: remote servers"));
        assert!(text.contains(REMOTE_GUIDE_URL));
        assert!(!text.chars().any(|c| c.is_ascii_digit()), "{text}");
    }

    #[test]
    fn notice_needs_an_interactive_local_tmux_command() {
        assert!(eligible(true, false, false, BackendType::Tmux));
        assert!(!eligible(false, false, false, BackendType::Tmux));
        assert!(!eligible(true, true, false, BackendType::Tmux));
        assert!(!eligible(true, false, true, BackendType::Tmux));
        assert!(!eligible(true, false, false, BackendType::WezTerm));
    }
}
