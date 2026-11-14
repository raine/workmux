use anyhow::{Result, anyhow};
use tracing::info;

use crate::config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::remote;
use crate::state::StateStore;
use crate::workflow::resurrect::{ResurrectAction, plan};
use crate::workflow::{self, SetupOptions, WorkflowContext};

pub fn run(dry_run: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let store = StateStore::new()?;

    let plan = plan(&store, mux.as_ref())?;

    if plan.candidates.is_empty()
        && plan.remote.is_empty()
        && plan.forgotten_remote_sources.is_empty()
        && plan.unmatched_states == 0
    {
        println!("No agent state files found. Nothing to restore.");
        return Ok(());
    }

    // Print plan
    let to_restore: Vec<_> = plan
        .candidates
        .iter()
        .filter(|c| matches!(c.action, ResurrectAction::Restore))
        .collect();

    for candidate in &plan.candidates {
        let status = match &candidate.action {
            ResurrectAction::Restore => "restoring",
            ResurrectAction::SkipAlreadyOpen => "skipping (already open)",
            ResurrectAction::SkipMain => "skipping (main worktree)",
        };
        println!("  {:<20} -> {}", candidate.handle, status);
    }

    // Remote workspaces come back as windows attached to agents that kept
    // running; an execution the remote reports ended has nothing to attach.
    let remote_candidates: Vec<_> = plan
        .remote
        .iter()
        .map(|c| {
            let restore = matches!(c.action, ResurrectAction::Restore);
            (c, restore && execution_ended(&c.record))
        })
        .collect();
    let remote_restore: Vec<_> = remote_candidates
        .iter()
        .filter(|(c, ended)| matches!(c.action, ResurrectAction::Restore) && !ended)
        .map(|(c, _)| *c)
        .collect();
    for (candidate, ended) in &remote_candidates {
        let status = match &candidate.action {
            ResurrectAction::Restore if *ended => {
                "skipping (agents exited; `workmux open` starts them)"
            }
            ResurrectAction::Restore => "restoring",
            ResurrectAction::SkipAlreadyOpen => "skipping (already open)",
            ResurrectAction::SkipMain => "skipping (main worktree)",
        };
        let name = remote::agents::label(&candidate.record.handle, &candidate.record.remote);
        println!("  {:<20} -> {}", name, status);
    }

    if plan.unmatched_states > 0 {
        println!(
            "  ({} unmatched state file(s) ignored)",
            plan.unmatched_states
        );
    }
    if !dry_run {
        // Their workspaces were removed or their agents stopped.
        let ended = remote_candidates
            .iter()
            .filter(|(_, ended)| *ended)
            .flat_map(|(c, _)| c.stale_sources.iter().cloned());
        let gone: Vec<_> = plan
            .forgotten_remote_sources
            .iter()
            .cloned()
            .chain(ended)
            .collect();
        if !gone.is_empty()
            && let Err(error) = store.consume_agent_sources(&gone)
        {
            tracing::warn!(%error, "resurrect:exec failed to consume stale remote state");
        }
    }

    if to_restore.is_empty() && remote_restore.is_empty() {
        println!("\nNothing to restore.");
        return Ok(());
    }

    if dry_run {
        println!(
            "\nDry run: would restore {} worktree(s)",
            to_restore.len() + remote_restore.len()
        );
        return Ok(());
    }

    // Execute restoration
    let context = WorkflowContext::new(config, mux, None)?;
    let mut restored = Vec::new();
    let mut failed = Vec::new();

    for candidate in &plan.candidates {
        if !matches!(candidate.action, ResurrectAction::Restore) {
            continue;
        }

        let options = SetupOptions {
            run_hooks: false,
            run_file_ops: false,
            run_pane_commands: true,
            prompt_file_path: None,
            focus_window: false,
            working_dir: None,
            config_root: None,
            open_if_exists: false,
            mode: candidate.mode,
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            window_token: None,
            primary_window: true,
            resume_mode: crate::multiplexer::types::ResumeMode::Continue,
        };

        info!(
            handle = candidate.handle,
            mode = ?candidate.mode,
            stale_keys = candidate.stale_sources.len(),
            "resurrect:exec opening worktree"
        );

        match workflow::open(
            &candidate.handle,
            &context,
            options,
            false,
            None,
            None,
            candidate.agent.as_deref(),
        ) {
            Ok(result) => {
                info!(
                    handle = candidate.handle,
                    resolved = result.resolved_handle,
                    branch = result.branch_name,
                    path = %result.worktree_path.display(),
                    "resurrect:exec restored successfully"
                );
                if let Err(error) = store.consume_agent_sources(&candidate.stale_sources) {
                    tracing::warn!(%error, "resurrect:exec failed to consume stale state");
                }
                restored.push(candidate.handle.clone());
            }
            Err(e) => {
                info!(
                    handle = candidate.handle,
                    error = %e,
                    "resurrect:exec failed to restore"
                );
                eprintln!("  Failed to restore '{}': {}", candidate.handle, e);
                failed.push(candidate.handle.clone());
            }
        }
    }

    for candidate in remote_restore {
        let record = &candidate.record;
        match remote::lifecycle::open_window(&context, record, false) {
            Ok(()) => {
                info!(
                    handle = record.handle,
                    remote = record.remote,
                    "resurrect:exec reattached"
                );
                if let Err(error) = store.consume_agent_sources(&candidate.stale_sources) {
                    tracing::warn!(%error, "resurrect:exec failed to consume stale state");
                }
                restored.push(remote::agents::label(&record.handle, &record.remote));
            }
            Err(e) => {
                eprintln!("  Failed to restore '{}': {:#}", record.handle, e);
                failed.push(record.handle.clone());
            }
        }
    }

    // Summary
    if !restored.is_empty() {
        println!(
            "\n✓ Restored {} worktree(s): {}",
            restored.len(),
            restored.join(", ")
        );
    }
    if !failed.is_empty() {
        eprintln!(
            "✗ Failed to restore {} worktree(s): {}",
            failed.len(),
            failed.join(", ")
        );
        return Err(anyhow!("Failed to restore {} worktree(s)", failed.len()));
    }

    Ok(())
}

/// Whether the remote, as last observed and still connected, reports every
/// pane of the workspace's execution ended. Unknown counts as running.
fn execution_ended(record: &remote::cache::WorkspaceRecord) -> bool {
    let Some(execution) = &record.execution else {
        return true;
    };
    remote::observe::load_connected(&record.remote).is_some_and(|observation| {
        observation
            .execution(&record.ws_id, &execution.ex_id)
            .is_some_and(|workspace| workspace.panes.iter().all(|pane| !pane.alive))
    })
}
