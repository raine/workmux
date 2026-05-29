use crate::command::args::{MultiArgs, PromptArgs, RescueArgs, SetupFlags};
use crate::config::{MuxMode, SidebarPosition};
use crate::{claude, command, config, git, nerdfont};
use anyhow::{Context, Result};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use std::path::PathBuf;

macro_rules! impl_passthrough_typed_value_parser {
    ($parser:ty) => {
        impl clap::builder::TypedValueParser for $parser {
            type Value = String;

            fn parse_ref(
                &self,
                cmd: &clap::Command,
                _arg: Option<&clap::Arg>,
                value: &std::ffi::OsStr,
            ) -> Result<Self::Value, clap::Error> {
                clap::builder::StringValueParser::new().parse_ref(cmd, None, value)
            }

            fn possible_values(
                &self,
            ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
                None
            }
        }
    };
}

fn try_list_worktrees() -> Option<Vec<(PathBuf, String)>> {
    if !git::is_git_repo().unwrap_or(false) {
        return None;
    }
    git::list_worktrees().ok()
}

#[derive(Clone, Debug)]
struct WorktreeBranchParser;

impl WorktreeBranchParser {
    fn new() -> Self {
        Self
    }

    fn get_branches(&self) -> Vec<String> {
        let Some(worktrees) = try_list_worktrees() else {
            return Vec::new();
        };

        let main_branch = git::get_default_branch().ok();

        worktrees
            .into_iter()
            .map(|(_, branch)| branch)
            // Filter out the main branch, as it's not a candidate for merging/removing.
            .filter(|branch| main_branch.as_deref() != Some(branch.as_str()))
            // Filter out detached HEAD states.
            .filter(|branch| branch != "(detached)")
            .collect()
    }
}

impl_passthrough_typed_value_parser!(WorktreeBranchParser);

/// Parser for worktree handles (directory names), used for open/path/remove commands.
#[derive(Clone, Debug)]
struct WorktreeHandleParser;

impl WorktreeHandleParser {
    fn new() -> Self {
        Self
    }

    fn get_handles() -> Vec<String> {
        let Some(worktrees) = try_list_worktrees() else {
            return Vec::new();
        };

        let main_worktree_root = git::get_main_worktree_root().ok();

        worktrees
            .into_iter()
            .filter_map(|(path, _)| {
                // Filter out the main worktree
                if main_worktree_root.as_ref() == Some(&path) {
                    return None;
                }
                // Extract directory name as the handle
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .collect()
    }
}

impl_passthrough_typed_value_parser!(WorktreeHandleParser);

/// Parser for agent targets, used for send/capture/status/wait/run commands.
///
/// Includes local worktree handles plus active agents from other projects.
/// Cross-project agents appear as `project:handle` when their name would be ambiguous.
#[derive(Clone, Debug)]
struct AgentTargetParser;

impl AgentTargetParser {
    fn new() -> Self {
        Self
    }

    fn get_targets() -> Vec<String> {
        // Start with local worktree handles
        let mut targets = WorktreeHandleParser::get_handles();

        // Also include the main worktree handle (agents can run there too)
        if git::is_git_repo().unwrap_or(false)
            && let Ok(main_root) = git::get_main_worktree_root()
            && let Some(name) = main_root.file_name()
        {
            let handle = name.to_string_lossy().to_string();
            if !targets.contains(&handle) {
                targets.push(handle);
            }
        }

        // Append global agent handles from reconciled state
        let mux = crate::multiplexer::create_backend(crate::multiplexer::detect_backend());
        if let Ok(store) = crate::state::StateStore::new()
            && let Ok(agents) = store.load_reconciled_agents(mux.as_ref())
        {
            for agent in &agents {
                let root = crate::workflow::find_worktree_root(&agent.path)
                    .unwrap_or_else(|| agent.path.clone());
                if let Some(name) = root.file_name() {
                    let handle = name.to_string_lossy().to_string();
                    if !targets.contains(&handle) {
                        targets.push(handle.clone());
                    }
                    // Also add qualified project:handle for disambiguation
                    if let Some(parent) = root.parent()
                        && let Some(proj) = parent.file_name()
                    {
                        let qualified = format!("{}:{}", proj.to_string_lossy(), handle);
                        if !targets.contains(&qualified) {
                            targets.push(qualified);
                        }
                    }
                }
            }
        }

        targets.sort();
        targets.dedup();
        targets
    }
}

impl_passthrough_typed_value_parser!(AgentTargetParser);

#[derive(Clone, Debug)]
struct GitBranchParser;

impl GitBranchParser {
    fn new() -> Self {
        Self
    }

    fn get_branches() -> Vec<String> {
        // Don't attempt completions if not in a git repo.
        if !git::is_git_repo().unwrap_or(false) {
            return Vec::new();
        }

        // Fail silently on completion; don't disrupt the user's shell.
        git::list_checkout_branches().unwrap_or_default()
    }
}

impl_passthrough_typed_value_parser!(GitBranchParser);

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "workmux")]
#[command(about = "An opinionated workflow tool that orchestrates git worktrees and tmux")]
#[command(help_template = "\
{about}

{usage-heading} {usage}

Worktree lifecycle:
  add          Create a new worktree and tmux window
  remove       Remove a worktree, tmux window, and branch without merging [rm]
  rename       Rename a worktree, tmux window/session, and optionally branch
  merge        Merge a branch, then clean up the worktree and tmux window
  rebase       Rebase a worktree branch onto its base branch
  open         Open a tmux window for an existing worktree
  close        Close a worktree's tmux window (keeps the worktree and branch)
  resurrect    Restore worktree windows after a tmux or computer crash

Monitoring:
  dashboard    Show a TUI dashboard of all active workmux agents
  sidebar      Toggle a live agent status sidebar in tmux
  list         List all worktrees [ls]
  path         Get the filesystem path of a worktree
  status       Query agent status for worktrees

Setup and configuration:
  init         Generate example .workmux.yaml configuration file
  setup        Set up agent status tracking hooks and install skills
  config       Manage global configuration
  sandbox      Manage sandbox settings
  sync-files   Re-apply file operations (copy/symlink) to worktrees
  claude       Claude Code integration commands

Agent interaction:
  send         Send a prompt or instruction to a running agent
  capture      Capture terminal output from a running agent
  wait         Wait for agents to reach a target status
  run          Run a command in a worktree's window
  reap-agents  Exit tracked agent processes older than a configured age

Help and updates:
  docs         Show detailed documentation (renders README.md)
  changelog    Show the changelog (what's new in each version)
  update       Update workmux to the latest version
  completions  Generate shell completions
  help         Print help for a command

Options:
  -h, --help     Print help
  -V, --version  Print version

Run 'workmux docs' for detailed documentation.
")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliMuxMode {
    Window,
    Session,
}

impl From<CliMuxMode> for MuxMode {
    fn from(value: CliMuxMode) -> Self {
        match value {
            CliMuxMode::Window => MuxMode::Window,
            CliMuxMode::Session => MuxMode::Session,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new worktree and tmux window
    Add {
        /// Name of the branch (creates if it doesn't exist) or remote ref (e.g., origin/feature).
        /// When used with --pr, this becomes the custom local branch name.
        #[arg(required_unless_present_any = ["pr", "auto_name"], value_parser = GitBranchParser::new())]
        branch_name: Option<String>,

        /// Pull request number to checkout
        #[arg(long, conflicts_with_all = ["base", "auto_name"])]
        pr: Option<u32>,

        /// Generate branch name from prompt using LLM
        #[arg(short = 'A', long = "auto-name", conflicts_with = "pr")]
        auto_name: bool,

        /// Base branch/commit/tag to branch from (overrides config base_branch, defaults to current branch)
        #[arg(long)]
        base: Option<String>,

        /// Explicit name for the worktree directory and tmux window (overrides worktree_naming strategy and worktree_prefix)
        #[arg(long)]
        name: Option<String>,

        /// Explicit name for the workmux-managed tmux target
        #[arg(long = "target-name")]
        target_name: Option<String>,

        /// Parent tmux session for window-mode targets
        #[arg(long = "parent-session")]
        parent_session: Option<String>,

        #[command(flatten)]
        prompt: PromptArgs,

        #[command(flatten)]
        setup: SetupFlags,

        #[command(flatten)]
        rescue: RescueArgs,

        #[command(flatten)]
        multi: MultiArgs,

        /// Use a named pane layout from config instead of default panes
        #[arg(short = 'l', long, conflicts_with = "agent")]
        layout: Option<String>,

        /// Fork the last conversation from the current worktree into the new one.
        /// Specify a session ID to fork a specific conversation, or omit for most recent.
        #[arg(long, num_args = 0..=1, default_missing_value = "", require_equals = true)]
        fork: Option<String>,

        /// Block until the created tmux window is closed
        #[arg(short = 'W', long)]
        wait: bool,

        /// Override the multiplexer mode for this command only
        #[arg(long, value_enum)]
        mode: Option<CliMuxMode>,

        /// Create the window in its own tmux session (useful for session-per-project workflows)
        #[arg(short = 's', long, conflicts_with = "mode")]
        session: bool,

        /// Use an alternate config file for this invocation (still merges with global config)
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        config: Option<PathBuf>,
    },

    /// Open a tmux window for an existing worktree
    Open {
        /// Worktree name(s) (directory name, visible in tmux window). Optional with --new.
        #[arg(value_parser = WorktreeHandleParser::new(), required_unless_present = "new")]
        names: Vec<String>,

        /// Re-run post-create hooks (e.g., pnpm install)
        #[arg(long)]
        run_hooks: bool,

        /// Re-apply file operations (copy/symlink)
        #[arg(long)]
        force_files: bool,

        /// Force opening in a new window (creates suffix like -2, -3) instead of switching to existing
        #[arg(long, short = 'n')]
        new: bool,

        /// Override the multiplexer mode for this command only
        #[arg(long, value_enum)]
        mode: Option<CliMuxMode>,

        /// Open in session mode (overrides stored mode for this worktree)
        #[arg(short = 's', long, conflicts_with = "mode")]
        session: bool,

        /// Explicit name for the workmux-managed tmux target
        #[arg(long = "target-name")]
        target_name: Option<String>,

        /// Parent tmux session for window-mode targets
        #[arg(long = "parent-session")]
        parent_session: Option<String>,

        /// Resume the agent's most recent conversation in this worktree
        #[arg(short = 'c', long = "continue")]
        continue_session: bool,

        #[command(flatten)]
        prompt: PromptArgs,

        /// Use an alternate config file for this invocation (still merges with global config)
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        config: Option<PathBuf>,
    },

    /// Close a worktree's tmux window (keeps the worktree and branch)
    Close {
        /// Worktree name (defaults to current directory if omitted)
        #[arg(value_parser = WorktreeHandleParser::new())]
        name: Option<String>,
    },

    /// Restore worktree windows after a tmux or computer crash
    ///
    /// Uses persisted agent state files to detect which worktrees had active
    /// agents before the crash.
    Resurrect {
        /// Show what would be restored without doing it
        #[arg(long)]
        dry_run: bool,
    },

    /// Merge a branch, then clean up the worktree and tmux window
    Merge {
        /// Worktree name or branch (defaults to current directory)
        #[arg(value_parser = WorktreeHandleParser::new())]
        name: Option<String>,

        /// The target branch to merge into (defaults to main_branch from config)
        #[arg(long, value_parser = GitBranchParser::new())]
        into: Option<String>,

        /// Ignore uncommitted and staged changes
        #[arg(long)]
        ignore_uncommitted: bool,

        /// Rebase the branch onto the main branch before merging (fast-forward)
        #[arg(long, group = "merge_strategy")]
        rebase: bool,

        /// Squash all commits from the branch into a single commit on the main branch
        #[arg(long, group = "merge_strategy")]
        squash: bool,

        /// Keep the worktree, window, and branch after merging (skip cleanup)
        #[arg(short = 'k', long)]
        keep: bool,

        /// Clean up the worktree, window, and branch after merging
        #[arg(long, conflicts_with = "keep")]
        cleanup: bool,

        /// Skip running pre-merge hooks
        #[arg(short = 'n', long)]
        no_verify: bool,

        /// Skip running all hooks (pre-merge and pre-remove)
        #[arg(long)]
        no_hooks: bool,

        /// Show a system notification on successful merge
        #[arg(long)]
        notification: bool,
    },

    /// Rebase a worktree branch onto its base branch
    Rebase {
        /// Worktree name or branch (defaults to current directory)
        #[arg(value_parser = WorktreeHandleParser::new())]
        name: Option<String>,
    },

    /// Rename a worktree, its tmux window/session, and (optionally) its branch
    Rename {
        /// [OLD_NAME] NEW_NAME. If only one argument is given, renames the current worktree.
        #[arg(required = true, num_args = 1..=2, value_parser = WorktreeHandleParser::new())]
        names: Vec<String>,

        /// Also rename the underlying git branch to match the new handle
        #[arg(short = 'b', long)]
        branch: bool,
    },

    /// Remove a worktree, tmux window, and branch without merging
    #[command(visible_alias = "rm")]
    Remove {
        /// Worktree names (defaults to current directory name if empty)
        #[arg(value_parser = WorktreeHandleParser::new(), conflicts_with_all = ["gone", "all"], num_args = 0..)]
        names: Vec<String>,

        /// Remove worktrees whose upstream remote branch has been deleted (e.g., after PR merge)
        #[arg(long, conflicts_with = "all")]
        gone: bool,

        /// Remove all worktrees (except the main worktree)
        #[arg(long)]
        all: bool,

        /// Skip confirmation and ignore uncommitted changes
        #[arg(short, long)]
        force: bool,

        /// Keep the local branch (only remove worktree and tmux window)
        #[arg(short = 'k', long)]
        keep_branch: bool,
    },

    /// List all worktrees
    #[command(visible_alias = "ls")]
    List {
        /// Show PR status for each worktree (requires gh CLI)
        #[arg(long)]
        pr: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Filter by worktree name or branch (supports multiple)
        #[arg(value_parser = WorktreeBranchParser::new())]
        filter: Vec<String>,
    },

    /// Get the filesystem path of a worktree
    Path {
        /// Worktree name (directory name)
        #[arg(value_parser = WorktreeHandleParser::new())]
        name: String,
    },

    /// Send a prompt or instruction to a running agent
    Send {
        /// Worktree name (supports cross-project with project:handle syntax)
        #[arg(value_parser = AgentTargetParser::new())]
        name: String,

        /// Text to send (reads from --file or stdin if omitted)
        #[arg(conflicts_with = "file")]
        text: Option<String>,

        /// Read prompt from file
        #[arg(short, long, conflicts_with = "text")]
        file: Option<String>,
    },

    /// Capture terminal output from a running agent
    Capture {
        /// Worktree name (supports cross-project with project:handle syntax)
        #[arg(value_parser = AgentTargetParser::new())]
        name: String,

        /// Number of lines to capture
        #[arg(short = 'n', long, default_value = "200")]
        lines: u16,
    },

    /// Query agent status for worktrees
    Status {
        /// Worktree names (supports cross-project with project:handle syntax)
        #[arg(value_parser = AgentTargetParser::new())]
        worktrees: Vec<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Include git info (staged/unstaged changes, unmerged commits)
        #[arg(long)]
        git: bool,
    },

    /// Wait for agents to reach a target status
    Wait {
        /// Worktree names (supports cross-project with project:handle syntax)
        #[arg(required = true, value_parser = AgentTargetParser::new())]
        worktrees: Vec<String>,

        /// Target status to wait for
        #[arg(long, default_value = "done")]
        status: String,

        /// Maximum wait time in seconds
        #[arg(long)]
        timeout: Option<u64>,

        /// Return when ANY worktree reaches target (default: wait for ALL)
        #[arg(long)]
        any: bool,
    },

    /// Run a command in a worktree's window
    Run {
        /// Worktree name (supports cross-project with project:handle syntax)
        #[arg(value_parser = AgentTargetParser::new())]
        name: String,

        /// Command to run (everything after --)
        #[arg(last = true, required = true)]
        command: Vec<String>,

        /// Run in background without waiting (default: wait and stream output)
        #[arg(short = 'b', long)]
        background: bool,

        /// Keep run artifacts after completion (for debugging)
        #[arg(long)]
        keep: bool,

        /// Maximum wait time in seconds
        #[arg(long)]
        timeout: Option<u64>,
    },

    /// Exit tracked agent processes older than a configured age
    #[command(name = "reap-agents")]
    ReapAgents {
        /// Age threshold in hours
        #[arg(long, default_value = "24", value_parser = clap::value_parser!(u64).range(1..))]
        hours: u64,

        /// Actually exit matching agents instead of showing what would happen
        #[arg(short, long)]
        force: bool,
    },

    /// Re-apply file operations (copy/symlink) to worktrees
    #[command(name = "sync-files")]
    SyncFiles {
        /// Sync all worktrees instead of just the current one
        #[arg(long)]
        all: bool,
    },

    /// Generate example .workmux.yaml configuration file
    Init,

    /// Set up agent status tracking hooks and install skills
    Setup {
        /// Only set up status tracking hooks
        #[arg(long)]
        hooks: bool,
        /// Only install skills
        #[arg(long)]
        skills: bool,
    },

    /// Remove agent status tracking hooks, skills, and cache/state dirs
    #[command(hide = true)]
    Uninstall {
        /// Show what would be removed without actually removing
        #[arg(long)]
        dry_run: bool,
    },

    /// Show detailed documentation (renders README.md)
    Docs,

    /// Show the changelog (what's new in each version)
    Changelog,

    /// Update workmux to the latest version
    Update,

    /// Toggle a live agent status sidebar in tmux
    Sidebar {
        /// Scope sidebar to this session, or toggle this session off when global sidebar is active
        #[arg(short = 's', long)]
        session: bool,

        /// Sidebar placement for this toggle
        #[arg(long, value_enum)]
        position: Option<SidebarPosition>,

        #[command(subcommand)]
        action: Option<SidebarAction>,
    },

    /// Run the sidebar TUI (internal use)
    #[command(hide = true, name = "_sidebar-run")]
    SidebarRun,

    /// Sync sidebar into a window (internal use, called by tmux hooks)
    #[command(hide = true, name = "_sidebar-sync")]
    SidebarSync {
        /// Target window ID (from tmux hook context)
        #[arg(long)]
        window: Option<String>,
    },

    /// Reflow sidebar layout after window resize (internal use, called by tmux hooks)
    #[command(hide = true, name = "_sidebar-reflow")]
    SidebarReflow {
        /// Target window ID
        #[arg(long)]
        window: Option<String>,
    },

    /// Reflow sidebar layouts in all windows (internal use, called by tmux hooks)
    #[command(hide = true, name = "_sidebar-reflow-all")]
    SidebarReflowAll {
        /// Exclude this window ID from reflow (e.g. the window that just lost a pane)
        #[arg(long)]
        exclude: Option<String>,
    },

    /// Run the sidebar daemon (internal use)
    #[command(hide = true, name = "_sidebar-daemon")]
    SidebarDaemon,

    /// Show a TUI dashboard of all active workmux agents across all sessions
    Dashboard {
        /// Preview pane size as percentage (10-90). Larger = more preview, less table.
        #[arg(long, short = 'P', value_parser = clap::value_parser!(u8).range(10..=90))]
        preview_size: Option<u8>,

        /// Open diff view directly for the current worktree
        #[arg(long, short = 'd')]
        diff: bool,

        /// Filter to only show agents in the current session
        #[arg(short = 's', long)]
        session: bool,

        /// Open directly on the specified tab
        #[arg(long, short = 't', value_enum)]
        tab: Option<command::dashboard::DashboardTab>,
    },

    /// Manage global configuration
    Config(command::config::ConfigArgs),

    /// Claude Code integration commands
    Claude {
        #[command(subcommand)]
        command: ClaudeCommands,
    },

    /// Manage sandbox settings
    Sandbox(command::sandbox::SandboxArgs),

    /// Set agent status for the current tmux window (used by hooks)
    #[command(hide = true)]
    SetWindowStatus {
        #[arg(value_enum)]
        command: command::set_window_status::SetWindowStatusCommand,
    },

    /// Set the base branch for the current worktree (used after rebasing)
    #[command(hide = true, name = "set-base")]
    SetBase {
        /// The new base branch
        #[arg(value_parser = GitBranchParser::new())]
        base: String,
    },

    /// Execute a run spec (internal use)
    #[command(hide = true, name = "_exec")]
    Exec {
        /// Absolute path to run directory
        #[arg(long)]
        run_dir: std::path::PathBuf,
    },

    /// Switch to the agent that most recently completed or is waiting for input
    #[command(hide = true, name = "last-done")]
    LastDone,

    /// Switch to the last visited agent (toggle between two)
    #[command(hide = true, name = "last-agent")]
    LastAgent,

    /// Execute a command on the host (used by guest shims)
    #[command(hide = true, name = "host-exec")]
    HostExec {
        /// Command name and arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },

    /// Read clipboard from host (used by sandbox clipboard shims)
    #[command(hide = true, name = "clipboard-read")]
    ClipboardRead {
        /// MIME type to read
        mime: String,
    },

    /// Generate shell completions
    Completions {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Output worktree branch names for shell completion (internal use)
    #[command(hide = true, name = "_complete-branches")]
    CompleteBranches,

    /// Output worktree handles for shell completion (internal use)
    #[command(hide = true, name = "_complete-handles")]
    CompleteHandles,

    /// Output git branches for shell completion (internal use)
    #[command(hide = true, name = "_complete-git-branches")]
    CompleteGitBranches,

    /// Output agent targets for shell completion (internal use)
    ///
    /// Includes local worktree handles plus active agents from other projects.
    #[command(hide = true, name = "_complete-agent-targets")]
    CompleteAgentTargets,

    /// Background update check (internal use)
    #[command(hide = true, name = "_check-update")]
    CheckUpdate,
}

#[derive(Subcommand, Debug)]
pub enum SidebarAction {
    /// Switch to the next agent in sidebar order
    Next,
    /// Switch to the previous agent in sidebar order
    Prev,
    /// Jump to the Nth agent in sidebar order (1-indexed)
    Jump {
        /// Agent number (1-9)
        #[arg(value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        index: u64,
    },
}

#[derive(Subcommand)]
enum ClaudeCommands {
    /// Remove stale entries from ~/.claude.json for deleted worktrees
    Prune,
}

/// Check if the command should show the nerdfont setup prompt.
/// Only commands that display icons should trigger the prompt.
fn should_prompt_nerdfont(cmd: &Commands) -> bool {
    matches!(
        cmd,
        Commands::Add { .. } | Commands::Init | Commands::Dashboard { .. } | Commands::List { .. }
    )
}

/// Check if the command should show the status tracking setup wizard.
/// Excludes `Setup` to avoid double-prompting (the setup command handles its own flow).
/// Excludes `Dashboard` because the wizard prompt interferes with the TUI.
fn should_prompt_status_setup(cmd: &Commands) -> bool {
    matches!(
        cmd,
        Commands::Add { .. } | Commands::Init | Commands::List { .. }
    )
}

/// Check if the command should trigger a background update check.
fn should_check_update(cmd: &Commands) -> bool {
    matches!(cmd, Commands::Add { .. })
}

// --- Public Entry Point ---
pub fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(mut e) => {
            // Filter hidden (underscore-prefixed) commands from "similar subcommands" suggestions.
            // Workaround for https://github.com/clap-rs/clap/issues/4853
            if e.kind() == ErrorKind::InvalidSubcommand {
                let visible = e
                    .get(ContextKind::SuggestedSubcommand)
                    .and_then(|v| match v {
                        ContextValue::Strings(suggestions) => {
                            let filtered: Vec<String> = suggestions
                                .iter()
                                .filter(|s| !s.starts_with('_'))
                                .cloned()
                                .collect();
                            Some(filtered)
                        }
                        _ => None,
                    });
                if let Some(visible) = visible {
                    e.insert(
                        ContextKind::SuggestedSubcommand,
                        ContextValue::Strings(visible),
                    );
                }
            }
            e.exit()
        }
    };

    // Extract config override early so the side-effect loads (nerdfont, update
    // check) respect the user's explicit --config choice.
    let config_override = match &cli.command {
        Commands::Add { config, .. } => config.as_deref(),
        Commands::Open { config, .. } => config.as_deref(),
        _ => None,
    };

    // Always initialize nerdfont setting for prefix consistency across commands.
    // Only prompt interactively for commands that display icons.
    // If config fails to load, skip the nerdfont wizard -- it will be shown on
    // the next successful run and the real error surfaces when the command loads
    // config with `?`.
    let (cfg, config_ok) = match config::Config::load_with_override(None, config_override) {
        Ok(cfg) => (cfg, true),
        Err(_) => (config::Config::default(), false),
    };
    let has_pua = nerdfont::config_has_pua(&cfg);
    let nerdfont_enabled = if cfg.nerdfont.is_some() || has_pua {
        // Already configured or PUA detected
        cfg.nerdfont.unwrap_or(has_pua)
    } else if config_ok && should_prompt_nerdfont(&cli.command) {
        // Prompt user (returns None in non-interactive mode)
        nerdfont::check_and_prompt(&cfg)?.unwrap_or(false)
    } else {
        false
    };
    nerdfont::init(Some(nerdfont_enabled), has_pua);

    // Check agent status tracking setup after nerdfont.
    // Uses a separate gate to avoid double-prompting when running `workmux setup`.
    if config_ok
        && should_prompt_status_setup(&cli.command)
        && let Err(e) = crate::agent_setup::prompt_wizard()
    {
        tracing::debug!(?e, "status setup wizard failed");
    }

    // Background update check: reads local cache, optionally shows a notice,
    // and spawns a background process to refresh the cache if stale.
    if should_check_update(&cli.command) {
        command::update::check_and_notify(&cfg);
    }

    match cli.command {
        Commands::Add {
            branch_name,
            pr,
            auto_name,
            base,
            name,
            target_name,
            parent_session,
            prompt,
            setup,
            rescue,
            multi,
            layout,
            fork,
            wait,
            mode,
            session,
            config,
        } => {
            let mode_override = mode
                .map(MuxMode::from)
                .or(session.then_some(MuxMode::Session));
            command::add::run(
                branch_name.as_deref(),
                pr,
                auto_name,
                base.as_deref(),
                name,
                target_name,
                parent_session,
                prompt,
                setup,
                rescue,
                multi,
                layout,
                fork,
                wait,
                mode_override,
                config.as_deref(),
            )
        }
        Commands::Open {
            names,
            run_hooks,
            force_files,
            new,
            mode,
            session,
            target_name,
            parent_session,
            continue_session,
            prompt,
            config,
        } => {
            let mode_override = mode
                .map(MuxMode::from)
                .or(session.then_some(MuxMode::Session));
            command::open::run(
                &names,
                run_hooks,
                force_files,
                new,
                mode_override,
                target_name,
                parent_session,
                continue_session,
                prompt,
                config.as_deref(),
            )
        }
        Commands::Close { name } => command::close::run(name.as_deref()),
        Commands::Resurrect { dry_run } => command::resurrect::run(dry_run),
        Commands::Merge {
            name,
            into,
            ignore_uncommitted,
            rebase,
            squash,
            keep,
            cleanup,
            no_verify,
            no_hooks,
            notification,
        } => command::merge::run(
            name.as_deref(),
            into.as_deref(),
            ignore_uncommitted,
            rebase,
            squash,
            keep,
            cleanup,
            no_verify,
            no_hooks,
            notification,
        ),
        Commands::Rebase { name } => command::rebase::run(name.as_deref()),
        Commands::Remove {
            names,
            gone,
            all,
            force,
            keep_branch,
        } => command::remove::run(names, gone, all, force, keep_branch),
        Commands::Rename { names, branch } => command::rename::run(names, branch),
        Commands::List { pr, json, filter } => command::list::run(pr, json, &filter),
        Commands::Path { name } => command::path::run(&name),
        Commands::Send { name, text, file } => {
            command::send::run(&name, text.as_deref(), file.as_deref())
        }
        Commands::Capture { name, lines } => command::capture::run(&name, lines),
        Commands::Status {
            worktrees,
            json,
            git,
        } => command::status::run(&worktrees, json, git),
        Commands::Wait {
            worktrees,
            status,
            timeout,
            any,
        } => command::wait::run(&worktrees, &status, timeout, any),
        Commands::Run {
            name,
            command,
            background,
            keep,
            timeout,
        } => command::run::run(&name, command, background, keep, timeout),
        Commands::ReapAgents { hours, force } => {
            command::reap_agents::run(hours.saturating_mul(3600), force)
        }
        Commands::Exec { run_dir } => command::exec::run(&run_dir),
        Commands::SyncFiles { all } => command::sync_files::run(all),
        Commands::Init => crate::config::Config::init(),
        Commands::Setup { hooks, skills } => command::setup::run(hooks, skills),
        Commands::Uninstall { dry_run } => command::uninstall::run(dry_run),
        Commands::Docs => command::docs::run(),
        Commands::Changelog => command::changelog::run(),
        Commands::Update => command::update::run(),
        Commands::Sidebar {
            session,
            position,
            action,
        } => match action {
            Some(SidebarAction::Next) => {
                command::sidebar::navigate(command::sidebar::NavAction::Next)
            }
            Some(SidebarAction::Prev) => {
                command::sidebar::navigate(command::sidebar::NavAction::Prev)
            }
            Some(SidebarAction::Jump { index }) => {
                command::sidebar::navigate(command::sidebar::NavAction::Jump(index as usize))
            }
            None => {
                if session {
                    command::sidebar::toggle_session(position)
                } else {
                    command::sidebar::toggle(position)
                }
            }
        },
        Commands::SidebarRun => command::sidebar::run_sidebar(),
        Commands::SidebarSync { window } => command::sidebar::sync(window.as_deref()),
        Commands::SidebarReflow { window } => command::sidebar::reflow(window.as_deref()),
        Commands::SidebarReflowAll { exclude } => command::sidebar::reflow_all(exclude.as_deref()),
        Commands::SidebarDaemon => command::sidebar::run_daemon(),
        Commands::Dashboard {
            preview_size,
            diff,
            session,
            tab,
        } => command::dashboard::run(preview_size, diff, session, tab),
        Commands::Config(args) => command::config::run(args),
        Commands::Claude { command } => match command {
            ClaudeCommands::Prune => prune_claude_config(),
        },
        Commands::Sandbox(args) => command::sandbox::run(args),
        Commands::SetWindowStatus { command } => command::set_window_status::run(command),
        Commands::SetBase { base } => command::set_base::run(&base),
        Commands::LastDone => command::last_done::run(),
        Commands::LastAgent => command::last_agent::run(),
        Commands::HostExec { args } => {
            let (command, cmd_args) = args
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("host-exec requires a command name"))?;
            let code = command::host_exec::run(command, cmd_args)?;
            std::process::exit(code);
        }
        Commands::ClipboardRead { mime } => {
            let code = command::clipboard_read::run(&mime)?;
            std::process::exit(code);
        }
        Commands::Completions { shell } => {
            generate_completions(shell);
            Ok(())
        }
        Commands::CompleteBranches => {
            for branch in WorktreeBranchParser::new().get_branches() {
                println!("{branch}");
            }
            Ok(())
        }
        Commands::CompleteHandles => {
            for handle in WorktreeHandleParser::get_handles() {
                println!("{handle}");
            }
            Ok(())
        }
        Commands::CompleteGitBranches => {
            for branch in GitBranchParser::get_branches() {
                println!("{branch}");
            }
            Ok(())
        }
        Commands::CompleteAgentTargets => {
            for target in AgentTargetParser::get_targets() {
                println!("{target}");
            }
            Ok(())
        }
        Commands::CheckUpdate => command::update::run_background_check(),
    }
}

fn prune_claude_config() -> Result<()> {
    claude::prune_stale_entries().context("Failed to prune Claude configuration")?;
    Ok(())
}

fn generate_completions(shell: Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();

    // Generate base completions
    let mut buf = Vec::new();
    generate(shell, &mut cmd, &name, &mut buf);
    let base_script = String::from_utf8_lossy(&buf);

    // Append dynamic branch completion for each shell
    // Note: PowerShell and Elvish are not supported because clap_complete generates
    // anonymous completers that can't be wrapped without breaking standard completions.
    match shell {
        Shell::Zsh => {
            let base = prepare_zsh_base(&base_script, &name);
            println!("{base}");
            print_zsh_dynamic_completion();
        }
        _ => {
            print!("{base_script}");
            match shell {
                Shell::Bash => print_bash_dynamic_completion(),
                Shell::Fish => print_fish_dynamic_completion(),
                _ => {}
            }
        }
    }
}

/// Rename the clap-generated zsh completion function so the dynamic wrapper
/// (in `zsh_dynamic.zsh`) can take the primary `_workmux` name.
///
/// The dynamic wrapper needs to BE `_workmux` — the function zsh autoloads
/// from fpath. It delegates flag completion to `_workmux_base` (the renamed
/// clap output) and handles positional args with dynamic helpers.
///
/// The `replace("_{name}", "_{name}_base")` is precise: `_{name}` (with the
/// leading underscore) only appears as zsh function identifiers in clap's
/// output. Bare `{name}` in `#compdef`, `_describe` strings, `curcontext`,
/// and state names is unaffected.
fn prepare_zsh_base(script: &str, name: &str) -> String {
    let fn_prefix = format!("_{name}");
    let base_fn_prefix = format!("_{name}_base");

    let script = script.replace(&fn_prefix, &base_fn_prefix);

    // Strip the autoload/eval detection block clap appends at the end.
    // After renaming it registers _workmux_base, which conflicts with our
    // dynamic wrapper's own registration.
    let funcstack_block = format!(
        "\nif [ \"$funcstack[1]\" = \"{base_fn_prefix}\" ]; then\n    \
         {base_fn_prefix} \"$@\"\nelse\n    \
         compdef {base_fn_prefix} {name}\nfi\n"
    );
    match script.strip_suffix(&funcstack_block) {
        Some(stripped) => stripped.to_string(),
        None => script,
    }
}

fn print_zsh_dynamic_completion() {
    print!("{}", include_str!("scripts/completions/zsh_dynamic.zsh"));
}

fn print_bash_dynamic_completion() {
    print!("{}", include_str!("scripts/completions/bash_dynamic.bash"));
}

fn print_fish_dynamic_completion() {
    print!("{}", include_str!("scripts/completions/fish_dynamic.fish"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_position_flag_parses_supported_values() {
        let cli = Cli::try_parse_from(["workmux", "sidebar", "--position", "top"]).unwrap();

        match cli.command {
            Commands::Sidebar {
                position, action, ..
            } => {
                assert_eq!(position, Some(SidebarPosition::Top));
                assert!(action.is_none());
            }
            _ => panic!("expected sidebar command"),
        }
    }

    #[test]
    fn sidebar_position_flag_rejects_unsupported_values() {
        let err = match Cli::try_parse_from(["workmux", "sidebar", "--position", "right"]) {
            Ok(_) => panic!("expected invalid position"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::InvalidValue);
        assert!(err.to_string().contains("left"));
        assert!(err.to_string().contains("top"));
    }

    #[test]
    fn prepare_zsh_base_renames_function_identifiers() {
        let input = concat!(
            "#compdef workmux\n",
            "_workmux() {\n",
            "  \":: :_workmux_commands\"\n",
            "}\n",
            "(( $+functions[_workmux_commands] )) ||\n",
            "_workmux_commands() {\n",
            "  _describe -t commands 'workmux commands' commands\n",
            "}\n",
            "\nif [ \"$funcstack[1]\" = \"_workmux\" ]; then\n",
            "    _workmux \"$@\"\n",
            "else\n",
            "    compdef _workmux workmux\n",
            "fi\n",
        );
        let result = prepare_zsh_base(input, "workmux");

        // Function identifiers are renamed
        assert!(result.contains("_workmux_base()"));
        assert!(result.contains("_workmux_base_commands"));
        assert!(!result.contains("_workmux()"));

        // Bare "workmux" in #compdef and _describe strings is preserved
        assert!(result.contains("#compdef workmux"));
        assert!(result.contains("'workmux commands'"));

        // funcstack block is stripped
        assert!(!result.contains("funcstack"));
        assert!(!result.contains("compdef _workmux_base"));
    }

    #[test]
    fn prepare_zsh_base_preserves_state_and_curcontext() {
        let input = concat!(
            "#compdef workmux\n",
            "_workmux() {\n",
            "  \"*::: :->workmux\"\n",
            "  curcontext=\"workmux-command-$line[1]:\"\n",
            "}\n",
            "\nif [ \"$funcstack[1]\" = \"_workmux\" ]; then\n",
            "    _workmux \"$@\"\n",
            "else\n",
            "    compdef _workmux workmux\n",
            "fi\n",
        );
        let result = prepare_zsh_base(input, "workmux");

        // State names and curcontext use bare "workmux" (no underscore), unchanged
        assert!(result.contains("->workmux"));
        assert!(result.contains("workmux-command-"));
    }

    #[test]
    fn prepare_zsh_base_tolerates_missing_funcstack_block() {
        let input = "_workmux() {\n  echo hello\n}\n";
        let result = prepare_zsh_base(input, "workmux");

        assert!(result.contains("_workmux_base()"));
        assert!(!result.contains("_workmux()"));
    }

    #[test]
    fn prepare_zsh_base_works_with_real_clap_output() {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut cmd, &name, &mut buf);
        let base_script = String::from_utf8_lossy(&buf);

        let result = prepare_zsh_base(&base_script, &name);

        // Main function is renamed
        assert!(result.contains("_workmux_base()"));
        assert!(!result.contains("\n_workmux()"));

        // Helpers are renamed
        assert!(result.contains("_workmux_base_commands"));

        // #compdef header preserved
        assert!(result.starts_with("#compdef workmux\n"));

        // _describe strings preserved
        assert!(result.contains("'workmux commands'"));

        // funcstack block stripped
        assert!(!result.contains("funcstack"));
    }

    /// Helper: produce the full `workmux completions zsh` output
    /// (base post-processed by prepare_zsh_base + dynamic wrapper).
    fn generate_full_zsh_completions() -> String {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut cmd, &name, &mut buf);
        let base_script = String::from_utf8_lossy(&buf);
        let base = prepare_zsh_base(&base_script, &name);
        let dynamic = include_str!("scripts/completions/zsh_dynamic.zsh");
        format!("{base}\n{dynamic}")
    }

    #[test]
    fn zsh_full_output_has_no_stale_workmux_functions() {
        let output = generate_full_zsh_completions();

        // The only _workmux() definition should be the dynamic wrapper.
        // There must be no clap-generated _workmux() left (it was renamed).
        let workmux_fn_count = output.matches("\n_workmux()").count();
        assert_eq!(
            workmux_fn_count, 1,
            "Expected exactly one _workmux() definition (the dynamic wrapper)"
        );

        // The wrapper must call _workmux_base, not itself
        let wrapper_section: &str = output
            .split("\n_workmux()")
            .nth(1)
            .expect("_workmux() not found");
        assert!(
            wrapper_section.contains("_workmux_base"),
            "Dynamic wrapper must delegate to _workmux_base"
        );
    }

    #[test]
    fn zsh_full_output_no_file_fallback_for_handle_commands() {
        let output = generate_full_zsh_completions();

        // The dynamic wrapper's case branches for handle commands should
        // call _workmux_handles, not _workmux_base (which has _default).
        // Extract the case block from the wrapper.
        let wrapper_start = output.find("\n_workmux()").expect("_workmux() not found");
        let wrapper = &output[wrapper_start..];

        // The handle commands should appear in the case pattern
        for cmd in ["open", "remove", "close", "merge", "rebase"] {
            assert!(wrapper.contains(cmd), "Wrapper should handle {cmd}");
        }
        assert!(
            wrapper.contains("_workmux_handles"),
            "Wrapper should call _workmux_handles for handle commands"
        );
        assert!(
            wrapper.contains("_workmux_git_branches"),
            "Wrapper should call _workmux_git_branches for add"
        );
    }

    #[test]
    fn zsh_full_output_autoload_and_eval_compatible() {
        let output = generate_full_zsh_completions();

        // Must start with #compdef for fpath autoloading
        assert!(
            output.starts_with("#compdef workmux\n"),
            "Must start with #compdef for fpath autoloading"
        );

        // Must have funcstack detection for autoload/eval compatibility
        assert!(
            output.contains(r#""$funcstack[1]" = "_workmux""#),
            "Must have funcstack check for autoload detection"
        );

        // Must have compdef registration for eval case
        assert!(
            output.contains("compdef _workmux workmux"),
            "Must register _workmux via compdef for eval case"
        );

        // Must NOT have compdef for _workmux_base (that was stripped)
        assert!(
            !output.contains("compdef _workmux_base"),
            "Must not register _workmux_base directly"
        );
    }

    #[test]
    fn bash_output_unaffected_by_zsh_postprocessing() {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut cmd, &name, &mut buf);
        let base_script = String::from_utf8_lossy(&buf);
        let dynamic = include_str!("scripts/completions/bash_dynamic.bash");
        let output = format!("{base_script}{dynamic}");

        // Bash uses _workmux directly (no _base rename)
        assert!(output.contains("_workmux()"));
        assert!(!output.contains("_workmux_base"));

        // Dynamic wrapper registered
        assert!(output.contains("complete -F _workmux_dynamic"));
    }

    #[test]
    fn fish_output_unaffected_by_zsh_postprocessing() {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        let mut buf = Vec::new();
        generate(Shell::Fish, &mut cmd, &name, &mut buf);
        let base_script = String::from_utf8_lossy(&buf);
        let dynamic = include_str!("scripts/completions/fish_dynamic.fish");
        let output = format!("{base_script}{dynamic}");

        // Fish uses workmux directly (no _base rename)
        assert!(output.contains("__fish_workmux"));
        assert!(!output.contains("workmux_base"));

        // Dynamic completions registered
        assert!(output.contains("__workmux_handles"));
        assert!(output.contains("__workmux_git_branches"));
    }
}
