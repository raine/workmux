//! Switch to the last visited agent (toggle between two agents).

use anyhow::Result;

use crate::command::pane_history::pane_to_remember;
use crate::multiplexer::{create_backend, detect_backend};
use crate::state::StateStore;

/// Switch to the last visited agent.
///
/// Reads `last_pane_id` from GlobalSettings and switches to that pane.
/// Updates last_pane_id to the current pane after a successful switch when it
/// differs from the target pane.
pub fn run() -> Result<()> {
    let mux = create_backend(detect_backend());
    let store = StateStore::new()?;

    // Load agents for window hints when the target is an agent pane.
    let agents = store
        .load_reconciled_agents(mux.as_ref())
        .unwrap_or_default();

    let settings = store.load_settings()?;
    let Some(target_pane_id) = settings.last_pane_id else {
        println!("No previous agent to switch to");
        return Ok(());
    };

    if mux.get_live_pane_info(&target_pane_id)?.is_none() {
        println!("Last pane no longer exists");
        return Ok(());
    }

    // Get current pane BEFORE switching (this is what becomes "last")
    let current_pane = mux.active_pane_id();

    // Guard: don't switch if already at target (avoids losing history)
    if current_pane.as_deref() == Some(target_pane_id.as_str()) {
        println!("Already at last agent");
        return Ok(());
    }

    // Attempt the switch
    let window_hint = agents
        .iter()
        .find(|a| a.pane_id == target_pane_id)
        .map(|a| a.window_name.as_str());
    if mux.switch_to_pane(&target_pane_id, window_hint).is_err() {
        println!("Failed to switch to last agent");
        return Ok(());
    }

    // Persist the pane we came from so hotkeys can toggle back even when
    // invoked from a normal shell/editor pane.
    if let Some(current) = pane_to_remember(current_pane.as_deref(), &target_pane_id) {
        let mut settings = store.load_settings()?;
        settings.last_pane_id = Some(current.to_string());
        store.save_settings(&settings)?;
    }

    Ok(())
}
