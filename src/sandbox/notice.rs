use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use console::style;

const NOTICE_VERSION: &str = "security-boundary-v1";
const DOCS_URL: &str = "https://workmux.dev/guide/sandbox/#security-model";

/// Show the sandbox boundary notice once during an interactive invocation.
pub(crate) fn show_once() {
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    let Ok(state_dir) = crate::xdg::state_dir() else {
        return;
    };
    let mut stderr = io::stderr().lock();
    let _ = show_once_in(&state_dir, interactive, &mut stderr);
}

fn show_once_in(state_dir: &Path, interactive: bool, output: &mut impl Write) -> io::Result<bool> {
    if !interactive {
        return Ok(false);
    }

    let notice_dir = state_dir.join("notices");
    let marker = notice_dir.join(NOTICE_VERSION);
    if fs::symlink_metadata(&marker).is_ok() {
        return Ok(false);
    }

    fs::create_dir_all(&notice_dir)?;
    write_notice(output)?;
    output.flush()?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(marker) {
        Ok(mut file) => file.write_all(b"shown\n")?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    Ok(true)
}

fn write_notice(output: &mut impl Write) -> io::Result<()> {
    let border = style("│").dim();
    let corner_top = style("┌").dim();
    let corner_bottom = style("└─").dim();

    writeln!(output)?;
    writeln!(
        output,
        "{} {}",
        corner_top,
        style("Sandbox Security").bold().cyan()
    )?;
    writeln!(output, "{border}")?;
    writeln!(
        output,
        "{border}  Sandboxed agent panes use noninteractive approval mode."
    )?;
    writeln!(
        output,
        "{border}  The configured sandbox backend is the security boundary."
    )?;
    writeln!(output, "{border}")?;
    writeln!(
        output,
        "{border}  Host-side commands from .workmux.yaml and repository code"
    )?;
    writeln!(
        output,
        "{border}  you later run on the host remain outside this boundary."
    )?;
    writeln!(output, "{border}")?;
    writeln!(
        output,
        "{}  Learn more: {}",
        corner_bottom,
        style(DOCS_URL).dim()
    )?;
    writeln!(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_notice_is_shown_once() {
        let temp = tempfile::tempdir().unwrap();
        let mut output = Vec::new();

        assert!(show_once_in(temp.path(), true, &mut output).unwrap());
        assert!(!show_once_in(temp.path(), true, &mut output).unwrap());

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches("Sandbox Security").count(), 1);
        assert!(text.contains("┌"));
        assert!(text.contains("└─"));
        assert!(text.contains(DOCS_URL));
        assert!(temp.path().join("notices").join(NOTICE_VERSION).is_file());
    }

    #[test]
    fn noninteractive_invocation_does_not_mark_notice_seen() {
        let temp = tempfile::tempdir().unwrap();
        let mut output = Vec::new();

        assert!(!show_once_in(temp.path(), false, &mut output).unwrap());

        assert!(output.is_empty());
        assert!(!temp.path().join("notices").join(NOTICE_VERSION).exists());
    }

    #[test]
    fn existing_marker_suppresses_notice() {
        let temp = tempfile::tempdir().unwrap();
        let notice_dir = temp.path().join("notices");
        fs::create_dir_all(&notice_dir).unwrap();
        fs::write(notice_dir.join(NOTICE_VERSION), "shown\n").unwrap();
        let mut output = Vec::new();

        assert!(!show_once_in(temp.path(), true, &mut output).unwrap());

        assert!(output.is_empty());
    }
}
