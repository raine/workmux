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
