use crate::command::args::{MultiArgs, PromptArgs, RescueArgs, SetupFlags};
use crate::{claude, command, config, git, nerdfont};
use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

#[derive(Clone, Debug)]
struct WorktreeBranchParser;

impl WorktreeBranchParser {
    fn new() -> Self {
        Self
    }

    fn get_branches(&self) -> Vec<String> {
        // Don't attempt completions if not in a git repo.
        if !git::is_git_repo().unwrap_or(false) {
            return Vec::new();
        }

        let worktrees = match git::list_worktrees() {
            Ok(wt) => wt,
            // Fail silently on completion; don't disrupt the user's shell.
            Err(_) => return Vec::new(),
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

impl clap::builder::TypedValueParser for WorktreeBranchParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        // Use the default string parser for validation.
        clap::builder::StringValueParser::new().parse_ref(cmd, None, value)
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        // Return None to avoid running git operations during completion script generation.
        // Dynamic completions are handled by the _complete-branches subcommand,
        // which is called by the shell only when the user presses TAB.
        None
    }
}

/// Parser for worktree handles (directory names), used for open/path/remove commands.
#[derive(Clone, Debug)]
struct WorktreeHandleParser;

impl WorktreeHandleParser {
    fn new() -> Self {
        Self
    }

    fn get_handles() -> Vec<String> {
        // Don't attempt completions if not in a git repo.
        if !git::is_git_repo().unwrap_or(false) {
            return Vec::new();
        }

        let worktrees = match git::list_worktrees() {
            Ok(wt) => wt,
            // Fail silently on completion; don't disrupt the user's shell.
            Err(_) => return Vec::new(),
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

impl clap::builder::TypedValueParser for WorktreeHandleParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        // Use the default string parser for validation.
        clap::builder::StringValueParser::new().parse_ref(cmd, None, value)
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        // Return None to avoid running git operations during completion script generation.
        // Dynamic completions are handled by the _complete-handles subcommand,
        // which is called by the shell only when the user presses TAB.
        None
    }
}

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

impl clap::builder::TypedValueParser for GitBranchParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        // Use the default string parser for validation.
        clap::builder::StringValueParser::new().parse_ref(cmd, None, value)
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        // Return None to avoid running git operations during completion script generation.
        // Dynamic completions are handled by the _complete-git-branches subcommand,
        // which is called by the shell only when the user presses TAB.
        None
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "workmux")]
#[command(about = "An opinionated workflow tool that orchestrates git worktrees and tmux")]
#[command(after_help = "Run 'workmux docs' for detailed documentation.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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

        /// Base branch/commit/tag to branch from (defaults to current branch)
        #[arg(long)]
        base: Option<String>,

        /// Explicit name for the worktree directory and tmux window (overrides worktree_naming strategy and worktree_prefix)
        #[arg(long)]
        name: Option<String>,

        #[command(flatten)]
        prompt: PromptArgs,

        #[command(flatten)]
        setup: SetupFlags,

        #[command(flatten)]
        rescue: RescueArgs,

        #[command(flatten)]
        multi: MultiArgs,

        /// Block until the created tmux window is closed
        #[arg(short = 'W', long)]
        wait: bool,

        /// Create the window in its own tmux session (useful for session-per-project workflows)
        #[arg(short = 's', long)]
        session: bool,
    },

    /// Open a tmux window for an existing worktree
    Open {
        /// Worktree name (directory name, visible in tmux window). Optional with --new.
        #[arg(value_parser = WorktreeHandleParser::new(), required_unless_present = "new")]
        name: Option<String>,

        /// Re-run post-create hooks (e.g., pnpm install)
        #[arg(long)]
        run_hooks: bool,

        /// Re-apply file operations (copy/symlink)
        #[arg(long)]
        force_files: bool,

        /// Force opening in a new window (creates suffix like -2, -3) instead of switching to existing
        #[arg(long, short = 'n', conflicts_with = "session")]
        new: bool,

        /// Open in session mode (overrides stored mode for this worktree)
        #[arg(short = 's', long)]
        session: bool,

        #[command(flatten)]
        prompt: PromptArgs,
    },

    /// Close a worktree's tmux window (keeps the worktree and branch)
    Close {
        /// Worktree name (defaults to current directory if omitted)
        #[arg(value_parser = WorktreeHandleParser::new())]
        name: Option<String>,
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
        /// Worktree name
        #[arg(value_parser = WorktreeHandleParser::new())]
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
        /// Worktree name
        #[arg(value_parser = WorktreeHandleParser::new())]
        name: String,

        /// Number of lines to capture
        #[arg(short = 'n', long, default_value = "200")]
        lines: u16,
    },

    /// Query agent status for worktrees
    Status {
        /// Worktree names (default: all with active agents)
        #[arg(value_parser = WorktreeHandleParser::new())]
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
        /// Worktree names to wait on
        #[arg(required = true, value_parser = WorktreeHandleParser::new())]
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
        /// Worktree name
        #[arg(value_parser = WorktreeHandleParser::new())]
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

    /// Generate example .workmux.yaml configuration file
    Init,

    /// Set up agent status tracking hooks
    Setup,

    /// Show detailed documentation (renders README.md)
    Docs,

    /// Show the changelog (what's new in each version)
    Changelog,

    /// Update workmux to the latest version
    Update,

    /// Show a TUI dashboard of all active workmux agents across all sessions
    Dashboard {
        /// Preview pane size as percentage (10-90). Larger = more preview, less table.
        #[arg(long, short = 'P', value_parser = clap::value_parser!(u8).range(10..=90))]
        preview_size: Option<u8>,

        /// Open diff view directly for the current worktree
        #[arg(long, short = 'd')]
        diff: bool,
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

    /// Switch to the agent that most recently completed its task
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

    /// Background update check (internal use)
    #[command(hide = true, name = "_check-update")]
    CheckUpdate,
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
    let cli = Cli::parse();

    // Always initialize nerdfont setting for prefix consistency across commands.
    // Only prompt interactively for commands that display icons.
    // If config fails to load, skip the nerdfont wizard -- it will be shown on
    // the next successful run and the real error surfaces when the command loads
    // config with `?`.
    let (cfg, config_ok) = match config::Config::load(None) {
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
            prompt,
            setup,
            rescue,
            multi,
            wait,
            session,
        } => command::add::run(
            branch_name.as_deref(),
            pr,
            auto_name,
            base.as_deref(),
            name,
            prompt,
            setup,
            rescue,
            multi,
            wait,
            session,
        ),
        Commands::Open {
            name,
            run_hooks,
            force_files,
            new,
            session,
            prompt,
        } => command::open::run(
            name.as_deref(),
            run_hooks,
            force_files,
            new,
            session,
            prompt,
        ),
        Commands::Close { name } => command::close::run(name.as_deref()),
        Commands::Merge {
            name,
            into,
            ignore_uncommitted,
            rebase,
            squash,
            keep,
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
            no_verify,
            no_hooks,
            notification,
        ),
        Commands::Remove {
            names,
            gone,
            all,
            force,
            keep_branch,
        } => command::remove::run(names, gone, all, force, keep_branch),
        Commands::List { pr, filter } => command::list::run(pr, &filter),
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
        Commands::Exec { run_dir } => command::exec::run(&run_dir),
        Commands::Init => crate::config::Config::init(),
        Commands::Setup => command::setup::run(),
        Commands::Docs => command::docs::run(),
        Commands::Changelog => command::changelog::run(),
        Commands::Update => command::update::run(),
        Commands::Dashboard { preview_size, diff } => command::dashboard::run(preview_size, diff),
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
        for cmd in ["open", "remove", "close", "merge"] {
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
