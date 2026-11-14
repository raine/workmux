//! Failures of commands that drive an agent, each with a stable name and
//! exit code that scripts can tell apart.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentErrorKind {
    /// The agent waits on a dialog that typed input would answer.
    Waiting,
    /// The target matches more than one agent.
    AmbiguousTarget,
    /// No live agent matches the target.
    NotFound,
    /// An agent that was not working showed no activity after a send.
    Stalled,
    /// The agent exited while being waited on.
    Gone,
    /// The agent has reported no status to wait on.
    StatusUnknown,
    /// The caller's deadline passed.
    Timeout,
}

impl AgentErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Waiting => "agent_waiting",
            Self::AmbiguousTarget => "ambiguous_target",
            Self::NotFound => "agent_not_found",
            Self::Stalled => "send_stalled",
            Self::Gone => "agent_gone",
            Self::StatusUnknown => "agent_status_unknown",
            Self::Timeout => "timeout",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Waiting => 3,
            Self::AmbiguousTarget => 4,
            Self::NotFound => 5,
            Self::Stalled => 6,
            Self::Gone => 7,
            Self::StatusUnknown => 8,
            Self::Timeout => 124,
        }
    }
}

#[derive(Debug)]
pub struct AgentError {
    pub kind: AgentErrorKind,
    message: String,
}

impl AgentError {
    pub fn error(kind: AgentErrorKind, message: impl Into<String>) -> anyhow::Error {
        Self {
            kind,
            message: message.into(),
        }
        .into()
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentError {}

/// The kind of `err`, if it is an [`AgentError`].
pub fn kind_of(err: &anyhow::Error) -> Option<AgentErrorKind> {
    err.downcast_ref::<AgentError>().map(|err| err.kind)
}
