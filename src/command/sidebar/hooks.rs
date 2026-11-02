//! Tmux hook installation and removal for sidebar lifecycle events.

use anyhow::{Result, anyhow};

use crate::cmd::Cmd;
use crate::shell::shell_quote;

/// All hook names installed by the sidebar.
const HOOK_NAMES: &[&str] = &[
    "after-new-window[99]",
    "after-new-session[99]",
    "window-resized[99]",
    "after-select-window[98]",
    "client-session-changed[98]",
    "after-kill-pane[98]",
    "pane-exited[98]",
];

/// Install tmux hooks so new windows automatically get a sidebar.
pub(super) fn install_hooks() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_str().ok_or_else(|| anyhow!("exe path not UTF-8"))?;
    let exe_arg = shell_quote(exe_str);

    let sync_cmd = run_shell_hook(&format!("{exe_arg} _sidebar-sync --window #{{window_id}}"));

    // Reflow only the window named by the hook. A terminal resize emits one
    // window-resized event per affected window, so reflowing every window for
    // every event multiplies the work quadratically.
    let reflow_cmd = run_shell_hook(&format!(
        "{exe_arg} _sidebar-reflow --window #{{window_id}}"
    ));

    // Dirty signal: send SIGUSR1 to daemon on window/session/pane changes
    let dirty_cmd = "run-shell -b 'kill -USR1 $(tmux show-option -gqv @workmux_sidebar_daemon_pid) 2>/dev/null || true'";
    let after_kill_pane_cmd = after_kill_pane_hook_command(&exe_arg);

    let hooks: &[(&str, &str)] = &[
        ("after-new-window[99]", &sync_cmd),
        ("after-new-session[99]", &sync_cmd),
        ("window-resized[99]", &reflow_cmd),
        ("after-select-window[98]", dirty_cmd),
        ("client-session-changed[98]", dirty_cmd),
        ("after-kill-pane[98]", &after_kill_pane_cmd),
        // Shell exits do not fire after-kill-pane. Wake the daemon so a lone
        // sidebar can exit without waiting for the periodic tmux observation.
        ("pane-exited[98]", dirty_cmd),
    ];

    let args = hook_install_args(hooks);
    Cmd::new("tmux").args(&args).run()?;

    Ok(())
}

fn hook_install_args<'a>(hooks: &[(&'a str, &'a str)]) -> Vec<&'a str> {
    let mut args = Vec::with_capacity(hooks.len() * 5);
    for (index, (hook, command)) in hooks.iter().enumerate() {
        if index > 0 {
            args.push(";");
        }
        args.extend_from_slice(&["set-hook", "-g", hook, command]);
    }
    args
}

fn run_shell_hook(command: &str) -> String {
    format!("run-shell -b {}", tmux_double_quote(command))
}

fn after_kill_pane_hook_command(exe_arg: &str) -> String {
    run_shell_hook(&format!(
        "{exe_arg} _sidebar-reflow --window #{{window_id}}; kill -USR1 $(tmux show-option -gqv @workmux_sidebar_daemon_pid) 2>/dev/null || true"
    ))
}

fn tmux_double_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Remove tmux hooks.
pub(super) fn remove_hooks() {
    for hook in HOOK_NAMES {
        let _ = Cmd::new("tmux").args(&["set-hook", "-gu", hook]).run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_shell_hook_preserves_shell_quoted_executable() {
        let exe_arg = shell_quote("/tmp/work mux/workmux");
        let command = run_shell_hook(&format!(
            "{exe_arg} _sidebar-reflow --window #{{window_id}}"
        ));

        assert_eq!(
            command,
            "run-shell -b \"'/tmp/work mux/workmux' _sidebar-reflow --window #{window_id}\""
        );
    }

    #[test]
    fn run_shell_hook_escapes_tmux_double_quotes() {
        let command = run_shell_hook("printf \"ok\"");

        assert_eq!(command, "run-shell -b \"printf \\\"ok\\\"\"");
    }

    #[test]
    fn after_kill_pane_hook_reflows_affected_window() {
        let exe_arg = shell_quote("/tmp/work mux/workmux");
        let command = after_kill_pane_hook_command(&exe_arg);

        assert!(command.contains("_sidebar-reflow --window #{window_id}"));
        assert!(!command.contains("_sidebar-reflow-all --exclude"));
    }

    #[test]
    fn install_args_separate_hook_commands() {
        let args = hook_install_args(&[("first", "one"), ("second", "two")]);

        assert_eq!(
            args,
            vec![
                "set-hook", "-g", "first", "one", ";", "set-hook", "-g", "second", "two"
            ]
        );
    }
}
