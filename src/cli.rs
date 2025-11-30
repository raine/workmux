use crate::command::args::{MultiArgs, PromptArgs, RescueArgs, SetupFlags};
use crate::{claude, command, git};
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
        // Dynamic completions are handled by the __complete-branches subcommand,
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
        // Dynamic completions are handled by the __complete-git-branches subcommand,
        // which is called by the shell only when the user presses TAB.
        None
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "workmux")]
#[command(about = "An opinionated workflow tool that orchestrates git worktrees and tmux")]
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
        #[arg(required_unless_present = "pr", value_parser = GitBranchParser::new())]
        branch_name: Option<String>,

        /// Pull request number to checkout
        #[arg(long, conflicts_with = "base")]
        pr: Option<u32>,

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
    },

    /// Open a tmux window for an existing worktree
    Open {
        /// Name of the branch with an existing worktree
        #[arg(value_parser = WorktreeBranchParser::new())]
        branch_name: String,

        /// Re-run post-create hooks (e.g., pnpm install)
        #[arg(long)]
        run_hooks: bool,

        /// Re-apply file operations (copy/symlink)
        #[arg(long)]
        force_files: bool,
    },

    /// Merge a branch, then clean up the worktree and tmux window
    Merge {
        /// Name of the branch to merge (defaults to current branch)
        #[arg(value_parser = WorktreeBranchParser::new())]
        branch_name: Option<String>,

        /// Ignore uncommitted and staged changes
        #[arg(long)]
        ignore_uncommitted: bool,

        /// Also delete the remote branch
        #[arg(short = 'r', long)]
        delete_remote: bool,

        /// Rebase the branch onto the main branch before merging (fast-forward)
        #[arg(long, group = "merge_strategy")]
        rebase: bool,

        /// Squash all commits from the branch into a single commit on the main branch
        #[arg(long, group = "merge_strategy")]
        squash: bool,

        /// Keep the worktree, window, and branch after merging (skip cleanup)
        #[arg(short = 'k', long, conflicts_with = "delete_remote")]
        keep: bool,
    },

    /// Remove a worktree, tmux window, and branch without merging
    #[command(visible_alias = "rm")]
    Remove {
        /// Name of the branch to remove (defaults to current branch)
        #[arg(value_parser = WorktreeBranchParser::new())]
        branch_name: Option<String>,

        /// Skip confirmation and ignore uncommitted changes
        #[arg(short, long)]
        force: bool,

        /// Also delete the remote branch
        #[arg(short = 'r', long)]
        delete_remote: bool,

        /// Keep the local branch (only remove worktree and tmux window)
        #[arg(short = 'k', long, conflicts_with = "delete_remote")]
        keep_branch: bool,
    },

    /// List all worktrees
    #[command(visible_alias = "ls")]
    List,

    /// Get the filesystem path of a worktree
    Path {
        /// Name of the branch
        #[arg(value_parser = WorktreeBranchParser::new())]
        branch_name: String,
    },

    /// Generate example .workmux.yaml configuration file
    Init,

    /// Claude Code integration commands
    Claude {
        #[command(subcommand)]
        command: ClaudeCommands,
    },

    /// Set agent status for the current tmux window (used by hooks)
    #[command(hide = true)]
    SetWindowStatus {
        #[command(subcommand)]
        command: command::set_window_status::SetWindowStatusCommand,
    },

    /// Generate shell completions
    Completions {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Output branch names for shell completion (internal use)
    #[command(hide = true, name = "__complete-branches")]
    CompleteBranches,

    /// Output git branches for shell completion (internal use)
    #[command(hide = true, name = "__complete-git-branches")]
    CompleteGitBranches,
}

#[derive(Subcommand)]
enum ClaudeCommands {
    /// Remove stale entries from ~/.claude.json for deleted worktrees
    Prune,
}

// --- Public Entry Point ---
pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Add {
            branch_name,
            pr,
            base,
            name,
            prompt,
            setup,
            rescue,
            multi,
        } => command::add::run(
            branch_name.as_deref(),
            pr,
            base.as_deref(),
            name,
            prompt,
            setup,
            rescue,
            multi,
        ),
        Commands::Open {
            branch_name,
            run_hooks,
            force_files,
        } => command::open::run(&branch_name, run_hooks, force_files),
        Commands::Merge {
            branch_name,
            ignore_uncommitted,
            delete_remote,
            rebase,
            squash,
            keep,
        } => command::merge::run(
            branch_name.as_deref(),
            ignore_uncommitted,
            delete_remote,
            rebase,
            squash,
            keep,
        ),
        Commands::Remove {
            branch_name,
            force,
            delete_remote,
            keep_branch,
        } => command::remove::run(branch_name.as_deref(), force, delete_remote, keep_branch),
        Commands::List => command::list::run(),
        Commands::Path { branch_name } => command::path::run(&branch_name),
        Commands::Init => crate::config::Config::init(),
        Commands::Claude { command } => match command {
            ClaudeCommands::Prune => prune_claude_config(),
        },
        Commands::SetWindowStatus { command } => command::set_window_status::run(command),
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
        Commands::CompleteGitBranches => {
            for branch in GitBranchParser::get_branches() {
                println!("{branch}");
            }
            Ok(())
        }
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
    print!("{base_script}");

    // Append dynamic branch completion for each shell
    // Note: PowerShell and Elvish are not supported because clap_complete generates
    // anonymous completers that can't be wrapped without breaking standard completions.
    match shell {
        Shell::Zsh => print_zsh_dynamic_completion(),
        Shell::Bash => print_bash_dynamic_completion(),
        Shell::Fish => print_fish_dynamic_completion(),
        _ => {}
    }
}

fn print_zsh_dynamic_completion() {
    print!(
        r#"
# Dynamic branch completion - runs git only when TAB is pressed
_workmux_branches() {{
    local branches
    branches=("${{(@f)$(workmux __complete-branches 2>/dev/null)}}")
    compadd -a branches
}}

# Dynamic git branch completion for add command
_workmux_git_branches() {{
    local branches
    branches=("${{(@f)$(workmux __complete-git-branches 2>/dev/null)}}")
    compadd -a branches
}}

# Override completion for commands that take branch names
_workmux_dynamic() {{
    # Get the subcommand (second word)
    local cmd="${{words[2]}}"

    # Only handle commands that need dynamic branch completion
    case "$cmd" in
        open|merge|remove|rm|path)
            # If completing a flag, use generated completions
            if [[ "${{words[CURRENT]}}" == -* ]]; then
                _workmux "$@"
                return
            fi
            # For positional args after the subcommand, offer branches
            if (( CURRENT > 2 )); then
                _workmux_branches
                return
            fi
            ;;
        add)
            # If completing a flag, use generated completions
            if [[ "${{words[CURRENT]}}" == -* ]]; then
                _workmux "$@"
                return
            fi
            # For positional args after the subcommand, offer git branches
            if (( CURRENT > 2 )); then
                _workmux_git_branches
                return
            fi
            ;;
    esac

    # For all other commands and cases, use generated completions
    _workmux "$@"
}}

compdef _workmux_dynamic workmux
"#
    );
}

fn print_bash_dynamic_completion() {
    print!(
        r#"
# Dynamic branch completion for open/merge/remove commands
_workmux_branches() {{
    workmux __complete-branches 2>/dev/null
}}

# Dynamic git branch completion for add command
_workmux_git_branches() {{
    workmux __complete-git-branches 2>/dev/null
}}

# Wrapper that adds dynamic branch completion
_workmux_dynamic() {{
    local cur prev words cword

    # Use _init_completion if available, otherwise fall back to manual parsing
    if declare -F _init_completion >/dev/null 2>&1; then
        _init_completion || return
    else
        COMPREPLY=()
        cur="${{COMP_WORDS[COMP_CWORD]}}"
        prev="${{COMP_WORDS[COMP_CWORD-1]}}"
        words=("${{COMP_WORDS[@]}}")
        cword=$COMP_CWORD
    fi

    # Check if we're completing a branch argument for specific commands
    if [[ ${{cword}} -ge 2 ]]; then
        local cmd="${{words[1]}}"
        case "$cmd" in
            open|merge|remove|rm|path)
                # If not typing a flag, complete with branches
                if [[ "$cur" != -* ]]; then
                    COMPREPLY=($(compgen -W "$(_workmux_branches)" -- "$cur"))
                    return
                fi
                ;;
            add)
                # If not typing a flag, complete with git branches
                if [[ "$cur" != -* ]]; then
                    COMPREPLY=($(compgen -W "$(_workmux_git_branches)" -- "$cur"))
                    return
                fi
                ;;
        esac
    fi

    # Fall back to generated completions
    _workmux
}}

complete -F _workmux_dynamic -o bashdefault -o default workmux
"#
    );
}

fn print_fish_dynamic_completion() {
    print!(
        r#"
# Dynamic branch completion for open/merge/remove commands
function __workmux_branches
    workmux __complete-branches 2>/dev/null
end

# Dynamic git branch completion for add command
function __workmux_git_branches
    workmux __complete-git-branches 2>/dev/null
end

# Add dynamic completions for commands that take branch names
complete -c workmux -n '__fish_seen_subcommand_from open merge remove rm path' -f -a '(__workmux_branches)'
complete -c workmux -n '__fish_seen_subcommand_from add' -f -a '(__workmux_git_branches)'
"#
    );
}
