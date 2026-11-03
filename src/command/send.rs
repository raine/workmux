use std::io::{IsTerminal, Read};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::agent_error::{AgentError, AgentErrorKind};
use crate::config;
use crate::multiplexer::{AgentPane, AgentStatus, Multiplexer, create_backend, detect_backend};
use crate::remote::agents::SentAgent;
use crate::remote::cache::{self, WorkspaceRecord};
use crate::state::sent::{TurnFence, same_second, sleep_to_next_second};
use crate::state::{PaneKey, StateStore};
use crate::util;
use crate::workflow::{self, AgentTarget};

/// How long an agent that was not working gets to show it acted on a prompt.
const ACTIVITY_WINDOW: Duration = Duration::from_secs(5);
/// How often `--wait` looks at the agent.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// What `--wait` holds out for after the send.
pub struct WaitFor {
    /// The statuses that end the wait once the agent acted on the prompt.
    pub until: Vec<AgentStatus>,
    pub timeout: Option<Duration>,
}

pub fn run(
    name: &str,
    text: Option<&str>,
    file: Option<&str>,
    wait: Option<WaitFor>,
) -> Result<()> {
    let cfg = config::Config::load(None).unwrap_or_default();
    let mux = create_backend(detect_backend());
    let target = workflow::resolve_agent_target(name, mux.as_ref())?;
    let content = read_content(text, file)?;
    let start = Instant::now();
    let deadline = wait
        .as_ref()
        .and_then(|wait| wait.timeout)
        .map(|timeout| start + timeout);
    let sent = match target {
        AgentTarget::Remote { record, pane } => {
            send_remote(&cfg, record, pane, &content, wait.is_some())?
        }
        AgentTarget::Local(agent) => {
            send_local(&cfg, mux.as_ref(), agent, &content, wait.is_some())?
        }
    };
    let (Some(wait), Some(mut sent)) = (wait, sent) else {
        return Ok(());
    };
    let status = await_turn(name, &mut sent, &wait.until, deadline)?;
    let elapsed = util::format_elapsed_duration(start.elapsed());
    eprintln!("{name}: {} ({elapsed})", status.name());
    Ok(())
}

/// A send `--wait` follows: the agent's state before it, and how to look at
/// the agent since.
struct Sent<'a> {
    fence: TurnFence,
    observe: Box<dyn FnMut() -> Result<Seen> + 'a>,
}

/// The agent a send went to, as last seen.
enum Seen {
    /// It cannot be seen right now.
    Unobserved,
    Gone,
    /// Its status, if the state belongs to the turn the send started.
    Agent(Option<AgentStatus>),
}

/// Type `content` into a local agent: refused while it is `waiting`, and,
/// with `wait`, while it has no status. A `done` or `waiting` state from
/// the current second is left behind first, so every status the prompt
/// causes carries a later timestamp.
fn send_local<'a>(
    cfg: &config::Config,
    mux: &'a dyn Multiplexer,
    agent: AgentPane,
    content: &str,
    wait: bool,
) -> Result<Option<Sent<'a>>> {
    let store = StateStore::new()?;
    let pane_id = agent.pane_id.clone();
    check_sendable(&agent, wait)?;
    let mut agent = agent;
    for _ in 0..3 {
        if !same_second(agent.status, agent.status_ts, cache::now()) {
            break;
        }
        sleep_to_next_second();
        agent = reload(&store, mux, &pane_id)?.ok_or_else(|| gone(&pane_id))?;
    }
    check_sendable(&agent, wait)?;
    let baseline = !same_second(agent.status, agent.status_ts, cache::now());
    let agent_identity = agent
        .agent_kind
        .as_deref()
        .or(cfg.agent.as_deref())
        .or(agent.agent_command.as_deref());
    mux.send_prompt_to_agent(&pane_id, content, agent_identity)?;
    let fence = TurnFence {
        status: agent.status.filter(|_| baseline),
        status_ts: agent.status_ts.filter(|_| baseline),
        sent_at: cache::now(),
    };
    let key = PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.clone(),
    };
    if let Err(err) = store.set_sent_turn(&key, &fence) {
        tracing::warn!(pane_id, "failed to record the sent turn: {err:#}");
    }
    if !wait {
        return Ok(None);
    }
    let observe_fence = fence.clone();
    Ok(Some(Sent {
        fence,
        observe: Box::new(move || {
            Ok(match reload(&store, mux, &pane_id)? {
                None => Seen::Gone,
                Some(agent) => Seen::Agent(
                    agent
                        .status
                        .filter(|_| observe_fence.admits(agent.status, agent.status_ts)),
                ),
            })
        }),
    }))
}

/// The live local agent in `pane_id`.
fn reload(store: &StateStore, mux: &dyn Multiplexer, pane_id: &str) -> Result<Option<AgentPane>> {
    Ok(store
        .load_reconciled_agents(mux)?
        .into_iter()
        .find(|agent| agent.pane_id == pane_id))
}

/// Refuse an agent whose dialog typed input would answer, or, for `wait`,
/// one whose turn could not be observed.
fn check_sendable(agent: &AgentPane, wait: bool) -> Result<()> {
    if agent.status == Some(AgentStatus::Waiting) {
        return Err(AgentError::error(
            AgentErrorKind::Waiting,
            format!(
                "the agent in pane {} is waiting on a dialog; nothing sent",
                agent.pane_id
            ),
        ));
    }
    if wait && agent.status.is_none() {
        return Err(AgentError::error(
            AgentErrorKind::StatusUnknown,
            format!(
                "the agent in pane {} has not reported a status; nothing sent",
                agent.pane_id
            ),
        ));
    }
    Ok(())
}

fn gone(pane_id: &str) -> anyhow::Error {
    AgentError::error(
        AgentErrorKind::Gone,
        format!("the agent in pane {pane_id} exited"),
    )
}

/// Type `content` into a remote agent through its node, which refuses as
/// [`send_local`] does.
fn send_remote(
    cfg: &config::Config,
    record: WorkspaceRecord,
    pane: Option<usize>,
    content: &str,
    wait: bool,
) -> Result<Option<Sent<'static>>> {
    if wait {
        // Observing takes a connection; start it before the turn does.
        crate::remote::observe::spawn(cfg, std::slice::from_ref(&record.remote));
    }
    let turn = crate::remote::agents::send(cfg, &record, pane, content, wait)?;
    if !wait {
        return Ok(None);
    }
    let turn = turn.ok_or_else(|| {
        anyhow!(
            "the node on '{}' cannot report the turn a send starts; `workmux remote setup {}` updates it",
            record.remote,
            record.remote
        )
    })?;
    Ok(Some(Sent {
        fence: turn.fence.clone(),
        observe: Box::new(move || {
            Ok(match crate::remote::agents::sent_agent(&record, &turn) {
                SentAgent::Unobserved => Seen::Unobserved,
                SentAgent::Gone => Seen::Gone,
                SentAgent::Present(agent) => {
                    Seen::Agent(agent.status.filter(|_| turn.admits(&agent)))
                }
            })
        }),
    }))
}

/// Wait until the agent `sent` went to acts on the prompt, then until the
/// turn shows one of `until`. An agent that was not working must show a
/// state of the turn within [`ACTIVITY_WINDOW`] of being first seen.
fn await_turn(
    name: &str,
    sent: &mut Sent<'_>,
    until: &[AgentStatus],
    deadline: Option<Instant>,
) -> Result<AgentStatus> {
    let mut active = sent.fence.status == Some(AgentStatus::Working);
    let mut seen_since: Option<Instant> = None;
    loop {
        match (sent.observe)()? {
            Seen::Unobserved => {}
            Seen::Gone => {
                return Err(AgentError::error(
                    AgentErrorKind::Gone,
                    format!("the agent of '{name}' exited before its turn settled"),
                ));
            }
            Seen::Agent(status) => {
                seen_since.get_or_insert_with(Instant::now);
                if let Some(status) = status {
                    active = true;
                    if until.contains(&status) {
                        return Ok(status);
                    }
                }
            }
        }
        if !active && seen_since.is_some_and(|since| since.elapsed() >= ACTIVITY_WINDOW) {
            return Err(AgentError::error(
                AgentErrorKind::Stalled,
                format!(
                    "the agent of '{name}' showed no activity within {}s of the send",
                    ACTIVITY_WINDOW.as_secs()
                ),
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(AgentError::error(
                AgentErrorKind::Timeout,
                format!("timed out waiting for the agent of '{name}'"),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// The text to send: positional arg > --file > stdin, without its trailing
/// newlines.
fn read_content(text: Option<&str>, file: Option<&str>) -> Result<String> {
    let content = if let Some(t) = text {
        t.to_string()
    } else if let Some(f) = file {
        std::fs::read_to_string(f)?
    } else {
        // Guard: don't block on interactive TTY
        if std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "No content to send. Provide text argument, --file, or pipe stdin"
            ));
        }
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let content = content.trim_end_matches('\n');
    if content.is_empty() {
        return Err(anyhow!("No content to send"));
    }
    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(was: Option<AgentStatus>, seen: Vec<Seen>) -> Sent<'static> {
        let mut seen = seen.into_iter();
        Sent {
            fence: TurnFence {
                status: was,
                status_ts: Some(1),
                sent_at: 1,
            },
            observe: Box::new(move || Ok(seen.next().unwrap_or(Seen::Unobserved))),
        }
    }

    fn kind(result: Result<AgentStatus>) -> AgentErrorKind {
        crate::agent_error::kind_of(&result.unwrap_err()).unwrap()
    }

    const SETTLED: &[AgentStatus] = &[AgentStatus::Done, AgentStatus::Waiting];

    #[test]
    fn the_wait_ends_at_the_first_until_status_of_the_turn() {
        let done = Some(AgentStatus::Done);
        let mut turn = sent(
            done,
            vec![
                Seen::Unobserved,
                Seen::Agent(None),
                Seen::Agent(Some(AgentStatus::Working)),
                Seen::Agent(done),
            ],
        );
        assert_eq!(
            await_turn("f", &mut turn, SETTLED, None).unwrap(),
            AgentStatus::Done
        );

        let mut turn = sent(done, vec![Seen::Agent(Some(AgentStatus::Working))]);
        assert_eq!(
            await_turn("f", &mut turn, &[AgentStatus::Working], None).unwrap(),
            AgentStatus::Working
        );
    }

    #[test]
    fn an_agent_that_leaves_is_gone() {
        let mut turn = sent(Some(AgentStatus::Done), vec![Seen::Agent(None), Seen::Gone]);
        assert_eq!(
            kind(await_turn("f", &mut turn, SETTLED, None)),
            AgentErrorKind::Gone
        );
    }

    #[test]
    fn the_deadline_ends_a_wait_for_a_working_agent() {
        let working = Some(AgentStatus::Working);
        let mut turn = sent(working, (0..100).map(|_| Seen::Agent(working)).collect());
        let deadline = Some(Instant::now() + Duration::from_millis(300));
        assert_eq!(
            kind(await_turn("f", &mut turn, SETTLED, deadline)),
            AgentErrorKind::Timeout
        );
    }
}
