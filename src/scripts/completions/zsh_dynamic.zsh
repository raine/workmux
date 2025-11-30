# Dynamic branch completion - runs git only when TAB is pressed
_workmux_branches() {
    local branches
    branches=("${(@f)$(workmux __complete-branches 2>/dev/null)}")
    compadd -a branches
}

# Dynamic git branch completion for add command
_workmux_git_branches() {
    local branches
    branches=("${(@f)$(workmux __complete-git-branches 2>/dev/null)}")
    compadd -a branches
}

# Override completion for commands that take branch names
_workmux_dynamic() {
    # Get the subcommand (second word)
    local cmd="${words[2]}"

    # Only handle commands that need dynamic branch completion
    case "$cmd" in
        open|merge|remove|rm|path)
            # If completing a flag, use generated completions
            if [[ "${words[CURRENT]}" == -* ]]; then
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
            if [[ "${words[CURRENT]}" == -* ]]; then
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
}

compdef _workmux_dynamic workmux
