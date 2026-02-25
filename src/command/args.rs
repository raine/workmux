use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub struct PromptArgs {
    /// Inline prompt text to store in the new worktree
    #[arg(short = 'p', long, conflicts_with_all = ["prompt_file", "prompt_editor"])]
    pub prompt: Option<String>,

    /// Path to a file whose contents should be used as the prompt
    #[arg(
        short = 'P',
        long = "prompt-file",
        conflicts_with_all = ["prompt", "prompt_editor"],
        value_hint = clap::ValueHint::FilePath
    )]
    pub prompt_file: Option<PathBuf>,

    /// Open $EDITOR to write the prompt
    #[arg(short = 'e', long = "prompt-editor", conflicts_with_all = ["prompt", "prompt_file"])]
    pub prompt_editor: bool,

    /// Write the prompt file without injecting it into agent commands.
    /// The prompt is written to .workmux/PROMPT-<branch>.md in the worktree,
    /// but no agent pane is required. Useful when your editor has an embedded
    /// agent that reads the prompt file directly.
    #[arg(long)]
    pub prompt_file_only: bool,
}

impl PromptArgs {
    pub fn has_any(&self) -> bool {
        self.prompt.is_some() || self.prompt_file.is_some() || self.prompt_editor
    }
}

#[derive(clap::Args, Debug)]
pub struct SetupFlags {
    /// Skip running post-create hooks
    #[arg(short = 'H', long)]
    pub no_hooks: bool,

    /// Skip file copy/symlink operations
    #[arg(short = 'F', long)]
    pub no_file_ops: bool,

    /// Skip executing pane commands (panes open with plain shells)
    #[arg(short = 'C', long)]
    pub no_pane_cmds: bool,

    /// Create tmux window in the background (do not switch to it)
    #[arg(short = 'b', long = "background")]
    pub background: bool,

    /// Open existing worktree if it exists instead of failing (like tmux new -A)
    #[arg(short = 'o', long, conflicts_with = "with_changes")]
    pub open_if_exists: bool,

    /// Enable sandbox mode even when disabled in config
    #[arg(short = 'S', long)]
    pub sandbox: bool,

    /// Apply per-window color tinting (tmux only)
    #[arg(long)]
    pub colors: bool,
}

#[derive(clap::Args, Debug)]
pub struct MultiArgs {
    /// The agent(s) to use. Creates one worktree per agent if -n is not specified.
    #[arg(short = 'a', long)]
    pub agent: Vec<String>,

    /// Number of worktree instances to create.
    /// Can be used with zero or one --agent. Incompatible with --foreach.
    #[arg(
        short = 'n',
        long,
        value_parser = clap::value_parser!(u32).range(1..),
        conflicts_with = "foreach"
    )]
    pub count: Option<u32>,

    /// Generate multiple worktrees from a variable matrix.
    /// Format: "var1:valA,valB;var2:valX,valY". Lists must have equal length.
    /// Incompatible with --agent and --count.
    #[arg(long, conflicts_with_all = ["agent", "count"])]
    pub foreach: Option<String>,

    /// Template for branch names in multi-worktree modes.
    /// Variables: {{ base_name }}, {{ agent }}, {{ num }}, {{ foreach_vars }}.
    #[arg(
        long,
        default_value = r#"{{ base_name }}{% if agent %}-{{ agent | slugify }}{% endif %}{% for key in foreach_vars %}-{{ foreach_vars[key] | slugify }}{% endfor %}{% if num %}-{{ num }}{% endif %}"#
    )]
    pub branch_template: String,

    /// Maximum number of worktrees to run concurrently.
    /// When set, waits for a slot to open before creating new worktrees.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_concurrent: Option<u32>,
}

#[derive(clap::Args, Debug)]
pub struct RescueArgs {
    /// Move uncommitted changes from the current worktree to the new worktree
    #[arg(short = 'w', long, conflicts_with_all = ["count", "foreach"])]
    pub with_changes: bool,

    /// Interactively select which changes to move (only applies with --with-changes)
    #[arg(long, requires = "with_changes")]
    pub patch: bool,

    /// Also move untracked files (only applies with --with-changes)
    #[arg(short = 'u', long, requires = "with_changes")]
    pub include_untracked: bool,
}
