//! Tmux hook installation and removal for sidebar lifecycle events.

use anyhow::{Result, anyhow};

use crate::cmd::Cmd;

/// All hook names installed by the sidebar.
const HOOK_NAMES: &[&str] = &[
    "after-new-window[99]",
    "after-new-session[99]",
    "window-resized[99]",
    "after-select-window[98]",
    "client-session-changed[98]",
    "after-kill-pane[98]",
];

/// Install tmux hooks so new windows automatically get a sidebar.
pub(super) fn install_hooks() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_str().ok_or_else(|| anyhow!("exe path not UTF-8"))?;

    let sync_cmd = format!(
        "run-shell -b '{} _sidebar-sync --window #{{window_id}}'",
        exe_str
    );

    // Reflow sidebar layouts in all windows when any window resizes.
    // This ensures inactive windows get corrected without waiting for the
    // user to visit them. window-resized fires on terminal resize AND when
    // switching to an unattached session (window-size=latest resizes windows
    // to match the new client).
    let reflow_cmd = format!("run-shell -b '{} _sidebar-reflow-all'", exe_str);

    // Dirty signal: send SIGUSR1 to daemon on window/session/pane changes
    let dirty_cmd = "run-shell -b 'kill -USR1 $(tmux show-option -gqv @workmux_sidebar_daemon_pid) 2>/dev/null || true'";

    let hooks: &[(&str, &str)] = &[
        ("after-new-window[99]", &sync_cmd),
        ("after-new-session[99]", &sync_cmd),
        ("window-resized[99]", &reflow_cmd),
        ("after-select-window[98]", dirty_cmd),
        ("client-session-changed[98]", dirty_cmd),
        (
            "after-kill-pane[98]",
            &format!("{reflow_cmd} ; {dirty_cmd}"),
        ),
    ];

    for (hook, cmd) in hooks {
        Cmd::new("tmux")
            .args(&["set-hook", "-g", hook, cmd])
            .run()?;
    }

    Ok(())
}

/// Remove tmux hooks.
pub(super) fn remove_hooks() {
    for hook in HOOK_NAMES {
        let _ = Cmd::new("tmux").args(&["set-hook", "-gu", hook]).run();
    }
}
