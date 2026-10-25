//! Wire contract between the laptop and `workmux node <verb> --json`.
//!
//! Provisioning reads a compressed binary stream; other verbs read JSON on
//! stdin. Each verb writes one JSON document on
//! stdout. Failures write [`ErrorEnvelope`] and exit non-zero. The protocol
//! version changes only when the wire contract changes and must match
//! exactly; it is independent of the crate version.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::RemoteIsolation;
use crate::remote::git::{Changes, Snapshot};

/// Node wire protocol version.
pub const NODE_PROTOCOL: u32 = 1;

/// Verbs this build's node entry point implements.
pub const CAPABILITIES: &[&str] = &[
    "hello",
    PROVISION_WORKSPACE,
    START_EXECUTION,
    REMOVE_WORKSPACE,
    LOGIN,
    CREDENTIALS,
    PUT_CREDENTIALS,
    WATCH,
    SEND,
    CAPTURE,
    STATUS_HOOKS,
    EXPORT,
    CHECKOUT,
    REBASE,
    DIFF,
    GC,
];

pub const PROVISION_WORKSPACE: &str = "provision-workspace";
pub const START_EXECUTION: &str = "start-execution";
pub const REMOVE_WORKSPACE: &str = "remove-workspace";
/// Terminal verb: runs an agent's login flow on the node.
pub const LOGIN: &str = "login";
pub const CREDENTIALS: &str = "credentials";
pub const PUT_CREDENTIALS: &str = "put-credentials";
/// Wire types in `remote::node::observe`.
pub const WATCH: &str = "watch";
pub const SEND: &str = "send";
pub const CAPTURE: &str = "capture";
/// Wire types in `remote::node::hooks`.
pub const STATUS_HOOKS: &str = "status-hooks";
pub const EXPORT: &str = "export";
pub const CHECKOUT: &str = "checkout";
pub const REBASE: &str = "rebase";
pub const DIFF: &str = "diff";
/// Wire types in `remote::node::gc`.
pub const GC: &str = "gc";

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
    /// The workspace has no worktree (provisioning stopped before it).
    pub const NO_WORKTREE: &str = "no_worktree";
    /// The workspace no longer matches the snapshot the request expected.
    pub const DIVERGED: &str = "diverged";
    /// A source omitted after prepare disappeared before provisioning.
    pub const MISSING_SEED: &str = "missing_seed";
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
    /// Candidate seed paths relative to the project repository.
    #[serde(default)]
    pub seed_paths: Vec<String>,
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
    /// Sources omitted because prepare found them; rechecked before file operations.
    #[serde(default)]
    pub cached_seeds: Vec<String>,
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
    /// Environment the workspace's containers get, read on the laptop when
    /// the workspace is created.
    #[serde(default)]
    pub secrets: Secrets,
}

/// Environment values that may be secret. `Debug` shows names only.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secrets(pub BTreeMap<String, String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
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
        /// Uncompressed body length in the provisioning stream.
        size: u64,
        /// Local source or validated staging file; never accepted from the wire.
        #[serde(skip)]
        source: std::path::PathBuf,
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
    /// Existing seed paths from the request.
    #[serde(default)]
    pub cached_seeds: Vec<String>,
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
    /// Whether the host rebooted under the execution this one replaces.
    #[serde(default)]
    pub after_reboot: bool,
    /// Whether the agent was started to continue its last conversation.
    #[serde(default)]
    pub continued: bool,
}

/// `remove-workspace`: stops the execution and deletes the worktree, branch,
/// base ref and records. Idempotent: an unknown id is already removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveRequest {
    pub ws_id: String,
    pub keep_branch: bool,
    pub run_hooks: bool,
    /// Remove only if the workspace still matches this snapshot, checked
    /// before and after its execution stops; otherwise reply
    /// [`code::DIVERGED`] (with the execution stopped if it changed late).
    #[serde(default)]
    pub expect: Option<Snapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveResponse {
    /// The workspace was unknown to the node.
    pub already_removed: bool,
}

/// `export`: the workspace's [`Snapshot`], stored as commits under
/// `refs/workmux/export/<ws_id>/{index,worktree}` for the laptop to fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    pub ws_id: String,
    /// Stop the execution first, so the snapshot is final.
    #[serde(default)]
    pub stop: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResponse {
    pub snapshot: Snapshot,
    /// Each list holds at most [`MAX_EXPORT_PATHS`] paths.
    pub changes: Changes,
    /// Some list of `changes` was cut short.
    #[serde(default)]
    pub truncated: bool,
    /// Absolute path of the project repository on the node.
    pub repo_path: String,
}

pub const MAX_EXPORT_PATHS: usize = 1000;

/// `checkout`: move the workspace branch to `head` (pushed to
/// `refs/workmux/kept/<ws_id>`) the way `git checkout -B` does, keeping
/// uncommitted work, if the workspace still matches `expect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutRequest {
    pub ws_id: String,
    pub head: String,
    pub expect: Snapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutResponse {}

/// `rebase`: rebase the workspace branch onto `onto` (pushed to
/// `refs/workmux/onto/<ws_id>`); success makes `onto` the new base. A
/// conflict leaves the rebase in progress and replies [`code::CONFLICT`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseRequest {
    pub ws_id: String,
    pub onto: String,
    /// Committer of the rebased commits: the laptop user, as for a local
    /// rebase.
    #[serde(default)]
    pub committer: Option<Committer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Committer {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseResponse {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMode {
    /// Uncommitted changes, untracked files included.
    Wip,
    /// The commits since the base.
    Branch,
}

/// `diff`: a unified diff of the workspace, at most [`MAX_DIFF_BYTES`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRequest {
    pub ws_id: String,
    pub mode: DiffMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResponse {
    pub text: String,
    pub truncated: bool,
}

pub const MAX_DIFF_BYTES: usize = 4 << 20;

/// `credentials`: which of `agents` have a login on the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsRequest {
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsResponse {
    pub agents: Vec<AgentCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCredentials {
    pub agent: String,
    /// What holds a login: credential files, or the setup token.
    pub found: Vec<String>,
}

/// `put-credentials`: write credential files into the node user's
/// directory for `agent`. Names come from the agent's fixed set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutCredentialsRequest {
    pub agent: String,
    pub files: Vec<CredentialFile>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialFile {
    pub name: String,
    /// Base64 contents.
    pub data: String,
}

impl std::fmt::Debug for CredentialFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialFile")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutCredentialsResponse {
    /// Paths written on the node.
    pub written: Vec<String>,
    /// Anything the user should know about the result.
    pub notes: Vec<String>,
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
