//! Best-effort per-process-tree memory usage for the sidebar.
//!
//! Reports PSS (proportional set size) rather than RSS so shared memory —
//! large for a browser's or an agent's many child processes — is counted once,
//! not N times. Linux only: everything is read from `/proc`; on other platforms
//! the functions return no data and the sidebar simply omits the readout.
//!
//! `tree_pss_kb_many` scans `/proc` a single time for any number of root pids,
//! so the daemon can price every agent (and the session total) in one pass.

use std::collections::HashMap;

/// Summed PSS, in kibibytes, of each root pid *plus all its descendants*.
///
/// The returned map has one entry per input root (absent if that pid no longer
/// exists). Panes never nest, so per-root subtrees are disjoint and a session
/// total is just the sum of the values. Returns an empty map on non-Linux or if
/// `/proc` cannot be read.
pub fn tree_pss_kb_many(roots: &[u32]) -> HashMap<u32, u64> {
    #[cfg(target_os = "linux")]
    {
        linux::tree_pss_kb_many(roots)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = roots;
        HashMap::new()
    }
}

/// Format a kibibyte count as a compact human string: `512M`, `5.1G`.
pub fn format_kb(kb: u64) -> String {
    const MIB: f64 = 1024.0;
    const GIB: f64 = 1024.0 * 1024.0;
    let kb_f = kb as f64;
    if kb_f >= GIB {
        format!("{:.1}G", kb_f / GIB)
    } else {
        format!("{:.0}M", kb_f / MIB)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::fs;

    pub fn tree_pss_kb_many(roots: &[u32]) -> HashMap<u32, u64> {
        let mut result = HashMap::new();
        if roots.is_empty() {
            return result;
        }

        // Single /proc pass: parent map (child -> parent) and per-pid PSS.
        let mut parent: HashMap<u32, u32> = HashMap::new();
        let mut pss: HashMap<u32, u64> = HashMap::new();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();

        let entries = match fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return result,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };

            if let Some(ppid) = read_ppid(pid) {
                parent.insert(pid, ppid);
                children.entry(ppid).or_default().push(pid);
            }
            if let Some(kb) = read_pss_kb(pid) {
                pss.insert(pid, kb);
            }
        }

        for &root in roots {
            // A root we never saw in /proc is gone: leave it out of the map.
            if !parent.contains_key(&root) && !pss.contains_key(&root) {
                continue;
            }
            let mut total = 0u64;
            // Iterative DFS over the subtree; guard against cycles just in case.
            let mut stack = vec![root];
            let mut seen = std::collections::HashSet::new();
            while let Some(pid) = stack.pop() {
                if !seen.insert(pid) {
                    continue;
                }
                total += pss.get(&pid).copied().unwrap_or(0);
                if let Some(kids) = children.get(&pid) {
                    stack.extend(kids.iter().copied());
                }
            }
            result.insert(root, total);
        }
        result
    }

    /// Parent pid from `/proc/<pid>/stat`. The `comm` field (2nd) is wrapped in
    /// parentheses and may itself contain spaces or ')', so split after the last
    /// ')': the remaining fields are `state ppid ...`.
    fn read_ppid(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rparen = stat.rfind(')')?;
        let rest = stat.get(rparen + 1..)?;
        let mut fields = rest.split_whitespace();
        let _state = fields.next()?;
        fields.next()?.parse::<u32>().ok()
    }

    /// PSS in KiB from `/proc/<pid>/smaps_rollup` (one `Pss:` line). Absent on
    /// very old kernels or for a vanished process: treated as no contribution.
    fn read_pss_kb(pid: u32) -> Option<u64> {
        let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
        for line in rollup.lines() {
            if let Some(rest) = line.strip_prefix("Pss:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_kb_scales() {
        assert_eq!(format_kb(0), "0M");
        assert_eq!(format_kb(512 * 1024), "512M");
        assert_eq!(format_kb(1024 * 1024), "1.0G");
        assert_eq!(format_kb(5 * 1024 * 1024 + 512 * 1024), "5.5G");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_tree_has_memory() {
        let pid = std::process::id();
        let kb = tree_pss_kb_many(&[pid])
            .get(&pid)
            .copied()
            .expect("own pid should be present in /proc");
        assert!(kb > 0, "own process tree PSS should be > 0, got {kb}");
    }
}
