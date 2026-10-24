//! Wire contract between the laptop and `workmux node <verb> --json`.
//!
//! Each verb reads one JSON object on stdin and writes one JSON document on
//! stdout. Failures write [`ErrorEnvelope`] and exit non-zero. The protocol
//! version changes only when the wire contract changes and must match
//! exactly; it is independent of the crate version.

use serde::{Deserialize, Serialize};

use crate::config::RemoteIsolation;

/// Node wire protocol version.
pub const NODE_PROTOCOL: u32 = 1;

/// Verbs this build's node entry point implements.
pub const CAPABILITIES: &[&str] = &[
    "hello",
    PROVISION_WORKSPACE,
    START_EXECUTION,
    REMOVE_WORKSPACE,
];

pub const PROVISION_WORKSPACE: &str = "provision-workspace";
pub const START_EXECUTION: &str = "start-execution";
pub const REMOVE_WORKSPACE: &str = "remove-workspace";

/// Error codes a node verb reports in its [`ErrorEnvelope`].
pub mod code {
    /// The request was malformed.
    pub const BAD_REQUEST: &str = "bad_request";
    /// Another call holds this workspace; retry later.
    pub const BUSY: &str = "busy";
    /// The id is already used with different inputs, or the handle or
    /// branch belongs to another workspace.
    pub const CONFLICT: &str = "conflict";
    /// The workspace id is unknown to the node.
    pub const NOT_FOUND: &str = "not_found";
    /// Container isolation needs a runtime the node does not support.
    pub const UNSUPPORTED_RUNTIME: &str = "unsupported_runtime";
    /// Removal would discard work that exists only on the node.
    pub const UNSAVED_WORK: &str = "unsaved_work";
    /// Anything else.
    pub const FAILED: &str = "failed";
}

/// What identifies a workspace; a second call with the same `ws_id` must
/// repeat these values exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub ws_id: String,
    pub project_id: String,
    pub handle: String,
    pub branch: String,
    /// Full object id of the commit the branch starts from.
    pub base_sha: String,
}

/// `provision-workspace`. Without `payload` it only prepares the project
/// repository so the laptop can push the base commit; with it, it creates
/// and provisions the worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionRequest {
    #[serde(flatten)]
    pub identity: WorkspaceIdentity,
    pub isolation: RemoteIsolation,
    pub payload: Option<ProvisionPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionPayload {
    /// Frozen configuration snapshot document (see `frozen_config`).
    pub config: serde_json::Value,
    /// Agent named with `--agent`, if any.
    pub agent: Option<String>,
    pub prompt: Option<String>,
    /// Write the prompt file without handing it to the agent.
    pub prompt_file_only: bool,
    /// `files.copy` results, relative to the worktree.
    pub copies: Vec<FileEntry>,
    /// Sources of `files.symlink` for the node's project directory, applied
    /// only where that directory lacks them.
    pub seeds: Vec<FileEntry>,
    /// `files.symlink` patterns, matched against the node's project directory.
    pub symlink_patterns: Vec<String>,
    /// Transferred paths the laptop's git ignores, through rules the node
    /// lacks (global excludes, `info/exclude`) as well as `.gitignore`.
    #[serde(default)]
    pub ignored: Vec<String>,
    pub run_file_ops: bool,
    pub run_hooks: bool,
    /// Laptop `user.name` / `user.email` for commits made on the node.
    pub git_user_name: Option<String>,
    pub git_user_email: Option<String>,
}

/// One file-tree entry; `path` is relative and never contains `..`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    #[serde(flatten)]
    pub kind: FileEntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileEntryKind {
    File {
        mode: u32,
        /// Base64 contents.
        data: String,
    },
    Dir,
    Symlink {
        target: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvisionResponse {
    /// Absolute path of the project repository on the node.
    pub repo_path: String,
    /// Absolute path of the workspace worktree on the node.
    pub worktree_path: String,
    /// Whether `refs/workmux/base/<ws_id>` already holds `base_sha`.
    pub has_base: bool,
}

/// `start-execution`: idempotent per `ex_id`; returns the workspace's live
/// execution instead when one exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRequest {
    pub ws_id: String,
    pub ex_id: String,
    /// Hand the workspace prompt to the agent (only on creation).
    pub use_prompt: bool,
    pub run_commands: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartResponse {
    pub ex_id: String,
    /// Indices of the configured panes that have a session, in order.
    pub panes: Vec<usize>,
    /// Whether an existing execution was returned.
    pub reused: bool,
}

/// `remove-workspace`: stops the execution and deletes the worktree, branch,
/// base ref and records. Idempotent: an unknown id is already removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveRequest {
    pub ws_id: String,
    /// Remove even with uncommitted changes or commits made on the node.
    pub force: bool,
    pub keep_branch: bool,
    pub run_hooks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveResponse {
    /// The workspace was unknown to the node.
    pub already_removed: bool,
}

/// tmux session of pane `index` of execution `ex_id`.
pub fn session_name(ex_id: &str, index: usize) -> String {
    format!("{ex_id}-{index}")
}

/// Response of `workmux node hello --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u32,
    /// Crate version of the node binary.
    pub version: String,
    /// `std::env::consts::OS` of the node binary.
    pub os: String,
    /// `std::env::consts::ARCH` of the node binary.
    pub arch: String,
    pub capabilities: Vec<String>,
}

impl Hello {
    pub fn local() -> Self {
        Self {
            protocol: NODE_PROTOCOL,
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: NodeError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NodeError {}

impl ErrorEnvelope {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            error: NodeError {
                code: code.to_string(),
                message: message.into(),
            },
        }
    }
}

/// Why a binary did not answer `node hello` with a usable response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloFailure {
    /// A workmux build without the `node` command.
    PredatesNodes,
    /// Exited non-zero or printed something other than a hello document.
    NotANode { detail: String },
}

/// Interpret the output of `<bin> node hello --json`.
pub fn parse_hello(
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> std::result::Result<Hello, HelloFailure> {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if exit_code == Some(0)
        && let Ok(hello) = serde_json::from_str::<Hello>(stdout.trim())
    {
        return Ok(hello);
    }
    if stderr.contains("unrecognized subcommand 'node'") {
        return Err(HelloFailure::PredatesNodes);
    }
    if let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(stdout.trim()) {
        return Err(HelloFailure::NotANode {
            detail: format!("{}: {}", envelope.error.code, envelope.error.message),
        });
    }
    let detail = first_line(&stderr)
        .or_else(|| first_line(&stdout))
        .map(str::to_string)
        .unwrap_or_else(|| match exit_code {
            Some(code) => format!("exit status {code}, no output"),
            None => "killed by a signal".to_string(),
        });
    Err(HelloFailure::NotANode { detail })
}

/// First non-empty line, trimmed.
pub(super) fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// Explain that the node binary at `bin_path` speaks another protocol.
pub fn mismatch_message(hello: &Hello, bin_path: &str) -> String {
    format!(
        "node binary {bin_path} speaks protocol {} (workmux {}), but this workmux {} \
         speaks protocol {NODE_PROTOCOL}",
        hello.protocol,
        hello.version,
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let hello = Hello::local();
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"protocol\":1"));
        assert_eq!(parse_hello(Some(0), json.as_bytes(), b""), Ok(hello));
    }

    #[test]
    fn hello_ignores_unknown_fields() {
        let json = r#"{"protocol":1,"version":"9.9","os":"linux","arch":"aarch64",
            "capabilities":["hello","later"],"extra":true}"#;
        let hello = parse_hello(Some(0), json.as_bytes(), b"").unwrap();
        assert_eq!(hello.capabilities, ["hello", "later"]);
    }

    #[test]
    fn old_workmux_predates_nodes() {
        let stderr = b"error: unrecognized subcommand 'node'\n\nUsage: workmux <COMMAND>\n";
        assert_eq!(
            parse_hello(Some(2), b"", stderr),
            Err(HelloFailure::PredatesNodes)
        );
    }

    #[test]
    fn non_json_output_is_not_a_node() {
        let result = parse_hello(Some(0), b"hello world\n", b"");
        assert_eq!(
            result,
            Err(HelloFailure::NotANode {
                detail: "hello world".into()
            })
        );
    }

    #[test]
    fn failure_prefers_stderr_first_line() {
        let result = parse_hello(
            Some(2),
            b"",
            b"\nsh: 1: Syntax error: word unexpected\nmore\n",
        );
        assert_eq!(
            result,
            Err(HelloFailure::NotANode {
                detail: "sh: 1: Syntax error: word unexpected".into()
            })
        );
    }

    #[test]
    fn error_envelope_is_reported() {
        let json = serde_json::to_string(&ErrorEnvelope::new("bad_input", "nope")).unwrap();
        assert_eq!(
            parse_hello(Some(1), json.as_bytes(), b""),
            Err(HelloFailure::NotANode {
                detail: "bad_input: nope".into()
            })
        );
    }

    #[test]
    fn silent_failure_mentions_status() {
        assert_eq!(
            parse_hello(Some(3), b"", b""),
            Err(HelloFailure::NotANode {
                detail: "exit status 3, no output".into()
            })
        );
        assert_eq!(
            parse_hello(None, b"", b""),
            Err(HelloFailure::NotANode {
                detail: "killed by a signal".into()
            })
        );
    }

    #[test]
    fn protocol_mismatch_is_explained() {
        let mut hello = Hello::local();
        hello.protocol = 999;
        hello.version = "0.0.1".into();
        let message = mismatch_message(&hello, "~/.workmux-node/bin/workmux-x");
        assert!(
            message.starts_with(
                "node binary ~/.workmux-node/bin/workmux-x speaks protocol 999 (workmux 0.0.1)"
            ),
            "{message}"
        );
        assert!(
            message.ends_with(&format!("speaks protocol {NODE_PROTOCOL}")),
            "{message}"
        );
    }
}
