//! `node attach <ex-id> <slot>`: attach the calling terminal to a session,
//! a pane of the execution or one of its shells.
//!
//! The exit status tells the laptop's relay what happened, since tmux exits
//! 0 both when detached and when the session ends.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::execution::{session_alive, tmux_socket};
use super::{NodeRoot, refuse, validate_id};
use crate::remote::protocol::code;

/// The session does not exist (the pane's program ended).
pub const SESSION_GONE: i32 = 103;
/// The client was detached; the session is still running.
pub const DETACHED: i32 = 104;

/// Terminal type used when the laptop's is unknown to the node.
const FALLBACK_TERM: &str = "xterm-256color";

pub fn attach(root: &NodeRoot, ex_id: &str, slot: &str) -> Result<i32> {
    validate_id(ex_id, 'x', "execution id")?;
    if !super::shell::valid_slot(slot) {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("invalid session '{slot}'"),
        ));
    }
    let socket = tmux_socket(root)?;
    let name = format!("{ex_id}-{slot}");
    if !session_alive(&socket, &name) {
        return Ok(SESSION_GONE);
    }
    let mut cmd = Command::new("tmux");
    // `-u`: the relay always carries UTF-8, whatever locale ssh passed on.
    // `-d`: a pane has one viewer, so other clients are leftovers of lost
    // connections, which sshd may not notice for hours.
    cmd.arg("-u")
        .arg("-S")
        .arg(&socket)
        .args(["attach-session", "-d", "-t", &format!("={name}")])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let term = std::env::var("TERM").unwrap_or_default();
    if !has_terminfo(&term) {
        cmd.env("TERM", FALLBACK_TERM);
    }
    let status = cmd.status().context("Failed to run tmux")?;
    if !session_alive(&socket, &name) {
        return Ok(SESSION_GONE);
    }
    if !status.success() {
        bail!("tmux attach failed with {status}");
    }
    Ok(DETACHED)
}

fn has_terminfo(term: &str) -> bool {
    let Some(first) = term.chars().next() else {
        return false;
    };
    if term.contains('/') {
        return false;
    }
    let mut dirs = vec![
        "/etc/terminfo".to_string(),
        "/lib/terminfo".to_string(),
        "/usr/share/terminfo".to_string(),
        "/usr/lib/terminfo".to_string(),
    ];
    if let Some(home) = home::home_dir() {
        dirs.insert(0, home.join(".terminfo").to_string_lossy().into_owned());
    }
    if let Ok(extra) = std::env::var("TERMINFO") {
        dirs.insert(0, extra);
    }
    let hex = format!("{:x}", first as u32);
    dirs.iter().any(|dir| {
        let dir = Path::new(dir);
        dir.join(first.to_string()).join(term).exists() || dir.join(&hex).join(term).exists()
    })
}
