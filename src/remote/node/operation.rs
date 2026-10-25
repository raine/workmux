//! Requests from a workspace for laptop-owned lifecycle operations.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use super::{NodeRoot, validate_id};
use crate::remote::cache;

pub const TIMEOUT: u64 = 15;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Remove {
        force: bool,
    },
    Merge {
        into: Option<String>,
        rebase: bool,
        squash: bool,
        ignore_uncommitted: bool,
        keep: bool,
        cleanup: bool,
        no_verify: bool,
        no_hooks: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub operation: Operation,
    pub requested_at: u64,
    pub claimed: bool,
    /// The laptop's stages so far, shown while the requester waits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress: Vec<String>,
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub success: bool,
    pub message: String,
}

impl Request {
    pub fn pending(&self, now: u64) -> bool {
        !self.claimed && self.outcome.is_none() && now.saturating_sub(self.requested_at) < TIMEOUT
    }
}

/// Either marker means this command must not operate on local worktrees.
pub fn in_session() -> bool {
    std::env::var_os(super::spawn::SESSION_ROOT_ENV).is_some()
        || std::env::var_os(super::spawn::SESSION_WORKSPACE_ENV).is_some()
}

pub fn validate_names(names: &[String], handle: &str) -> Result<()> {
    if names.len() > 1
        || names
            .first()
            .is_some_and(|name| name != "." && name != handle)
    {
        bail!("a remote workspace may only request an operation on its own handle");
    }
    Ok(())
}

pub fn submit(names: &[String], operation: Operation) -> Result<()> {
    let (path, ws_id) = super::spawn::session_parent().ok_or_else(|| {
        anyhow::anyhow!("incomplete remote workspace session; run this on the laptop")
    })?;
    validate_id(&ws_id, 'w', "workspace id")?;
    let root = NodeRoot::at(path)?;
    let id = cache::new_id('r');
    {
        let _lock = root.lock_workspace(&ws_id)?;
        let mut ws = root
            .load(&ws_id)?
            .ok_or_else(|| anyhow::anyhow!("workspace no longer exists"))?;
        validate_names(names, &ws.identity.handle)?;
        if ws
            .operation
            .as_ref()
            .is_some_and(|r| r.outcome.is_none() && (r.claimed || r.pending(cache::now())))
        {
            bail!("a laptop operation is already pending for this workspace");
        }
        let verb = match operation {
            Operation::Remove { .. } => "Removing",
            Operation::Merge { .. } => "Merging",
        };
        println!("{verb} '{}' on the laptop…", ws.identity.handle);
        ws.operation = Some(Request {
            id: id.clone(),
            operation,
            requested_at: cache::now(),
            claimed: false,
            progress: Vec::new(),
            outcome: None,
        });
        root.save(&ws)?;
    }
    let mut status = Status::new();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Some(ws) = root.load(&ws_id)? else {
            status.finish();
            return Ok(());
        };
        let Some(request) = ws.operation.as_ref().filter(|r| r.id == id) else {
            status.finish();
            bail!("laptop request was withdrawn");
        };
        status.show(&request.progress);
        if let Some(outcome) = &request.outcome {
            status.finish();
            if outcome.success {
                println!("{}", outcome.message);
                return Ok(());
            }
            bail!("{}", outcome.message);
        }
        if !request.claimed && !request.pending(cache::now()) {
            // Claim and withdrawal serialize on the same lock and node clock.
            if let Ok(_lock) = root.lock_workspace(&ws_id) {
                let Some(mut ws) = root.load(&ws_id)? else {
                    return Ok(());
                };
                if ws
                    .operation
                    .as_ref()
                    .is_some_and(|r| r.id == id && !r.claimed)
                {
                    ws.operation = None;
                    root.save(&ws)?;
                    status.finish();
                    bail!("the laptop isn't connected or runs an older workmux; request withdrawn");
                }
            }
        }
    }
}

/// The waiting requester's view of the laptop's progress. A terminal gets one
/// line per stage with a live elapsed time on the latest; other output gets
/// plain lines.
struct Status {
    seen: usize,
    current: Option<String>,
    since: Instant,
    shown: u64,
    live: bool,
}

impl Status {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            seen: 0,
            current: None,
            since: Instant::now(),
            shown: 0,
            live: std::io::stdout().is_terminal(),
        }
    }

    fn show(&mut self, stages: &[String]) {
        use std::io::Write;
        for stage in stages.iter().skip(self.seen) {
            self.finish();
            // The laptop is trusted, but a terminal must not interpret its text.
            let stage: String = stage.chars().filter(|c| !c.is_control()).collect();
            if self.live {
                print!("{stage}");
                self.current = Some(stage);
            } else {
                println!("{stage}");
            }
            self.since = Instant::now();
            self.shown = 0;
        }
        self.seen = self.seen.max(stages.len());
        let elapsed = self.since.elapsed().as_secs();
        if let Some(stage) = &self.current
            && elapsed > self.shown
        {
            self.shown = elapsed;
            print!("\r\x1b[K{stage} ({elapsed}s)");
        }
        let _ = std::io::stdout().flush();
    }

    fn finish(&mut self) {
        if self.current.take().is_some() {
            println!();
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Update {
    pub ws_id: String,
    pub id: String,
    /// Reports a stage of a claimed request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// Neither field claims the request; Some reports its outcome.
    pub outcome: Option<Outcome>,
}

pub fn update(root: &NodeRoot, update: Update) -> Result<Operation> {
    validate_id(&update.ws_id, 'w', "workspace id")?;
    validate_id(&update.id, 'r', "request id")?;
    // Claims and replies must survive brief contention with workspace verbs.
    let deadline = Instant::now() + Duration::from_secs(5);
    let _lock = loop {
        match root.lock_workspace(&update.ws_id) {
            Ok(lock) => break lock,
            Err(err)
                if crate::remote::client::error_code(&err)
                    == Some(crate::remote::protocol::code::BUSY)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err),
        }
    };
    let mut ws = root
        .load(&update.ws_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace no longer exists"))?;
    let request = ws
        .operation
        .as_mut()
        .filter(|r| r.id == update.id)
        .ok_or_else(|| anyhow::anyhow!("request no longer exists"))?;
    if update.outcome.is_some() || update.progress.is_some() {
        if !request.claimed || request.outcome.is_some() {
            bail!("request is not running")
        }
        // A few stages per operation; the bound keeps the record small.
        if let Some(progress) = update.progress
            && request.progress.len() < 32
        {
            request.progress.push(progress);
        }
        if let Some(outcome) = update.outcome {
            request.outcome = Some(outcome);
        }
    } else {
        if !request.pending(cache::now()) {
            bail!("request expired or already claimed")
        }
        request.claimed = true;
    }
    let operation = request.operation.clone();
    root.save(&ws)?;
    Ok(operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_atomic_and_expired_requests_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot::at(dir.path().to_owned()).unwrap();
        let identity = crate::remote::protocol::WorkspaceIdentity {
            ws_id: cache::new_id('w'),
            project_id: "project".into(),
            handle: "mine".into(),
            branch: "mine".into(),
            base_sha: "0".repeat(40),
        };
        let mut ws =
            super::super::Workspace::new(&root, identity, crate::config::RemoteIsolation::None);
        let id = cache::new_id('r');
        ws.operation = Some(Request {
            id: id.clone(),
            operation: Operation::Remove { force: false },
            requested_at: cache::now(),
            claimed: false,
            progress: Vec::new(),
            outcome: None,
        });
        root.save(&ws).unwrap();
        let claim = || Update {
            ws_id: ws.identity.ws_id.clone(),
            id: id.clone(),
            progress: None,
            outcome: None,
        };
        let mut progress = claim();
        progress.progress = Some("Running pre_merge hooks…".into());
        assert!(update(&root, progress).is_err());
        assert!(update(&root, claim()).is_ok());
        assert!(update(&root, claim()).is_err());
        let mut progress = claim();
        progress.progress = Some("Running pre_merge hooks…".into());
        update(&root, progress).unwrap();
        assert_eq!(
            root.load(&ws.identity.ws_id)
                .unwrap()
                .unwrap()
                .operation
                .unwrap()
                .progress,
            ["Running pre_merge hooks…"]
        );
        let mut complete = claim();
        complete.outcome = Some(Outcome {
            success: false,
            message: "refused".into(),
        });
        update(&root, complete).unwrap();
        assert!(update(&root, claim()).is_err());
        ws.operation.as_mut().unwrap().requested_at = cache::now() - TIMEOUT;
        root.save(&ws).unwrap();
        assert!(update(&root, claim()).is_err());
        ws.operation = None;
        root.save(&ws).unwrap();
        assert!(update(&root, claim()).is_err());
    }

    #[test]
    fn own_handle_only() {
        for names in [vec![], vec![".".into()], vec!["mine".into()]] {
            assert!(validate_names(&names, "mine").is_ok());
        }
        for names in [vec!["other".into()], vec!["mine".into(), "mine".into()]] {
            assert!(validate_names(&names, "mine").is_err());
        }
    }

    #[test]
    fn expiry_and_claim() {
        let mut request = Request {
            id: cache::new_id('r'),
            operation: Operation::Remove { force: false },
            requested_at: 100,
            claimed: false,
            progress: Vec::new(),
            outcome: None,
        };
        assert!(request.pending(114));
        assert!(!request.pending(115));
        request.claimed = true;
        assert!(!request.pending(101));
    }
}
