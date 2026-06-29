//! Process-tree helper for Pi nested-agent status.
//!
//! Pi subagents run as child `pi` processes under the parent `pi` process in
//! the same terminal pane. We use that relationship to avoid letting a child
//! agent's `done` event mark the pane done while its parent is still alive,
//! without keeping any Workmux-owned persistent scope state.
//!
//! Linux-only by design: this reads `/proc`, matching the existing
//! `multiplexer::tmux::process_ancestors` helper. On other platforms
//! `has_pi_ancestor` returns `false`, so child `done` events are not
//! suppressed there.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub command: String,
}

const MAX_ANCESTOR_DEPTH: usize = 32;

/// Return true when `pid` has a live ancestor that looks like a Pi process.
///
/// The starting process itself is not considered an ancestor. A normal Pi pane
/// usually looks like `tmux -> zsh -> pi`, so this returns false for the pane's
/// top-level Pi process. A subagent looks like `tmux -> zsh -> pi -> pi`, so it
/// returns true for the child Pi process while the parent is still alive.
pub fn has_pi_ancestor(pid: u32) -> bool {
    has_pi_ancestor_with(pid, read_process_info)
}

/// Return true when `pid` has a live descendant that looks like a Pi process.
///
/// tmux can transiently report a pane's foreground command as the shell (for
/// example `zsh`) even while the long-lived Pi process is still running as a
/// child of that shell. Reconciliation uses this to avoid deleting a live Pi
/// pane just because `pane_current_command` briefly drifted away from `pi`.
pub fn has_pi_descendant(pid: u32) -> bool {
    read_all_processes()
        .map(|processes| has_pi_descendant_with(pid, processes))
        .unwrap_or(false)
}

pub(crate) fn has_pi_ancestor_with<F>(pid: u32, mut read_info: F) -> bool
where
    F: FnMut(u32) -> Option<ProcessInfo>,
{
    let Some(current) = read_info(pid) else {
        return false;
    };

    let mut next = current.ppid;
    let mut seen = HashSet::from([pid]);

    for _ in 0..MAX_ANCESTOR_DEPTH {
        if next == 0 || !seen.insert(next) {
            return false;
        }

        let Some(info) = read_info(next) else {
            return false;
        };

        if is_pi_command(&info.command) {
            return true;
        }

        next = info.ppid;
    }

    false
}

pub(crate) fn has_pi_descendant_with<I>(pid: u32, processes: I) -> bool
where
    I: IntoIterator<Item = ProcessInfo>,
{
    let mut children: HashMap<u32, Vec<ProcessInfo>> = HashMap::new();
    for process in processes {
        children.entry(process.ppid).or_default().push(process);
    }

    let mut stack = children.remove(&pid).unwrap_or_default();
    let mut seen = HashSet::from([pid]);

    while let Some(process) = stack.pop() {
        if !seen.insert(process.pid) {
            continue;
        }
        if is_pi_command(&process.command) {
            return true;
        }
        if let Some(grandchildren) = children.remove(&process.pid) {
            stack.extend(grandchildren);
        }
    }

    false
}

fn is_pi_command(command: &str) -> bool {
    if command.contains("pi-coding-agent") {
        return true;
    }

    let mut parts = command.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };

    let first_name = basename(first);
    if matches!(first_name, "pi" | "omp" | "oh-my-pi") {
        return true;
    }

    // Pi installed through npm may run as `node /path/to/.bin/pi`, and procfs
    // can report the short comm as `node-MainThread`. Inspecting cmdline lets
    // us recognize that parent as Pi without depending on the short comm.
    if first_name.starts_with("node") {
        if let Some(second) = parts.next() {
            let second_name = basename(second);
            return matches!(second_name, "pi" | "omp" | "oh-my-pi");
        }
    }

    false
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(target_os = "linux")]
fn read_all_processes() -> Option<Vec<ProcessInfo>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    Some(
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
            .filter_map(read_process_info)
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn read_process_info(pid: u32) -> Option<ProcessInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut info = parse_linux_stat(&stat)?;
    if let Some(cmdline) = read_linux_cmdline(pid) {
        info.command = cmdline;
    }
    Some(info)
}

#[cfg(target_os = "linux")]
fn read_linux_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let command = bytes
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if command.trim().is_empty() {
        None
    } else {
        Some(command)
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(stat: &str) -> Option<ProcessInfo> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let pid = stat[..open].trim().parse().ok()?;
    let command = stat[open + 1..close].to_string();
    let rest = stat[close + 1..].trim();
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;

    Some(ProcessInfo { pid, ppid, command })
}

#[cfg(not(target_os = "linux"))]
fn read_all_processes() -> Option<Vec<ProcessInfo>> {
    None
}

#[cfg(not(target_os = "linux"))]
fn read_process_info(_pid: u32) -> Option<ProcessInfo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn info(pid: u32, ppid: u32, command: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            command: command.to_string(),
        }
    }

    fn lookup(map: HashMap<u32, ProcessInfo>) -> impl FnMut(u32) -> Option<ProcessInfo> {
        move |pid| map.get(&pid).cloned()
    }

    #[test]
    fn recognizes_node_backed_pi_command_line() {
        assert!(is_pi_command(
            "node /home/amit/.npm/_npx/hash/node_modules/.bin/pi --offline"
        ));
        assert!(is_pi_command("/usr/local/bin/pi"));
        assert!(!is_pi_command("node /usr/bin/not-pi"));
        assert!(!is_pi_command("bash -c pi --help"));
    }

    #[test]
    fn normal_pi_without_pi_ancestor_returns_false() {
        let map = HashMap::from([
            (10, info(10, 9, "pi")),
            (9, info(9, 8, "zsh")),
            (8, info(8, 1, "tmux: server")),
            (1, info(1, 0, "systemd")),
        ]);

        assert!(!has_pi_ancestor_with(10, lookup(map)));
    }

    #[test]
    fn child_pi_with_pi_parent_returns_true() {
        let map = HashMap::from([
            (20, info(20, 10, "pi")),
            (10, info(10, 9, "pi")),
            (9, info(9, 8, "zsh")),
            (8, info(8, 1, "tmux: server")),
            (1, info(1, 0, "systemd")),
        ]);

        assert!(has_pi_ancestor_with(20, lookup(map)));
    }

    #[test]
    fn missing_parent_returns_false() {
        let map = HashMap::from([(20, info(20, 10, "pi"))]);

        assert!(!has_pi_ancestor_with(20, lookup(map)));
    }

    #[test]
    fn shell_with_pi_child_has_pi_descendant() {
        let processes = vec![
            info(8, 1, "tmux: server"),
            info(9, 8, "zsh"),
            info(10, 9, "pi"),
            info(11, 10, "bash"),
        ];

        assert!(has_pi_descendant_with(9, processes));
    }

    #[test]
    fn shell_without_pi_child_has_no_pi_descendant() {
        let processes = vec![
            info(8, 1, "tmux: server"),
            info(9, 8, "zsh"),
            info(10, 9, "bash"),
        ];

        assert!(!has_pi_descendant_with(9, processes));
    }

    #[test]
    fn cycles_return_false() {
        let map = HashMap::from([(20, info(20, 10, "pi")), (10, info(10, 20, "zsh"))]);

        assert!(!has_pi_ancestor_with(20, lookup(map)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_stat() {
        let stat = "1234 (pi) S 1000 1234 1234 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 123456 0";
        assert_eq!(
            parse_linux_stat(stat),
            Some(ProcessInfo {
                pid: 1234,
                ppid: 1000,
                command: "pi".to_string(),
            })
        );
    }
}
